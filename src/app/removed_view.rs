//! Removed tab: what Heft moved to the Recycle Bin / Trash, and putting it
//! back.

use std::collections::HashSet;
use std::time::Duration;

use crossbeam_channel::Receiver;
use eframe::egui::{self, RichText};

use super::HeftApp;
use crate::platform;
use crate::trashlog::{self, Removed};
use crate::util::{fmt_ago, fmt_size};

#[derive(Default)]
pub(super) struct RemovedState {
    entries: Vec<Removed>,
    loaded: bool,
    /// Keys of the logged items still in the trash (None until checked).
    in_trash: Option<HashSet<String>>,
    checking: Option<Receiver<Result<HashSet<String>, String>>>,
    restoring: Option<Receiver<Result<usize, String>>>,
    checked: HashSet<usize>,
}

impl RemovedState {
    /// Re-read the log next time the tab is shown.
    pub(super) fn reload(&mut self) {
        self.loaded = false;
    }
}

impl HeftApp {
    pub(super) fn removed_tab(&mut self, ui: &mut egui::Ui) {
        let st = &mut self.removed;
        if !st.loaded {
            st.entries = trashlog::read();
            st.loaded = true;
            st.checked.clear();
            st.in_trash = None;
            if !st.entries.is_empty() {
                let (tx, rx) = crossbeam_channel::bounded(1);
                let (items, ctx) = (st.entries.clone(), ui.ctx().clone());
                std::thread::spawn(move || {
                    let _ = tx.send(trashlog::still_in_trash(&items));
                    ctx.request_repaint();
                });
                st.checking = Some(rx);
            }
        }
        if let Some(rx) = &st.checking {
            match rx.try_recv() {
                Ok(r) => {
                    st.in_trash = r.ok();
                    st.checking = None;
                }
                Err(_) => ui.ctx().request_repaint_after(Duration::from_millis(100)),
            }
        }
        let mut notice = None;
        if let Some(rx) = &st.restoring {
            match rx.try_recv() {
                Ok(r) => {
                    st.restoring = None;
                    st.loaded = false;
                    notice = Some(match r {
                        Ok(n) => (format!("Put back {n} item(s). Rescan to see them in the map."), false),
                        Err(e) => (format!("Couldn't put it back: {e}"), true),
                    });
                }
                Err(_) => ui.ctx().request_repaint_after(Duration::from_millis(100)),
            }
        }
        if let Some((msg, err)) = notice {
            self.toast(msg, err);
        }

        let st = &mut self.removed;
        ui.label(RichText::new(format!("What Heft moved to the {}", platform::TRASH)).strong());
        if st.entries.is_empty() {
            ui.label(RichText::new("Nothing yet. Anything you remove with Heft shows up here so you can find it later.").weak());
            return;
        }
        // Items an older Heft moved to the Trash on a Mac didn't have their
        // place in the Trash recorded; only Finder can find those.
        let finder_only = |r: &Removed| cfg!(target_os = "macos") && r.trashed_at.is_none();
        if st.entries.iter().any(|r| !r.restored && finder_only(r)) {
            ui.label(
                RichText::new(
                    "Items removed with an older version of Heft can only be put back from Finder: open the Trash, \
                     right-click an item and choose Put Back.",
                )
                .weak(),
            );
            if ui.button("Open the Trash").clicked()
                && let Some(home) = platform::home_dir()
            {
                platform::open_path(&format!("{home}/.Trash"));
            }
        }
        ui.add_space(4.0);

        let now = platform::now_unix();
        let mut restore_now: Vec<Removed> = Vec::new();
        egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("removed_rows").max_height(ui.available_height() - 34.0).show(ui, |ui| {
            for (i, r) in st.entries.iter().enumerate().take(500) {
                let available = !r.restored && !finder_only(r) && st.in_trash.as_ref().is_none_or(|s| s.contains(r.key()));
                ui.horizontal(|ui| {
                    let mut on = st.checked.contains(&i);
                    if ui.add_enabled(available, egui::Checkbox::new(&mut on, "")).changed() {
                        if on {
                            st.checked.insert(i);
                        } else {
                            st.checked.remove(&i);
                        }
                    }
                    ui.label(RichText::new(fmt_size(r.size)).monospace().size(11.5));
                    let status = if r.restored {
                        "put back".to_string()
                    } else if finder_only(r) {
                        format!("{}, use Finder to put it back", fmt_ago(now - r.when))
                    } else if st.in_trash.as_ref().is_some_and(|s| !s.contains(r.key())) {
                        format!("no longer in the {}", platform::TRASH)
                    } else {
                        fmt_ago(now - r.when)
                    };
                    ui.label(RichText::new(status).weak().size(11.5));
                    let mut path = r.path.clone();
                    if r.is_dir {
                        path.push(platform::SEP);
                    }
                    let text = if available { RichText::new(path) } else { RichText::new(path).weak() };
                    ui.add(egui::Label::new(text.size(12.0)).truncate());
                });
            }
        });
        ui.horizontal(|ui| {
            let n = st.checked.len();
            let busy = st.restoring.is_some();
            if ui.add_enabled(n > 0 && !busy, egui::Button::new(format!("Put back {n} selected"))).clicked() {
                restore_now = st.checked.iter().filter_map(|&i| st.entries.get(i).cloned()).collect();
            }
            if busy || st.checking.is_some() {
                ui.spinner();
            }
        });
        if !restore_now.is_empty() {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let ctx = ui.ctx().clone();
            std::thread::spawn(move || {
                let _ = tx.send(trashlog::restore(&restore_now));
                ctx.request_repaint();
            });
            self.removed.restoring = Some(rx);
            self.removed.checked.clear();
        }
    }
}
