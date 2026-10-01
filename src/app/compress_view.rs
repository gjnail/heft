//! Compressing folders you rarely change (Windows NTFS, macOS APFS and
//! HFS+), from a folder's menu or the Suggestions page.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use eframe::egui::{self, RichText};

use super::{warnings, HeftApp};
use crate::compress::{self, Level};
use crate::risk::Level as RiskLevel;
use crate::tree::{flags, NodeId, Tree};
use crate::util::{fmt_count, fmt_size};

#[derive(Default)]
pub(super) struct CompressState {
    dialog: Option<Dialog>,
    level: Level,
    /// macOS: also do the files only root can change, after the password.
    with_admin: bool,
    job: Option<Job>,
    /// (tree root, whether that drive supports compression).
    supported: Option<(String, bool)>,
}

impl CompressState {
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }
}

struct Dialog {
    title: String,
    /// (path, size) of every file worth trying.
    files: Vec<(String, u64)>,
    on_disk: u64,
    /// Bytes in files that are already compressed formats, or too small.
    skipped: u64,
    risk: Option<crate::risk::Risk>,
    /// Some of the files are inside an app bundle (macOS).
    has_apps: bool,
    /// macOS: files in apps installed for all users, which only root can
    /// change. Kept apart, and only done if the user agrees to the password.
    admin_files: Vec<(String, u64)>,
}

#[cfg(not(target_os = "macos"))]
const HOW: &str = "Windows keeps these files compressed and unpacks them as they're read. They stay where they are and \
                   open as usual, if a little slower on an old computer. This suits programs and games you don't update \
                   often, and old projects or documents.";
#[cfg(target_os = "macos")]
const HOW: &str = "macOS keeps these files compressed and unpacks them as they're read, the way it stores its own system \
                   files. They stay where they are and open as usual, if a little slower on an old Mac. This suits apps \
                   and games you don't update often, and old projects or documents.";

#[cfg(not(target_os = "macos"))]
const LATER: &str = "If a program changes one of these files later, that file is stored uncompressed again. Nothing \
                     else changes.";
#[cfg(target_os = "macos")]
const LATER: &str = "If an app changes one of these files later, that file is stored uncompressed again. Each file is \
                     compressed into a copy next to it, checked byte for byte against the original and only then \
                     swapped in, keeping its dates, permissions and tags.";

#[cfg(not(target_os = "macos"))]
const SAFE_HERE: &str = "Compressing is safe here. Files a running program has open are skipped.";
#[cfg(target_os = "macos")]
const SAFE_HERE: &str = "Compressing is safe here. Files that apps have open, and apps that are running, are skipped.";

#[cfg(not(target_os = "macos"))]
const NOT_SYSTEM: &str = "Heft won't compress system files. Windows can compress itself safely: run \
                          `compact /CompactOS:always` in a terminal opened as administrator.";
#[cfg(target_os = "macos")]
const NOT_SYSTEM: &str = "Heft won't compress system files. macOS already keeps its own files compressed.";

#[derive(Default)]
struct Progress {
    done: AtomicU64,
    bytes: AtomicU64,
    cancel: AtomicBool,
}

/// A file's path and size, and its new size on disk (`None`: not worth it).
type Outcome = (String, u64, Result<Option<u64>, String>);

struct Job {
    p: Arc<Progress>,
    total: u64,
    undo: bool,
    started: Instant,
    rx: Receiver<Vec<Outcome>>,
}

impl HeftApp {
    /// Whether the scanned drive supports compression.
    pub(super) fn can_compress(&mut self) -> bool {
        let Some(tree) = &self.tree else { return false };
        if self.demo {
            return false;
        }
        match &self.compress.supported {
            Some((root, ok)) if *root == tree.root_path => *ok,
            _ => {
                let ok = compress::supported(Path::new(&tree.root_path));
                self.compress.supported = Some((tree.root_path.clone(), ok));
                ok
            }
        }
    }

    /// Open the compression dialog for some folders.
    pub(super) fn open_compress(&mut self, folders: &[NodeId]) {
        let Some(tree) = self.tree.clone() else { return };
        let mut d =
            Dialog { title: String::new(), files: Vec::new(), on_disk: 0, skipped: 0, risk: None, has_apps: false, admin_files: Vec::new() };
        d.title = match folders {
            [one] => {
                let path = tree.path(*one);
                let name = Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(path);
                format!("Compress \u{201c}{name}\u{201d}?")
            }
            _ => format!("Compress {} folders?", folders.len()),
        };
        for &f in folders {
            if let Some(r) = crate::risk::assess(&tree, f)
                && d.risk.is_none_or(|w| r.level > w.level)
            {
                d.risk = Some(r);
            }
            for id in tree.files_under(f) {
                let n = tree.node(id);
                if n.flags & (flags::CLOUD | flags::LINK | flags::HARDLINK | flags::DELETED) != 0 {
                    continue;
                }
                d.on_disk += n.alloc;
                if compress::worth_trying(tree.ext_name(id), n.size) {
                    let path = tree.path(id);
                    d.has_apps |= cfg!(target_os = "macos") && path.to_lowercase().contains(".app/contents/");
                    #[cfg(target_os = "macos")]
                    if compress::needs_admin(Path::new(&path)) {
                        d.admin_files.push((path, n.size));
                        continue;
                    }
                    d.files.push((path, n.size));
                } else {
                    d.skipped += n.alloc;
                }
            }
        }
        self.compress.dialog = Some(d);
    }

    pub(super) fn compress_modal(&mut self, ctx: &egui::Context) {
        if let Some(job) = &self.compress.job {
            let (done, total) = (job.p.done.load(Ordering::Relaxed), job.total.max(1));
            let mut cancel = false;
            egui::Modal::new(egui::Id::new("compressing")).show(ctx, |ui| {
                ui.set_width(420.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(if job.undo { "Uncompressing…" } else { "Compressing…" }).strong());
                });
                ui.add(
                    egui::ProgressBar::new(done as f32 / total as f32)
                        .text(format!("{} / {} files", fmt_count(done), fmt_count(total))),
                );
                ui.label(
                    RichText::new(format!(
                        "{} · {} s",
                        fmt_size(job.p.bytes.load(Ordering::Relaxed)),
                        job.started.elapsed().as_secs()
                    ))
                    .weak(),
                );
                if ui.button("Stop").clicked() {
                    cancel = true;
                }
            });
            if cancel {
                job.p.cancel.store(true, Ordering::Relaxed);
            }
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }

        let Some(d) = &self.compress.dialog else { return };
        let mut level = self.compress.level;
        let mut with_admin = self.compress.with_admin;
        let (mut go, mut undo, mut close) = (false, false, false);
        let blocked = d.risk.filter(|r| r.level == RiskLevel::Danger);
        let resp = egui::Modal::new(egui::Id::new("confirm_compress")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.heading(&d.title);
            ui.add_space(4.0);
            let n = d.files.len() + if with_admin { d.admin_files.len() } else { 0 };
            ui.label(format!("{} files to try · {} on disk now", fmt_count(n as u64), fmt_size(d.on_disk)));
            ui.add_space(6.0);
            ui.label(HOW);
            ui.label(LATER);
            if cfg!(target_os = "macos") {
                ui.label(
                    RichText::new(
                        "Files that belong to macOS or another user, locked files and files with more than one name \
                         are left as they are. Time Machine backs the compressed files up once more.",
                    )
                    .weak(),
                );
            }
            if !d.admin_files.is_empty() {
                ui.add_space(4.0);
                let size: u64 = d.admin_files.iter().map(|f| f.1).sum();
                ui.checkbox(
                    &mut with_admin,
                    format!(
                        "Also the {} files ({}) in apps installed for all users, which asks for your administrator password",
                        fmt_count(d.admin_files.len() as u64),
                        fmt_size(size)
                    ),
                )
                .on_hover_text(
                    "Apps from the App Store and from installer packages belong to the system. Heft does the same careful \
                     copy, check and swap for their files as root.",
                );
            }
            if d.has_apps {
                ui.label(
                    RichText::new(
                        "macOS protects apps from being changed by other apps. If it stops Heft, allow Heft in System \
                         Settings \u{203a} Privacy & Security \u{203a} App Management, then try again.",
                    )
                    .weak(),
                );
            }
            if d.skipped > 0 {
                ui.label(
                    RichText::new(format!(
                        "Photos, music, video, archives and very small files ({}) are already as small as they get, \
                         so Heft skips them.",
                        fmt_size(d.skipped)
                    ))
                    .weak(),
                );
            }
            // macOS picks the algorithm itself.
            if compress::HAS_LEVELS {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.radio_value(&mut level, Level::Fast, "Faster to open");
                    ui.radio_value(&mut level, Level::Small, "Smaller, slower to open");
                });
            }
            if let Some(r) = &d.risk {
                ui.add_space(6.0);
                warnings::explain(ui, r);
                if r.level == RiskLevel::Danger {
                    ui.colored_label(warnings::color(r.level), NOT_SYSTEM);
                } else {
                    ui.label(RichText::new(SAFE_HERE).weak());
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let ok = blocked.is_none() && (!d.files.is_empty() || (with_admin && !d.admin_files.is_empty()));
                if ui.add_enabled(ok, egui::Button::new(RichText::new("Compress").strong())).clicked() {
                    go = true;
                }
                if ui
                    .add_enabled(ok, egui::Button::new("Uncompress"))
                    .on_hover_text("Undo earlier compression. Needs as much free space as the files take uncompressed.")
                    .clicked()
                {
                    undo = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        self.compress.level = level;
        self.compress.with_admin = with_admin;
        if go || undo {
            if let Some(d) = self.compress.dialog.take() {
                let admin = if with_admin { d.admin_files } else { Vec::new() };
                self.start_compress(d.files, admin, undo, ctx);
            }
        } else if close || resp.should_close() {
            self.compress.dialog = None;
        }
    }

    /// Compress (or uncompress) `files` as you, then `admin` as root after
    /// the administrator password (macOS).
    fn start_compress(&mut self, files: Vec<(String, u64)>, admin: Vec<(String, u64)>, undo: bool, ctx: &egui::Context) {
        let p = Arc::new(Progress::default());
        let level = self.compress.level;
        let total = (files.len() + admin.len()) as u64;
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (p2, ctx) = (p.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut out = Vec::with_capacity(files.len());
            for (path, size) in files {
                if p2.cancel.load(Ordering::Relaxed) {
                    break;
                }
                let r = if undo {
                    compress::decompress(Path::new(&path)).map(Some)
                } else {
                    compress::compress(Path::new(&path), level)
                };
                p2.done.fetch_add(1, Ordering::Relaxed);
                p2.bytes.fetch_add(size, Ordering::Relaxed);
                out.push((path, size, r));
            }
            #[cfg(target_os = "macos")]
            if !admin.is_empty() && !p2.cancel.load(Ordering::Relaxed) {
                let paths: Vec<String> = admin.iter().map(|a| a.0.clone()).collect();
                let results = compress::rewrite_as_admin(&paths, undo)
                    .unwrap_or_else(|e| paths.iter().map(|_| Err(if e == crate::mac::CANCELLED { "the password prompt was cancelled".into() } else { e.clone() })).collect());
                for ((path, size), r) in admin.into_iter().zip(results) {
                    p2.done.fetch_add(1, Ordering::Relaxed);
                    p2.bytes.fetch_add(size, Ordering::Relaxed);
                    out.push((path, size, r));
                }
            }
            #[cfg(not(target_os = "macos"))]
            let _ = admin;
            let _ = tx.send(out);
            ctx.request_repaint();
        });
        self.compress.job = Some(Job { p, total, undo, started: Instant::now(), rx });
    }

    pub(super) fn poll_compress(&mut self) {
        let Some(job) = &self.compress.job else { return };
        let Ok(results) = job.rx.try_recv() else { return };
        let undo = job.undo;
        let stopped = job.p.cancel.load(Ordering::Relaxed);
        self.compress.job = None;
        let Some(arc) = self.tree.as_mut() else { return };
        let tree: &mut Tree = Arc::make_mut(arc);
        let (mut before, mut after, mut changed, mut not_worth) = (0u64, 0u64, 0usize, 0usize);
        let mut failures = Vec::new();
        for (path, size, r) in results {
            match r {
                Ok(Some(alloc)) => {
                    if let Some(id) = tree.find(&path) {
                        before += tree.node(id).alloc;
                        after += alloc;
                        tree.resize_file(id, size, alloc);
                    }
                    changed += 1;
                }
                Ok(None) => not_worth += 1,
                Err(e) => failures.push(format!("{path}: {e}")),
            }
        }
        self.rows_dirty = true;
        self.search.invalidate();
        self.hotspots_key = None;

        let mut msg = if undo {
            format!("Uncompressed {} file(s); they take {} more", fmt_count(changed as u64), fmt_size(after.saturating_sub(before)))
        } else {
            format!("Compressed {} file(s), saving {}", fmt_count(changed as u64), fmt_size(before.saturating_sub(after)))
        };
        if not_worth > 0 {
            msg.push_str(&format!(". {} didn't shrink and were left as they were", fmt_count(not_worth as u64)));
        }
        if stopped {
            msg.push_str(". Stopped early");
        }
        if let Some(first) = failures.first() {
            msg.push_str(&format!(". {} skipped, e.g. {first}", failures.len()));
        }
        self.toast(msg, false);
    }
}
