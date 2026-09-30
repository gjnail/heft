//! Windows-only parts of the cleaner page: WSL and Docker disk compaction,
//! the weekly scheduled clean, and shortcuts to Windows' own cleanup tools.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use eframe::egui::{self, RichText};

use super::{card, Cx, AMBER, GREEN};
use crate::clean::{self, vdisk};
use crate::util::fmt_size;
use crate::winsys;

type Outcome = Result<Vec<vdisk::Compacted>, String>;

pub struct WinExtras {
    schedule: Option<bool>,
    schedule_job: Option<Receiver<bool>>,
    disks: Vec<vdisk::VDisk>,
    disks_job: Option<Receiver<Vec<vdisk::VDisk>>>,
    chosen: HashSet<PathBuf>,
    trim: bool,
    compacting: Option<(Arc<vdisk::Progress>, Receiver<Outcome>)>,
    report: Option<Outcome>,
    confirm: bool,
}

impl WinExtras {
    pub fn new() -> Self {
        WinExtras {
            schedule: None,
            schedule_job: None,
            disks: Vec::new(),
            disks_job: None,
            chosen: HashSet::new(),
            trim: true,
            compacting: None,
            report: None,
            confirm: false,
        }
    }

    /// Look up the schedule and find virtual disks in the background.
    pub fn start(&mut self, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send(clean::schedule_enabled());
        });
        self.schedule_job = Some(rx);
        self.find_disks(ctx);
    }

    fn find_disks(&mut self, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(vdisk::find());
            ctx.request_repaint();
        });
        self.disks_job = Some(rx);
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.schedule_job
            && let Ok(on) = rx.try_recv()
        {
            self.schedule = Some(on);
            self.schedule_job = None;
        }
        if let Some(rx) = &self.disks_job
            && let Ok(d) = rx.try_recv()
        {
            // Big non-sparse disks are ticked by default.
            self.chosen = d.iter().filter(|d| !d.sparse && d.size > 1 << 30).map(|d| d.path.clone()).collect();
            self.disks = d;
            self.disks_job = None;
        }
        if let Some((_, rx)) = &self.compacting {
            match rx.try_recv() {
                Ok(r) => {
                    self.compacting = None;
                    self.report = Some(r);
                    self.find_disks(ctx);
                }
                Err(_) => ctx.request_repaint_after(Duration::from_millis(250)),
            }
        }
    }

    /// The "WSL and Docker disks" section. Nothing is shown when there are none.
    pub fn disks_ui(&mut self, ui: &mut egui::Ui) {
        if self.disks.is_empty() && self.report.is_none() {
            return;
        }
        ui.add_space(14.0);
        ui.label(RichText::new("WSL and Docker disks").strong());
        ui.label(
            RichText::new(
                "Linux under WSL and Docker Desktop keep their files in virtual disks that grow but never shrink. \
                 Compacting gives the space Linux has freed back to Windows.",
            )
            .weak()
            .size(11.5),
        );
        ui.add_space(4.0);
        card(ui, |ui| {
            if let Some((p, _)) = &self.compacting {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(p.phase.lock().unwrap().clone());
                });
                return;
            }
            match &self.report {
                Some(Ok(done)) if !done.is_empty() => {
                    let freed: u64 = done.iter().map(|c| c.before.saturating_sub(c.after)).sum();
                    ui.label(RichText::new(format!("Freed {}", fmt_size(freed))).strong().color(GREEN));
                    for c in done {
                        ui.label(RichText::new(format!("{}: {} to {}", c.name, fmt_size(c.before), fmt_size(c.after))).weak());
                    }
                    ui.add_space(6.0);
                }
                Some(Err(e)) => {
                    ui.colored_label(AMBER, e);
                    ui.add_space(6.0);
                }
                _ => {}
            }
            for d in &self.disks {
                ui.horizontal(|ui| {
                    let mut on = self.chosen.contains(&d.path);
                    let cb = ui.add_enabled(!d.sparse, egui::Checkbox::without_text(&mut on));
                    if cb.on_disabled_hover_text("Sparse disks shrink by themselves").changed() {
                        if on {
                            self.chosen.insert(d.path.clone());
                        } else {
                            self.chosen.remove(&d.path);
                        }
                    }
                    ui.label(RichText::new(fmt_size(d.size)).strong());
                    ui.vertical(|ui| {
                        ui.label(&d.name);
                        ui.add(egui::Label::new(RichText::new(d.path.to_string_lossy()).weak().size(11.0)).truncate());
                    });
                });
            }
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.trim, "Trim free space inside WSL first")
                    .on_hover_text("Runs fstrim in each distro so compacting can find the free space. Recommended.");
                let n = self.chosen.len();
                if ui.add_enabled(n > 0, egui::Button::new(format!("Compact {n} disk(s)…"))).clicked() {
                    self.confirm = true;
                }
            });
        });
    }

    pub fn modals(&mut self, ctx: &egui::Context) {
        if !self.confirm {
            return;
        }
        let picked: Vec<vdisk::VDisk> = self.disks.iter().filter(|d| self.chosen.contains(&d.path)).cloned().collect();
        let docker = picked.iter().any(|d| d.docker);
        let mut choice = None;
        let resp = egui::Modal::new(egui::Id::new("confirm_vdisk")).show(ctx, |ui| {
            ui.set_width(500.0);
            ui.heading(format!("Compact {} disk(s)?", picked.len()));
            ui.add_space(4.0);
            ui.label("Heft will:");
            if self.trim {
                ui.label("1. Trim free space inside each WSL distro (it starts briefly if it isn't running).");
            }
            ui.label(format!("{}. Shut down WSL (wsl --shutdown). Every running distro and open Linux terminal stops.", if self.trim { 2 } else { 1 }));
            ui.label(format!(
                "{}. Compact the disks with diskpart. Windows asks for administrator permission; large disks take minutes.",
                if self.trim { 3 } else { 2 }
            ));
            if docker {
                ui.add_space(4.0);
                ui.colored_label(AMBER, "Quit Docker Desktop first (right-click its tray icon, Quit Docker Desktop).");
            }
            ui.add_space(4.0);
            ui.label(RichText::new("Your Linux files aren't changed; only unused space is released.").weak());
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Compact").strong()).clicked() {
                    choice = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
            });
        });
        if choice == Some(true) {
            self.confirm = false;
            self.report = None;
            let progress = Arc::new(vdisk::Progress::default());
            let (tx, rx) = crossbeam_channel::bounded(1);
            let (p, ctx2, trim) = (progress.clone(), ctx.clone(), self.trim);
            std::thread::spawn(move || {
                let _ = tx.send(vdisk::compact(&picked, trim, &p));
                ctx2.request_repaint();
            });
            self.compacting = Some((progress, rx));
        } else if choice == Some(false) || resp.should_close() {
            self.confirm = false;
        }
    }

    /// Weekly cleaning and Windows' own tools.
    pub fn footer(&mut self, ui: &mut egui::Ui, cx: &mut Cx) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Automatic cleaning").strong());
            match self.schedule {
                None => {
                    ui.spinner();
                }
                Some(mut on) => {
                    let resp = ui
                        .checkbox(&mut on, "Clean the selected items every Sunday")
                        .on_hover_text("Adds a Windows scheduled task that runs `heft --clean` with the selection above.");
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
            ui.label(RichText::new("Windows' own tools").strong());
            if ui.button("Storage Sense").on_hover_text("Settings › System › Storage").clicked() {
                winsys::shell_open("ms-settings:storagesense");
            }
            if ui.button("Disk Cleanup").on_hover_text("Includes previous Windows installations (Windows.old)").clicked() {
                winsys::shell_open("cleanmgr.exe");
            }
            let dism = ui.button("Component store cleanup").on_hover_text(
                "Removes superseded Windows updates from WinSxS with DISM. Runs in a console window as administrator and can take a while.",
            );
            if dism.clicked() {
                let args = "/c title Heft - component store cleanup & Dism.exe /Online /Cleanup-Image /StartComponentCleanup & echo. & pause";
                if let Err(e) = winsys::launch("cmd.exe", args, true) {
                    cx.toast(format!("Could not start DISM: {e}"), true);
                }
            }
        });
    }
}
