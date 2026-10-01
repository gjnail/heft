//! Moving a folder to another drive and leaving a link in its place.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use eframe::egui::{self, RichText};

use super::{warnings, HeftApp};
use crate::platform::{self, DriveInfo};
use crate::relocate;
use crate::risk::{Level, Risk};
use crate::tree::{flags, NodeId, ROOT};
use crate::util::{fmt_count, fmt_size};

/// Room to leave on the destination beyond the folder itself.
const MARGIN: u64 = 1 << 30;

#[derive(Default)]
pub(super) struct RelocateState {
    dialog: Option<Dialog>,
    job: Option<Job>,
}

impl RelocateState {
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }
}

struct Dialog {
    path: String,
    name: String,
    size: u64,
    files: u32,
    drives: Vec<DriveInfo>,
    /// The folder the moved folder will go into.
    dest: Option<String>,
    risk: Option<Risk>,
    blocked: Option<String>,
    ack: bool,
}

struct Job {
    p: Arc<relocate::Progress>,
    total: u64,
    started: Instant,
    path: String,
    moved_to: String,
    /// Ok(None) when the original went to the Recycle Bin or Trash, Ok(Some(path))
    /// when it couldn't and is still at that path.
    rx: Receiver<Result<Option<String>, String>>,
}

impl HeftApp {
    pub(super) fn open_relocate(&mut self, id: NodeId) {
        let Some(tree) = self.tree.clone() else { return };
        let path = tree.path(id);
        let n = *tree.node(id);
        let risk = crate::risk::assess(&tree, id);
        let mut blocked = None;
        if id == ROOT {
            blocked = Some("This is the folder you scanned. Scan the folder above it to move this one.".to_string());
        } else if let Some(r) = risk.filter(|r| r.level == Level::Danger) {
            blocked = Some(format!("{}. {}", r.title, r.detail));
        } else if n.flags & (flags::LINK | flags::MOUNT | flags::SEEN) != 0 {
            blocked = Some("This is already a link to somewhere else.".into());
        } else {
            let mut stack = vec![id];
            while let Some(x) = stack.pop() {
                let f = tree.node(x).flags;
                if f & (flags::LINK | flags::MOUNT | flags::SEEN) != 0 && x != id {
                    blocked = Some(format!("It contains a link ({}), and moving it could break it.", tree.path(x)));
                    break;
                }
                if f & flags::CLOUD != 0 {
                    blocked = Some("It holds online-only files, and copying them would download them.".into());
                    break;
                }
                stack.extend_from_slice(tree.children(x));
            }
        }
        let here = crate::dedupe::volume(Path::new(&path));
        let drives = platform::list_drives()
            .into_iter()
            .filter(|d| matches!(d.kind, "Local disk" | "Removable"))
            .filter(|d| crate::dedupe::volume(Path::new(&d.root)) != here)
            .collect();
        let name = Path::new(&path).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        self.relocate.dialog =
            Some(Dialog { path, name, size: n.alloc.max(n.size), files: n.files, drives, dest: None, risk, blocked, ack: false });
    }

    pub(super) fn relocate_modal(&mut self, ctx: &egui::Context) {
        if let Some(job) = &self.relocate.job {
            let phase = *job.p.phase.lock().unwrap();
            let done = job.p.bytes.load(Ordering::Relaxed);
            let mut stop = false;
            egui::Modal::new(egui::Id::new("relocating")).show(ctx, |ui| {
                ui.set_width(440.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(format!("{phase}…")).strong());
                });
                ui.add(egui::ProgressBar::new((done as f32 / job.total.max(1) as f32).min(1.0)));
                ui.label(
                    RichText::new(format!(
                        "{} of {} · {} s",
                        fmt_size(done),
                        fmt_size(job.total),
                        job.started.elapsed().as_secs()
                    ))
                    .weak(),
                );
                let can_stop = matches!(phase, "Copying" | "Checking the copy");
                if ui.add_enabled(can_stop, egui::Button::new("Stop")).clicked() {
                    stop = true;
                }
            });
            if stop {
                job.p.cancel.store(true, Ordering::Relaxed);
            }
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }

        let Some(d) = &mut self.relocate.dialog else { return };
        let (mut go, mut close) = (false, false);
        let resp = egui::Modal::new(egui::Id::new("confirm_relocate")).show(ctx, |ui| {
            ui.set_width(580.0);
            ui.heading(format!("Move \u{201c}{}\u{201d} to another drive?", d.name));
            ui.add_space(4.0);
            ui.label(format!(
                "{} in {} files. Heft moves the folder to the drive you pick and leaves a link here, so programs \
                 that use this location keep finding it.",
                fmt_size(d.size),
                fmt_count(d.files as u64)
            ));
            if let Some(why) = &d.blocked {
                ui.add_space(6.0);
                ui.colored_label(warnings::color(Level::Danger), format!("Heft won't move this folder. {why}"));
                ui.add_space(10.0);
                if ui.button("Close").clicked() {
                    close = true;
                }
                return;
            }

            ui.add_space(6.0);
            ui.label(RichText::new("Move it to").strong());
            if d.drives.is_empty() {
                ui.label(RichText::new("No other drives are connected.").weak());
            }
            for drive in &d.drives {
                let fits = drive.free >= d.size + MARGIN;
                let label = format!(
                    "{}  {}  ·  {} free{}",
                    drive.root,
                    if drive.label.is_empty() { drive.kind } else { &drive.label },
                    fmt_size(drive.free),
                    if drive.kind == "Removable" { "  ·  removable" } else { "" }
                );
                let selected = d.dest.as_deref() == Some(drive.root.as_str());
                let r = ui.add_enabled(fits, egui::RadioButton::new(selected, label));
                if r.on_disabled_hover_text("Not enough free space").clicked() {
                    d.dest = Some(drive.root.clone());
                }
            }
            ui.horizontal(|ui| {
                if ui.button("Choose a folder…").clicked()
                    && let Some(p) = rfd::FileDialog::new().set_title("Move the folder into").pick_folder()
                {
                    d.dest = Some(p.to_string_lossy().into_owned());
                }
            });
            let target = d.dest.as_ref().map(|p| PathBuf::from(p).join(&d.name));
            let same_drive = d.dest.as_ref().is_some_and(|p| {
                crate::dedupe::volume(Path::new(p)) == crate::dedupe::volume(Path::new(&d.path))
            });
            if let Some(t) = &target {
                ui.label(RichText::new(format!("It will end up at {}", t.display())).monospace().size(11.5));
                if same_drive {
                    ui.colored_label(ui.visuals().warn_fg_color, "That's on the same drive, so it wouldn't free anything.");
                } else if t.exists() {
                    ui.colored_label(ui.visuals().warn_fg_color, "Something with that name is already there.");
                }
            }

            ui.add_space(8.0);
            let c = warnings::color(Level::Caution);
            ui.label(RichText::new("⚠ Keep that drive connected").color(c).strong());
            ui.label(
                "If the drive is unplugged, or its letter changes, anything that uses this folder will fail until \
                 it's back. That makes removable drives a poor fit for anything you use every day.",
            );
            if let Some(r) = &d.risk {
                ui.add_space(4.0);
                warnings::explain(ui, r);
                ui.label(
                    "Programs installed here may not update or uninstall properly after a move. If that happens, \
                     move the folder back.",
                );
                ui.checkbox(&mut d.ack, "I understand, move it anyway");
            }
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!(
                    "Heft copies everything, checks each file against the original, and only then swaps in the \
                     link. The original then goes to the {}; empty it to get the space back.",
                    platform::TRASH
                ))
                .weak(),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let ready = target.as_ref().is_some_and(|t| !t.exists()) && !same_drive && (d.risk.is_none() || d.ack);
                if ui.add_enabled(ready, egui::Button::new(RichText::new("Move").strong())).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if go {
            if let Some(d) = self.relocate.dialog.take()
                && let Some(dest) = d.dest
            {
                self.start_relocate(d.path, dest, d.size, ctx);
            }
        } else if close || resp.should_close() {
            self.relocate.dialog = None;
        }
    }

    fn start_relocate(&mut self, path: String, dest: String, size: u64, ctx: &egui::Context) {
        let p = Arc::new(relocate::Progress::default());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (p2, ctx, src, parent) = (p.clone(), ctx.clone(), path.clone(), dest.clone());
        std::thread::spawn(move || {
            let r = relocate::relocate(Path::new(&src), Path::new(&parent), &p2).map(|old| {
                *p2.phase.lock().unwrap() =
                    if cfg!(windows) { "Moving the original to the Recycle Bin" } else { "Moving the original to the Trash" };
                match platform::move_to_trash(&old) {
                    Ok(_) => None,
                    Err(_) => Some(old.to_string_lossy().into_owned()),
                }
            });
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        let moved_to = PathBuf::from(&dest).join(Path::new(&path).file_name().unwrap_or_default()).to_string_lossy().into_owned();
        self.relocate.job = Some(Job { p, total: size, started: Instant::now(), path, moved_to, rx });
    }

    pub(super) fn poll_relocate(&mut self) {
        let Some(job) = &self.relocate.job else { return };
        let Ok(result) = job.rx.try_recv() else { return };
        let (path, moved_to) = (job.path.clone(), job.moved_to.clone());
        self.relocate.job = None;
        match result {
            Ok(leftover) => {
                if let Some(arc) = self.tree.as_mut() {
                    let tree = Arc::make_mut(arc);
                    if let Some(id) = tree.find(&path) {
                        tree.remove(id);
                    }
                    self.forget_removed();
                }
                let msg = match leftover {
                    None => format!(
                        "Moved to {moved_to}, with a link left in its place. Empty the {} to get the space back.",
                        platform::TRASH
                    ),
                    Some(old) => format!(
                        "Moved to {moved_to}, with a link left in its place. The original is still at {old}; \
                         delete it once you're sure everything works."
                    ),
                };
                self.toast(msg, false);
            }
            Err(e) if e.contains("cancelled") => self.toast("Stopped. Nothing was moved.", false),
            Err(e) => self.toast(format!("Couldn't move {path}: {e}. Nothing was changed."), true),
        }
    }
}
