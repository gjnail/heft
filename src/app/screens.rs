//! Start page, scan progress page, and the main two-pane layout.

use std::sync::atomic::Ordering;

use eframe::egui::{self, pos2, vec2, Align2, Color32, CornerRadius, FontId, Rect, RichText, Sense, Stroke, StrokeKind};

use super::{Action, HeftApp, Tab};
use crate::platform;
use crate::tree::{ScanMode, ROOT};
use crate::util::{fmt_ago, fmt_count, fmt_duration_ms, fmt_size, pct};

impl HeftApp {
    pub(super) fn start_screen(&mut self, ui: &mut egui::Ui) {
        self.accept_dropped_folder(ui);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.set_max_width(900.0);
                ui.add_space(36.0);
                ui.label(RichText::new("What's filling your disks?").size(28.0).strong());
                ui.add_space(6.0);
                ui.label(
                    RichText::new(
                        "Pick a drive or folder. Heft maps every file as a treemap, finds duplicates, \
                         and remembers each scan so you can see what grew.",
                    )
                    .weak(),
                );
                ui.add_space(18.0);

                if let Some(err) = &self.error {
                    ui.colored_label(ui.visuals().error_fg_color, format!("Scan failed: {err}"));
                    ui.add_space(10.0);
                }

                if platform::CAN_ELEVATE && !self.elevated {
                    egui::Frame::group(ui.style())
                        .fill(ui.visuals().faint_bg_color)
                        .corner_radius(8.0)
                        .inner_margin(12.0)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.label(RichText::new("Scans are much faster as administrator").strong());
                                    ui.label(
                                        RichText::new(
                                            "As administrator, Heft reads the NTFS master file table directly instead of \
                                             opening every folder.",
                                        )
                                        .weak(),
                                    );
                                });
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.button("Restart as administrator").clicked() {
                                        let ctx = ui.ctx().clone();
                                        self.restart_elevated(&ctx);
                                    }
                                });
                            });
                        });
                    ui.add_space(18.0);
                }

                // Drive cards, wrapped.
                let card = vec2(272.0, 128.0);
                let gap = 14.0;
                let per_row = ((ui.available_width() + gap) / (card.x + gap)).floor().max(1.0) as usize;
                let drives = self.drives.clone();
                for chunk in drives.chunks(per_row) {
                    let row_w = chunk.len() as f32 * (card.x + gap) - gap;
                    ui.horizontal(|ui| {
                        ui.add_space(((ui.available_width() - row_w) / 2.0).max(0.0));
                        ui.spacing_mut().item_spacing.x = gap;
                        for d in chunk {
                            let idx = drives.iter().position(|x| x.root == d.root).unwrap_or(0);
                            let last = self.last_scans.get(idx).copied().flatten();
                            if self.drive_card(ui, card, d, last) {
                                self.actions.push(Action::ScanPath(d.root.clone()));
                            }
                        }
                    });
                    ui.add_space(gap);
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let w = 300.0;
                    ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                    if ui.add(egui::Button::new("Scan a folder…").min_size(vec2(150.0, 30.0))).clicked()
                        && let Some(p) = rfd::FileDialog::new().set_title("Choose a folder to scan").pick_folder() {
                            self.actions.push(Action::ScanPath(p.to_string_lossy().into_owned()));
                        }
                    ui.label(RichText::new("or drop one onto this window").weak());
                });

                {
                    ui.add_space(28.0);
                    let tidy = if cfg!(target_os = "macos") { "Or tidy up the Mac" } else { "Or tidy up the PC" };
                    ui.label(RichText::new(tidy).strong());
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        #[cfg(windows)]
                        let links = [
                            (super::Workspace::Cleaner, "Clean junk files"),
                            (super::Workspace::Startup, "Startup programs"),
                            (super::Workspace::Programs, "Uninstall and update programs"),
                        ];
                        #[cfg(not(windows))]
                        let links = [(super::Workspace::Cleaner, "Clean junk files")];
                        let n = links.len() as f32;
                        let w = n * 230.0 + (n - 1.0) * 10.0;
                        ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                        for (ws, label) in links {
                            if ui.add(egui::Button::new(label).min_size(vec2(230.0, 34.0))).clicked() {
                                self.workspace = ws;
                            }
                        }
                    });
                }
            });
        });
    }

    fn drive_card(&mut self, ui: &mut egui::Ui, size: egui::Vec2, d: &platform::DriveInfo, last: Option<i64>) -> bool {
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
        let v = ui.visuals();
        let hovered = resp.hovered();
        let fill = if hovered { v.widgets.hovered.weak_bg_fill } else { v.faint_bg_color };
        let p = ui.painter();
        p.rect(rect, 10.0, fill, Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color), StrokeKind::Inside);
        let text = v.text_color();
        let weak = v.weak_text_color();
        let x = rect.left() + 14.0;

        let title = if d.label.is_empty() { d.kind.to_string() } else { d.label.clone() };
        let header = p.with_clip_rect(Rect::from_min_max(
            pos2(x, rect.top()),
            pos2(rect.right() - 12.0, rect.top() + 40.0),
        ));
        let is_drive_root = d.root.chars().count() <= 3; // `C:\` or `/`
        if is_drive_root {
            header.text(pos2(x, rect.top() + 20.0), Align2::LEFT_CENTER, &d.root, FontId::proportional(20.0), text);
            header.text(pos2(x + 42.0, rect.top() + 21.0), Align2::LEFT_CENTER, title, FontId::proportional(14.0), text);
        } else {
            header.text(pos2(x, rect.top() + 20.0), Align2::LEFT_CENTER, title, FontId::proportional(17.0), text);
        }

        // usage bar
        let used = d.total.saturating_sub(d.free);
        let frac = if d.total > 0 { used as f32 / d.total as f32 } else { 0.0 };
        let bar = Rect::from_min_size(pos2(x, rect.top() + 44.0), vec2(rect.width() - 28.0, 10.0));
        p.rect_filled(bar, 5.0, v.extreme_bg_color);
        let color = if frac > 0.9 {
            Color32::from_rgb(235, 90, 80)
        } else if frac > 0.75 {
            Color32::from_rgb(240, 180, 60)
        } else {
            Color32::from_rgb(80, 150, 245)
        };
        let mut filled = bar;
        filled.set_width(bar.width() * frac);
        p.rect_filled(filled, 5.0, color);
        p.text(
            pos2(x, rect.top() + 70.0),
            Align2::LEFT_CENTER,
            format!("{} free of {}", fmt_size(d.free), fmt_size(d.total)),
            FontId::proportional(13.0),
            text,
        );
        p.text(
            pos2(rect.right() - 14.0, rect.top() + 70.0),
            Align2::RIGHT_CENTER,
            format!("{:.0}% used", frac * 100.0),
            FontId::proportional(12.0),
            weak,
        );

        let fast = d.is_ntfs() && self.elevated && self.use_mft && d.kind != "Network";
        let (badge, badge_color) = if fast {
            (format!("fast scan · {}", d.fs), Color32::from_rgb(230, 170, 40))
        } else if d.kind == "Folder" {
            (d.root.clone(), weak)
        } else if d.fs.is_empty() {
            (d.kind.to_string(), weak)
        } else {
            (format!("{} · {}", d.kind, d.fs), weak)
        };
        let badge_right = if last.is_some() { rect.right() - 128.0 } else { rect.right() - 12.0 };
        p.with_clip_rect(Rect::from_min_max(pos2(x, rect.bottom() - 34.0), pos2(badge_right, rect.bottom())))
            .text(pos2(x, rect.bottom() - 20.0), Align2::LEFT_CENTER, badge, FontId::proportional(12.0), badge_color);
        if let Some(t) = last {
            p.text(
                pos2(rect.right() - 14.0, rect.bottom() - 20.0),
                Align2::RIGHT_CENTER,
                format!("last scan {}", fmt_ago(platform::now_unix() - t)),
                FontId::proportional(11.5),
                weak,
            );
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
    }

    pub(super) fn accept_dropped_folder(&mut self, ui: &egui::Ui) {
        let dropped: Vec<std::path::PathBuf> =
            ui.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if let Some(p) = dropped.into_iter().find(|p| p.is_dir()) {
            self.actions.push(Action::ScanPath(p.to_string_lossy().into_owned()));
        }
    }

    pub(super) fn scanning_screen(&mut self, ui: &mut egui::Ui) {
        let Some(h) = &self.scan else { return };
        let p = &h.progress;
        let files = p.files.load(Ordering::Relaxed);
        let dirs = p.dirs.load(Ordering::Relaxed);
        let bytes = p.bytes.load(Ordering::Relaxed);
        let elapsed = h.started.elapsed();
        let mode = *p.mode.lock().unwrap();
        let phase = p.phase.lock().unwrap().clone();
        let root = h.root.clone();
        let fraction = p.fraction();

        ui.vertical_centered(|ui| {
            ui.set_max_width(560.0);
            ui.add_space((ui.available_height() * 0.22).max(20.0));
            ui.horizontal(|ui| {
                ui.add_space(((ui.available_width() - 320.0) / 2.0).max(0.0));
                ui.spinner();
                ui.label(RichText::new(format!("Scanning {root}")).size(22.0).strong());
            });
            ui.add_space(4.0);
            let mode_text = match mode {
                Some(ScanMode::Mft) => RichText::new("Reading the NTFS master file table directly")
                    .color(Color32::from_rgb(250, 200, 70)),
                Some(ScanMode::Walk) => RichText::new("Walking folders in parallel").weak(),
                None => RichText::new("Starting…").weak(),
            };
            ui.label(mode_text);
            ui.add_space(16.0);

            if let Some(f) = fraction {
                ui.add(egui::ProgressBar::new(f).show_percentage().desired_width(520.0));
                ui.add_space(4.0);
            }
            ui.label(RichText::new(&phase).weak());
            ui.add_space(16.0);

            egui::Grid::new("scan_stats").num_columns(2).spacing([28.0, 6.0]).show(ui, |ui| {
                let rate = files as f64 / elapsed.as_secs_f64().max(0.001);
                ui.label(RichText::new("Files").weak());
                ui.label(RichText::new(fmt_count(files)).strong().size(16.0));
                ui.end_row();
                if mode == Some(ScanMode::Walk) {
                    ui.label(RichText::new("Folders").weak());
                    ui.label(RichText::new(fmt_count(dirs)).strong().size(16.0));
                    ui.end_row();
                    ui.label(RichText::new("Size so far").weak());
                    ui.label(RichText::new(fmt_size(bytes)).strong().size(16.0));
                    ui.end_row();
                }
                ui.label(RichText::new("Elapsed").weak());
                ui.label(RichText::new(fmt_duration_ms(elapsed.as_millis() as u64)).strong().size(16.0));
                ui.end_row();
                ui.label(RichText::new("Rate").weak());
                ui.label(RichText::new(format!("{} files/s", fmt_count(rate as u64))).strong().size(16.0));
                ui.end_row();
            });
            ui.add_space(20.0);
            if ui.add(egui::Button::new("Cancel").min_size(vec2(100.0, 28.0))).clicked() {
                p.cancel.store(true, Ordering::Relaxed);
            }
        });
    }

    pub(super) fn side_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            for (tab, label) in [
                (Tab::Suggestions, "Suggestions"),
                (Tab::Tree, "Folders"),
                (Tab::Types, "File types"),
                (Tab::Search, "Search"),
                (Tab::Duplicates, "Duplicates"),
                (Tab::Junk, "Build junk"),
                (Tab::Changes, "Changes"),
                (Tab::Removed, "Removed"),
            ] {
                let mut text = RichText::new(label);
                if self.tab == tab {
                    text = text.strong();
                }
                let badge = match tab {
                    Tab::Suggestions if self.suggest.count() > 0 => format!(" ({})", self.suggest.count()),
                    Tab::Duplicates if !self.dupes.groups.is_empty() => format!(" ({})", self.dupes.groups.len()),
                    Tab::Changes if self.diff.is_some() => " •".to_string(),
                    _ => String::new(),
                };
                let text = if badge.is_empty() { text } else { RichText::new(format!("{label}{badge}")).strong() };
                if ui.add(egui::Button::new(text).selected(self.tab == tab)).clicked() {
                    self.tab = tab;
                }
            }
        });
        ui.add_space(4.0);
        ui.separator();
        match self.tab {
            Tab::Suggestions => self.suggestions_tab(ui),
            Tab::Tree => self.tree_view(ui),
            Tab::Types => self.types_tab(ui),
            Tab::Search => self.search_tab(ui),
            Tab::Duplicates => self.dupes_tab(ui),
            Tab::Junk => self.junk_tab(ui),
            Tab::Changes => self.changes_tab(ui),
            Tab::Removed => self.removed_tab(ui),
        }
    }

    pub(super) fn treemap_panel(&mut self, ui: &mut egui::Ui) {
        self.accept_dropped_folder(ui);
        let Some(tree) = self.tree.clone() else { return };

        // Breadcrumbs
        ui.horizontal(|ui| {
            let up = ui.add_enabled(self.view_root != ROOT, egui::Button::new("⬆").min_size(vec2(26.0, 22.0)));
            if up.on_hover_text("Up one level (Backspace)").clicked() {
                self.actions.push(Action::ZoomOut);
            }
            ui.spacing_mut().item_spacing.x = 2.0;
            let chain = tree.ancestors(self.view_root);
            for (i, &id) in chain.iter().enumerate() {
                if i > 0 {
                    ui.label(RichText::new("›").weak());
                }
                let name = if id == ROOT { tree.root_path.trim_end_matches('\\').to_string() } else { tree.name(id).to_string() };
                let last = i + 1 == chain.len();
                let text = if last { RichText::new(name).strong() } else { RichText::new(name) };
                if ui.add(egui::Button::new(text).frame(false)).clicked() && !last {
                    self.view_root = id;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let n = tree.node(self.view_root);
                let total = tree.node(ROOT).size;
                let share = if self.view_root == ROOT {
                    String::new()
                } else {
                    format!(" · {:.1}% of scan", pct(n.size, total))
                };
                ui.label(
                    RichText::new(format!("{} · {} files{share}", fmt_size(n.size), fmt_count(n.files as u64))).weak(),
                );
            });
        });
        ui.add_space(4.0);

        egui::Frame::new().corner_radius(CornerRadius::same(4)).show(ui, |ui| self.treemap_view(ui));
    }
}
