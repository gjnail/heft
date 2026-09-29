//! Registry issues workspace.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::Receiver;
use eframe::egui::{self, Align, Layout, RichText};

use super::{big_button, card, heading, Cx, AMBER, GREEN};
use crate::regclean::{self, Backup, Category, FixReport, Issue};
use crate::util::fmt_size;

#[derive(Default)]
pub struct State {
    issues: Vec<Issue>,
    scanning: Option<Receiver<Vec<Issue>>>,
    scanned: bool,
    checked: HashSet<usize>,
    collapsed: HashSet<Category>,
    report: Option<FixReport>,
    confirm_fix: bool,
    backups: Vec<Backup>,
    backups_loaded: bool,
    confirm_restore: Option<PathBuf>,
}

impl State {
    fn scan(&mut self, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(regclean::scan());
            ctx.request_repaint();
        });
        self.scanning = Some(rx);
        self.report = None;
    }

    fn fixable(&self, i: &Issue, elevated: bool) -> bool {
        elevated || !i.needs_admin()
    }

    pub fn show(&mut self, ui: &mut egui::Ui, mut cx: Cx) {
        let ctx = ui.ctx().clone();
        if !self.scanned && self.scanning.is_none() {
            // Read-only and quick, so there's nothing to wait for.
            self.scan(&ctx);
        }
        if !self.backups_loaded {
            self.backups_loaded = true;
            self.backups = regclean::backups();
        }
        if let Some(rx) = &self.scanning {
            match rx.try_recv() {
                Ok(v) => {
                    self.checked =
                        (0..v.len()).filter(|&i| v[i].category.default_on() && self.fixable(&v[i], cx.elevated)).collect();
                    // Big inert categories start collapsed so the useful ones are visible.
                    self.collapsed = Category::ALL.into_iter().filter(|c| !c.default_on()).collect();
                    self.issues = v;
                    self.scanning = None;
                    self.scanned = true;
                }
                Err(_) => ctx.request_repaint_after(Duration::from_millis(150)),
            }
        }

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("registry_main").auto_shrink([false, false]).show(ui, |ui| {
                heading(
                    ui,
                    "Registry issues",
                    "Entries that point to programs, files or folders that no longer exist.",
                );
                card(ui, |ui| {
                    ui.label(RichText::new("What this is, and isn't").strong());
                    ui.label(
                        RichText::new(
                            "Cleaning the registry won't make your PC faster; anyone who says otherwise is selling something. \
                             It does tidy up entries that make Windows try to start missing programs, or that clutter Apps & Features. \
                             Heft only reports an entry when the file it names is provably gone, never touches COM or file-type \
                             registrations, and saves a .reg backup before every change.",
                        )
                        .weak(),
                    );
                });
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    if big_button(ui, "Scan for issues", self.scanning.is_none()).clicked() {
                        self.scan(&ctx);
                    }
                    if self.scanning.is_some() {
                        ui.spinner();
                        ui.label("Checking the registry…");
                    }
                    let n = self.checked.len();
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if !self.issues.is_empty() && big_button(ui, &format!("Fix {n} selected"), n > 0).clicked() {
                            self.confirm_fix = true;
                        }
                    });
                });
                if !cx.elevated && self.issues.iter().any(|i| i.needs_admin()) {
                    cx.admin_notice(ui, "Issues in HKEY_LOCAL_MACHINE can only be fixed as administrator.");
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
                if self.scanned && self.issues.is_empty() {
                    ui.label(RichText::new("No issues found.").size(16.0).color(GREEN));
                }
                self.issue_list(ui, cx.elevated);

                ui.add_space(14.0);
                ui.separator();
                self.backup_list(ui);
            });
        });
        self.modals(&ctx, &mut cx);
    }

    fn issue_list(&mut self, ui: &mut egui::Ui, elevated: bool) {
        for cat in Category::ALL {
            let members: Vec<usize> = (0..self.issues.len()).filter(|&i| self.issues[i].category == cat).collect();
            if members.is_empty() {
                continue;
            }
            let fixable: Vec<usize> = members.iter().copied().filter(|&i| self.fixable(&self.issues[i], elevated)).collect();
            let on = fixable.iter().filter(|i| self.checked.contains(i)).count();
            ui.horizontal(|ui| {
                let collapsed = self.collapsed.contains(&cat);
                if ui.add(egui::Button::new(if collapsed { "⏵" } else { "⏷" }).frame(false)).clicked() && !self.collapsed.remove(&cat) {
                    self.collapsed.insert(cat);
                }
                let mut all = !fixable.is_empty() && on == fixable.len();
                let cb = egui::Checkbox::new(&mut all, RichText::new(format!("{} ({})", cat.label(), members.len())).strong())
                    .indeterminate(on > 0 && on < fixable.len());
                if ui.add_enabled(!fixable.is_empty(), cb).on_hover_text(cat.about()).changed() {
                    for &i in &fixable {
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
                let ok = self.fixable(issue, elevated);
                ui.horizontal(|ui| {
                    ui.add_space(28.0);
                    let mut on = self.checked.contains(&i);
                    let cb = ui.add_enabled(ok, egui::Checkbox::without_text(&mut on)).on_disabled_hover_text("Needs administrator rights");
                    if cb.changed() {
                        if on {
                            self.checked.insert(i);
                        } else {
                            self.checked.remove(&i);
                        }
                    }
                    ui.vertical(|ui| {
                        ui.add(egui::Label::new(&issue.detail).truncate());
                        ui.add(egui::Label::new(RichText::new(issue.location()).monospace().size(10.5).weak()).truncate());
                    });
                });
            }
            ui.add_space(6.0);
        }
    }

    fn backup_list(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(format!("Backups ({})", self.backups.len())).id_salt("reg_backups").show(ui, |ui| {
            ui.label(
                RichText::new("Every registry change Heft makes (here, in Startup and in Programs) is saved here first. Restoring merges the backup back in.")
                    .weak()
                    .size(11.5),
            );
            ui.horizontal(|ui| {
                if ui.small_button("Open folder").clicked() {
                    let dir = crate::reg::backup_dir();
                    let _ = std::fs::create_dir_all(&dir);
                    crate::platform::open_path(&dir.to_string_lossy());
                }
                if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                    self.backups = regclean::backups();
                }
            });
            for b in self.backups.clone().iter().take(50) {
                ui.horizontal(|ui| {
                    let name = b.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    ui.label(RichText::new(name).monospace().size(11.5));
                    ui.label(RichText::new(format!("{} · {}", crate::platform::fmt_datetime(b.modified), fmt_size(b.size))).weak().size(11.5));
                    if ui.small_button("Restore…").clicked() {
                        self.confirm_restore = Some(b.path.clone());
                    }
                });
            }
        });
    }

    fn modals(&mut self, ctx: &egui::Context, cx: &mut Cx) {
        if self.confirm_fix {
            let n = self.checked.len();
            let mut choice = None;
            let resp = egui::Modal::new(egui::Id::new("confirm_reg_fix")).show(ctx, |ui| {
                ui.set_width(440.0);
                ui.heading(format!("Fix {n} issue(s)?"));
                ui.add_space(4.0);
                ui.label(RichText::new("A .reg backup of every entry is saved first, so this can be undone from the Backups list.").weak());
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
                let picked: Vec<Issue> = self.checked.iter().map(|&i| self.issues[i].clone()).collect();
                match regclean::fix(&picked) {
                    Ok(r) => {
                        let fixed = std::mem::take(&mut self.checked);
                        let mut i = 0;
                        self.issues.retain(|_| {
                            let keep = !fixed.contains(&i);
                            i += 1;
                            keep
                        });
                        self.report = Some(r);
                        self.backups = regclean::backups();
                    }
                    Err(e) => cx.toast(e, true),
                }
            } else if choice == Some(false) || resp.should_close() {
                self.confirm_fix = false;
            }
        }

        if let Some(path) = self.confirm_restore.clone() {
            let mut choice = None;
            let resp = egui::Modal::new(egui::Id::new("confirm_reg_restore")).show(ctx, |ui| {
                ui.set_width(440.0);
                ui.heading("Restore this backup?");
                ui.add_space(4.0);
                ui.label(RichText::new(path.to_string_lossy()).monospace().size(11.5));
                ui.label(RichText::new("The saved entries are written back into the registry. Entries for all users need administrator rights.").weak());
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Restore").strong()).clicked() {
                        choice = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(false);
                    }
                });
            });
            if choice == Some(true) {
                self.confirm_restore = None;
                match crate::reg::import(&path) {
                    Ok(()) => cx.toast("Backup restored", false),
                    Err(e) => cx.toast(format!("Restore failed: {e}"), true),
                }
            } else if choice == Some(false) || resp.should_close() {
                self.confirm_restore = None;
            }
        }
    }
}
