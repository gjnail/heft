//! Search tab. With no filters it lists the largest files; type a name,
//! `*.ext` or a wildcard pattern, or pick a size, age or kind to narrow it.

use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use eframe::egui::{self, pos2, vec2, Align2, FontId, Rect, RichText, Sense};

use super::tree_view::ROW_H;
use super::{Action, HeftApp};
use crate::colors::to_color32;
use crate::platform;
use crate::search::{self, Kind, Query};
use crate::tree::{flags, NodeId, Tree};
use crate::util::{fmt_count, fmt_size};

const LIMIT: usize = 1000;

const SIZES: [(u64, &str); 6] =
    [(0, "any size"), (1 << 20, "1 MB"), (10 << 20, "10 MB"), (100 << 20, "100 MB"), (1 << 30, "1 GB"), (10 << 30, "10 GB")];

/// (label, older than days, newer than days)
const AGES: [(&str, u32, u32); 7] = [
    ("any time", 0, 0),
    ("in the last week", 0, 7),
    ("in the last month", 0, 30),
    ("in the last year", 0, 365),
    ("over 6 months ago", 182, 0),
    ("over a year ago", 365, 0),
    ("over 2 years ago", 730, 0),
];

pub(super) struct SearchState {
    text: String,
    min_size: u64,
    age: usize,
    kind: Kind,
    results: Vec<NodeId>,
    key: Option<(usize, u64, NodeId, Query)>,
    running: Option<Receiver<Vec<NodeId>>>,
}

impl Default for SearchState {
    fn default() -> Self {
        SearchState {
            text: String::new(),
            min_size: 0,
            age: 0,
            kind: Kind::Files,
            results: Vec::new(),
            key: None,
            running: None,
        }
    }
}

impl SearchState {
    /// Forget cached results (the tree changed).
    pub(super) fn invalidate(&mut self) {
        self.key = None;
    }

    fn query(&self) -> Query {
        let (_, older, newer) = AGES[self.age];
        Query { text: self.text.clone(), min_size: self.min_size, older_than_days: older, newer_than_days: newer, kind: self.kind }
    }
}

impl HeftApp {
    pub(super) fn search_tab(&mut self, ui: &mut egui::Ui) {
        let Some(tree) = self.tree.clone() else { return };

        ui.horizontal_wrapped(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.search.text).hint_text("Name, *.ext or pattern*").desired_width(200.0),
            );
            self.typing |= field.has_focus();
            egui::ComboBox::from_id_salt("search_kind")
                .selected_text(match self.search.kind {
                    Kind::Any => "files and folders",
                    Kind::Files => "files",
                    Kind::Folders => "folders",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.search.kind, Kind::Files, "files");
                    ui.selectable_value(&mut self.search.kind, Kind::Folders, "folders");
                    ui.selectable_value(&mut self.search.kind, Kind::Any, "files and folders");
                });
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("at least");
            let size_label = SIZES.iter().find(|s| s.0 == self.search.min_size).map(|s| s.1).unwrap_or("any size");
            egui::ComboBox::from_id_salt("search_size").selected_text(size_label).show_ui(ui, |ui| {
                for (s, l) in SIZES {
                    ui.selectable_value(&mut self.search.min_size, s, l);
                }
            });
            ui.label("modified");
            egui::ComboBox::from_id_salt("search_age").selected_text(AGES[self.search.age].0).show_ui(ui, |ui| {
                for (i, a) in AGES.iter().enumerate() {
                    ui.selectable_value(&mut self.search.age, i, a.0);
                }
            });
        });

        // Searching millions of items takes a moment, so it runs in the background.
        let q = self.search.query();
        let key = (Arc::as_ptr(&tree) as usize, tree.version, self.view_root, q.clone());
        if self.search.key.as_ref() != Some(&key) {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let (t, root, ctx, q) = (tree.clone(), self.view_root, ui.ctx().clone(), q.clone());
            std::thread::spawn(move || {
                let _ = tx.send(search::run(&t, root, &q, platform::now_unix(), LIMIT));
                ctx.request_repaint();
            });
            self.search.running = Some(rx);
            self.search.key = Some(key);
        }
        if let Some(rx) = &self.search.running {
            match rx.try_recv() {
                Ok(r) => {
                    self.search.results = r;
                    self.search.running = None;
                }
                Err(_) => ui.ctx().request_repaint_after(Duration::from_millis(50)),
            }
        }

        let filtered = !q.is_empty();
        let results = self.search.results.clone();
        ui.horizontal(|ui| {
            let what = if !filtered && q.kind == Kind::Files {
                "Largest files".to_string()
            } else if results.len() >= LIMIT {
                format!("The {} biggest matches", fmt_count(LIMIT as u64))
            } else {
                format!("{} matches", fmt_count(results.len() as u64))
            };
            ui.label(RichText::new(format!("{what} in {}", tree.path(self.view_root))).weak().size(11.5));
            if self.search.running.is_some() {
                ui.spinner();
            }
        });
        if filtered && !results.is_empty() {
            let total: u64 = results.iter().map(|&id| tree.node(id).size).sum();
            if ui.button(format!("Move these {} to the {}… ({})", results.len(), platform::TRASH, fmt_size(total))).clicked() {
                self.actions.push(Action::Delete(results.clone()));
            }
        }
        ui.add_space(4.0);
        if results.is_empty() && self.search.running.is_none() {
            ui.label(RichText::new("Nothing matches.").weak());
            return;
        }
        self.result_rows(ui, &tree, &results, "search_rows");
    }

    /// Two-line rows (size, name, folder, date) used by the search tab.
    fn result_rows(&mut self, ui: &mut egui::Ui, tree: &Tree, results: &[NodeId], id_salt: &str) {
        let view_size = tree.node(self.view_root).size.max(1);
        let h = ROW_H * 1.6;
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt(id_salt).show_rows(ui, h, results.len(), |ui, range| {
                for &id in &results[range] {
                    let n = *tree.node(id);
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
                    let selected = self.selected == Some(id);
                    if selected {
                        ui.painter().rect_filled(rect, 3.0, ui.visuals().selection.bg_fill);
                    } else if resp.hovered() {
                        ui.painter().rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
                    }
                    let v = ui.visuals();
                    let p = ui.painter();
                    let top = rect.top() + 10.0;
                    let bottom = rect.bottom() - 9.0;
                    p.text(pos2(rect.left() + 6.0, top), Align2::LEFT_CENTER, fmt_size(n.size), FontId::proportional(13.0), v.strong_text_color());
                    let color = if n.is_dir() { crate::colors::to_color32(crate::colors::DIR_COLOR) } else { to_color32(self.ext_colors[n.ext as usize]) };
                    super::tabs::share_bar(
                        ui,
                        Rect::from_min_size(pos2(rect.left() + 6.0, bottom - 3.0), vec2(62.0, 6.0)),
                        n.size as f32 / view_size as f32,
                        color,
                    );
                    let risk = crate::risk::assess(tree, id);
                    let text_x = rect.left() + 80.0;
                    let text_end = rect.right() - if risk.is_some() { 110.0 } else { 90.0 };
                    let clip = Rect::from_min_max(pos2(text_x, rect.top()), pos2(text_end, rect.bottom()));
                    let pc = p.with_clip_rect(clip.intersect(p.clip_rect()));
                    let mut name = tree.name(id).to_string();
                    if n.is_dir() {
                        name.push(platform::SEP);
                    }
                    pc.text(pos2(text_x, top), Align2::LEFT_CENTER, &name, FontId::proportional(13.0), v.text_color());
                    pc.text(pos2(text_x, bottom), Align2::LEFT_CENTER, tree.path(n.parent), FontId::proportional(11.0), v.weak_text_color());
                    p.text(pos2(rect.right() - 6.0, top), Align2::RIGHT_CENTER, platform::fmt_date(n.mtime), FontId::proportional(11.5), v.weak_text_color());
                    if let Some(r) = &risk {
                        p.text(pos2(rect.right() - 92.0, top), Align2::RIGHT_CENTER, "⚠", FontId::proportional(13.0), super::warnings::color(r.level));
                    }
                    if n.flags & flags::CLOUD != 0 {
                        p.text(pos2(rect.right() - 6.0, bottom), Align2::RIGHT_CENTER, "☁ online-only", FontId::proportional(11.0), v.weak_text_color());
                    }
                    if resp.hovered() {
                        self.list_hover = Some(id);
                    }
                    if resp.clicked() {
                        self.actions.push(Action::Reveal(id));
                    }
                    if resp.double_clicked() {
                        self.actions.push(Action::ShowInFileManager(id));
                    }
                    resp.widget_info(|| {
                        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, format!("{name}, {}", fmt_size(n.size)))
                    });
                    resp.context_menu(|ui| self.node_menu(ui, tree, id));
                }
            });
        });
    }
}
