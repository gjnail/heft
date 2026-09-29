//! Installed programs workspace: sizes, uninstalling (with a leftover
//! check afterwards) and updates through winget.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use eframe::egui::{self, vec2, Align, Layout, RichText};

use super::{big_button, card, heading, Cx, Event, AMBER, GREEN};
use crate::programs::{self, Leftover, Program};
use crate::util::{fmt_count, fmt_size};
use crate::winget::{self, Upgrade};
use crate::winsys::PathState;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Sort {
    #[default]
    Size,
    Name,
    Installed,
    Publisher,
}

enum Outcome {
    /// The entry is still registered: cancelled, failed, or still running.
    StillInstalled,
    Removed(Vec<Leftover>),
}

struct Uninstalling {
    program: Program,
    rx: Receiver<Outcome>,
    stop: Arc<AtomicBool>,
    started: Instant,
}

struct Leftovers {
    program: Program,
    items: Vec<Leftover>,
    checked: HashSet<usize>,
}

#[derive(Default)]
pub struct State {
    programs: Vec<Program>,
    loading: Option<Receiver<Vec<Program>>>,
    sizes: Option<Receiver<(String, u64)>>,
    loaded: bool,
    filter: String,
    sort: Sort,
    upgrades: Option<Result<Vec<Upgrade>, String>>,
    upgrades_job: Option<Receiver<Result<Vec<Upgrade>, String>>>,
    /// Program registry key → index into `upgrades`.
    badges: HashMap<String, usize>,
    show_updates: bool,
    confirm_uninstall: Option<Program>,
    confirm_remove_entry: Option<Program>,
    confirm_update_all: bool,
    uninstalling: Option<Uninstalling>,
    leftovers: Option<Leftovers>,
    trash_job: Option<Receiver<Result<u64, String>>>,
}

impl State {
    fn reload(&mut self, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(programs::list());
            ctx.request_repaint();
        });
        self.loading = Some(rx);
    }

    /// Measure install folders in the background, largest programs first.
    /// Another program installed inside a folder isn't counted towards it.
    fn measure(&mut self, ctx: &egui::Context) {
        let mut jobs: Vec<(String, String, u64, Vec<String>)> = self
            .programs
            .iter()
            .filter_map(|p| {
                let nested = programs::nested_locations(p, &self.programs);
                p.location.clone().map(|l| (p.key.clone(), l, p.estimated, nested))
            })
            .collect();
        jobs.sort_by_key(|j| std::cmp::Reverse(j.2));
        let (tx, rx) = crossbeam_channel::unbounded();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            use rayon::prelude::*;
            jobs.par_iter().for_each(|(key, loc, _, nested)| {
                let size = programs::folder_size_excluding(std::path::Path::new(loc), nested);
                let _ = tx.send((key.clone(), size));
                ctx.request_repaint();
            });
        });
        self.sizes = Some(rx);
    }

    fn check_updates(&mut self, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = if winget::available() {
                winget::list_upgrades()
            } else {
                Err("winget isn't installed. Get “App Installer” from the Microsoft Store to enable updates.".into())
            };
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        self.upgrades_job = Some(rx);
    }

    /// Match winget's (possibly shortened) names to installed programs.
    fn match_upgrades(&mut self) {
        self.badges.clear();
        let Some(Ok(ups)) = &self.upgrades else { return };
        for p in &self.programs {
            let name = p.name.to_lowercase();
            let hit = ups.iter().position(|u| {
                let un = u.name.to_lowercase();
                match un.strip_suffix('…') {
                    Some(prefix) => name.starts_with(prefix),
                    None => un == name,
                }
            });
            if let Some(i) = hit {
                self.badges.insert(p.key.clone(), i);
            }
        }
    }

    fn start_uninstall(&mut self, p: Program, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let stop = Arc::new(AtomicBool::new(false));
        let (prog, stop2, ctx) = (p.clone(), stop.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = programs::run_uninstaller(&prog);
            // Some uninstallers hand off to a copy of themselves and exit
            // at once, so give the entry a while to disappear.
            let deadline = Instant::now() + Duration::from_secs(120);
            while programs::still_installed(&prog) && Instant::now() < deadline && !stop2.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(700));
            }
            let outcome = if programs::still_installed(&prog) {
                Outcome::StillInstalled
            } else {
                // Checked against what's still installed now, so a shared
                // folder is never offered.
                Outcome::Removed(programs::find_leftovers(&prog, &programs::list()))
            };
            let _ = tx.send(outcome);
            ctx.request_repaint();
        });
        self.uninstalling = Some(Uninstalling { program: p, rx, stop, started: Instant::now() });
    }

    fn poll(&mut self, ctx: &egui::Context, cx: &mut Cx) {
        let mut repaint = false;
        if let Some(rx) = &self.loading {
            match rx.try_recv() {
                Ok(v) => {
                    self.programs = v;
                    self.loading = None;
                    self.match_upgrades();
                    self.measure(ctx);
                }
                Err(_) => repaint = true,
            }
        }
        if let Some(rx) = &self.sizes {
            for (key, size) in rx.try_iter() {
                if let Some(p) = self.programs.iter_mut().find(|p| p.key == key) {
                    p.measured = Some(size);
                }
            }
        }
        if let Some(rx) = &self.upgrades_job {
            match rx.try_recv() {
                Ok(r) => {
                    self.show_updates = r.as_ref().is_ok_and(|u| !u.is_empty());
                    self.upgrades = Some(r);
                    self.upgrades_job = None;
                    self.match_upgrades();
                }
                Err(_) => repaint = true,
            }
        }
        if let Some(u) = &self.uninstalling {
            match u.rx.try_recv() {
                Ok(Outcome::StillInstalled) => {
                    cx.toast(format!("{} is still installed.", u.program.name), false);
                    self.uninstalling = None;
                    self.reload(ctx);
                }
                Ok(Outcome::Removed(items)) => {
                    let program = u.program.clone();
                    self.uninstalling = None;
                    if items.is_empty() {
                        cx.toast(format!("{} was uninstalled. Nothing was left behind.", program.name), false);
                    } else {
                        let checked = (0..items.len()).filter(|&i| items[i].confident).collect();
                        self.leftovers = Some(Leftovers { program, items, checked });
                    }
                    self.reload(ctx);
                }
                Err(_) => repaint = true,
            }
        }
        if let Some(rx) = &self.trash_job {
            match rx.try_recv() {
                Ok(Ok(bytes)) => {
                    cx.toast(format!("Moved leftovers to the Recycle Bin ({})", fmt_size(bytes)), false);
                    self.trash_job = None;
                }
                Ok(Err(e)) => {
                    cx.toast(format!("Some leftovers couldn't be removed: {e}"), true);
                    self.trash_job = None;
                }
                Err(_) => repaint = true,
            }
        }
        if repaint {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, mut cx: Cx) {
        let ctx = ui.ctx().clone();
        if !self.loaded {
            self.loaded = true;
            self.reload(&ctx);
        }
        self.poll(&ctx, &mut cx);

        egui::CentralPanel::default().show(ui, |ui| {
            heading(
                ui,
                "Programs",
                "Everything installed, with the space it really takes. Uninstalling runs the program's own uninstaller, then Heft checks what it left behind.",
            );
            self.summary(ui, &ctx, &mut cx);
            ui.add_space(8.0);
            if self.show_updates {
                self.updates_list(ui, &mut cx);
                ui.add_space(8.0);
            }
            self.program_list(ui, &mut cx);
        });
        self.modals(&ctx, &mut cx);
    }

    fn summary(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, cx: &mut Cx) {
        let total: u64 = self.programs.iter().map(|p| p.size()).sum();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{} programs", self.programs.len())).size(18.0).strong());
                        if self.loading.is_some() {
                            ui.spinner();
                        }
                    });
                    ui.label(RichText::new(format!("{} in total", fmt_size(total))).weak());
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let checking = self.upgrades_job.is_some();
                    let label = match &self.upgrades {
                        Some(Ok(u)) if !u.is_empty() => format!("{} updates", u.len()),
                        _ => "Check for updates".into(),
                    };
                    if big_button(ui, &label, !checking).clicked() {
                        if matches!(&self.upgrades, Some(Ok(u)) if !u.is_empty()) {
                            self.show_updates = !self.show_updates;
                        } else {
                            self.check_updates(ctx);
                        }
                    }
                    if checking {
                        ui.spinner();
                        ui.label(RichText::new("Asking winget…").weak());
                    }
                    if ui.add_enabled(self.loading.is_none(), egui::Button::new("⟳  Refresh")).clicked() {
                        self.reload(ctx);
                    }
                });
            });
            if let Some(Err(e)) = &self.upgrades {
                ui.colored_label(AMBER, e);
            }
            if let Some(Ok(u)) = &self.upgrades
                && u.is_empty()
            {
                ui.colored_label(GREEN, "Everything winget knows about is up to date.");
            }
            if let Some(u) = &self.uninstalling {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!(
                        "Waiting for the {} uninstaller… ({}s)",
                        u.program.name,
                        u.started.elapsed().as_secs()
                    ));
                    if ui.small_button("Stop waiting").clicked() {
                        u.stop.store(true, Ordering::Relaxed);
                    }
                });
            }
            if self.trash_job.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Moving leftovers to the Recycle Bin…");
                });
            }
            let _ = cx;
        });
    }

    fn updates_list(&mut self, ui: &mut egui::Ui, cx: &mut Cx) {
        let Some(Ok(ups)) = &self.upgrades else { return };
        let ups = ups.clone();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} updates available", ups.len())).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Update all…").on_hover_text("Runs winget upgrade --all in a console window").clicked() {
                        self.confirm_update_all = true;
                    }
                });
            });
            ui.label(RichText::new("Installers run in a console window so you can see their prompts and license terms.").weak().size(11.5));
            egui::ScrollArea::vertical().id_salt("upgrades").max_height(200.0).show(ui, |ui| {
                egui::Grid::new("upgrade_grid").num_columns(4).striped(true).spacing([16.0, 4.0]).show(ui, |ui| {
                    for u in &ups {
                        ui.label(&u.name);
                        ui.label(RichText::new(&u.version).weak());
                        ui.label(RichText::new(format!("to {}", u.available)).color(GREEN));
                        let b = ui.add_enabled(u.id_is_exact(), egui::Button::new("Update"));
                        if b.on_disabled_hover_text("winget shortened this package's id; use Update all").clicked() {
                            match winget::upgrade(&u.id) {
                                Ok(_) => cx.toast(format!("Updating {} in a separate window", u.name), false),
                                Err(e) => cx.toast(format!("Could not start winget: {e}"), true),
                            }
                        }
                        ui.end_row();
                    }
                });
            });
        });
    }

    fn program_list(&mut self, ui: &mut egui::Ui, cx: &mut Cx) {
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Search name or publisher").desired_width(220.0));
            ui.label("Sort by");
            egui::ComboBox::from_id_salt("program_sort")
                .selected_text(match self.sort {
                    Sort::Size => "Size",
                    Sort::Name => "Name",
                    Sort::Installed => "Install date",
                    Sort::Publisher => "Publisher",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.sort, Sort::Size, "Size");
                    ui.selectable_value(&mut self.sort, Sort::Name, "Name");
                    ui.selectable_value(&mut self.sort, Sort::Installed, "Install date");
                    ui.selectable_value(&mut self.sort, Sort::Publisher, "Publisher");
                });
            if self.sizes.is_some() && self.programs.iter().any(|p| p.location.is_some() && p.measured.is_none()) {
                ui.spinner();
                ui.label(RichText::new("measuring folders…").weak());
            }
        });
        ui.add_space(4.0);

        let filter = self.filter.to_lowercase();
        let mut order: Vec<usize> = (0..self.programs.len())
            .filter(|&i| {
                let p = &self.programs[i];
                filter.is_empty() || p.name.to_lowercase().contains(&filter) || p.publisher.to_lowercase().contains(&filter)
            })
            .collect();
        let ps = &self.programs;
        match self.sort {
            Sort::Size => order.sort_by_key(|&i| std::cmp::Reverse(ps[i].size())),
            Sort::Name => order.sort_by_key(|&i| ps[i].name.to_lowercase()),
            Sort::Installed => order.sort_by(|&a, &b| ps[b].installed.cmp(&ps[a].installed)),
            Sort::Publisher => order.sort_by_key(|&i| (ps[i].publisher.to_lowercase(), ps[i].name.to_lowercase())),
        }

        let busy = self.uninstalling.is_some();
        egui::ScrollArea::vertical().id_salt("program_rows").auto_shrink([false, false]).show_rows(ui, 46.0, order.len(), |ui, range| {
            for &i in &order[range] {
                let p = self.programs[i].clone();
                let upgrade = self.badges.get(&p.key).and_then(|&u| self.upgrades.as_ref()?.as_ref().ok()?.get(u).cloned());
                ui.horizontal(|ui| {
                    ui.set_height(42.0);
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&p.name).strong());
                            ui.label(RichText::new(&p.version).weak());
                            if let Some(u) = &upgrade {
                                ui.label(RichText::new(format!("⬆ {}", u.available)).color(GREEN)).on_hover_text("An update is available through winget");
                            }
                            if p.uninstaller_state() == PathState::Missing {
                                ui.colored_label(AMBER, "⚠ uninstaller missing");
                            }
                        });
                        let mut meta = Vec::new();
                        if !p.publisher.is_empty() {
                            meta.push(p.publisher.clone());
                        }
                        if !p.installed.is_empty() {
                            meta.push(format!("installed {}", p.installed));
                        }
                        if let Some(l) = &p.location {
                            meta.push(l.clone());
                        }
                        ui.add(egui::Label::new(RichText::new(meta.join("  ·  ")).weak().size(11.5)).truncate());
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.menu_button("☰", |ui| self.program_menu(ui, &p, upgrade.as_ref(), cx));
                        let b = ui.add_enabled(!busy, egui::Button::new("Uninstall").min_size(vec2(84.0, 24.0)));
                        if b.clicked() {
                            self.confirm_uninstall = Some(p.clone());
                        }
                        let size = match (p.measured, p.estimated) {
                            (Some(m), _) if m > 0 => RichText::new(fmt_size(m)).strong(),
                            (_, e) if e > 0 => RichText::new(format!("~{}", fmt_size(e))),
                            _ => RichText::new("-").weak(),
                        };
                        ui.label(size).on_hover_text(if p.measured.is_some_and(|m| m > 0) {
                            "Measured size of the install folder"
                        } else {
                            "Size reported by the installer"
                        });
                    });
                });
                ui.separator();
            }
        });
    }

    fn program_menu(&mut self, ui: &mut egui::Ui, p: &Program, upgrade: Option<&Upgrade>, cx: &mut Cx) {
        if let Some(u) = upgrade
            && ui.add_enabled(u.id_is_exact(), egui::Button::new(format!("Update to {}", u.available))).clicked()
        {
            match winget::upgrade(&u.id) {
                Ok(_) => cx.toast(format!("Updating {} in a separate window", p.name), false),
                Err(e) => cx.toast(format!("Could not start winget: {e}"), true),
            }
            ui.close();
        }
        if let Some(loc) = &p.location {
            if ui.button("Open install folder").clicked() {
                crate::platform::open_path(loc);
                ui.close();
            }
            if ui.button("Show in disk map").on_hover_text("Scan the install folder in Disk usage").clicked() {
                cx.events.push(Event::Scan(loc.clone()));
                ui.close();
            }
        }
        if ui.button("Copy uninstall command").clicked() {
            ui.ctx().copy_text(p.uninstall.clone());
            ui.close();
        }
        if p.uninstaller_state() == PathState::Missing {
            ui.separator();
            let locked = p.hive.needs_admin() && !cx.elevated;
            let b = ui.add_enabled(!locked, egui::Button::new("Remove from the list…"));
            if b.on_disabled_hover_text("Needs administrator rights").clicked() {
                self.confirm_remove_entry = Some(p.clone());
                ui.close();
            }
        }
    }

    fn modals(&mut self, ctx: &egui::Context, cx: &mut Cx) {
        if self.confirm_update_all {
            let n = self.upgrades.as_ref().and_then(|u| u.as_ref().ok()).map_or(0, |u| u.len());
            let mut choice = None;
            let resp = egui::Modal::new(egui::Id::new("confirm_update_all")).show(ctx, |ui| {
                ui.set_width(460.0);
                ui.heading(format!("Update {n} programs?"));
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "winget downloads and runs each installer in a console window. Some may ask for administrator rights \
                         or to accept license terms there. Close the programs you're updating first.",
                    )
                    .weak(),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Update all").strong()).clicked() {
                        choice = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(false);
                    }
                });
            });
            if choice == Some(true) {
                self.confirm_update_all = false;
                match winget::upgrade_all() {
                    Ok(_) => cx.toast("Updating in a separate window. Check for updates again when it's done.", false),
                    Err(e) => cx.toast(format!("Could not start winget: {e}"), true),
                }
            } else if choice == Some(false) || resp.should_close() {
                self.confirm_update_all = false;
            }
        }

        if let Some(p) = self.confirm_uninstall.clone() {
            let mut choice = None;
            let resp = egui::Modal::new(egui::Id::new("confirm_uninstall")).show(ctx, |ui| {
                ui.set_width(460.0);
                ui.heading(format!("Uninstall {}?", p.name));
                ui.add_space(4.0);
                ui.label(RichText::new("Its own uninstaller will open. When it finishes, Heft looks for folders it left behind.").weak());
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Uninstall").strong()).clicked() {
                        choice = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(false);
                    }
                });
            });
            if choice == Some(true) {
                self.confirm_uninstall = None;
                self.start_uninstall(p, ctx);
            } else if choice == Some(false) || resp.should_close() {
                self.confirm_uninstall = None;
            }
        }

        if let Some(p) = self.confirm_remove_entry.clone() {
            let mut choice = None;
            let resp = egui::Modal::new(egui::Id::new("confirm_remove_entry")).show(ctx, |ui| {
                ui.set_width(460.0);
                ui.heading(format!("Remove {} from the list?", p.name));
                ui.add_space(4.0);
                ui.label(RichText::new("Its uninstaller no longer exists, so it can't be uninstalled normally. Heft deletes the leftover registry entry after saving a .reg backup.").weak());
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Remove").strong()).clicked() {
                        choice = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(false);
                    }
                });
            });
            if choice == Some(true) {
                self.confirm_remove_entry = None;
                match programs::remove_entry(&p) {
                    Ok(backup) => cx.toast(format!("Removed. Backup saved to {}", backup.display()), false),
                    Err(e) => cx.toast(format!("Could not remove the entry: {e}"), true),
                }
                self.reload(ctx);
            } else if choice == Some(false) || resp.should_close() {
                self.confirm_remove_entry = None;
            }
        }

        let mut close = false;
        let mut trash: Option<Vec<std::path::PathBuf>> = None;
        if let Some(l) = &mut self.leftovers {
            let resp = egui::Modal::new(egui::Id::new("leftovers")).show(ctx, |ui| {
                ui.set_width(560.0);
                ui.heading(format!("{} left some things behind", l.program.name));
                ui.add_space(4.0);
                ui.label(RichText::new("Pick what to move to the Recycle Bin. Folders that only share the program's name are unticked, so check them first.").weak());
                ui.add_space(6.0);
                for (i, item) in l.items.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let mut on = l.checked.contains(&i);
                        if ui.checkbox(&mut on, "").changed() {
                            if on {
                                l.checked.insert(i);
                            } else {
                                l.checked.remove(&i);
                            }
                        }
                        ui.label(RichText::new(fmt_size(item.size)).strong());
                        ui.vertical(|ui| {
                            ui.add(egui::Label::new(RichText::new(item.path.to_string_lossy()).monospace().size(11.5)).truncate());
                            ui.label(RichText::new(item.reason).weak().size(11.0));
                        });
                    });
                }
                let total: u64 = l.checked.iter().map(|&i| l.items[i].size).sum();
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let b = ui.add_enabled(!l.checked.is_empty(), egui::Button::new(RichText::new(format!("Move {} to the Recycle Bin", fmt_size(total))).strong()));
                    if b.clicked() {
                        trash = Some(l.checked.iter().map(|&i| l.items[i].path.clone()).collect());
                    }
                    if ui.button("Keep everything").clicked() {
                        close = true;
                    }
                });
                ui.label(RichText::new(format!("{} folders found", fmt_count(l.items.len() as u64))).weak().size(11.0));
            });
            if resp.should_close() {
                close = true;
            }
        }
        if let Some(paths) = trash {
            let sizes: u64 = self.leftovers.as_ref().map(|l| l.checked.iter().map(|&i| l.items[i].size).sum()).unwrap_or(0);
            let (tx, rx) = crossbeam_channel::bounded(1);
            let ctx2 = ctx.clone();
            std::thread::spawn(move || {
                let r = trash::delete_all(&paths).map(|_| sizes).map_err(|e| e.to_string());
                let _ = tx.send(r);
                ctx2.request_repaint();
            });
            self.trash_job = Some(rx);
            close = true;
        }
        if close {
            self.leftovers = None;
        }
    }
}
