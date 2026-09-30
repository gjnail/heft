//! Keeping a scan current: quick rescans from the NTFS change journal, and
//! optional automatic updates while Heft is open.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use eframe::egui;

use super::HeftApp;
use crate::scan::{self, Incremental, Progress, RefreshOutcome};
use crate::tree::{NodeId, Tree};
use crate::util::fmt_duration_ms;

/// How often automatic updates check the journal, at the most. Rebuilding
/// the tree takes a while on a big drive, so after each update the wait is
/// stretched to ten times what the rebuild took, to keep the CPU mostly idle.
const EVERY: Duration = Duration::from_secs(5);

pub(super) struct LiveState {
    /// Left by the last MFT scan; `None` for walker scans and off Windows.
    pub state: Option<Arc<Mutex<Incremental>>>,
    /// Update the scan automatically while Heft is open.
    pub auto: bool,
    job: Option<(Receiver<RefreshOutcome>, bool)>,
    last: Instant,
    wait: Duration,
}

impl LiveState {
    pub fn new(auto: bool) -> Self {
        LiveState { state: None, auto, job: None, last: Instant::now(), wait: EVERY }
    }

    pub fn busy(&self) -> bool {
        self.job.is_some()
    }
}

impl HeftApp {
    /// Rescan: from the change journal when the last scan left one to read,
    /// otherwise a full scan.
    pub(super) fn rescan(&mut self, ctx: &egui::Context) {
        let Some(tree) = self.tree.clone() else { return };
        if self.live.state.is_some() && !self.live.busy() {
            self.start_refresh(ctx, false);
            return;
        }
        if self.view_root != crate::tree::ROOT {
            self.restore_view = Some(tree.path(self.view_root));
        }
        self.start_scan(&tree.root_path.clone(), ctx);
    }

    fn start_refresh(&mut self, ctx: &egui::Context, quiet: bool) {
        let Some(state) = self.live.state.clone() else { return };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let progress = Progress::default();
            let outcome = match state.lock() {
                Ok(mut st) => scan::refresh(&mut st, &progress),
                Err(_) => RefreshOutcome::NeedFullScan("internal error".into()),
            };
            let _ = tx.send(outcome);
            ctx.request_repaint();
        });
        self.live.job = Some((rx, quiet));
        self.live.last = Instant::now();
    }

    pub(super) fn poll_live(&mut self, ctx: &egui::Context) {
        if let Some((rx, quiet)) = &self.live.job {
            let quiet = *quiet;
            match rx.try_recv() {
                Ok(outcome) => {
                    self.live.job = None;
                    self.live.last = Instant::now();
                    match outcome {
                        RefreshOutcome::Unchanged => {
                            if !quiet {
                                self.toast("Nothing has changed since the last scan.", false);
                            }
                        }
                        RefreshOutcome::Updated(tree) => {
                            let took = tree.info.duration_ms;
                            self.live.wait = EVERY.max(Duration::from_millis(took * 10));
                            self.swap_tree(tree, !quiet, ctx);
                            if !quiet {
                                self.toast(format!("Updated from the change journal in {}", fmt_duration_ms(took)), false);
                            }
                        }
                        RefreshOutcome::NeedFullScan(why) => {
                            self.live.state = None;
                            if quiet {
                                self.live.auto = false;
                                self.toast(format!("Automatic updates stopped: {why}. Rescan to continue."), true);
                            } else if let Some(tree) = self.tree.clone() {
                                if self.view_root != crate::tree::ROOT {
                                    self.restore_view = Some(tree.path(self.view_root));
                                }
                                self.start_scan(&tree.root_path.clone(), ctx);
                            }
                        }
                    }
                }
                Err(_) => ctx.request_repaint_after(Duration::from_millis(50)),
            }
        }
        if self.live.auto && self.live.state.is_some() {
            if self.live.job.is_none() && self.scan.is_none() && self.delete_job.is_none() && !self.share.busy() && !self.compress.busy() && !self.relocate.busy() && self.live.last.elapsed() >= self.live.wait {
                self.start_refresh(ctx, true);
            }
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    /// Show an updated scan of the same place without losing your spot: the
    /// zoomed folder, selection, expanded folders and duplicate results are
    /// carried over by path.
    fn swap_tree(&mut self, tree: Tree, save_history: bool, ctx: &egui::Context) {
        let Some(old) = self.tree.clone() else {
            self.set_tree(tree, save_history);
            return;
        };
        let view = old.path(self.view_root);
        let selected = self.selected.map(|s| old.path(s));
        let expanded: Vec<String> = self.expanded.iter().take(5000).map(|&e| old.path(e)).collect();
        let dup_groups: Vec<(u64, Vec<String>)> =
            self.dupes.groups.iter().map(|g| (g.size, g.files.iter().map(|&f| old.path(f)).collect())).collect();
        let dup_checked: Vec<String> = self.dupes.checked.iter().map(|&f| old.path(f)).collect();
        let (dup_searched, dup_scope) = (self.dupes.searched, old.path(self.dupes.scope));
        let had_diff = self.diff.is_some();
        let (tab, compare_to) = (self.tab, self.compare_to);

        self.set_tree(tree, save_history);

        let Some(new) = self.tree.clone() else { return };
        let find = |p: &String| new.find(p);
        if let Some(v) = find(&view).filter(|&v| new.node(v).is_dir()) {
            self.view_root = v;
        }
        self.selected = selected.as_ref().and_then(find);
        self.expanded = expanded.iter().filter_map(find).collect::<HashSet<NodeId>>();
        self.expanded.insert(crate::tree::ROOT);
        self.rows_dirty = true;
        self.dupes.groups = dup_groups
            .into_iter()
            .filter_map(|(size, files)| {
                let files: Vec<NodeId> = files.iter().filter_map(find).collect();
                (files.len() > 1).then_some(crate::dupes::DupGroup { size, files })
            })
            .collect();
        self.dupes.checked = dup_checked.iter().filter_map(find).collect();
        self.dupes.searched = dup_searched;
        self.dupes.scope = find(&dup_scope).unwrap_or(crate::tree::ROOT);
        self.tab = tab;
        self.compare_to = compare_to;
        if had_diff && !self.snapshots.is_empty() {
            self.start_diff(ctx);
        }
    }
}

