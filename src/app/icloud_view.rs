//! Removing the downloaded copies of iCloud Drive files (macOS), from an
//! item's menu or the Suggestions page. Nothing is deleted: the files stay in
//! iCloud Drive and in their folders, and download again when opened.

use std::path::Path;
use std::sync::Arc;

use crossbeam_channel::Receiver;
use eframe::egui::{self, RichText};

use super::HeftApp;
use crate::mac::icloud::{self, State};
use crate::tree::{flags, NodeId, Tree};
use crate::util::{fmt_count, fmt_size};

/// Each file, its space on disk, and whether its download was removed.
type Removed = Vec<(NodeId, u64, Result<(), String>)>;

#[derive(Default)]
pub(super) struct IcloudState {
    confirm: Option<Plan>,
    job: Option<Receiver<Removed>>,
    /// The last item a menu asked about: (item, tree version, answer).
    offer: Option<(NodeId, u64, bool)>,
}

struct Plan {
    /// (file, path, space on disk)
    files: Vec<(NodeId, String, u64)>,
}

impl IcloudState {
    /// Whether to offer Remove download for an item: a file downloaded from
    /// iCloud, or a folder in iCloud Drive. Remembered for the item, since a
    /// menu asks every frame while it's open.
    pub fn offered(&mut self, tree: &Tree, id: NodeId) -> bool {
        if let Some((i, v, yes)) = self.offer
            && i == id
            && v == tree.version
        {
            return yes;
        }
        let path = tree.path(id);
        let yes = if tree.node(id).is_dir() {
            icloud::in_icloud(Path::new(&path))
        } else {
            icloud::state(Path::new(&path)) == State::Downloaded
        };
        self.offer = Some((id, tree.version, yes));
        yes
    }
}

/// The files under the chosen items whose downloads could be removed:
/// everything that takes space and isn't only in the cloud already.
fn files_under(tree: &Tree, ids: &[NodeId]) -> Vec<(NodeId, String, u64)> {
    let skip = flags::CLOUD | flags::LINK | flags::HARDLINK | flags::DELETED | flags::MOUNT | flags::SEEN;
    let mut out = Vec::new();
    let mut stack: Vec<NodeId> = ids.to_vec();
    while let Some(id) = stack.pop() {
        let n = tree.node(id);
        if n.flags & skip != 0 {
            continue;
        }
        if n.is_dir() {
            stack.extend_from_slice(tree.children(id));
        } else if n.alloc > 0 {
            out.push((id, tree.path(id), n.alloc));
        }
    }
    out.sort_by_key(|f| std::cmp::Reverse(f.2));
    out
}

impl HeftApp {
    pub(super) fn open_remove_download(&mut self, ids: &[NodeId]) {
        let Some(tree) = &self.tree else { return };
        let files = files_under(tree, ids);
        if files.is_empty() {
            self.toast("Nothing here is downloaded from iCloud.", false);
            return;
        }
        self.icloud.confirm = Some(Plan { files });
    }

    pub(super) fn remove_download_modal(&mut self, ctx: &egui::Context) {
        if self.icloud.job.is_some() {
            egui::Modal::new(egui::Id::new("removing_downloads")).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Removing downloads…");
                });
            });
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        }
        let Some(plan) = &self.icloud.confirm else { return };
        let total: u64 = plan.files.iter().map(|f| f.2).sum();
        let n = plan.files.len();
        let mut go = false;
        let mut close = false;
        let resp = egui::Modal::new(egui::Id::new("confirm_remove_download")).show(ctx, |ui| {
            ui.set_width(540.0);
            let what = if n == 1 { "this file".to_string() } else { format!("{} files", fmt_count(n as u64)) };
            ui.heading(format!("Remove the download of {what}?"));
            ui.add_space(4.0);
            ui.label(format!("Up to {} on this Mac", fmt_size(total)));
            ui.add_space(6.0);
            egui::ScrollArea::vertical().id_salt("download_paths").max_height(150.0).show(ui, |ui| {
                for (_, path, size) in plan.files.iter().take(200) {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(fmt_size(*size)).monospace().size(11.5));
                        ui.add(egui::Label::new(RichText::new(path).monospace().size(11.5).weak()).truncate());
                    });
                }
                if n > 200 {
                    ui.label(format!("…and {} more", n - 200));
                }
            });
            ui.add_space(6.0);
            ui.label(
                "Nothing is deleted. Each file stays in iCloud Drive and in its folder here, and downloads again when you \
                 open it, which needs an internet connection. This is what Finder's Remove Download does.",
            );
            ui.label(
                RichText::new("Files that aren't in iCloud, or that iCloud hasn't finished syncing, are left as they are.")
                    .weak()
                    .size(11.5),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Remove downloads").strong()).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if go {
            let files = self.icloud.confirm.take().map(|p| p.files).unwrap_or_default();
            let (tx, rx) = crossbeam_channel::bounded(1);
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let results: Removed =
                    files.into_iter().map(|(id, path, alloc)| (id, alloc, icloud::remove_download(Path::new(&path)))).collect();
                let _ = tx.send(results);
                ctx.request_repaint();
            });
            self.icloud.job = Some(rx);
        } else if close || resp.should_close() {
            self.icloud.confirm = None;
        }
    }

    pub(super) fn poll_icloud(&mut self) {
        let Some(rx) = &self.icloud.job else { return };
        let Ok(results) = rx.try_recv() else { return };
        self.icloud.job = None;
        let Some(arc) = self.tree.as_mut() else { return };
        let tree: &mut Tree = Arc::make_mut(arc);
        let (mut freed, mut done, mut skipped) = (0u64, 0usize, Vec::new());
        for (id, alloc, r) in results {
            match r {
                Ok(()) => {
                    tree.mark_cloud(id);
                    freed += alloc;
                    done += 1;
                }
                Err(e) => skipped.push(e),
            }
        }
        self.rows_dirty = true;
        self.search.invalidate();
        self.hotspots_key = None;
        let mut msg = if done == 0 {
            "No downloads were removed".to_string()
        } else {
            format!("Removed {} download(s), freeing {}. They're still in iCloud Drive", fmt_count(done as u64), fmt_size(freed))
        };
        if let Some(first) = skipped.first() {
            msg.push_str(&format!(". {} left as they were ({first})", fmt_count(skipped.len() as u64)));
        }
        self.toast(msg, done == 0);
    }
}
