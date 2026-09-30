//! The folder tree: a virtualised, custom-painted table so it stays smooth
//! with hundreds of thousands of expanded rows.

use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Rect, Sense, Shape, Stroke};

use super::{Action, HeftApp};
use crate::colors::to_color32;
use crate::history::Old;
use crate::platform;
use crate::tree::{flags, NodeId, Tree, ROOT};
use crate::util::{fmt_count, fmt_delta, fmt_size, pct};

pub const ROW_H: f32 = 22.0;
const INDENT: f32 = 14.0;
const MIN_NAME: f32 = 190.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Col {
    Size,
    OnDisk,
    Share,
    Files,
    Modified,
    Delta,
}

impl Col {
    fn width(self) -> f32 {
        match self {
            Col::Size => 70.0,
            Col::OnDisk => 70.0,
            Col::Share => 118.0,
            Col::Files => 72.0,
            Col::Modified => 80.0,
            Col::Delta => 84.0,
        }
    }
    fn title(self) -> &'static str {
        match self {
            Col::Size => "Size",
            Col::OnDisk => "On disk",
            Col::Share => "% of parent",
            Col::Files => "Files",
            Col::Modified => "Modified",
            Col::Delta => "Change",
        }
    }
}

struct Layout {
    name_end: f32,
    cols: Vec<(Col, f32, f32)>,
}

fn layout(left: f32, width: f32, with_delta: bool) -> Layout {
    // Columns in priority order; drop the least important when narrow.
    let mut wanted = vec![Col::Size, Col::Share];
    if with_delta {
        wanted.push(Col::Delta);
    }
    wanted.extend([Col::Files, Col::Modified, Col::OnDisk]);
    let mut budget = width - MIN_NAME;
    let chosen: Vec<Col> = wanted
        .into_iter()
        .filter(|c| {
            let ok = budget >= c.width();
            if ok {
                budget -= c.width();
            }
            ok
        })
        .collect();
    let order = [Col::Size, Col::OnDisk, Col::Share, Col::Delta, Col::Files, Col::Modified];
    let mut x = left + width - 4.0;
    let mut cols = Vec::new();
    for c in order.iter().rev().filter(|c| chosen.contains(c)) {
        cols.push((*c, x - c.width(), x));
        x -= c.width();
    }
    cols.reverse();
    Layout { name_end: x - 6.0, cols }
}

/// Subtree share bars get a hue per depth, like WinDirStat.
fn depth_color(depth: u16) -> Color32 {
    const C: [[u8; 3]; 6] = [
        [86, 146, 238],
        [76, 184, 170],
        [128, 190, 92],
        [226, 180, 72],
        [226, 128, 76],
        [196, 104, 176],
    ];
    let c = C[depth as usize % C.len()];
    Color32::from_rgb(c[0], c[1], c[2])
}

impl HeftApp {
    fn rebuild_rows(&mut self, tree: &Tree) {
        self.rows.clear();
        let mut stack = vec![(ROOT, 0u16)];
        while let Some((id, depth)) = stack.pop() {
            self.rows.push((id, depth));
            if self.expanded.contains(&id) {
                for &c in tree.children(id).iter().rev() {
                    stack.push((c, depth + 1));
                }
            }
        }
        self.rows_dirty = false;
    }

    pub(super) fn tree_view(&mut self, ui: &mut egui::Ui) {
        let Some(tree) = self.tree.clone() else { return };
        if self.rows_dirty {
            self.rebuild_rows(&tree);
        }
        let with_delta = self.diff.is_some();
        let full = ui.available_width();
        let (hrect, _) = ui.allocate_exact_size(vec2(full, 18.0), Sense::hover());
        let lay = layout(hrect.left(), full, with_delta);
        {
            let p = ui.painter();
            let weak = ui.visuals().weak_text_color();
            let f = FontId::proportional(11.5);
            p.text(pos2(hrect.left() + 6.0, hrect.center().y), Align2::LEFT_CENTER, "Name", f.clone(), weak);
            for &(c, x0, x1) in &lay.cols {
                let (pos, align) = match c {
                    Col::Share => (pos2(x0 + 4.0, hrect.center().y), Align2::LEFT_CENTER),
                    _ => (pos2(x1 - 6.0, hrect.center().y), Align2::RIGHT_CENTER),
                };
                p.text(pos, align, c.title(), f.clone(), weak);
            }
        }

        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("tree_rows");
        if self.scroll_to_selected {
            self.scroll_to_selected = false;
            if let Some(i) = self.rows.iter().position(|r| Some(r.0) == self.selected) {
                let y = i as f32 * ROW_H;
                let (offset, height) = ui.data(|d| d.get_temp::<(f32, f32)>(egui::Id::new("tree_scroll"))).unwrap_or((0.0, 0.0));
                if y < offset || y + ROW_H > offset + height {
                    area = area.vertical_scroll_offset((y - height / 3.0).max(0.0));
                }
            }
        }
        let rows = std::mem::take(&mut self.rows);
        let out = ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            area.show_rows(ui, ROW_H, rows.len(), |ui, range| {
                for i in range {
                    let (id, depth) = rows[i];
                    self.tree_row(ui, &tree, id, depth, &lay);
                }
            })
        });
        self.rows = rows;
        let state = (out.inner.state.offset.y, out.inner.inner_rect.height());
        ui.data_mut(|d| d.insert_temp(egui::Id::new("tree_scroll"), state));
    }

    fn tree_row(&mut self, ui: &mut egui::Ui, tree: &Tree, id: NodeId, depth: u16, lay: &Layout) {
        let width = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::click());
        if !ui.is_rect_visible(rect) {
            return;
        }
        let n = *tree.node(id);
        let v = ui.visuals().clone();
        let p = ui.painter();
        let selected = self.selected == Some(id);
        if selected {
            p.rect_filled(rect, 3.0, v.selection.bg_fill);
        } else if resp.hovered() || self.hovered == Some(id) {
            p.rect_filled(rect, 3.0, v.widgets.hovered.weak_bg_fill);
        } else if id == self.view_root && id != ROOT {
            p.rect_filled(rect, 3.0, v.faint_bg_color);
        }
        let text = if selected { v.selection.stroke.color } else { v.text_color() };
        let weak = if selected { v.selection.stroke.color.gamma_multiply(0.75) } else { v.weak_text_color() };
        let cy = rect.center().y;
        let font = FontId::proportional(13.0);
        let small = FontId::proportional(12.0);

        // Expander
        let mut x = rect.left() + 4.0 + depth as f32 * INDENT;
        let has_kids = n.is_dir() && !tree.children(id).is_empty();
        let exp_rect = Rect::from_center_size(pos2(x + 6.0, cy), vec2(16.0, ROW_H));
        if has_kids {
            let c = exp_rect.center();
            let pts = if self.expanded.contains(&id) {
                vec![pos2(c.x - 4.0, c.y - 2.0), pos2(c.x + 4.0, c.y - 2.0), pos2(c.x, c.y + 3.0)]
            } else {
                vec![pos2(c.x - 2.0, c.y - 4.0), pos2(c.x + 3.0, c.y), pos2(c.x - 2.0, c.y + 4.0)]
            };
            p.add(Shape::convex_polygon(pts, weak, Stroke::NONE));
        }
        x += 16.0;

        // Icon: folder tab or file-type swatch
        let icon = Rect::from_center_size(pos2(x + 6.0, cy), vec2(12.0, 10.0));
        if n.is_dir() {
            let folder = if n.flags & flags::LINK != 0 { Color32::from_gray(130) } else { Color32::from_rgb(232, 180, 76) };
            p.rect_filled(Rect::from_min_size(icon.min - vec2(0.0, 2.0), vec2(5.0, 3.0)), 1.0, folder);
            p.rect_filled(icon, 2.0, folder);
        } else {
            p.rect_filled(icon.shrink(1.0), 2.0, to_color32(self.ext_colors[n.ext as usize]));
        }
        x += 18.0;

        // Name (+ status markers)
        let mut name = if id == ROOT { tree.root_path.clone() } else { tree.name(id).to_string() };
        if n.flags & flags::LINK != 0 {
            name.push_str("  (link)");
        }
        if n.flags & flags::UNREADABLE != 0 {
            name.push_str("  (access denied)");
        }
        if n.flags & flags::CLOUD != 0 {
            name.push_str("  ☁");
        }
        if n.flags & flags::HARDLINK != 0 {
            name.push_str("  (hard link)");
        }
        if n.flags & flags::MOUNT != 0 {
            name.push_str("  (virtual file system, not scanned)");
        }
        if n.flags & flags::SEEN != 0 {
            name.push_str("  (same folder as another path)");
        }
        // Risk marker at the end of the name column; hover it for the reason.
        let risk = if id == ROOT { None } else { crate::risk::assess(tree, id) };
        let mut name_end = lay.name_end;
        if let Some(r) = &risk {
            let mark = Rect::from_center_size(pos2(lay.name_end - 8.0, cy), vec2(16.0, ROW_H));
            p.text(mark.center(), Align2::CENTER_CENTER, "⚠", font.clone(), super::warnings::color(r.level));
            name_end -= 18.0;
            if resp.hover_pos().is_some_and(|pt| mark.contains(pt)) {
                let r = *r;
                resp.clone().on_hover_ui(|ui| {
                    ui.set_max_width(360.0);
                    super::warnings::explain(ui, &r);
                });
            }
        }
        let name_clip = Rect::from_min_max(pos2(x, rect.top()), pos2(name_end, rect.bottom()));
        p.with_clip_rect(name_clip.intersect(p.clip_rect())).text(
            pos2(x, cy),
            Align2::LEFT_CENTER,
            name,
            font.clone(),
            if n.flags & flags::HIDDEN != 0 && !selected { text.gamma_multiply(0.7) } else { text },
        );

        let parent_size = if id == ROOT { n.size } else { tree.node(n.parent).size };
        for &(c, x0, x1) in &lay.cols {
            let right = pos2(x1 - 6.0, cy);
            match c {
                Col::Size => {
                    p.text(right, Align2::RIGHT_CENTER, fmt_size(n.size), font.clone(), text);
                }
                Col::OnDisk => {
                    p.text(right, Align2::RIGHT_CENTER, fmt_size(n.alloc), small.clone(), weak);
                }
                Col::Share => {
                    let share = pct(n.size, parent_size);
                    let bar = Rect::from_min_size(pos2(x0 + 4.0, cy - 5.0), vec2(56.0, 10.0));
                    p.rect_filled(bar, 2.0, v.extreme_bg_color);
                    let mut fill = bar;
                    fill.set_width(bar.width() * share / 100.0);
                    p.rect_filled(fill, 2.0, depth_color(depth));
                    p.text(right, Align2::RIGHT_CENTER, format!("{share:.1}%"), small.clone(), weak);
                }
                Col::Files => {
                    if n.is_dir() {
                        p.text(right, Align2::RIGHT_CENTER, fmt_count(n.files as u64), small.clone(), weak);
                    }
                }
                Col::Modified => {
                    p.text(right, Align2::RIGHT_CENTER, platform::fmt_date(n.mtime), small.clone(), weak);
                }
                Col::Delta => {
                    if let Some(d) = &self.diff {
                        let (label, color) = match d.old_size(id) {
                            Old::New => ("new".to_string(), Color32::from_rgb(240, 110, 220)),
                            Old::Size(old) => {
                                let delta = n.size as i64 - old as i64;
                                let color = if delta > 0 {
                                    Color32::from_rgb(240, 110, 90)
                                } else if delta < 0 {
                                    Color32::from_rgb(100, 210, 120)
                                } else {
                                    weak
                                };
                                (if delta == 0 { "-".into() } else { fmt_delta(delta) }, color)
                            }
                            Old::Unknown => (String::new(), weak),
                        };
                        p.text(right, Align2::RIGHT_CENTER, label, small.clone(), color);
                    }
                }
            }
        }

        // Interaction
        let on_expander = resp.interact_pointer_pos().is_some_and(|pt| exp_rect.contains(pt));
        if resp.clicked() {
            if on_expander && has_kids {
                self.actions.push(Action::ToggleExpand(id));
            } else {
                self.actions.push(Action::Select(id));
            }
        }
        if resp.double_clicked() && !on_expander {
            if n.is_dir() {
                self.actions.push(Action::Zoom(id));
                if !self.expanded.contains(&id) {
                    self.actions.push(Action::ToggleExpand(id));
                }
            } else {
                self.actions.push(Action::ShowInFileManager(id));
            }
        }
        if resp.hovered() {
            self.list_hover = Some(id);
        }
        resp.widget_info(|| {
            let kind = if n.is_dir() { "folder" } else { "file" };
            let mut label = format!("{}, {kind}, {}", tree.name(id), fmt_size(n.size));
            if let Some(r) = &risk {
                label.push_str(&format!(". Warning: {}", r.title));
            }
            egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, label)
        });
        resp.context_menu(|ui| self.node_menu(ui, tree, id));
    }
}
