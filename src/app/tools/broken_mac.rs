//! Broken items workspace (macOS), the counterpart of Windows' registry
//! check: things that point at apps and files that no longer exist.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::Receiver;
use eframe::egui::{self, Align, Layout, RichText};

use super::{big_button, card, heading, Cx, AMBER, GREEN};
use crate::mac::broken::{self, Backup, Category, FixReport, Issue, RestoreReport, Saved};
use crate::util::fmt_size;

/// A backup the user picked to restore, with what's in it.
struct Restore {
    path: PathBuf,
    items: Result<Vec<Saved>, String>,
}

/// A fix in progress (it may be waiting for the password).
struct Fixing {
    /// The issues it covers.
    picked: HashSet<usize>,
    rx: Receiver<Result<FixReport, String>>,
}

#[derive(Default)]
pub struct State {
    issues: Vec<Issue>,
    scanning: Option<Receiver<Vec<Issue>>>,
    scanned: bool,
    checked: HashSet<usize>,
    collapsed: HashSet<Category>,
    report: Option<FixReport>,
    confirm_fix: bool,
    fixing: Option<Fixing>,
    backups: Vec<Backup>,
    backups_loaded: bool,
    confirm_restore: Option<Restore>,
    restoring: Option<Receiver<RestoreReport>>,
}

impl State {
    fn scan(&mut self, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(broken::scan());
            ctx.request_repaint();
        });
        self.scanning = Some(rx);
        self.report = None;
    }

    fn busy(&self) -> bool {
        self.scanning.is_some() || self.fixing.is_some() || self.restoring.is_some()
    }

    fn poll(&mut self, ctx: &egui::Context, cx: &mut Cx) {
        if let Some(rx) = &self.scanning {
            match rx.try_recv() {
                Ok(v) => {
                    self.checked = (0..v.len()).filter(|&i| v[i].category.default_on()).collect();
                    // Inert categories start collapsed so the useful ones are visible.
                    self.collapsed = Category::ALL.into_iter().filter(|c| !c.default_on()).collect();
                    self.issues = v;
                    self.scanning = None;
                    self.scanned = true;
                }
                Err(_) => ctx.request_repaint_after(Duration::from_millis(150)),
            }
        }
        if let Some(f) = &self.fixing {
            match f.rx.try_recv() {
                Ok(Ok(r)) => {
                    let picked = f.picked.clone();
                    let mut i = 0;
                    self.issues.retain(|_| {
                        let keep = !picked.contains(&i);
                        i += 1;
                        keep
                    });
                    self.checked.clear();
                    self.report = Some(r);
                    self.backups = broken::backups();
                    self.fixing = None;
                }
                Ok(Err(e)) => {
                    cx.toast(e, true);
                    self.fixing = None;
                }
                Err(_) => ctx.request_repaint_after(Duration::from_millis(150)),
            }
        }
        if let Some(rx) = &self.restoring {
            match rx.try_recv() {
                Ok(r) => {
                    if r.failed.is_empty() {
                        cx.toast(format!("Restored {} item(s)", r.restored), false);
                    } else {
                        cx.toast(format!("Restored {} item(s). Not restored: {}", r.restored, r.failed.join("; ")), true);
                    }
                    self.restoring = None;
                }
                Err(_) => ctx.request_repaint_after(Duration::from_millis(150)),
            }
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, mut cx: Cx) {
        let ctx = ui.ctx().clone();
        if !self.scanned && self.scanning.is_none() {
            // Read-only, so there's nothing to wait for.
            self.scan(&ctx);
        }
        if !self.backups_loaded {
            self.backups_loaded = true;
            self.backups = broken::backups();
        }
        self.poll(&ctx, &mut cx);

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("broken_main").auto_shrink([false, false]).show(ui, |ui| {
                heading(
                    ui,
                    "Broken items",
                    "Login items, launch agents, links and records that point to apps and files that no longer exist.",
                );
                card(ui, |ui| {
                    ui.label(RichText::new("What this is, and isn't").strong());
                    ui.label(
                        RichText::new(
                            "Removing these won't make your Mac faster; anyone who says otherwise is selling something. \
                             It does stop macOS trying to start programs that are gone at every login, and tidies up what deleted apps left behind. \
                             Heft only reports an item when the file it names is provably gone (never for disks that aren't connected, or folders it \
                             can't look in), and saves a backup before every change.",
                        )
                        .weak(),
                    );
                });
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    if big_button(ui, "Scan for issues", !self.busy()).clicked() {
                        self.scan(&ctx);
                    }
                    if self.scanning.is_some() {
                        ui.spinner();
                        ui.label("Checking…");
                    } else if self.fixing.is_some() || self.restoring.is_some() {
                        ui.spinner();
                        ui.label("Working… macOS may ask for your administrator password.");
                    }
                    let n = self.checked.len();
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if !self.issues.is_empty() && big_button(ui, &format!("Fix {n} selected"), n > 0 && !self.busy()).clicked() {
                            self.confirm_fix = true;
                        }
                    });
                });
                if self.issues.iter().any(Issue::needs_admin) {
                    cx.admin_notice(ui, "Fixing items outside your home folder asks for your administrator password, once for all of them.");
                }

                if let Some(r) = &self.report {
                    ui.add_space(6.0);
                    card(ui, |ui| {
                        ui.label(RichText::new(format!("Fixed {} issue(s)", r.fixed)).strong().color(GREEN));
                        if let Some(b) = &r.backup {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("Backup: {}", b.display())).weak().size(11.5));
                                if ui.small_button("Show").clicked() {
                                    crate::platform::reveal(&b.to_string_lossy());
                                }
                            });
                        }
                        for f in &r.failed {
                            ui.colored_label(AMBER, f);
                        }
                    });
                }

                ui.add_space(8.0);
                if self.scanned && self.issues.is_empty() && self.scanning.is_none() {
                    ui.label(RichText::new("No issues found.").size(16.0).color(GREEN));
                }
                self.issue_list(ui);

                ui.add_space(14.0);
                ui.separator();
                self.backup_list(ui);
            });
        });
        self.modals(&ctx);
    }

    fn issue_list(&mut self, ui: &mut egui::Ui) {
        for cat in Category::ALL {
            let members: Vec<usize> = (0..self.issues.len()).filter(|&i| self.issues[i].category == cat).collect();
            if members.is_empty() {
                continue;
            }
            let on = members.iter().filter(|i| self.checked.contains(i)).count();
            ui.horizontal(|ui| {
                let collapsed = self.collapsed.contains(&cat);
                if ui.add(egui::Button::new(if collapsed { "⏵" } else { "⏷" }).frame(false)).clicked() && !self.collapsed.remove(&cat) {
                    self.collapsed.insert(cat);
                }
                let mut all = on == members.len();
                let cb = egui::Checkbox::new(&mut all, RichText::new(format!("{} ({})", cat.label(), members.len())).strong())
                    .indeterminate(on > 0 && on < members.len());
                if ui.add_enabled(self.fixing.is_none(), cb).on_hover_text(cat.about()).changed() {
                    for &i in &members {
                        if all {
                            self.checked.insert(i);
                        } else {
                            self.checked.remove(&i);
                        }
                    }
                }
            });
            if self.collapsed.contains(&cat) {
                continue;
            }
            for &i in &members {
                let issue = &self.issues[i];
                ui.horizontal(|ui| {
                    ui.add_space(28.0);
                    let mut on = self.checked.contains(&i);
                    let cb = ui.add_enabled(self.fixing.is_none(), egui::Checkbox::without_text(&mut on));
                    let cb = if issue.needs_admin() { cb.on_hover_text("Asks for your administrator password") } else { cb };
                    if cb.changed() {
                        if on {
                            self.checked.insert(i);
                        } else {
                            self.checked.remove(&i);
                        }
                    }
                    ui.vertical(|ui| {
                        ui.add(egui::Label::new(&issue.detail).truncate());
                        ui.add(egui::Label::new(RichText::new(&issue.location).monospace().size(10.5).weak()).truncate());
                    });
                });
            }
            ui.add_space(6.0);
        }
    }

    fn backup_list(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(format!("Backups ({})", self.backups.len())).id_salt("broken_backups").show(ui, |ui| {
            ui.label(
                RichText::new(
                    "Everything Heft removes, here and in Login items, is saved here first. Restoring puts it back where it was.",
                )
                .weak()
                .size(11.5),
            );
            ui.horizontal(|ui| {
                if ui.small_button("Open folder").clicked() {
                    let dir = crate::mac::backup_dir();
                    let _ = std::fs::create_dir_all(&dir);
                    crate::platform::open_path(&dir.to_string_lossy());
                }
                if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                    self.backups = broken::backups();
                }
            });
            for b in self.backups.clone().iter().take(50) {
                ui.horizontal(|ui| {
                    let name = b.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    ui.label(RichText::new(name).monospace().size(11.5));
                    ui.label(RichText::new(format!("{} · {}", crate::platform::fmt_datetime(b.modified), fmt_size(b.size))).weak().size(11.5));
                    if ui.add_enabled(!self.busy(), egui::Button::new("Restore…").small()).clicked() {
                        self.confirm_restore = Some(Restore { path: b.path.clone(), items: broken::read_backup(&b.path) });
                    }
                });
            }
        });
    }

    fn modals(&mut self, ctx: &egui::Context) {
        if self.confirm_fix {
            let picked: Vec<usize> = self.checked.iter().copied().collect();
            let mut choice = None;
            let resp = egui::Modal::new(egui::Id::new("confirm_broken_fix")).show(ctx, |ui| {
                ui.set_width(480.0);
                ui.heading(format!("Fix {} issue(s)?", picked.len()));
                ui.add_space(4.0);
                for cat in Category::ALL {
                    let n = picked.iter().filter(|&&i| self.issues[i].category == cat).count();
                    if n > 0 {
                        ui.label(format!("• {}", what_happens(cat, n)));
                    }
                }
                let admin = picked.iter().filter(|&&i| self.issues[i].needs_admin()).count();
                ui.add_space(4.0);
                if admin > 0 {
                    ui.label(RichText::new(format!("macOS asks for your administrator password once, for the {admin} item(s) outside your home folder.")).weak());
                }
                ui.label(
                    RichText::new(
                        "Everything is saved to a backup first, so this can be undone from the Backups list. \
                         (Open With entries need nothing restored: macOS registers an app again when it comes back.)",
                    )
                    .weak(),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Back up and fix").strong()).clicked() {
                        choice = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(false);
                    }
                });
            });
            if choice == Some(true) {
                self.confirm_fix = false;
                let issues: Vec<Issue> = picked.iter().map(|&i| self.issues[i].clone()).collect();
                let (tx, rx) = crossbeam_channel::bounded(1);
                let c = ctx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(broken::fix(&issues));
                    c.request_repaint();
                });
                self.fixing = Some(Fixing { picked: picked.into_iter().collect(), rx });
            } else if choice == Some(false) || resp.should_close() {
                self.confirm_fix = false;
            }
        }

        let Some(r) = &self.confirm_restore else { return };
        let mut choice = None;
        let resp = egui::Modal::new(egui::Id::new("confirm_broken_restore")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading("Restore this backup?");
            ui.add_space(4.0);
            ui.label(RichText::new(r.path.to_string_lossy()).monospace().size(11.5));
            ui.add_space(6.0);
            let mut can = 0;
            match &r.items {
                Err(e) => {
                    ui.colored_label(AMBER, format!("⚠ {e}"));
                }
                Ok(items) => {
                    let (ok, blocked): (Vec<&Saved>, Vec<&Saved>) = items.iter().partition(|s| s.blocker().is_none());
                    can = ok.len();
                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                        if !ok.is_empty() {
                            ui.label(RichText::new("Put back").strong());
                            for s in &ok {
                                let admin = if s.needs_admin() { "  (administrator password)" } else { "" };
                                ui.label(format!("• {}{admin}", s.describe()));
                            }
                        }
                        if !blocked.is_empty() {
                            ui.add_space(4.0);
                            ui.label(RichText::new("Can't be put back").strong());
                            for s in &blocked {
                                ui.label(RichText::new(format!("• {}: {}", s.describe(), s.blocker().unwrap_or_default())).weak());
                            }
                        }
                    });
                    ui.add_space(4.0);
                    ui.label(RichText::new("Launch agents and daemons load again the next time you sign in or restart.").weak());
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(can > 0, egui::Button::new(RichText::new("Restore").strong())).clicked() {
                    choice = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
            });
        });
        if choice == Some(true) {
            let items = self.confirm_restore.take().and_then(|r| r.items.ok()).unwrap_or_default();
            let (tx, rx) = crossbeam_channel::bounded(1);
            let c = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(broken::restore(&items));
                c.request_repaint();
            });
            self.restoring = Some(rx);
        } else if choice == Some(false) || resp.should_close() {
            self.confirm_restore = None;
        }
    }
}

/// What fixing `n` issues of a kind does, for the confirmation.
fn what_happens(cat: Category, n: usize) -> String {
    match cat {
        Category::LaunchJobs => format!(
            "{n} launch agent(s) or daemon(s): stopped if loaded, and the .plist moved to the Trash (or deleted, for ones in /Library)"
        ),
        Category::LoginItems => format!("{n} login item(s): taken off the list"),
        Category::Dock => format!("{n} Dock icon(s): taken out of the Dock, which then restarts"),
        Category::Links => format!("{n} link(s): deleted"),
        Category::OpenWith => format!("{n} Open With entr{}: unregistered from Launch Services", if n == 1 { "y" } else { "ies" }),
        Category::Receipts => format!("{n} package receipt(s): forgotten with pkgutil; no installed files are touched"),
    }
}
