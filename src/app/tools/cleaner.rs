//! Junk cleaner workspace: rule checklist on the left, analysis, results and
//! cleaning on the right.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError};
use eframe::egui::{self, pos2, vec2, Align, Align2, FontId, Layout, Rect, RichText, Sense};

use super::{big_button, card, heading, row_background, share_bar, Cx, AMBER, GREEN};
use crate::clean::{self, rules, rules::Group, Cleaned, Found};
use crate::util::{fmt_count, fmt_duration_ms, fmt_size};

const ROW_H: f32 = 24.0;

struct Analysis {
    progress: Arc<clean::Progress>,
    rx: Receiver<Found>,
}

struct Cleaning {
    progress: Arc<clean::Progress>,
    rx: Receiver<Vec<Cleaned>>,
    started: Instant,
}

struct Report {
    cleaned: Vec<Cleaned>,
    took: Duration,
}

pub struct State {
    selected: HashSet<usize>,
    results: HashMap<usize, Found>,
    analysis: Option<Analysis>,
    cleaning: Option<Cleaning>,
    report: Option<Report>,
    collapsed: HashSet<Group>,
    show_missing: bool,
    /// Rule whose files are listed, with the file order (largest first).
    detail: Option<(usize, Vec<u32>)>,
    confirm: bool,
    #[cfg(windows)]
    win: super::cleaner_win::WinExtras,
    started: bool,
    /// Every rule has reported at least once, so "found nothing" is known.
    analyzed_once: bool,
}

impl State {
    pub fn new() -> Self {
        State {
            selected: clean::load_selection(),
            results: HashMap::new(),
            analysis: None,
            cleaning: None,
            report: None,
            collapsed: HashSet::new(),
            show_missing: false,
            detail: None,
            confirm: false,
            #[cfg(windows)]
            win: super::cleaner_win::WinExtras::new(),
            started: false,
            analyzed_once: false,
        }
    }

    fn start_analysis(&mut self, ctx: &egui::Context) {
        let progress = Arc::new(clean::Progress::default());
        let (tx, rx) = crossbeam_channel::unbounded();
        let (p, ctx) = (progress.clone(), ctx.clone());
        // `poll` keeps repainting while results stream in.
        std::thread::spawn(move || {
            clean::analyze_all((0..rules::all().len()).collect(), tx, &p);
            ctx.request_repaint();
        });
        self.detail = None;
        self.analysis = Some(Analysis { progress, rx });
    }

    fn start_cleaning(&mut self, ctx: &egui::Context) {
        let jobs: Vec<Found> = self
            .results
            .iter()
            .filter(|(r, f)| self.selected.contains(r) && f.is_actionable())
            .map(|(_, f)| f.clone())
            .collect();
        let progress = Arc::new(clean::Progress::default());
        progress.total.store(jobs.iter().map(|f| f.files.len() as u64).sum(), Ordering::Relaxed);
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (p, ctx) = (progress.clone(), ctx.clone());
        std::thread::spawn(move || {
            let running = clean::running_processes();
            let out: Vec<Cleaned> = jobs.iter().map(|f| clean::clean(f, &running, &p)).collect();
            let _ = tx.send(out);
            ctx.request_repaint();
        });
        self.report = None;
        self.detail = None;
        self.cleaning = Some(Cleaning { progress, rx, started: Instant::now() });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(a) = &self.analysis {
            loop {
                match a.rx.try_recv() {
                    Ok(f) => {
                        self.results.insert(f.rule, f);
                    }
                    Err(TryRecvError::Empty) => {
                        ctx.request_repaint_after(Duration::from_millis(150));
                        break;
                    }
                    Err(TryRecvError::Disconnected) => {
                        self.analysis = None;
                        self.analyzed_once = true;
                        break;
                    }
                }
            }
        }
        if let Some(c) = &self.cleaning {
            if let Ok(cleaned) = c.rx.try_recv() {
                let took = c.started.elapsed();
                self.cleaning = None;
                // What was cleaned is stale; the rest stays until re-analysis replaces it.
                for c in &cleaned {
                    self.results.remove(&c.rule);
                }
                self.report = Some(Report { cleaned, took });
                // Refresh the numbers so they reflect what's left.
                self.start_analysis(ctx);
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
        #[cfg(windows)]
        self.win.poll(ctx);
    }

    fn set_selected(&mut self, rule: usize, on: bool, cx: &mut Cx) {
        let changed = if on { self.selected.insert(rule) } else { self.selected.remove(&rule) };
        if changed && let Err(e) = clean::save_selection(&self.selected) {
            cx.toast(format!("Could not save the selection: {e}"), true);
        }
    }

    /// Selected, analyzed and cleanable right now.
    fn ready(&self) -> impl Iterator<Item = &Found> {
        self.results.iter().filter(|(r, f)| self.selected.contains(r) && f.is_actionable()).map(|(_, f)| f)
    }

    /// Nothing was found for this rule (after analysis): usually the program
    /// isn't installed.
    fn not_found(&self, r: usize) -> bool {
        // Without administrator rights, "nothing found" may just mean "couldn't look".
        self.results.get(&r).is_some_and(|f| f.items == 0 && !f.special_only() && f.bytes == 0 && !f.needs_admin)
    }

    pub fn show(&mut self, ui: &mut egui::Ui, mut cx: Cx) {
        let ctx = ui.ctx().clone();
        if !self.started {
            self.started = true;
            self.start_analysis(&ctx);
            #[cfg(windows)]
            self.win.start(&ctx);
        }
        self.poll(&ctx);

        egui::Panel::left("clean_rules").default_size(390.0).min_size(300.0).show(ui, |ui| self.rule_list(ui, &mut cx));
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("clean_main").auto_shrink([false, false]).show(ui, |ui| {
                self.main(ui, &mut cx);
            });
        });
        self.confirm_modal(&ctx);
        #[cfg(windows)]
        self.win.modals(&ctx);
    }

    // ------------------------------------------------------------------
    // Left: what to clean

    fn rule_list(&mut self, ui: &mut egui::Ui, cx: &mut Cx) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("What to clean").strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("None").clicked() {
                    self.selected.clear();
                    let _ = clean::save_selection(&self.selected);
                }
                if ui.small_button("Recommended").on_hover_text("Heft's defaults: caches and temporary files only").clicked() {
                    let all = rules::all();
                    self.selected = (0..all.len()).filter(|&i| all[i].default_on).collect();
                    let _ = clean::save_selection(&self.selected);
                }
            });
        });
        ui.separator();

        let analyzed = self.analyzed_once;
        let hidden = if analyzed { (0..rules::all().len()).filter(|&r| self.not_found(r)).count() } else { 0 };

        egui::ScrollArea::vertical().id_salt("rule_list").auto_shrink([false, false]).show(ui, |ui| {
            for group in Group::ALL {
                self.group_rows(ui, group, analyzed, cx);
            }
            if hidden > 0 {
                ui.add_space(6.0);
                let label = if self.show_missing {
                    "Hide programs that aren't installed".to_string()
                } else {
                    format!("Show {hidden} more (nothing found on this PC)")
                };
                if ui.add(egui::Button::new(RichText::new(label).weak()).frame(false)).clicked() {
                    self.show_missing = !self.show_missing;
                }
            }
        });
    }

    fn group_rows(&mut self, ui: &mut egui::Ui, group: Group, analyzed: bool, cx: &mut Cx) {
        let all = rules::all();
        let members: Vec<usize> = (0..all.len())
            .filter(|&r| all[r].group == group)
            .filter(|&r| !analyzed || self.show_missing || !self.not_found(r))
            .collect();
        if members.is_empty() {
            return;
        }
        let on = members.iter().filter(|r| self.selected.contains(r)).count();
        let size: u64 = members
            .iter()
            .filter(|r| self.selected.contains(r))
            .filter_map(|r| self.results.get(r))
            .map(|f| f.bytes)
            .sum();

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let collapsed = self.collapsed.contains(&group);
            if ui.add(egui::Button::new(if collapsed { "⏵" } else { "⏷" }).frame(false)).clicked() {
                if collapsed {
                    self.collapsed.remove(&group);
                } else {
                    self.collapsed.insert(group);
                }
            }
            let mut all_on = on == members.len();
            let cb = egui::Checkbox::new(&mut all_on, RichText::new(group.label()).strong())
                .indeterminate(on > 0 && on < members.len());
            if ui.add(cb).changed() {
                for &r in &members {
                    self.set_selected(r, all_on, cx);
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if size > 0 {
                    ui.label(RichText::new(fmt_size(size)).strong());
                }
            });
        });
        if self.collapsed.contains(&group) {
            return;
        }

        let mut last_app = "";
        for &r in &members {
            let rule = &all[r];
            let siblings = members.iter().filter(|&&m| all[m].app == rule.app).count();
            let under_app = siblings > 1 && rule.app != clean::SYSTEM;
            if under_app && rule.app != last_app {
                ui.horizontal(|ui| {
                    ui.add_space(26.0);
                    ui.label(RichText::new(rule.app).strong().size(12.5));
                });
            }
            last_app = rule.app;
            let label = if under_app || rule.app == clean::SYSTEM {
                RichText::new(rule.name)
            } else {
                RichText::new(format!("{} · {}", rule.app, rule.name))
            };
            self.rule_row(ui, r, label, if under_app { 40.0 } else { 26.0 }, cx);
        }
    }

    fn rule_row(&mut self, ui: &mut egui::Ui, r: usize, label: RichText, indent: f32, cx: &mut Cx) {
        let rule = &rules::all()[r];
        let found = self.results.get(&r);
        let analyzing = self.analysis.is_some();
        let mut toggled = None;
        ui.horizontal(|ui| {
            ui.set_height(ROW_H - 4.0);
            ui.add_space(indent);
            let mut on = self.selected.contains(&r);
            let mut hover = rule.about.to_string();
            if let Some(w) = rule.warning {
                hover.push_str(&format!("\n\n⚠ {w}"));
            }
            let dim = found.is_some_and(|f| f.items == 0 && !f.special_only());
            let label = if dim { label.weak() } else { label };
            if ui.checkbox(&mut on, label).on_hover_text(hover).changed() {
                toggled = Some(on);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(f) = found {
                    if f.bytes > 0 {
                        ui.label(fmt_size(f.bytes));
                    } else if f.items > 0 {
                        ui.label(RichText::new(format!("{} items", fmt_count(f.items))).weak());
                    }
                    if let Some(exe) = f.blocked_by {
                        ui.label(RichText::new("⏸").color(AMBER)).on_hover_text(format!("{exe} is running. Close it to clean this."));
                    }
                } else if analyzing {
                    ui.spinner();
                }
                if rule.admin && !cx.elevated {
                    ui.label(RichText::new("🛡").weak()).on_hover_text("Needs administrator rights");
                }
                if rule.warning.is_some() {
                    ui.label(RichText::new("⚠").color(AMBER)).on_hover_text(rule.warning.unwrap_or_default());
                }
            });
        });
        if let Some(on) = toggled {
            self.set_selected(r, on, cx);
        }
    }

    // ------------------------------------------------------------------
    // Right: summary, results, details

    fn main(&mut self, ui: &mut egui::Ui, cx: &mut Cx) {
        heading(
            ui,
            "Junk cleaner",
            "Caches, temporary files and leftovers that programs recreate when needed. Nothing is touched until you press Clean.",
        );
        let ctx = ui.ctx().clone();

        if let Some(c) = &self.cleaning {
            let (done, total) = (c.progress.done.load(Ordering::Relaxed), c.progress.total.load(Ordering::Relaxed).max(1));
            let current = c.progress.current.lock().unwrap().clone();
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(format!("Cleaning… {}", fmt_size(c.progress.freed.load(Ordering::Relaxed)))).size(18.0).strong());
                });
                ui.add(egui::ProgressBar::new(done as f32 / total as f32).text(format!("{} / {} files", fmt_count(done), fmt_count(total))));
                ui.label(RichText::new(current).weak());
                if ui.button("Stop").clicked() {
                    c.progress.cancel.store(true, Ordering::Relaxed);
                }
            });
            return;
        }

        let bytes: u64 = self.ready().map(|f| f.bytes).sum();
        let items: u64 = self.ready().map(|f| f.items).sum();
        let categories = self.ready().count();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    if let Some(a) = &self.analysis {
                        let (done, total) = (a.progress.done.load(Ordering::Relaxed), a.progress.total.load(Ordering::Relaxed).max(1));
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new("Analyzing…").size(22.0).strong());
                        });
                        ui.add(egui::ProgressBar::new(done as f32 / total as f32).desired_width(260.0));
                    } else if self.results.is_empty() {
                        ui.label(RichText::new("Not analyzed yet").size(22.0).strong());
                    } else {
                        ui.label(RichText::new(format!("{} can be freed", fmt_size(bytes))).size(26.0).strong().color(GREEN));
                        ui.label(RichText::new(format!("{} items in {} categories", fmt_count(items), categories)).weak());
                    }
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let can_clean = self.analysis.is_none() && categories > 0;
                    if big_button(ui, "Clean", can_clean).clicked() {
                        self.confirm = true;
                    }
                    if big_button(ui, "Analyze", self.analysis.is_none()).clicked() {
                        self.start_analysis(&ctx);
                    }
                });
            });

            // Why some selected junk can't be cleaned right now.
            let mut blocked: Vec<(&str, u64)> = Vec::new();
            let mut admin_bytes = 0u64;
            for (r, f) in &self.results {
                if !self.selected.contains(r) || f.bytes == 0 {
                    continue;
                }
                if f.needs_admin {
                    admin_bytes += f.bytes;
                } else if f.blocked_by.is_some() {
                    let app = f.rule().app;
                    match blocked.iter_mut().find(|b| b.0 == app) {
                        Some(b) => b.1 += f.bytes,
                        None => blocked.push((app, f.bytes)),
                    }
                }
            }
            if !blocked.is_empty() {
                blocked.sort_by_key(|b| std::cmp::Reverse(b.1));
                let total: u64 = blocked.iter().map(|b| b.1).sum();
                let names: Vec<&str> = blocked.iter().map(|b| b.0).collect();
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!("⏸ {} more once you close {}, then analyze again.", fmt_size(total), names.join(", ")))
                        .color(AMBER),
                );
            }
            if admin_bytes > 0 {
                ui.add_space(2.0);
                cx.admin_notice(ui, &format!("{} more in system locations needs administrator rights.", fmt_size(admin_bytes)));
            }
        });

        if let Some(rep) = &self.report {
            let freed: u64 = rep.cleaned.iter().map(|c| c.freed).sum();
            let removed: u64 = rep.cleaned.iter().map(|c| c.removed).sum();
            let skipped: u64 = rep.cleaned.iter().map(|c| c.skipped).sum();
            ui.add_space(8.0);
            card(ui, |ui| {
                ui.label(RichText::new(format!("Freed {}", fmt_size(freed))).size(18.0).strong().color(GREEN));
                let mut line = format!("{} items removed in {}", fmt_count(removed), fmt_duration_ms(rep.took.as_millis() as u64));
                if skipped > 0 {
                    line.push_str(&format!(" · {} in use or protected, left alone", fmt_count(skipped)));
                }
                ui.label(RichText::new(line).weak());
                for c in rep.cleaned.iter().filter(|c| c.note.is_some()) {
                    let r = &rules::all()[c.rule];
                    ui.label(RichText::new(format!("{} · {}: {}", r.app, r.name, c.note.as_deref().unwrap_or(""))).weak().size(11.5));
                }
            });
        }

        ui.add_space(10.0);
        if let Some((r, order)) = self.detail.clone() {
            self.detail_view(ui, r, &order);
        } else {
            self.results_table(ui);
        }

        ui.add_space(14.0);
        #[cfg(windows)]
        {
            self.win.disks_ui(ui);
            ui.add_space(14.0);
            ui.separator();
            self.win.footer(ui, cx);
        }
    }

    fn results_table(&mut self, ui: &mut egui::Ui) {
        let mut rows: Vec<&Found> =
            self.results.iter().filter(|(r, f)| self.selected.contains(r) && (f.items > 0 || f.bytes > 0)).map(|(_, f)| f).collect();
        if rows.is_empty() {
            return;
        }
        rows.sort_by_key(|f| std::cmp::Reverse(f.bytes));
        let max = rows.first().map(|f| f.bytes).unwrap_or(1).max(1);
        ui.label(RichText::new("Results").strong());
        ui.label(RichText::new("Click a row to see exactly which files would be removed.").weak().size(11.5));
        ui.add_space(4.0);
        let mut open = None;
        for f in rows {
            let rule = f.rule();
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
            row_background(ui, rect, &resp, false);
            let cy = rect.center().y;
            let v = ui.visuals();
            let p = ui.painter();
            let right = rect.right() - 6.0;
            let status = if f.needs_admin {
                Some("needs administrator".to_string())
            } else {
                f.blocked_by.map(|_| format!("close {}", rule.app))
            };
            let name_color = if status.is_some() { v.weak_text_color() } else { v.text_color() };
            let clip = Rect::from_min_max(rect.min, pos2(right - 330.0, rect.bottom()));
            p.with_clip_rect(clip.intersect(p.clip_rect())).text(
                pos2(rect.left() + 6.0, cy),
                Align2::LEFT_CENTER,
                format!("{} · {}", rule.app, rule.name),
                FontId::proportional(13.0),
                name_color,
            );
            p.text(pos2(right, cy), Align2::RIGHT_CENTER, fmt_size(f.bytes), FontId::proportional(13.0), v.strong_text_color());
            p.text(pos2(right - 80.0, cy), Align2::RIGHT_CENTER, format!("{} items", fmt_count(f.items)), FontId::proportional(11.5), v.weak_text_color());
            let bar = Rect::from_min_size(pos2(right - 240.0, cy - 4.0), vec2(70.0, 8.0));
            share_bar(ui, bar, f.bytes as f32 / max as f32, if status.is_some() { v.weak_text_color() } else { GREEN });
            if let Some(s) = status {
                ui.painter().text(pos2(right - 250.0, cy), Align2::RIGHT_CENTER, s, FontId::proportional(11.0), AMBER);
            }
            if resp.clicked() && !f.files.is_empty() {
                open = Some(f.rule);
            }
            resp.on_hover_text(rule.about);
        }
        if let Some(r) = open {
            let files = &self.results[&r].files;
            let mut order: Vec<u32> = (0..files.len() as u32).collect();
            order.sort_by_key(|&i| std::cmp::Reverse(files[i as usize].1));
            self.detail = Some((r, order));
        }
    }

    fn detail_view(&mut self, ui: &mut egui::Ui, r: usize, order: &[u32]) {
        let rule = &rules::all()[r];
        let Some(found) = self.results.get(&r) else {
            self.detail = None;
            return;
        };
        ui.horizontal(|ui| {
            if ui.button("Back to all results").clicked() {
                self.detail = None;
            }
            ui.label(RichText::new(format!("{} · {}", rule.app, rule.name)).strong());
            ui.label(RichText::new(format!("{} · {} files", fmt_size(found.bytes), fmt_count(found.files.len() as u64))).weak());
        });
        ui.label(RichText::new(rule.about).weak());
        if let Some(w) = rule.warning {
            ui.colored_label(AMBER, format!("⚠ {w}"));
        }
        ui.add_space(4.0);
        let files = found.files.clone();
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::ScrollArea::vertical().id_salt("clean_detail").max_height(420.0).auto_shrink([false, true]).show_rows(
                ui,
                20.0,
                order.len(),
                |ui, range| {
                    for &i in &order[range] {
                        let (path, size) = &files[i as usize];
                        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::click());
                        row_background(ui, rect, &resp, false);
                        let v = ui.visuals();
                        let p = ui.painter();
                        p.text(pos2(rect.left() + 70.0, rect.center().y), Align2::RIGHT_CENTER, fmt_size(*size), FontId::proportional(12.0), v.text_color());
                        let clip = Rect::from_min_max(pos2(rect.left() + 80.0, rect.top()), rect.max);
                        p.with_clip_rect(clip.intersect(p.clip_rect())).text(
                            pos2(clip.left(), rect.center().y),
                            Align2::LEFT_CENTER,
                            path.to_string_lossy(),
                            FontId::monospace(11.5),
                            v.weak_text_color(),
                        );
                        if resp.double_clicked() {
                            crate::platform::reveal(&path.to_string_lossy());
                        }
                        resp.on_hover_text("Double-click to show in Explorer");
                    }
                },
            );
        });
    }

    fn confirm_modal(&mut self, ctx: &egui::Context) {
        if !self.confirm {
            return;
        }
        let ready: Vec<&Found> = self.ready().collect();
        let bytes: u64 = ready.iter().map(|f| f.bytes).sum();
        let items: u64 = ready.iter().map(|f| f.items).sum();
        let warnings: Vec<String> = ready
            .iter()
            .filter_map(|f| f.rule().warning.map(|w| format!("{} · {}: {w}", f.rule().app, f.rule().name)))
            .collect();
        // Linux: system caches go through pkexec, which asks for a password.
        let asks_password = !crate::platform::CAN_ELEVATE && ready.iter().any(|f| f.rule().admin);
        let mut go = false;
        let mut close = false;
        let resp = egui::Modal::new(egui::Id::new("confirm_clean")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading("Clean now?");
            ui.add_space(4.0);
            ui.label(format!("{} · {} items in {} categories", fmt_size(bytes), fmt_count(items), ready.len()));
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!(
                    "These files are deleted, not moved to the {}. Programs recreate what they need.",
                    crate::platform::TRASH
                ))
                .weak(),
            );
            if asks_password {
                ui.label(
                    RichText::new("System caches are cleaned by their own tools (apt, journalctl, snap), so you'll be asked for your password.")
                        .weak(),
                );
            }
            if !warnings.is_empty() {
                ui.add_space(6.0);
                for w in &warnings {
                    ui.colored_label(AMBER, format!("⚠ {w}"));
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Clean").strong()).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if go {
            self.confirm = false;
            self.start_cleaning(ctx);
        } else if close || resp.should_close() {
            self.confirm = false;
        }
    }
}
