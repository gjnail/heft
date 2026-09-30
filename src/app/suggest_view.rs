//! Suggestions tab: what the scan found that's probably worth cleaning up,
//! each with its reason and the right way to do it.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use eframe::egui::{self, RichText};

use super::{Action, HeftApp};
use crate::platform;
use crate::recommend::{self, Action as Do, Suggestion, Tool};
use crate::tree::{NodeId, Tree};
use crate::util::{fmt_ago, fmt_count, fmt_size};

/// Items listed per suggestion before "show all".
const PREVIEW: usize = 6;

#[derive(Default)]
pub(super) struct SuggestState {
    key: Option<(usize, u64)>,
    running: Option<Receiver<Vec<Suggestion>>>,
    list: Vec<Suggestion>,
    checked: HashSet<NodeId>,
    expanded: HashSet<String>,
}

impl SuggestState {
    pub(super) fn count(&self) -> usize {
        self.list.len()
    }
}

impl HeftApp {
    pub(super) fn suggestions_tab(&mut self, ui: &mut egui::Ui) {
        let Some(tree) = self.tree.clone() else { return };
        let key = (Arc::as_ptr(&tree) as usize, tree.version);
        if self.suggest.key != Some(key) {
            let (tx, rx) = crossbeam_channel::bounded(2);
            let (t, ctx, demo) = (tree.clone(), ui.ctx().clone(), self.demo);
            std::thread::spawn(move || {
                let mut list = recommend::analyze(&t, platform::now_unix());
                if demo {
                    list.retain(|s| !matches!(s.action, Do::Compress));
                }
                let _ = tx.send(list.clone());
                ctx.request_repaint();
                // The cleaner looks at this machine, so it has nothing to say about the demo disk.
                if !demo && let Some(c) = recommend::cleaner_summary() {
                    list.push(c);
                    list.sort_by_key(|s| std::cmp::Reverse(s.bytes));
                    let _ = tx.send(list);
                    ctx.request_repaint();
                }
            });
            self.suggest.running = Some(rx);
            self.suggest.key = Some(key);
        }
        while let Some(rx) = &self.suggest.running {
            match rx.try_recv() {
                Ok(list) => {
                    // Keep choices the user made; tick new clearly-disposable items.
                    let still: HashSet<NodeId> = list.iter().flat_map(|s| s.items.iter().copied()).collect();
                    self.suggest.checked.retain(|id| still.contains(id));
                    let first_run = self.suggest.list.is_empty();
                    for s in &list {
                        if first_run && matches!(s.action, Do::Trash { preselect: true } | Do::Compress) {
                            self.suggest.checked.extend(s.items.iter().copied());
                        }
                    }
                    self.suggest.list = list;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    ui.ctx().request_repaint_after(Duration::from_millis(80));
                    break;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => self.suggest.running = None,
            }
        }

        if self.suggest.running.is_some() && self.suggest.list.is_empty() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Looking for things to clean up…");
            });
            return;
        }
        let list = self.suggest.list.clone();
        let total: u64 = list.iter().map(|s| s.bytes).sum();
        if list.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("Nothing to suggest here.").strong());
            ui.label(
                RichText::new(
                    "No old installers, stale build folders, big logs or forgotten games turned up. The Search and \
                     Duplicates tabs are good next stops.",
                )
                .weak(),
            );
            return;
        }
        ui.label(RichText::new(format!("{} suggestions covering {}", list.len(), fmt_size(total))).strong());
        ui.label(RichText::new("Nothing is removed until you confirm. Click any item to see it in the map.").weak().size(11.5));
        ui.add_space(4.0);

        egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("suggestions").show(ui, |ui| {
            for s in &list {
                egui::Frame::group(ui.style()).inner_margin(8.0).corner_radius(6.0).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    self.suggestion_card(ui, &tree, s);
                });
                ui.add_space(6.0);
            }
        });
    }

    fn suggestion_card(&mut self, ui: &mut egui::Ui, tree: &Tree, s: &Suggestion) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&s.title).strong().size(14.0));
            if s.bytes > 0 {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(fmt_size(s.bytes)).strong());
                });
            }
        });
        ui.label(RichText::new(s.detail).weak());
        ui.add_space(4.0);

        match &s.action {
            Do::Trash { .. } => {
                let open = self.suggest.expanded.contains(&s.title);
                let shown = if open { s.items.len() } else { s.items.len().min(PREVIEW) };
                for &id in &s.items[..shown] {
                    self.item_row(ui, tree, id, true);
                }
                ui.horizontal(|ui| {
                    if s.items.len() > PREVIEW {
                        let label = if open { "Show fewer".to_string() } else { format!("Show all {}", fmt_count(s.items.len() as u64)) };
                        if ui.small_button(label).clicked() && !self.suggest.expanded.remove(&s.title) {
                            self.suggest.expanded.insert(s.title.clone());
                        }
                    }
                    let chosen: Vec<NodeId> = s.items.iter().copied().filter(|i| self.suggest.checked.contains(i)).collect();
                    let bytes: u64 = chosen.iter().map(|&i| tree.node(i).size).sum();
                    let button = egui::Button::new(format!("Move {} selected to the {}… ({})", chosen.len(), platform::TRASH, fmt_size(bytes)));
                    if ui.add_enabled(!chosen.is_empty(), button).clicked() {
                        self.actions.push(Action::Delete(chosen));
                    }
                });
            }
            Do::Compress => {
                for &id in &s.items {
                    self.item_row(ui, tree, id, true);
                }
                let chosen: Vec<NodeId> = s.items.iter().copied().filter(|i| self.suggest.checked.contains(i)).collect();
                let button = egui::Button::new(format!("Compress {} selected…", chosen.len()));
                if ui.add_enabled(!chosen.is_empty() && self.can_compress(), button).clicked() {
                    self.actions.push(Action::Compress(chosen));
                }
            }
            Do::Steam(games) => {
                let now = platform::now_unix();
                for g in games {
                    ui.horizontal(|ui| {
                        let played = if g.last_played == 0 { "never played".to_string() } else { format!("last played {}", fmt_ago(now - g.last_played)) };
                        let name = ui.add(egui::Label::new(RichText::new(&g.name)).sense(egui::Sense::click()));
                        if name.clicked()
                            && let Some(f) = g.folder
                        {
                            self.actions.push(Action::Reveal(f));
                        }
                        ui.label(RichText::new(format!("{} · {played}", fmt_size(g.bytes))).weak());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Uninstall in Steam").clicked() {
                                platform::open_path(&format!("steam://uninstall/{}", g.appid));
                            }
                        });
                    });
                }
            }
            Do::Explain { how, tool } => {
                for &id in s.items.iter().take(PREVIEW) {
                    self.item_row(ui, tree, id, false);
                }
                ui.add_space(2.0);
                ui.label(RichText::new(*how).color(ui.visuals().text_color()));
                if let Some(tool) = tool {
                    match tool {
                        Tool::DiskCleanup => {
                            if ui.button("Open Disk Cleanup").clicked() {
                                let _ = std::process::Command::new("cleanmgr.exe").spawn();
                            }
                        }
                        Tool::Cleaner => {
                            if ui.button("Open the Cleaner page").clicked() {
                                self.workspace = super::Workspace::Cleaner;
                            }
                        }
                    }
                }
            }
        }
    }

    /// One item inside a suggestion: optional checkbox, name, size, folder.
    fn item_row(&mut self, ui: &mut egui::Ui, tree: &Tree, id: NodeId, checkable: bool) {
        let n = tree.node(id);
        ui.horizontal(|ui| {
            if checkable {
                let mut on = self.suggest.checked.contains(&id);
                if super::warnings::checkbox(ui, &mut on, tree.name(id)).changed() {
                    if on {
                        self.suggest.checked.insert(id);
                    } else {
                        self.suggest.checked.remove(&id);
                    }
                }
            }
            ui.label(RichText::new(fmt_size(n.size)).monospace().size(11.5));
            if let Some(r) = crate::risk::assess(tree, id) {
                ui.label(RichText::new("⚠").color(super::warnings::color(r.level)))
                    .on_hover_ui(|ui| super::warnings::explain(ui, &r));
            }
            let path = ui.add(
                egui::Label::new(RichText::new(tree.path(id)).weak().size(12.0)).truncate().sense(egui::Sense::click()),
            );
            if path.hovered() {
                self.list_hover = Some(id);
            }
            if path.clicked() {
                self.actions.push(Action::Reveal(id));
            }
            path.context_menu(|ui| self.node_menu(ui, tree, id));
        });
    }
}
