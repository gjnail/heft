//! Low-space alerts: a banner in the window, the settings in the View menu,
//! and on Windows, staying in the notification area after the window closes.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crossbeam_channel::Receiver;
use eframe::egui::{self, RichText};

use super::{warnings, Action, HeftApp, Tab, Workspace};
use crate::monitor::{self, Low, Settings};
use crate::risk::Level;
use crate::util::fmt_size;

pub(super) struct AlertState {
    settings: Arc<Settings>,
    rx: Receiver<Vec<Low>>,
    low: Vec<Low>,
    dismissed: HashSet<String>,
    /// Keep running in the notification area when the window is closed.
    pub background: bool,
    /// Started with Windows (`--tray`): hide the window as soon as it opens.
    #[cfg_attr(not(windows), allow(dead_code))]
    start_hidden: bool,
}

impl AlertState {
    pub fn new(ctx: &egui::Context, enabled: bool, limit: u64, background: bool, start_hidden: bool) -> Self {
        let settings = Arc::new(Settings { enabled: enabled.into(), limit: limit.into() });
        let ctx = ctx.clone();
        let rx = monitor::start(settings.clone(), move || ctx.request_repaint());
        #[cfg(windows)]
        if background {
            crate::tray::start();
        }
        #[cfg(all(windows, debug_assertions))]
        if std::env::var_os("HEFT_DEBUG_TRAY").is_some() {
            crate::tray::self_test();
        }
        AlertState { settings, rx, low: Vec::new(), dismissed: HashSet::new(), background, start_hidden }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        let on = self.settings.enabled.load(Ordering::Relaxed);
        storage.set_string("alerts", if on { "1" } else { "0" }.into());
        storage.set_string("alert_limit", (self.settings.limit.load(Ordering::Relaxed) >> 30).to_string());
        storage.set_string("background", if self.background { "1" } else { "0" }.into());
    }
}

impl HeftApp {
    pub(super) fn poll_alerts(&mut self, ctx: &egui::Context) {
        while let Ok(low) = self.alerts.rx.try_recv() {
            // A drive that stopped being low and becomes low again is news.
            self.alerts.dismissed.retain(|root| low.iter().any(|l| &l.root == root));
            self.alerts.low = low;
        }
        #[cfg(windows)]
        {
            use crate::tray;
            // eframe shows the window after its first frame no matter what,
            // so hide it once the icon is there to bring it back.
            if self.alerts.start_hidden {
                if tray::running() {
                    self.alerts.start_hidden = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                } else {
                    ctx.request_repaint_after(std::time::Duration::from_millis(20));
                }
            }
            if tray::take_shown() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            if self.alerts.background
                && tray::running()
                && !tray::quitting()
                && ctx.input(|i| i.viewport().close_requested())
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }
        #[cfg(not(windows))]
        let _ = ctx;
    }

    /// A strip under the toolbar for each drive that's almost full.
    pub(super) fn alert_banner(&mut self, ui: &mut egui::Ui) {
        let show: Vec<Low> = self.alerts.low.iter().filter(|l| !self.alerts.dismissed.contains(&l.root)).cloned().collect();
        if show.is_empty() {
            return;
        }
        let scanned = self.tree.as_ref().map(|t| t.root_path.clone());
        egui::Panel::top("alerts").show(ui, |ui| {
            for l in &show {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "⚠ {} is almost full: {} free of {}.",
                            l.name,
                            fmt_size(l.free),
                            fmt_size(l.total)
                        ))
                        .color(warnings::color(Level::Caution))
                        .strong(),
                    );
                    let here = scanned.as_deref().is_some_and(|r| crate::platform::names_eq(r, &l.root));
                    if here {
                        if ui.small_button("See suggestions").clicked() {
                            self.workspace = Workspace::Disk;
                            self.tab = Tab::Suggestions;
                        }
                    } else if ui.small_button("Scan it").clicked() {
                        self.workspace = Workspace::Disk;
                        self.actions.push(Action::ScanPath(l.root.clone()));
                    }
                    if ui.small_button("Dismiss").clicked() {
                        self.alerts.dismissed.insert(l.root.clone());
                    }
                });
            }
        });
    }

    /// Settings, shown in the View menu.
    pub(super) fn alert_menu(&mut self, ui: &mut egui::Ui) {
        let s = self.alerts.settings.clone();
        let mut on = s.enabled.load(Ordering::Relaxed);
        let mut limit = s.limit.load(Ordering::Relaxed);
        ui.label(RichText::new("Free space alerts").weak());
        ui.horizontal(|ui| {
            ui.checkbox(&mut on, "Warn when a drive has less than");
            egui::ComboBox::from_id_salt("alert_limit").selected_text(fmt_size(limit)).show_ui(ui, |ui| {
                for l in monitor::LIMITS {
                    ui.selectable_value(&mut limit, l, fmt_size(l));
                }
            });
        });
        s.enabled.store(on, Ordering::Relaxed);
        s.limit.store(limit, Ordering::Relaxed);
        if !on {
            self.alerts.low.clear();
        }

        #[cfg(windows)]
        {
            use crate::tray;
            let before = self.alerts.background;
            ui.checkbox(&mut self.alerts.background, "Keep watching in the notification area after closing")
                .on_hover_text("Closing the window hides Heft next to the clock. Right-click its icon to quit.");
            if self.alerts.background != before {
                if self.alerts.background { tray::start() } else { tray::stop() }
            }
            let mut login = tray::starts_with_windows();
            if ui
                .checkbox(&mut login, "Start with Windows, in the notification area")
                .on_hover_text("Heft starts hidden when you sign in and only speaks up when a drive is almost full.")
                .changed()
            {
                match tray::set_start_with_windows(login) {
                    Ok(()) if login => {
                        self.alerts.background = true;
                        tray::start();
                        s.enabled.store(true, Ordering::Relaxed);
                    }
                    Ok(()) => {}
                    Err(e) => self.toast(format!("Couldn't change the startup setting: {e}"), true),
                }
            }
        }
    }
}
