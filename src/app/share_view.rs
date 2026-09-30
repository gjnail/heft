//! Replacing duplicates with hard links or clones, from the Duplicates tab.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::Receiver;
use eframe::egui::{self, RichText};

use super::{warnings, HeftApp};
use crate::dedupe::{self, Method};
use crate::risk::Level;
use crate::tree::{NodeId, Tree};
use crate::util::{fmt_count, fmt_size};

#[derive(Default)]
pub(super) struct ShareState {
    plan: Option<Plan>,
    ack: bool,
    job: Option<Job>,
    /// (tree root, how that drive can share storage), for the button label.
    root_method: Option<(String, Option<Method>)>,
}

impl ShareState {
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }
}

struct Item {
    keep: String,
    dup: String,
    method: Method,
    size: u64,
}

struct Plan {
    items: Vec<Item>,
    skipped: Vec<(String, String)>,
}

struct Job {
    p: Arc<dedupe::Progress>,
    total: u64,
    started: Instant,
    rx: Receiver<Vec<(Item, Result<(), String>)>>,
}

impl HeftApp {
    /// How the scanned drive can share storage, if it can.
    pub(super) fn share_method(&mut self) -> Option<Method> {
        let root = self.tree.as_ref()?.root_path.clone();
        match &self.share.root_method {
            Some((r, m)) if *r == root => *m,
            _ => {
                let m = dedupe::method_for(Path::new(&root));
                self.share.root_method = Some((root, m));
                m
            }
        }
    }

    /// Work out what "replace with links" would do for the selected
    /// duplicates, and open the dialog.
    pub(super) fn plan_share(&mut self) {
        let Some(tree) = self.tree.clone() else { return };
        let mut volumes: HashMap<String, Option<(String, Option<Method>)>> = HashMap::new();
        let mut volume_of = |path: &str| -> Option<(String, Option<Method>)> {
            let parent = Path::new(path).parent()?.to_string_lossy().into_owned();
            volumes
                .entry(parent.clone())
                .or_insert_with(|| {
                    let v = dedupe::volume(Path::new(&parent))?;
                    Some((v, dedupe::method_for(Path::new(&parent))))
                })
                .clone()
        };
        let mut plan = Plan { items: Vec::new(), skipped: Vec::new() };
        for g in &self.dupes.groups {
            let keeps: Vec<NodeId> = g.files.iter().copied().filter(|f| !self.dupes.checked.contains(f)).collect();
            for &dup in g.files.iter().filter(|f| self.dupes.checked.contains(f)) {
                let dup_path = tree.path(dup);
                let skip = |why: &str| (dup_path.clone(), why.to_string());
                if keeps.is_empty() {
                    plan.skipped.push(skip("every copy in its group is selected, so there's none to keep"));
                    continue;
                }
                if let Some(r) = crate::risk::assess(&tree, dup) {
                    plan.skipped.push(skip(&format!("{}: Heft doesn't change files there", r.title.to_lowercase())));
                    continue;
                }
                let Some((vol, method)) = volume_of(&dup_path) else {
                    plan.skipped.push(skip("can't tell which drive it's on"));
                    continue;
                };
                let Some(method) = method else {
                    plan.skipped.push(skip("this drive can't share storage between files"));
                    continue;
                };
                // The oldest kept copy on the same drive that isn't somewhere risky.
                let keep = keeps.iter().copied().find(|&k| {
                    let kp = tree.path(k);
                    volume_of(&kp).is_some_and(|(v, _)| v == vol) && crate::risk::assess(&tree, k).is_none()
                });
                let Some(keep) = keep else {
                    plan.skipped.push(skip("no kept copy on the same drive outside system and app folders"));
                    continue;
                };
                plan.items.push(Item { keep: tree.path(keep), dup: dup_path, method, size: tree.node(dup).size });
            }
        }
        self.share.plan = Some(plan);
        self.share.ack = false;
    }

    pub(super) fn share_modal(&mut self, ctx: &egui::Context) {
        if let Some(job) = &self.share.job {
            let (done, total) = (job.p.bytes.load(Ordering::Relaxed), job.total.max(1));
            let mut cancel = false;
            egui::Modal::new(egui::Id::new("sharing")).show(ctx, |ui| {
                ui.set_width(420.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Checking and replacing duplicates…").strong());
                });
                ui.add(egui::ProgressBar::new((done as f32 / total as f32).min(1.0)));
                ui.label(
                    RichText::new(format!("{} read · {} s", fmt_size(done), job.started.elapsed().as_secs())).weak(),
                );
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
            if cancel {
                job.p.cancel.store(true, Ordering::Relaxed);
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        }

        let Some(plan) = &self.share.plan else { return };
        let links = plan.items.iter().filter(|i| i.method == Method::HardLink).count();
        let clones = plan.items.len() - links;
        let frees: u64 = plan.items.iter().map(|i| i.size).sum();
        let mut ack = self.share.ack;
        let (mut go, mut close) = (false, false);
        let resp = egui::Modal::new(egui::Id::new("confirm_share")).show(ctx, |ui| {
            ui.set_width(580.0);
            ui.heading(match (links, clones) {
                (0, _) => "Replace duplicates with clones?",
                (_, 0) => "Replace duplicates with hard links?",
                _ => "Replace duplicates with clones and hard links?",
            });
            ui.add_space(4.0);
            if plan.items.is_empty() {
                ui.label("None of the selected files can be replaced.");
            } else {
                ui.label(format!("{} file(s) · frees about {}", fmt_count(plan.items.len() as u64), fmt_size(frees)));
            }
            ui.add_space(6.0);
            if links > 0 {
                let c = warnings::color(Level::Caution);
                ui.label(RichText::new("⚠ With hard links, the copies become one file").color(c).strong());
                ui.label(
                    "Each selected copy becomes another name for the copy you keep. Both stay where they are and \
                     open as before, but they're now the same file: change one and the other changes too. \
                     Deleting one leaves the other.",
                );
                ui.label(
                    "Good for photos, music, videos, installers and archives you don't edit. Don't use it for \
                     documents or projects you work on, or files a program keeps updating.",
                );
                ui.add_space(4.0);
                ui.checkbox(&mut ack, "I understand that changing one copy will change all of them");
                ui.add_space(6.0);
            }
            if clones > 0 {
                ui.label(RichText::new("With clones, the copies share space until one changes").strong());
                ui.label(
                    "Each selected copy is replaced by a clone of the one you keep. They stay separate files: \
                     changing one doesn't touch the other. Scans still show both at full size, since each is a \
                     complete file; the drive's free space shows the difference.",
                );
                ui.add_space(6.0);
            }
            if !plan.items.is_empty() {
                ui.label(RichText::new("Heft compares every byte again right before replacing a file.").weak());
            }
            if !plan.skipped.is_empty() {
                ui.add_space(6.0);
                ui.label(RichText::new(format!("{} will be left alone:", plan.skipped.len())).strong());
                egui::ScrollArea::vertical().id_salt("share_skipped").max_height(140.0).show(ui, |ui| {
                    for (path, why) in plan.skipped.iter().take(100) {
                        ui.add(egui::Label::new(RichText::new(path).monospace().size(11.0)).truncate());
                        ui.label(RichText::new(why).weak().size(11.5));
                    }
                });
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let allowed = !plan.items.is_empty() && (links == 0 || ack);
                let label = format!("Replace {}", fmt_count(plan.items.len() as u64));
                if ui.add_enabled(allowed, egui::Button::new(RichText::new(label).strong())).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        self.share.ack = ack;
        if go {
            let plan = self.share.plan.take().unwrap_or(Plan { items: Vec::new(), skipped: Vec::new() });
            self.start_share(plan.items, ctx);
        } else if close || resp.should_close() {
            self.share.plan = None;
        }
    }

    fn start_share(&mut self, items: Vec<Item>, ctx: &egui::Context) {
        let p = Arc::new(dedupe::Progress::default());
        // Every byte of both copies is read, and clones are read again.
        let total = items.iter().map(|i| i.size * if i.method == Method::Clone { 4 } else { 2 }).sum();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (p2, ctx) = (p.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let r = if p2.cancel.load(Ordering::Relaxed) {
                    Err("cancelled".into())
                } else {
                    dedupe::share(Path::new(&item.keep), Path::new(&item.dup), item.method, &p2)
                };
                out.push((item, r));
            }
            let _ = tx.send(out);
            ctx.request_repaint();
        });
        self.share.job = Some(Job { p, total, started: Instant::now(), rx });
    }

    pub(super) fn poll_share(&mut self) {
        let Some(job) = &self.share.job else { return };
        let Ok(results) = job.rx.try_recv() else { return };
        self.share.job = None;
        let Some(arc) = self.tree.as_mut() else { return };
        let tree: &mut Tree = Arc::make_mut(arc);
        let (mut freed, mut ok, mut cancelled) = (0u64, 0usize, 0usize);
        let mut failures = Vec::new();
        let mut done = Vec::new();
        for (item, r) in results {
            match r {
                Ok(()) => {
                    freed += item.size;
                    ok += 1;
                    if let Some(id) = tree.find(&item.dup) {
                        if item.method == Method::HardLink {
                            tree.mark_hard_link(id);
                        }
                        done.push(id);
                    }
                }
                Err(e) if e == "cancelled" => cancelled += 1,
                Err(e) => failures.push(format!("{}: {e}", item.dup)),
            }
        }
        for g in &mut self.dupes.groups {
            g.files.retain(|f| !done.contains(f));
        }
        self.dupes.groups.retain(|g| g.files.len() > 1);
        self.dupes.checked.retain(|f| !done.contains(f));
        self.rows_dirty = true;
        self.search.invalidate();
        self.hotspots_key = None;

        let mut msg = format!("Replaced {ok} duplicate(s), freeing about {}", fmt_size(freed));
        if cancelled > 0 {
            msg.push_str(&format!(". Cancelled before {cancelled} more"));
        }
        if let Some(first) = failures.first() {
            msg.push_str(&format!(". {} couldn't be replaced. {first}", failures.len()));
        }
        self.toast(msg, !failures.is_empty());
    }
}
