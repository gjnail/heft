//! Side-panel tabs: file types, largest files, duplicates, build junk, changes.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use eframe::egui::{self, pos2, vec2, Align, Align2, Color32, FontId, Layout, Rect, RichText, Sense};

use super::tree_view::ROW_H;
use super::{Action, HeftApp};
use crate::colors::{to_color32, Category, CATEGORIES};
use crate::history;
use crate::platform;
use crate::tree::{flags, NodeId, Tree, ROOT};
use crate::treemap::{ColorMode, Highlight};
use crate::util::{fmt_ago, fmt_count, fmt_delta, fmt_duration_ms, fmt_size, pct};

const AGE_CHOICES: [(u32, &str); 5] = [(0, "any age"), (30, "1 month"), (182, "6 months"), (365, "1 year"), (730, "2 years")];
const DUP_SIZES: [(u64, &str); 4] = [(100 << 10, "100 KB"), (1 << 20, "1 MB"), (10 << 20, "10 MB"), (100 << 20, "100 MB")];

/// Case-insensitive substring match; `needle` must already be lowercase.
fn contains_ci(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if hay.is_ascii() && needle.is_ascii() {
        let (h, n) = (hay.as_bytes(), needle.as_bytes());
        return h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n));
    }
    hay.to_lowercase().contains(needle)
}

fn share_bar(ui: &egui::Ui, rect: Rect, frac: f32, color: Color32) {
    let p = ui.painter();
    p.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let mut f = rect;
    f.set_width(rect.width() * frac.clamp(0.0, 1.0));
    p.rect_filled(f, 2.0, color);
}

impl HeftApp {
    // ------------------------------------------------------------------
    // File types

    pub(super) fn types_tab(&mut self, ui: &mut egui::Ui) {
        let Some(tree) = self.tree.clone() else { return };
        let total = tree.node(ROOT).size.max(1);

        ui.label(RichText::new("Click a row to highlight it in the treemap.").weak());
        ui.add_space(6.0);

        let mut by_cat: HashMap<Category, (u64, u64)> = HashMap::new();
        for e in &tree.exts {
            let s = by_cat.entry(e.category).or_default();
            s.0 += e.size;
            s.1 += e.count;
        }
        let mut cats: Vec<(Category, u64, u64)> =
            CATEGORIES.iter().map(|&c| (c, by_cat.get(&c).map(|s| s.0).unwrap_or(0), by_cat.get(&c).map(|s| s.1).unwrap_or(0))).collect();
        cats.retain(|c| c.1 > 0);
        cats.sort_by(|a, b| b.1.cmp(&a.1));

        for (cat, size, count) in cats {
            let lit = self.highlight == Highlight::Category(cat);
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
            if lit {
                ui.painter().rect_filled(rect, 3.0, ui.visuals().selection.bg_fill);
            } else if resp.hovered() {
                ui.painter().rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
            }
            let cy = rect.center().y;
            let p = ui.painter();
            p.rect_filled(Rect::from_center_size(pos2(rect.left() + 12.0, cy), vec2(12.0, 12.0)), 3.0, to_color32(cat.color()));
            p.text(pos2(rect.left() + 26.0, cy), Align2::LEFT_CENTER, cat.label(), FontId::proportional(13.0), ui.visuals().text_color());
            let right = rect.right() - 6.0;
            p.text(pos2(right, cy), Align2::RIGHT_CENTER, format!("{} files", fmt_count(count)), FontId::proportional(11.5), ui.visuals().weak_text_color());
            p.text(pos2(right - 90.0, cy), Align2::RIGHT_CENTER, fmt_size(size), FontId::proportional(13.0), ui.visuals().text_color());
            share_bar(ui, Rect::from_min_size(pos2(right - 210.0, cy - 5.0), vec2(56.0, 10.0)), size as f32 / total as f32, to_color32(cat.color()));
            ui.painter().text(pos2(right - 150.0, cy), Align2::LEFT_CENTER, format!("{:.1}%", pct(size, total)), FontId::proportional(11.5), ui.visuals().weak_text_color());
            if resp.clicked() {
                self.actions.push(Action::SetHighlight(Highlight::Category(cat)));
                if self.color_mode != ColorMode::Category && self.color_mode != ColorMode::Extension {
                    self.color_mode = ColorMode::Category;
                }
            }
        }

        ui.add_space(8.0);
        ui.separator();
        ui.label(RichText::new("Extensions").strong());
        let mut exts: Vec<usize> = (0..tree.exts.len()).filter(|&i| tree.exts[i].count > 0).collect();
        exts.sort_by(|&a, &b| tree.exts[b].size.cmp(&tree.exts[a].size));

        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("ext_rows").show_rows(ui, ROW_H, exts.len(), |ui, range| {
                for &i in &exts[range] {
                    let e = &tree.exts[i];
                    let lit = self.highlight == Highlight::Ext(i as u16);
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                    if lit {
                        ui.painter().rect_filled(rect, 3.0, ui.visuals().selection.bg_fill);
                    } else if resp.hovered() {
                        ui.painter().rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
                    }
                    let cy = rect.center().y;
                    let color = match self.color_mode {
                        ColorMode::Category => e.category.color(),
                        _ => self.ext_colors[i],
                    };
                    let p = ui.painter();
                    p.rect_filled(Rect::from_center_size(pos2(rect.left() + 12.0, cy), vec2(12.0, 12.0)), 3.0, to_color32(color));
                    let name = if e.name.is_empty() { "(no extension)".to_string() } else { format!(".{}", e.name) };
                    p.text(pos2(rect.left() + 26.0, cy), Align2::LEFT_CENTER, name, FontId::proportional(13.0), ui.visuals().text_color());
                    let right = rect.right() - 6.0;
                    p.text(pos2(right, cy), Align2::RIGHT_CENTER, format!("{} files", fmt_count(e.count)), FontId::proportional(11.5), ui.visuals().weak_text_color());
                    p.text(pos2(right - 90.0, cy), Align2::RIGHT_CENTER, fmt_size(e.size), FontId::proportional(13.0), ui.visuals().text_color());
                    p.text(pos2(right - 150.0, cy), Align2::LEFT_CENTER, format!("{:.1}%", pct(e.size, total)), FontId::proportional(11.5), ui.visuals().weak_text_color());
                    p.text(pos2(right - 250.0, cy), Align2::LEFT_CENTER, e.category.label(), FontId::proportional(11.0), ui.visuals().weak_text_color());
                    if resp.clicked() {
                        self.actions.push(Action::SetHighlight(Highlight::Ext(i as u16)));
                    }
                }
            });
        });
    }

    // ------------------------------------------------------------------
    // Largest files

    pub(super) fn largest_tab(&mut self, ui: &mut egui::Ui) {
        let Some(tree) = self.tree.clone() else { return };
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.largest.filter).hint_text("Filter: name or .ext").desired_width(190.0));
            ui.label("older than");
            let current = AGE_CHOICES.iter().find(|c| c.0 == self.largest.min_age_days).map(|c| c.1).unwrap_or("any age");
            egui::ComboBox::from_id_salt("age_filter").selected_text(current).show_ui(ui, |ui| {
                for (d, l) in AGE_CHOICES {
                    ui.selectable_value(&mut self.largest.min_age_days, d, l);
                }
            });
        });
        ui.label(RichText::new(format!("Top 500 in {}", tree.path(self.view_root))).weak().size(11.5));
        ui.add_space(4.0);

        let filter = self.largest.filter.trim().to_lowercase();
        let key = (Arc::as_ptr(&tree) as usize, tree.version, self.view_root, filter.clone(), self.largest.min_age_days);
        if self.largest.key.as_ref() != Some(&key) {
            let cutoff = if self.largest.min_age_days == 0 { i64::MAX } else { platform::now_unix() - self.largest.min_age_days as i64 * 86_400 };
            let ext_filter = filter.strip_prefix("*.").or_else(|| filter.strip_prefix('.')).map(str::to_string);
            let ext_id = ext_filter.as_ref().and_then(|x| tree.exts.iter().position(|e| &e.name == x));
            self.largest.results = tree.largest_files(self.view_root, 500, |id, n| {
                if n.mtime > cutoff {
                    return false;
                }
                match (&ext_filter, ext_id) {
                    (Some(_), Some(e)) => n.ext as usize == e,
                    (Some(_), None) => false,
                    _ => contains_ci(tree.name(id), &filter),
                }
            });
            self.largest.key = Some(key);
        }

        if self.largest.results.is_empty() {
            ui.label(RichText::new("No matching files.").weak());
            return;
        }
        let results = self.largest.results.clone();
        let view_size = tree.node(self.view_root).size.max(1);
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("largest_rows").show_rows(ui, ROW_H * 1.6, results.len(), |ui, range| {
                for &id in &results[range] {
                    let n = *tree.node(id);
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H * 1.6), Sense::click());
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
                    share_bar(ui, Rect::from_min_size(pos2(rect.left() + 6.0, bottom - 3.0), vec2(62.0, 6.0)), n.size as f32 / view_size as f32, to_color32(self.ext_colors[n.ext as usize]));
                    let text_x = rect.left() + 80.0;
                    let clip = Rect::from_min_max(pos2(text_x, rect.top()), pos2(rect.right() - 90.0, rect.bottom()));
                    let pc = p.with_clip_rect(clip.intersect(p.clip_rect()));
                    pc.text(pos2(text_x, top), Align2::LEFT_CENTER, tree.name(id), FontId::proportional(13.0), v.text_color());
                    pc.text(pos2(text_x, bottom), Align2::LEFT_CENTER, tree.path(n.parent), FontId::proportional(11.0), v.weak_text_color());
                    p.text(pos2(rect.right() - 6.0, top), Align2::RIGHT_CENTER, platform::fmt_date(n.mtime), FontId::proportional(11.5), v.weak_text_color());
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
                    resp.context_menu(|ui| self.node_menu(ui, &tree, id));
                }
            });
        });
    }

    // ------------------------------------------------------------------
    // Duplicates

    pub(super) fn dupes_tab(&mut self, ui: &mut egui::Ui) {
        let Some(tree) = self.tree.clone() else { return };

        if let Some((p, _, started)) = &self.dupes.running {
            let phase = p.phase.lock().unwrap().clone();
            let (done, total) = (p.done.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new(phase).strong());
            });
            let frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
            ui.add(egui::ProgressBar::new(frac).text(format!("{} / {} files", fmt_count(done), fmt_count(total))));
            ui.label(RichText::new(format!("{} read · {}", fmt_size(p.bytes.load(Ordering::Relaxed)), fmt_duration_ms(started.elapsed().as_millis() as u64))).weak());
            ui.add_space(6.0);
            if ui.button("Cancel").clicked() {
                p.cancel.store(true, Ordering::Relaxed);
            }
            return;
        }

        ui.horizontal_wrapped(|ui| {
            ui.label("Files of at least");
            let current = DUP_SIZES.iter().find(|s| s.0 == self.dupes.min_size).map(|s| s.1).unwrap_or("1 MB");
            egui::ComboBox::from_id_salt("dup_min").selected_text(current).show_ui(ui, |ui| {
                for (s, l) in DUP_SIZES {
                    ui.selectable_value(&mut self.dupes.min_size, s, l);
                }
            });
            ui.label("in");
            ui.label(RichText::new(tree.path(self.view_root)).strong());
            if ui.button("Find duplicates").clicked() {
                let ctx = ui.ctx().clone();
                self.start_dupes(&ctx);
            }
        });
        ui.label(
            RichText::new("Compares size, then samples, then full contents. Hard links and online-only files are skipped.")
                .weak()
                .size(11.5),
        );
        ui.add_space(6.0);

        if !self.dupes.searched {
            return;
        }
        if self.dupes.groups.is_empty() {
            ui.label(RichText::new("No duplicates found.").strong());
            return;
        }

        let wasted: u64 = self.dupes.groups.iter().map(|g| g.wasted()).sum();
        let checked_size: u64 = self.dupes.checked.iter().map(|&id| tree.node(id).size).sum();
        ui.label(
            RichText::new(format!("{} groups · {} could be freed", fmt_count(self.dupes.groups.len() as u64), fmt_size(wasted)))
                .strong(),
        );
        ui.horizontal_wrapped(|ui| {
            if ui.button("Select all but the oldest").on_hover_text("Keeps the oldest copy in every group").clicked() {
                self.dupes.checked.clear();
                for g in &self.dupes.groups {
                    self.dupes.checked.extend(g.files.iter().skip(1));
                }
            }
            if ui.button("Clear selection").clicked() {
                self.dupes.checked.clear();
            }
            let n = self.dupes.checked.len();
            let del = ui.add_enabled(n > 0, egui::Button::new(format!("Recycle {n} selected ({})", fmt_size(checked_size))));
            if del.clicked() {
                let mut ids: Vec<NodeId> = self.dupes.checked.iter().copied().collect();
                ids.sort_unstable();
                self.actions.push(Action::Delete(ids));
            }
        });
        let unsafe_groups = self
            .dupes
            .groups
            .iter()
            .filter(|g| !g.files.is_empty() && g.files.iter().all(|f| self.dupes.checked.contains(f)))
            .count();
        if unsafe_groups > 0 {
            ui.colored_label(ui.visuals().warn_fg_color, format!("⚠ {unsafe_groups} group(s) have every copy selected."));
        }
        ui.add_space(4.0);

        enum Row {
            Header(usize),
            File(NodeId),
        }
        let mut rows = Vec::new();
        for (gi, g) in self.dupes.groups.iter().enumerate() {
            rows.push(Row::Header(gi));
            if !self.dupes.collapsed.contains(&gi) {
                rows.extend(g.files.iter().map(|&f| Row::File(f)));
            }
        }

        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("dup_rows").show_rows(ui, ROW_H, rows.len(), |ui, range| {
                for row in &rows[range] {
                    match *row {
                        Row::Header(gi) => {
                            let g = &self.dupes.groups[gi];
                            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                            ui.painter().rect_filled(rect.shrink2(vec2(0.0, 1.0)), 3.0, ui.visuals().faint_bg_color);
                            let arrow = if self.dupes.collapsed.contains(&gi) { "⏵" } else { "⏷" };
                            let name = g.files.first().map(|&f| tree.name(f)).unwrap_or("");
                            let clip = rect.shrink2(vec2(6.0, 0.0));
                            let p = ui.painter().with_clip_rect(clip.intersect(ui.clip_rect()));
                            p.text(
                                pos2(rect.left() + 6.0, rect.center().y),
                                Align2::LEFT_CENTER,
                                format!("{arrow}  {} × {}   {name}", g.files.len(), fmt_size(g.size)),
                                FontId::proportional(13.0),
                                ui.visuals().strong_text_color(),
                            );
                            p.text(
                                pos2(rect.right() - 6.0, rect.center().y),
                                Align2::RIGHT_CENTER,
                                format!("{} wasted", fmt_size(g.wasted())),
                                FontId::proportional(11.5),
                                ui.visuals().weak_text_color(),
                            );
                            if resp.clicked() && !self.dupes.collapsed.remove(&gi) {
                                self.dupes.collapsed.insert(gi);
                            }
                        }
                        Row::File(id) => {
                            ui.allocate_ui_with_layout(vec2(ui.available_width(), ROW_H), Layout::left_to_right(Align::Center), |ui| {
                                ui.add_space(14.0);
                                let mut on = self.dupes.checked.contains(&id);
                                if ui.checkbox(&mut on, "").changed() {
                                    if on {
                                        self.dupes.checked.insert(id);
                                    } else {
                                        self.dupes.checked.remove(&id);
                                    }
                                }
                                let date = platform::fmt_date(tree.node(id).mtime);
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.label(RichText::new(date).weak().size(11.5));
                                    let path = ui.add(egui::Label::new(tree.path(id)).truncate().sense(Sense::click()));
                                    if path.hovered() {
                                        self.list_hover = Some(id);
                                    }
                                    if path.clicked() {
                                        self.actions.push(Action::Reveal(id));
                                    }
                                    if path.double_clicked() {
                                        self.actions.push(Action::ShowInFileManager(id));
                                    }
                                    path.context_menu(|ui| self.node_menu(ui, &tree, id));
                                });
                            });
                        }
                    }
                }
            });
        });
    }

    // ------------------------------------------------------------------
    // Build junk

    pub(super) fn junk_tab(&mut self, ui: &mut egui::Ui) {
        let Some(tree) = self.tree.clone() else { return };
        let key = (Arc::as_ptr(&tree) as usize, tree.version, self.view_root);
        if self.junk.key != Some(key) {
            self.junk.results = crate::devjunk::find(&tree, self.view_root);
            let ids: HashSet<NodeId> = self.junk.results.iter().map(|j| j.id).collect();
            self.junk.checked.retain(|id| ids.contains(id));
            self.junk.key = Some(key);
        }
        ui.label(
            RichText::new("Folders that build tools and package managers recreate on demand: node_modules, Cargo target, Unity and Unreal caches, and so on.")
                .weak(),
        );
        ui.label(RichText::new(format!("In {}", tree.path(self.view_root))).weak().size(11.5));
        ui.add_space(6.0);
        if self.junk.results.is_empty() {
            ui.label(RichText::new("No build junk here.").strong());
            return;
        }

        let total: u64 = self.junk.results.iter().map(|j| tree.node(j.id).size).sum();
        let checked_size: u64 = self.junk.checked.iter().map(|&id| tree.node(id).size).sum();
        ui.label(RichText::new(format!("{} folders · {}", fmt_count(self.junk.results.len() as u64), fmt_size(total))).strong());
        ui.horizontal_wrapped(|ui| {
            ui.label("Select untouched for");
            let now = platform::now_unix();
            for (days, label) in [(30i64, "1 month"), (90, "3 months"), (365, "1 year")] {
                if ui.button(label).on_hover_text("Nothing inside was modified in that time").clicked() {
                    let cutoff = now - days * 86_400;
                    self.junk.checked = self.junk.results.iter().filter(|j| tree.node(j.id).mtime < cutoff).map(|j| j.id).collect();
                }
            }
            if ui.button("Clear").clicked() {
                self.junk.checked.clear();
            }
            let n = self.junk.checked.len();
            let del = ui.add_enabled(n > 0, egui::Button::new(format!("Recycle {n} selected ({})", fmt_size(checked_size))));
            if del.clicked() {
                let mut ids: Vec<NodeId> = self.junk.checked.iter().copied().collect();
                ids.sort_unstable();
                self.actions.push(Action::Delete(ids));
            }
        });
        ui.add_space(4.0);

        let results = self.junk.results.clone();
        let base = tree.path(self.view_root);
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("junk_rows").show_rows(ui, ROW_H * 1.6, results.len(), |ui, range| {
                for j in &results[range] {
                    let n = *tree.node(j.id);
                    ui.allocate_ui_with_layout(vec2(ui.available_width(), ROW_H * 1.6), Layout::left_to_right(Align::Center), |ui| {
                        let mut on = self.junk.checked.contains(&j.id);
                        if ui.checkbox(&mut on, "").changed() {
                            if on {
                                self.junk.checked.insert(j.id);
                            } else {
                                self.junk.checked.remove(&j.id);
                            }
                        }
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(fmt_size(n.size)).strong());
                                ui.label(RichText::new(j.kind).weak());
                            });
                            let mut rel = tree.path(j.id);
                            if let Some(stripped) = rel.strip_prefix(base.trim_end_matches(platform::SEP)) {
                                rel = format!("…{stripped}");
                            }
                            let path = ui.add(egui::Label::new(RichText::new(rel).size(11.5)).truncate().sense(Sense::click()));
                            if path.hovered() {
                                self.list_hover = Some(j.id);
                            }
                            if path.clicked() {
                                self.actions.push(Action::Reveal(j.id));
                            }
                            if path.double_clicked() {
                                self.actions.push(Action::ShowInFileManager(j.id));
                            }
                            path.context_menu(|ui| self.node_menu(ui, &tree, j.id));
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(platform::fmt_date(n.mtime)).weak().size(11.5)).on_hover_text("Last modified (anything inside)");
                        });
                    });
                }
            });
        });
    }

    // ------------------------------------------------------------------
    // Changes since an earlier scan

    pub(super) fn changes_tab(&mut self, ui: &mut egui::Ui) {
        let Some(tree) = self.tree.clone() else { return };
        if self.snapshots.is_empty() {
            ui.add_space(10.0);
            ui.label(RichText::new("No earlier scans of this location yet.").strong());
            ui.label(
                RichText::new(
                    "Heft saves a compact snapshot after every scan. Scan this location again later and \
                     this tab will show exactly which folders grew or shrank.",
                )
                .weak(),
            );
            ui.add_space(6.0);
            ui.label(RichText::new(format!("Snapshots are stored in {}", history::history_dir().display())).weak().size(11.0));
            return;
        }

        let now = platform::now_unix();
        let describe = |m: &history::SnapMeta| {
            format!("{}  ({}),  {}", platform::fmt_datetime(m.taken_at), fmt_ago(now - m.taken_at), fmt_size(m.total_size))
        };
        ui.horizontal(|ui| {
            ui.label("Compare with");
            let idx = self.compare_to.min(self.snapshots.len() - 1);
            egui::ComboBox::from_id_salt("compare_to").width(300.0).selected_text(describe(&self.snapshots[idx])).show_ui(ui, |ui| {
                for (i, m) in self.snapshots.iter().enumerate() {
                    ui.selectable_value(&mut self.compare_to, i, describe(m));
                }
            });
            let busy = self.diff_job.is_some();
            if ui.add_enabled(!busy, egui::Button::new("Compare")).clicked() {
                let ctx = ui.ctx().clone();
                self.start_diff(&ctx);
            }
            if busy {
                ui.spinner();
            }
        });

        let Some(diff) = self.diff.clone() else {
            ui.add_space(6.0);
            ui.label(RichText::new("Pick an earlier scan and press Compare.").weak());
            return;
        };

        ui.add_space(6.0);
        let total = diff.delta(&tree, ROOT).unwrap_or(0);
        let color = if total > 0 { Color32::from_rgb(240, 110, 90) } else { Color32::from_rgb(100, 210, 120) };
        ui.horizontal(|ui| {
            ui.label(RichText::new(fmt_delta(total)).size(20.0).strong().color(color));
            ui.label(RichText::new(format!(
                "since {} ({})",
                platform::fmt_datetime(diff.against.taken_at),
                fmt_ago(now - diff.against.taken_at)
            )).weak());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Clear").clicked() {
                    self.diff = None;
                    if self.color_mode == ColorMode::Growth {
                        self.color_mode = ColorMode::Extension;
                    }
                }
            });
        });
        let files_now = tree.node(ROOT).files as i64;
        ui.label(RichText::new(format!(
            "{} files, was {} ({:+})",
            fmt_count(files_now as u64),
            fmt_count(diff.against.total_files),
            files_now - diff.against.total_files as i64
        )).weak());
        ui.add_space(6.0);

        let key = (tree.version, self.view_root, Arc::as_ptr(&diff) as usize);
        if self.hotspots_key != Some(key) {
            self.hotspots = history::hotspots(&tree, &diff, self.view_root, 300);
            self.hotspots_key = Some(key);
        }
        ui.label(RichText::new("Where it changed").strong());
        ui.label(RichText::new(format!("Within {}", tree.path(self.view_root))).weak().size(11.5));

        let hot = self.hotspots.clone();
        let removed_h = if diff.removed.is_empty() { 0.0 } else { 140.0 };
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::ScrollArea::vertical()
                .id_salt("hot_rows")
                .max_height((ui.available_height() - removed_h).max(120.0))
                .auto_shrink([false, true])
                .show_rows(ui, ROW_H, hot.len(), |ui, range| {
                    for &(id, delta) in &hot[range] {
                        self.change_row(ui, &tree, id, delta, &diff);
                    }
                });
        });
        if hot.is_empty() {
            ui.label(RichText::new("No significant changes here.").weak());
        }

        if !diff.removed.is_empty() {
            ui.add_space(6.0);
            egui::CollapsingHeader::new(format!("Removed since then ({})", diff.removed.len())).show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("removed_rows").max_height(110.0).show(ui, |ui| {
                    for r in diff.removed.iter().take(500) {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("−{}", fmt_size(r.size))).color(Color32::from_rgb(100, 210, 120)));
                            let sep = if r.is_dir { platform::SEP.to_string() } else { String::new() };
                            ui.add(egui::Label::new(RichText::new(format!("{}{sep}", r.path)).weak()).truncate());
                        });
                    }
                });
            });
        }
    }

    fn change_row(&mut self, ui: &mut egui::Ui, tree: &Tree, id: NodeId, delta: i64, diff: &history::Diff) {
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
        if self.selected == Some(id) {
            ui.painter().rect_filled(rect, 3.0, ui.visuals().selection.bg_fill);
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
        }
        let cy = rect.center().y;
        let is_new = diff.old_size(id) == history::Old::New;
        let color = if is_new {
            Color32::from_rgb(240, 110, 220)
        } else if delta > 0 {
            Color32::from_rgb(240, 110, 90)
        } else {
            Color32::from_rgb(100, 210, 120)
        };
        let p = ui.painter();
        p.text(pos2(rect.left() + 86.0, cy), Align2::RIGHT_CENTER, fmt_delta(delta), FontId::proportional(13.0), color);
        let clip = Rect::from_min_max(pos2(rect.left() + 96.0, rect.top()), pos2(rect.right() - 4.0, rect.bottom()));
        let mut rel = tree.path(id);
        let base = tree.path(self.view_root);
        if let Some(stripped) = rel.strip_prefix(base.trim_end_matches(platform::SEP)) {
            rel = format!("…{stripped}");
        }
        if tree.node(id).is_dir() {
            rel.push(platform::SEP);
        }
        if is_new {
            rel.push_str("   (new)");
        }
        p.with_clip_rect(clip.intersect(p.clip_rect())).text(
            pos2(clip.left(), cy),
            Align2::LEFT_CENTER,
            rel,
            FontId::proportional(12.5),
            ui.visuals().text_color(),
        );
        if resp.hovered() {
            self.list_hover = Some(id);
        }
        if resp.clicked() {
            self.actions.push(Action::Reveal(id));
        }
        if resp.double_clicked() && tree.node(id).is_dir() {
            self.actions.push(Action::Zoom(id));
        }
        resp.context_menu(|ui| self.node_menu(ui, tree, id));
    }
}
