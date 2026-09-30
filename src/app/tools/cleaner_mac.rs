//! macOS-only parts of the cleaner page: the weekly scheduled clean (a
//! launch agent), a note when Heft can't see protected folders, and
//! shortcuts to macOS's own storage tools.

use crossbeam_channel::Receiver;
use eframe::egui::{self, RichText};

use super::{Cx, AMBER};
use crate::clean;

/// Looked up in the background when the page opens.
struct Facts {
    schedule: bool,
    full_disk_access: Option<bool>,
    snapshots: Option<usize>,
}

pub struct MacExtras {
    job: Option<Receiver<Facts>>,
    schedule: Option<bool>,
    full_disk_access: Option<bool>,
    snapshots: Option<usize>,
    confirm_thin: bool,
}

impl MacExtras {
    pub fn new() -> Self {
        MacExtras { job: None, schedule: None, full_disk_access: None, snapshots: None, confirm_thin: false }
    }

    pub fn start(&mut self, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Facts {
                schedule: clean::schedule_enabled(),
                full_disk_access: clean::has_full_disk_access(),
                snapshots: clean::local_snapshots(),
            });
            ctx.request_repaint();
        });
        self.job = Some(rx);
    }

    pub fn poll(&mut self) {
        if let Some(rx) = &self.job
            && let Ok(f) = rx.try_recv()
        {
            self.schedule = Some(f.schedule);
            self.full_disk_access = f.full_disk_access;
            self.snapshots = f.snapshots;
            self.job = None;
        }
    }

    /// Weekly cleaning, Full Disk Access, and macOS's own tools.
    pub fn footer(&mut self, ui: &mut egui::Ui, cx: &mut Cx) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Automatic cleaning").strong());
            match self.schedule {
                None => {
                    ui.spinner();
                }
                Some(mut on) => {
                    let resp = ui.checkbox(&mut on, "Clean the selected items every Sunday").on_hover_text(
                        "Adds a launch agent that runs `heft --clean` with the selection above on Sundays at noon. \
                         Items that need the administrator password or restart Finder are skipped. \
                         macOS lists it in System Settings › General › Login Items.",
                    );
                    if resp.changed() {
                        match clean::set_schedule(on) {
                            Ok(()) => {
                                self.schedule = Some(on);
                                cx.toast(if on { "Weekly cleaning scheduled" } else { "Weekly cleaning turned off" }, false);
                            }
                            Err(e) => cx.toast(format!("Could not change the schedule: {e}"), true),
                        }
                    }
                }
            }
        });
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("macOS's own tools").strong());
            let storage = ui.button("Storage settings").on_hover_text(
                "System Settings › General › Storage: what uses the space, and recommendations such as emptying the Trash automatically",
            );
            if storage.clicked() {
                crate::mac::open_url("x-apple.systempreferences:com.apple.settings.Storage");
            }
            if let Some(n) = self.snapshots.filter(|&n| n > 0) {
                let thin = ui.button(format!("Thin Time Machine snapshots ({n})…")).on_hover_text(
                    "Local snapshots Time Machine keeps on this disk. macOS removes them by itself when space runs low; this does it now.",
                );
                if thin.clicked() {
                    self.confirm_thin = true;
                }
            }
        });
        if self.full_disk_access == Some(false) {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("⚠").color(AMBER));
                ui.label(
                    RichText::new(
                        "Without Full Disk Access, Heft can't see inside the Trash, Safari's cache or Recent items, and doesn't \
                         list Safari's cookies, history and last session or new Teams' cache.",
                    )
                        .weak(),
                );
                if ui.small_button("Open Privacy & Security").clicked() {
                    crate::mac::open_url(crate::mac::FULL_DISK_ACCESS_SETTINGS);
                }
            });
        }
    }

    pub fn modals(&mut self, ctx: &egui::Context, cx: &mut Cx) {
        if !self.confirm_thin {
            return;
        }
        let mut choice = None;
        let resp = egui::Modal::new(egui::Id::new("confirm_thin")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.heading("Thin Time Machine snapshots?");
            ui.add_space(4.0);
            ui.label(
                "Time Machine keeps hourly snapshots of this disk, and macOS removes them by itself when space runs low. \
                 This asks it to remove them now. Backups on your Time Machine disk aren't touched.",
            );
            ui.add_space(4.0);
            ui.label(RichText::new(format!("Runs `{}` in a Terminal window.", clean::THIN_SNAPSHOTS)).weak());
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Thin snapshots").strong()).clicked() {
                    choice = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
            });
        });
        if choice == Some(true) {
            self.confirm_thin = false;
            match crate::mac::run_in_terminal("Heft - Time Machine snapshots", clean::THIN_SNAPSHOTS) {
                // Counted again the next time Heft starts.
                Ok(()) => self.snapshots = None,
                Err(e) => cx.toast(format!("Could not open Terminal: {e}"), true),
            }
        } else if choice == Some(false) || resp.should_close() {
            self.confirm_thin = false;
        }
    }
}
