//! Startup programs workspace.

use std::time::Duration;

use crossbeam_channel::Receiver;
use eframe::egui::{self, vec2, Align, Layout, RichText};

use super::{card, heading, Cx, AMBER};
use crate::startup::{self, Entry, Source};
use crate::winsys::PathState;

#[derive(Default)]
pub struct State {
    entries: Vec<Entry>,
    loading: Option<Receiver<Vec<Entry>>>,
    loaded: bool,
    filter: String,
    confirm_remove: Option<Entry>,
}

impl State {
    fn reload(&mut self, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut v = startup::list(true);
            v.sort_by_key(|e| (!e.enabled, e.title().to_lowercase()));
            let _ = tx.send(v);
            ctx.request_repaint();
        });
        self.loading = Some(rx);
    }

    pub fn show(&mut self, ui: &mut egui::Ui, mut cx: Cx) {
        let ctx = ui.ctx().clone();
        if !self.loaded {
            self.loaded = true;
            self.reload(&ctx);
        }
        if let Some(rx) = &self.loading {
            match rx.try_recv() {
                Ok(v) => {
                    self.entries = v;
                    self.loading = None;
                }
                Err(_) => ctx.request_repaint_after(Duration::from_millis(150)),
            }
        }

        egui::CentralPanel::default().show(ui, |ui| {
            heading(
                ui,
                "Startup programs",
                "What launches when you sign in. Turning an item off uses the same switch as Task Manager, so nothing is deleted.",
            );
            let enabled = self.entries.iter().filter(|e| e.enabled).count();
            let missing = self.entries.iter().filter(|e| e.state == PathState::Missing).count();
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{enabled} of {} start automatically", self.entries.len())).size(18.0).strong());
                    if self.loading.is_some() {
                        ui.spinner();
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add_enabled(self.loading.is_none(), egui::Button::new("⟳  Refresh")).clicked() {
                            self.reload(&ctx);
                        }
                        ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter").desired_width(160.0));
                    });
                });
                if missing > 0 {
                    ui.colored_label(AMBER, format!("⚠ {missing} point to programs that no longer exist and can be removed."));
                }
                if !cx.elevated && self.entries.iter().any(|e| e.source.needs_admin()) {
                    cx.admin_notice(ui, "Entries for all users and scheduled tasks can only be changed as administrator.");
                }
            });
            ui.add_space(8.0);

            let filter = self.filter.to_lowercase();
            let mut changed: Option<(usize, bool)> = None;
            egui::ScrollArea::vertical().id_salt("startup_rows").auto_shrink([false, false]).show(ui, |ui| {
                for (i, e) in self.entries.iter().enumerate() {
                    if !filter.is_empty()
                        && !format!("{} {} {} {}", e.name, e.description, e.company, e.command).to_lowercase().contains(&filter)
                    {
                        continue;
                    }
                    let locked = e.source.needs_admin() && !cx.elevated;
                    ui.horizontal(|ui| {
                        ui.set_min_height(40.0);
                        let mut on = e.enabled;
                        let toggle = ui.add_enabled(e.can_toggle() && !locked, egui::Checkbox::without_text(&mut on));
                        let toggle = if locked {
                            toggle.on_disabled_hover_text("Needs administrator rights")
                        } else if !e.can_toggle() {
                            toggle.on_disabled_hover_text("Run-once entries can only be removed")
                        } else {
                            toggle.on_hover_text(if e.enabled { "Don't start automatically" } else { "Start automatically" })
                        };
                        if toggle.changed() {
                            changed = Some((i, on));
                        }
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                let title = RichText::new(e.title()).strong();
                                ui.label(if e.enabled { title } else { title.weak() });
                                if !e.company.is_empty() {
                                    ui.label(RichText::new(&e.company).weak());
                                }
                                if e.state == PathState::Missing {
                                    ui.colored_label(AMBER, "⚠ program missing");
                                }
                            });
                            ui.add(egui::Label::new(RichText::new(&e.command).monospace().size(11.0).weak()).truncate())
                                .on_hover_text(&e.command);
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let remove = ui.add_enabled(!locked, egui::Button::new("🗑").min_size(vec2(26.0, 22.0)));
                            if remove.on_hover_text(remove_hint(e.source)).clicked() {
                                self.confirm_remove = Some(e.clone());
                            }
                            let target = e.target.clone().filter(|_| e.state == PathState::Exists);
                            let folder = ui.add_enabled(target.is_some(), egui::Button::new("📂").min_size(vec2(26.0, 22.0)));
                            if folder.on_hover_text("Show in Explorer").clicked()
                                && let Some(t) = target
                            {
                                crate::platform::reveal(&crate::winsys::expand_env(&t));
                            }
                            ui.label(RichText::new(e.source.label()).weak().size(11.5));
                        });
                    });
                    ui.separator();
                }
                if self.entries.is_empty() && self.loading.is_none() {
                    ui.label(RichText::new("Nothing starts automatically.").weak());
                }
            });
            if let Some((i, on)) = changed {
                match startup::set_enabled(&self.entries[i], on) {
                    Ok(()) => self.entries[i].enabled = on,
                    Err(err) => cx.toast(format!("Could not change {}: {err}", self.entries[i].title()), true),
                }
            }
        });
        self.remove_modal(&ctx, &mut cx);
    }

    fn remove_modal(&mut self, ctx: &egui::Context, cx: &mut Cx) {
        let Some(e) = self.confirm_remove.clone() else { return };
        let mut go = false;
        let mut close = false;
        let resp = egui::Modal::new(egui::Id::new("confirm_startup_remove")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.heading(format!("Remove {}?", e.title()));
            ui.add_space(4.0);
            ui.label(RichText::new(&e.command).monospace().size(11.5));
            ui.add_space(4.0);
            ui.label(RichText::new(remove_hint(e.source)).weak());
            if e.state != PathState::Missing && e.can_toggle() {
                ui.label(RichText::new("To stop it starting but keep the entry, turn it off instead.").weak());
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Remove").strong()).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if go {
            self.confirm_remove = None;
            match startup::remove(&e) {
                Ok(Some(backup)) => cx.toast(format!("Removed. Backup saved to {}", backup.display()), false),
                Ok(None) => cx.toast(format!("Removed {}", e.title()), false),
                Err(err) => cx.toast(format!("Could not remove {}: {err}", e.title()), true),
            }
            self.reload(ctx);
        } else if close || resp.should_close() {
            self.confirm_remove = None;
        }
    }
}

fn remove_hint(source: Source) -> &'static str {
    match source {
        Source::FolderUser | Source::FolderCommon => "The shortcut is moved to the Recycle Bin.",
        Source::Task => "The scheduled task is deleted.",
        _ => "The registry entry is deleted after a .reg backup is saved.",
    }
}
