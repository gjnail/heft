//! Interactive treemap: hover tooltips, click to select, double-click to
//! drill in one level, right-click for actions.

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

        self.legend(&painter, rect);

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

/// The direct child of `view` on the path down to `id` (or `id` itself).
fn child_of_view(tree: &Tree, view: NodeId, id: NodeId) -> Option<NodeId> {
    let chain = tree.ancestors(id);
    let i = chain.iter().position(|&a| a == view)?;
    chain.get(i + 1).copied()
}
