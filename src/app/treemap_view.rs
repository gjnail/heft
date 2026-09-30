//! Interactive treemap: hover tooltips, click to select, double-click to
//! drill in one level, right-click for actions, arrow keys to move between
//! neighbouring rectangles once it has keyboard focus.

use std::sync::Arc;

use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind};

use super::{Action, HeftApp, RenderKey};
use crate::colors;
use crate::history::Old;
use crate::platform;
use crate::tree::{NodeId, Tree, ROOT};
use crate::treemap::{self, ColorMode, Style, TmRect};
use crate::util::{fmt_count, fmt_delta, fmt_size, pct};

impl HeftApp {
    pub(super) fn treemap_view(&mut self, ui: &mut egui::Ui) {
        let Some(tree) = self.tree.clone() else { return };
        let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click());
        if resp.clicked() || resp.secondary_clicked() {
            resp.request_focus();
        }
        // Keep arrow keys for moving around the map instead of egui's focus moves.
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                resp.id,
                egui::EventFilter { horizontal_arrows: true, vertical_arrows: true, ..Default::default() },
            )
        });
        self.treemap_focus = resp.has_focus();
        let ppp = ui.ctx().pixels_per_point();
        let px = [(rect.width() * ppp).round().max(1.0) as usize, (rect.height() * ppp).round().max(1.0) as usize];

        let key = RenderKey {
            tree_version: tree.version,
            tree_ptr: Arc::as_ptr(&tree) as usize,
            root: self.view_root,
            size: px,
            mode: self.color_mode,
            highlight: self.highlight,
            diff_ptr: self.diff.as_ref().map(|d| Arc::as_ptr(d) as usize).unwrap_or(0),
            by_alloc: self.size_by_alloc,
        };
        if self.render_key.as_ref() != Some(&key) {
            self.render_seq += 1;
            self.worker.request(treemap::Request {
                seq: self.render_seq,
                tree: tree.clone(),
                root: self.view_root,
                width: px[0],
                height: px[1],
                style: Style {
                    mode: self.color_mode,
                    ext_colors: self.ext_colors.clone(),
                    now: platform::now_unix(),
                    diff: self.diff.clone(),
                    highlight: self.highlight,
                    by_alloc: self.size_by_alloc,
                },
            });
            self.render_key = Some(key);
        }

        let painter = ui.painter_at(rect);
        match &self.texture {
            Some(t) => {
                painter.image(t.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            }
            None => {
                painter.rect_filled(rect, 0.0, Color32::from_gray(24));
            }
        }
        let pending = self.rendered.as_ref().map(|r| r.seq) != Some(self.render_seq);
        if pending {
            let c = rect.right_top() + vec2(-18.0, 18.0);
            painter.circle_filled(c, 11.0, Color32::from_black_alpha(140));
            egui::Spinner::new().size(14.0).paint_at(ui, Rect::from_center_size(c, vec2(14.0, 14.0)));
        }

        let Some(r) = &self.rendered else { return };
        // Hit-testing only makes sense against a layout of what we're showing.
        let valid = r.root == self.view_root && r.tree_version == tree.version;
        let [iw, ih] = r.size();
        let (sx, sy) = (iw as f32 / rect.width(), ih as f32 / rect.height());
        let to_screen = |t: &TmRect| {
            Rect::from_min_max(
                pos2(rect.left() + t.x0 / sx, rect.top() + t.y0 / sy),
                pos2(rect.left() + t.x1 / sx, rect.top() + t.y1 / sy),
            )
        };
        let rect_for = |mut id: NodeId| -> Option<Rect> {
            loop {
                if let Some(t) = r.rect_of(id) {
                    return Some(to_screen(&t));
                }
                if id == ROOT || id == self.view_root {
                    return None;
                }
                id = tree.node(id).parent;
            }
        };

        self.hovered = None;
        if valid {
            if let Some(p) = resp.hover_pos() {
                self.hovered = r.hit((p.x - rect.left()) * sx, (p.y - rect.top()) * sy);
            }
            let in_view = |id: NodeId| tree.is_ancestor(self.view_root, id);
            if let Some(sel) = self.selected.filter(|&s| in_view(s))
                && let Some(sr) = rect_for(sel) {
                    painter.rect_stroke(sr, 0.0, Stroke::new(3.0, Color32::from_black_alpha(200)), StrokeKind::Inside);
                    painter.rect_stroke(sr.shrink(1.0), 0.0, Stroke::new(1.5, Color32::WHITE), StrokeKind::Inside);
                }
            if let Some(lh) = self.list_hover.filter(|&s| in_view(s))
                && let Some(hr) = rect_for(lh) {
                    painter.rect_stroke(hr, 0.0, Stroke::new(2.0, Color32::from_rgb(255, 230, 120)), StrokeKind::Inside);
                }
            if let Some(h) = self.hovered {
                if let Some(hr) = rect_for(h) {
                    painter.rect_stroke(hr, 0.0, Stroke::new(1.0, Color32::from_white_alpha(200)), StrokeKind::Inside);
                }
                // Also outline the top-level group under the pointer.
                if let Some(group) = child_of_view(&tree, self.view_root, h)
                    && group != h
                        && let Some(gr) = rect_for(group) {
                            painter.rect_stroke(gr, 0.0, Stroke::new(1.0, Color32::from_white_alpha(90)), StrokeKind::Inside);
                        }
            }
        }

        if valid && self.show_labels {
            draw_labels(&painter, &tree, r, &to_screen, self.size_by_alloc);
        }
        if self.treemap_focus {
            painter.rect_stroke(rect, 0.0, Stroke::new(1.5, ui.visuals().selection.stroke.color), StrokeKind::Inside);
        }
        if valid
            && self.treemap_focus
            && let Some(n) = keyboard_target(ui, &tree, r, self.selected, self.view_root)
        {
            self.actions.push(Action::Reveal(n));
        }
        self.legend(&painter, rect);

        let view_name = tree.path(self.view_root);
        let selected = self.selected.map(|s| format!("{}, {}", tree.name(s), fmt_size(tree.node(s).size)));
        resp.widget_info(|| {
            let mut label = format!("Treemap of {view_name}. Arrow keys move between items, Enter zooms in, Backspace zooms out.");
            if let Some(s) = &selected {
                label.push_str(&format!(" Selected: {s}."));
            }
            egui::WidgetInfo::labeled(egui::WidgetType::Other, true, label)
        });

        let mut resp = resp;
        if let Some(h) = self.hovered {
            resp = resp.on_hover_ui_at_pointer(|ui| self.tooltip(ui, &tree, h));
        }
        if resp.clicked()
            && let Some(h) = self.hovered {
                self.actions.push(Action::Reveal(h));
            }
        if resp.double_clicked()
            && let Some(target) = self.hovered.and_then(|h| child_of_view(&tree, self.view_root, h))
                && tree.node(target).is_dir() {
                    self.actions.push(Action::Zoom(target));
                }
        if resp.secondary_clicked() {
            self.menu_node = self.hovered;
        }
        if let Some(m) = self.menu_node {
            resp.context_menu(|ui| self.node_menu(ui, &tree, m));
        }
    }

    fn tooltip(&self, ui: &mut egui::Ui, tree: &Tree, id: NodeId) {
        let n = tree.node(id);
        ui.set_max_width(460.0);
        ui.label(RichText::new(tree.name(id)).strong());
        ui.label(RichText::new(tree.path(id)).weak().size(11.5));
        if let Some(r) = crate::risk::assess(tree, id) {
            ui.add_space(3.0);
            super::warnings::explain(ui, &r);
        }
        ui.add_space(3.0);
        let view_size = tree.node(self.view_root).size;
        ui.label(format!("{}  ·  {:.2}% of this view", fmt_size(n.size), pct(n.size, view_size)));
        if n.alloc != n.size {
            ui.label(RichText::new(format!("{} on disk", fmt_size(n.alloc))).weak());
        }
        if n.is_dir() {
            ui.label(format!("{} files", fmt_count(n.files as u64)));
        } else {
            let e = &tree.exts[n.ext as usize];
            let ext = if e.name.is_empty() { "no extension".to_string() } else { format!(".{}", e.name) };
            ui.label(RichText::new(format!("{ext} · {}", e.category.label())).weak());
        }
        if n.mtime > 0 {
            ui.label(RichText::new(format!("Modified {}", platform::fmt_datetime(n.mtime))).weak());
        }
        if n.flags & crate::tree::flags::CLOUD != 0 {
            ui.label(RichText::new("☁ Online-only: not stored on this disk").weak());
        }
        if let Some(d) = &self.diff {
            match d.old_size(id) {
                Old::New => {
                    ui.colored_label(Color32::from_rgb(240, 110, 220), "New since the earlier scan");
                }
                Old::Size(old) => {
                    let delta = n.size as i64 - old as i64;
                    if delta != 0 {
                        ui.label(format!("{} since the earlier scan", fmt_delta(delta)));
                    }
                }
                Old::Unknown => {}
            }
        }
        ui.add_space(2.0);
        ui.label(RichText::new("Double-click to zoom · right-click for actions").weak().size(10.5));
    }

    fn legend(&self, painter: &egui::Painter, rect: Rect) {
        let items: Vec<(String, [f32; 3])> = match self.color_mode {
            ColorMode::Age => colors::AGE_LEGEND.iter().map(|(l, d)| (l.to_string(), colors::age_color(*d))).collect(),
            ColorMode::Growth => vec![
                ("new".into(), colors::NEW_COLOR),
                ("grew".into(), colors::GREW_COLOR),
                ("shrank".into(), colors::SHRANK_COLOR),
                ("unchanged".into(), colors::SAME_COLOR),
                ("no data".into(), colors::UNKNOWN_COLOR),
            ],
            _ => return,
        };
        let font = FontId::proportional(11.5);
        let pad = 8.0;
        let widths: Vec<f32> = items
            .iter()
            .map(|(l, _)| painter.layout_no_wrap(l.clone(), font.clone(), Color32::WHITE).size().x + 22.0)
            .collect();
        let total: f32 = widths.iter().sum::<f32>() + pad * 2.0;
        let bg = Rect::from_min_size(pos2(rect.left() + 10.0, rect.bottom() - 34.0), vec2(total, 24.0));
        painter.rect_filled(bg, 6.0, Color32::from_black_alpha(170));
        let mut x = bg.left() + pad;
        for ((label, c), w) in items.iter().zip(widths) {
            let sw = Rect::from_center_size(Pos2::new(x + 6.0, bg.center().y), vec2(11.0, 11.0));
            painter.rect_filled(sw, 2.0, colors::to_color32(*c));
            painter.text(pos2(x + 16.0, bg.center().y), Align2::LEFT_CENTER, label, font.clone(), Color32::from_gray(230));
            x += w;
        }
    }
}

/// Arrow keys pick the nearest rectangle at the same depth in that direction;
/// with nothing selected they start at the biggest item.
fn keyboard_target(
    ui: &egui::Ui,
    tree: &Tree,
    r: &treemap::Rendered,
    selected: Option<NodeId>,
    view_root: NodeId,
) -> Option<NodeId> {
    let dir = ui.input(|i| {
        if i.key_pressed(egui::Key::ArrowLeft) {
            Some((-1.0, 0.0))
        } else if i.key_pressed(egui::Key::ArrowRight) {
            Some((1.0, 0.0))
        } else if i.key_pressed(egui::Key::ArrowUp) {
            Some((0.0, -1.0))
        } else if i.key_pressed(egui::Key::ArrowDown) {
            Some((0.0, 1.0))
        } else {
            None
        }
    })?;
    let from = selected
        .filter(|&s| s != view_root && tree.is_ancestor(view_root, s))
        .and_then(|s| tree.ancestors(s).into_iter().rev().find_map(|a| r.rect_of(a)));
    match from {
        Some(f) => neighbour(r, &f, dir),
        None => r.rects.iter().find(|t| t.depth == 1).map(|t| t.node),
    }
}

/// Closest rectangle at `from`'s depth in direction `dir`, favouring ones
/// straight ahead over ones off to the side.
fn neighbour(r: &treemap::Rendered, from: &TmRect, dir: (f32, f32)) -> Option<NodeId> {
    let centre = |t: &TmRect| ((t.x0 + t.x1) / 2.0, (t.y0 + t.y1) / 2.0);
    let (cx, cy) = centre(from);
    r.rects
        .iter()
        .filter(|t| t.depth == from.depth && t.node != from.node)
        .filter_map(|t| {
            let (x, y) = centre(t);
            let (dx, dy) = (x - cx, y - cy);
            let ahead = dx * dir.0 + dy * dir.1;
            let side = (dx * dir.1 - dy * dir.0).abs();
            (ahead > 1.0).then_some((ahead + 2.0 * side, t.node))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, n)| n)
}

/// Names on rectangles big enough to hold them: top-level items get "name
/// size" in their top-left corner, big files deeper down get their name
/// centred.
fn draw_labels(
    painter: &egui::Painter,
    tree: &Tree,
    r: &treemap::Rendered,
    to_screen: &dyn Fn(&TmRect) -> Rect,
    by_alloc: bool,
) {
    let font = FontId::proportional(12.0);
    let mut drawn = 0;
    for t in &r.rects {
        if drawn >= 300 || t.depth == 0 {
            continue;
        }
        let sr = to_screen(t);
        let n = tree.node(t.node);
        let top_level = t.depth == 1;
        let (min_w, min_h) = if top_level { (70.0, 20.0) } else { (110.0, 34.0) };
        if sr.width() < min_w || sr.height() < min_h || (!top_level && n.is_dir()) {
            continue;
        }
        let size = if by_alloc { n.alloc } else { n.size };
        let mut galley = painter.layout_no_wrap(
            if top_level { format!("{}  {}", tree.name(t.node), fmt_size(size)) } else { tree.name(t.node).to_string() },
            font.clone(),
            Color32::WHITE,
        );
        if top_level && galley.size().x + 12.0 > sr.width() {
            galley = painter.layout_no_wrap(tree.name(t.node).to_string(), font.clone(), Color32::WHITE);
        }
        let pad = vec2(4.0, 2.0);
        let pos = if top_level {
            sr.min + vec2(3.0, 3.0)
        } else {
            sr.center() - galley.size() / 2.0
        };
        let bg = Rect::from_min_size(pos - pad, galley.size() + pad * 2.0).intersect(sr.shrink(1.0));
        if bg.width() < 24.0 {
            continue;
        }
        let clipped = painter.with_clip_rect(bg);
        clipped.rect_filled(bg, 3.0, Color32::from_black_alpha(150));
        clipped.galley(pos, galley, Color32::WHITE);
        drawn += 1;
    }
}

/// The direct child of `view` on the path down to `id` (or `id` itself).
fn child_of_view(tree: &Tree, view: NodeId, id: NodeId) -> Option<NodeId> {
    let chain = tree.ancestors(id);
    let i = chain.iter().position(|&a| a == view)?;
    chain.get(i + 1).copied()
}
