//! Login items workspace (macOS): login items, and launch agents and daemons.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::Receiver;
use eframe::egui::{self, vec2, Align, Layout, RichText};

use super::{card, heading, Cx, AMBER, GREEN};
use crate::mac::broken::PathState;
use crate::mac::startup::{self, BackgroundItem, BundledItem, Entry, Item, Listing, LoginError, Signer, Source};
use crate::mac::Domain;

const LOGIN_ITEMS_SETTINGS: &str = "x-apple.systempreferences:com.apple.LoginItems-Settings.extension";
const AUTOMATION_SETTINGS: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_Automation";

/// A change that ran in the background (it may have waited for a password).
enum Done {
    Toggled { entry: Box<Entry>, on: bool, result: Result<(), String> },
    Removed { title: String, result: Result<PathBuf, String> },
}

#[derive(Default)]
pub struct State {
    entries: Vec<Entry>,
    bundled: Vec<BundledItem>,
    login_error: Option<LoginError>,
    loading: Option<Receiver<Listing>>,
    loaded: bool,
    /// The user asked to read login items through System Events.
    allow_system_events: bool,
    filter: String,
    /// Program path → who signed it.
    signers: HashMap<String, Option<Signer>>,
    signing: Option<Receiver<(String, Option<Signer>)>>,
    busy: Option<(String, Receiver<Done>)>,
    confirm_remove: Option<Entry>,
    /// macOS's own list of every background item, read as root on request.
    all_items: Option<Result<Vec<BackgroundItem>, String>>,
    all_items_job: Option<Receiver<Result<Vec<BackgroundItem>, String>>>,
}

impl State {
    fn reload(&mut self, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        let allow = self.allow_system_events;
        std::thread::spawn(move || {
            let _ = tx.send(startup::list(allow));
            ctx.request_repaint();
        });
        self.loading = Some(rx);
    }

    /// Look up code signatures in the background; codesign takes a moment each.
    fn sign(&mut self, ctx: &egui::Context) {
        let mut paths: Vec<String> = self
            .entries
            .iter()
            .filter(|e| e.state == PathState::Exists)
            .filter_map(|e| e.target.clone())
            .filter(|p| !self.signers.contains_key(p))
            .collect();
        paths.sort();
        paths.dedup();
        if paths.is_empty() {
            return;
        }
        let (tx, rx) = crossbeam_channel::unbounded();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            for p in paths {
                let s = startup::signer(&p);
                if tx.send((p, s)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        self.signing = Some(rx);
    }

    fn start(&mut self, ctx: &egui::Context, what: String, job: impl FnOnce() -> Done + Send + 'static) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job());
            ctx.request_repaint();
        });
        self.busy = Some((what, rx));
    }

    fn poll(&mut self, ctx: &egui::Context, cx: &mut Cx) {
        if let Some(rx) = &self.all_items_job {
            match rx.try_recv() {
                Ok(r) => {
                    self.all_items_job = None;
                    match r {
                        Err(e) if e == crate::mac::CANCELLED => {}
                        Ok(items) => {
                            cx.toast(format!("{} background items, listed at the bottom of the page", items.len()), false);
                            self.all_items = Some(Ok(items));
                        }
                        r => self.all_items = Some(r),
                    }
                }
                Err(_) => ctx.request_repaint_after(Duration::from_millis(150)),
            }
        }
        if let Some(rx) = &self.loading {
            match rx.try_recv() {
                Ok(l) => {
                    self.entries = l.entries;
                    self.bundled = l.bundled;
                    self.login_error = l.login_error;
                    self.loading = None;
                    self.sign(ctx);
                }
                Err(_) => ctx.request_repaint_after(Duration::from_millis(150)),
            }
        }
        if let Some(rx) = &self.signing {
            while let Ok((p, s)) = rx.try_recv() {
                self.signers.insert(p, s);
            }
        }
        let Some((_, rx)) = &self.busy else { return };
        let Ok(done) = rx.try_recv() else {
            ctx.request_repaint_after(Duration::from_millis(150));
            return;
        };
        self.busy = None;
        match done {
            Done::Toggled { entry, on, result: Ok(()) } => {
                // launchd doesn't stop a job that's turned off, like Task Manager.
                if !on && entry.running {
                    let when = if entry.source() == Source::Job(Domain::Daemon) { "restart" } else { "sign in" };
                    cx.toast(format!("{} is turned off. It keeps running until you next {when}.", entry.title()), false);
                }
            }
            Done::Toggled { entry, result: Err(err), .. } => {
                cx.toast(format!("Could not change {}: {}", entry.title(), friendly(&err)), true)
            }
            Done::Removed { result: Ok(backup), .. } => cx.toast(format!("Removed. Backup saved to {}", backup.display()), false),
            Done::Removed { title, result: Err(err) } => cx.toast(format!("Could not remove {title}: {}", friendly(&err)), true),
        }
        self.reload(ctx);
    }

    pub fn show(&mut self, ui: &mut egui::Ui, mut cx: Cx) {
        let ctx = ui.ctx().clone();
        if !self.loaded {
            self.loaded = true;
            self.reload(&ctx);
        }
        self.poll(&ctx, &mut cx);

        egui::CentralPanel::default().show(ui, |ui| {
            heading(
                ui,
                "Login items",
                "What opens when you sign in: login items, and launch agents and daemons. Turning an item off uses macOS's own switch, so nothing is deleted.",
            );
            self.summary(ui, &ctx, &mut cx);
            ui.add_space(8.0);

            let filter = self.filter.to_lowercase();
            let busy = self.busy.is_some();
            let mut changed: Option<(usize, bool)> = None;
            egui::ScrollArea::vertical().id_salt("login_rows").auto_shrink([false, false]).show(ui, |ui| {
                for (i, e) in self.entries.iter().enumerate() {
                    let signer = e.target.as_ref().and_then(|t| self.signers.get(t)).and_then(Option::as_ref);
                    if !filter.is_empty()
                        && !format!("{} {} {} {}", e.name, e.title(), signer.map_or("", |s| s.text()), e.command)
                            .to_lowercase()
                            .contains(&filter)
                    {
                        continue;
                    }
                    ui.horizontal(|ui| {
                        ui.set_min_height(40.0);
                        let mut on = e.enabled;
                        let toggle = ui.add_enabled(!busy, egui::Checkbox::without_text(&mut on));
                        let mut hint = if e.enabled { "Don't start automatically" } else { "Start automatically" }.to_string();
                        if e.source().toggle_needs_admin() {
                            hint.push_str(" (asks for your administrator password)");
                        }
                        if toggle.on_hover_text(hint).changed() {
                            changed = Some((i, on));
                        }
                        ui.vertical(|ui| {
                            // Leave room for the source label and buttons on the right.
                            ui.set_max_width((ui.available_width() - 320.0).max(200.0));
                            ui.horizontal(|ui| {
                                let title = RichText::new(e.title()).strong();
                                ui.label(if e.enabled { title } else { title.weak() });
                                if let Some(s) = signer {
                                    ui.label(RichText::new(s.text()).weak()).on_hover_text(s.hover());
                                }
                                if e.state == PathState::Missing {
                                    let what = if matches!(e.item, Item::Login(_)) { "⚠ app missing" } else { "⚠ program missing" };
                                    ui.colored_label(AMBER, what);
                                }
                                if e.running {
                                    ui.label(RichText::new("running").weak().size(11.5));
                                }
                            });
                            let hover = match &e.item {
                                Item::Job(j) => format!("{}\n\nLabel: {}\nFile: {}", e.command, j.label, j.plist.display()),
                                Item::Login(_) => e.command.clone(),
                            };
                            ui.add(egui::Label::new(RichText::new(&e.command).monospace().size(11.0).weak()).truncate())
                                .on_hover_text(hover);
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let remove = ui.add_enabled(!busy, egui::Button::new("🗑").min_size(vec2(26.0, 22.0)));
                            if remove.on_hover_text(remove_hint(e.source())).clicked() {
                                self.confirm_remove = Some(e.clone());
                            }
                            let reveal = e.reveal_path();
                            let folder = ui.add_enabled(reveal.is_some(), egui::Button::new("📂").min_size(vec2(26.0, 22.0)));
                            let tip = match (&e.item, e.state) {
                                (Item::Job(_), s) if s != PathState::Exists => "Show the .plist in Finder",
                                _ => "Show in Finder",
                            };
                            if folder.on_hover_text(tip).clicked()
                                && let Some(p) = reveal
                            {
                                crate::platform::reveal(&p);
                            }
                            ui.label(RichText::new(format!("{} · {}", e.source().label(), e.starts.label())).weak().size(11.5));
                        });
                    });
                    ui.separator();
                }
                if self.entries.is_empty() && self.loading.is_none() {
                    ui.label(RichText::new("Nothing starts automatically.").weak());
                }
                bundled_list(ui, &self.bundled, &filter);
                self.all_items_section(ui, &filter);
            });
            if let Some((i, on)) = changed {
                let entry = self.entries[i].clone();
                let what = if entry.source().toggle_needs_admin() { "Waiting for the administrator password…" } else { "Saving…" };
                self.start(&ctx, what.into(), move || {
                    let result = startup::set_enabled(&entry, on);
                    Done::Toggled { entry: Box::new(entry), on, result }
                });
            }
        });
        self.remove_modal(&ctx);
    }

    fn summary(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, cx: &mut Cx) {
        let enabled = self.entries.iter().filter(|e| e.enabled).count();
        let missing = self.entries.iter().filter(|e| e.state == PathState::Missing).count();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{enabled} of {} start automatically", self.entries.len())).size(18.0).strong());
                if self.loading.is_some() || self.busy.is_some() {
                    ui.spinner();
                }
                if let Some((what, _)) = &self.busy {
                    ui.label(RichText::new(what).weak());
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add_enabled(self.loading.is_none(), egui::Button::new("⟳  Refresh")).clicked() {
                        self.reload(ctx);
                    }
                    ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter").desired_width(160.0));
                });
            });
            if missing > 0 {
                ui.colored_label(AMBER, format!("⚠ {missing} point to programs that no longer exist and can be removed."));
            }
            match &self.login_error {
                Some(LoginError::NeedsPermission) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("On this Mac, login items can only be read through System Events, and macOS asks for your permission the first time.").weak());
                        if ui.small_button("Show login items").clicked() {
                            self.allow_system_events = true;
                            self.reload(ctx);
                        }
                    });
                }
                Some(LoginError::NotAllowed) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(AMBER, "⚠ macOS didn't let Heft read your login items.");
                        ui.label(
                            RichText::new("Allow Heft to control System Events in System Settings › Privacy & Security › Automation, then refresh.").weak(),
                        );
                        if ui.small_button("Open Privacy & Security").clicked() {
                            crate::mac::open_url(AUTOMATION_SETTINGS);
                        }
                    });
                }
                Some(LoginError::Failed(e)) => {
                    ui.colored_label(AMBER, format!("⚠ Login items couldn't be read: {e}"));
                }
                None => {}
            }
            if self.entries.iter().any(|e| e.source().remove_needs_admin()) {
                cx.admin_notice(ui, "Turning launch daemons on or off, and removing items for all users, asks for your administrator password.");
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(
                        "Changes take effect the next time you sign in or restart. Switches in System Settings › Login Items & Extensions are separate: an item turned off there stays off.",
                    )
                    .weak()
                    .size(11.5),
                );
                if ui.small_button("Open Login Items settings").clicked() {
                    crate::mac::open_url(LOGIN_ITEMS_SETTINGS);
                }
                let reading = self.all_items_job.is_some();
                let label = if self.all_items.is_some() { "Read all background items again…" } else { "Show all background items…" };
                let b = ui.add_enabled(!reading, egui::Button::new(label).small()).on_hover_text(
                    "macOS's own list of login and background items, as System Settings shows them, listed below. \
                     Reading it asks for your administrator password.",
                );
                if b.clicked() {
                    self.read_all_items(ctx);
                }
                if reading {
                    ui.spinner();
                    ui.label(RichText::new("Waiting for the administrator password…").weak().size(11.5));
                }
            });
        });
    }

}

impl State {
    /// Read macOS's own list of every login and background item, which
    /// only root can, after the password prompt.
    fn read_all_items(&mut self, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(startup::background_items());
            ctx.request_repaint();
        });
        self.all_items_job = Some(rx);
    }

    /// Everything in System Settings › Login Items & Extensions, once read.
    fn all_items_section(&mut self, ui: &mut egui::Ui, filter: &str) {
        let Some(result) = &self.all_items else { return };
        ui.add_space(8.0);
        let items = match result {
            Ok(items) => items,
            Err(e) => {
                ui.colored_label(AMBER, format!("⚠ macOS didn't list them: {e}"));
                return;
            }
        };
        let allowed = items.iter().filter(|i| i.allowed).count();
        egui::CollapsingHeader::new(format!("All background items ({allowed} of {} allowed)", items.len()))
            .id_salt("login_all_items")
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(
                            "Everything macOS lists in System Settings › General › Login Items & Extensions, for every user. \
                             macOS doesn't let other apps switch these, so change them there.",
                        )
                        .weak()
                        .size(11.5),
                    );
                    if ui.small_button("Open Login Items settings").clicked() {
                        crate::mac::open_url(LOGIN_ITEMS_SETTINGS);
                    }
                });
                ui.add_space(4.0);
                for i in items {
                    let text = format!("{} {} {}", i.name, i.developer.as_deref().unwrap_or(""), i.path.as_deref().unwrap_or("")).to_lowercase();
                    if !filter.is_empty() && !text.contains(filter) {
                        continue;
                    }
                    ui.horizontal(|ui| {
                        ui.set_min_height(26.0);
                        ui.label(RichText::new(&i.name).strong());
                        if let Some(d) = i.developer.as_ref().filter(|d| **d != i.name) {
                            ui.label(RichText::new(d).weak());
                        }
                        if let Some(p) = &i.path {
                            ui.add(egui::Label::new(RichText::new(p).monospace().size(11.0).weak()).truncate());
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if let Some(p) = i.path.as_ref().filter(|p| std::path::Path::new(p).exists())
                                && ui.add(egui::Button::new("📂").min_size(vec2(26.0, 22.0))).on_hover_text("Show in Finder").clicked()
                            {
                                crate::platform::reveal(p);
                            }
                            ui.label(RichText::new(&i.kind).weak().size(11.5));
                            if i.allowed {
                                ui.colored_label(GREEN, "allowed");
                            } else {
                                ui.label(RichText::new("not allowed").weak());
                            }
                        });
                    });
                }
            });
    }
}

fn bundled_list(ui: &mut egui::Ui, bundled: &[BundledItem], filter: &str) {
        if bundled.is_empty() {
            return;
        }
        ui.add_space(8.0);
        let active = bundled.iter().filter(|b| b.active).count();
        egui::CollapsingHeader::new(format!("Background items inside apps ({active} of {} active)", bundled.len()))
            .id_salt("login_bundled")
            .show(ui, |ui| {
                ui.label(
                    RichText::new(
                        "Apps can switch these helpers on themselves. macOS doesn't tell other apps which ones are allowed, so they're listed for reference: \
                         \"active\" means one is loaded right now. Turn them on or off in System Settings › General › Login Items & Extensions.",
                    )
                    .weak()
                    .size(11.5),
                );
                ui.add_space(4.0);
                for b in bundled {
                    if !filter.is_empty() && !format!("{} {}", b.app, b.label).to_lowercase().contains(filter) {
                        continue;
                    }
                    ui.horizontal(|ui| {
                        ui.set_min_height(26.0);
                        ui.label(RichText::new(&b.app).strong());
                        ui.add(egui::Label::new(RichText::new(&b.label).monospace().size(11.0).weak()).truncate());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.add(egui::Button::new("📂").min_size(vec2(26.0, 22.0))).on_hover_text("Show in Finder").clicked() {
                                crate::platform::reveal(&b.path.to_string_lossy());
                            }
                            ui.label(RichText::new(b.kind.label()).weak().size(11.5));
                            if b.active {
                                ui.colored_label(GREEN, "active");
                            } else {
                                ui.label(RichText::new("not active").weak());
                            }
                        });
                    });
                }
            });
}

impl State {
    fn remove_modal(&mut self, ctx: &egui::Context) {
        let Some(e) = self.confirm_remove.clone() else { return };
        let mut go = false;
        let mut close = false;
        let resp = egui::Modal::new(egui::Id::new("confirm_login_remove")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.heading(format!("Remove {}?", e.title()));
            ui.add_space(4.0);
            ui.label(RichText::new(&e.command).monospace().size(11.5));
            if let Item::Job(j) = &e.item {
                ui.label(RichText::new(j.plist.to_string_lossy()).monospace().size(11.5).weak());
            }
            ui.add_space(4.0);
            ui.label(RichText::new(remove_hint(e.source())).weak());
            if e.state != PathState::Missing {
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
            let what = if e.source().remove_needs_admin() { "Waiting for the administrator password…" } else { "Removing…" };
            self.start(ctx, what.into(), move || Done::Removed { title: e.title().to_string(), result: startup::remove(&e) });
        } else if close || resp.should_close() {
            self.confirm_remove = None;
        }
    }
}

fn remove_hint(source: Source) -> &'static str {
    match source {
        Source::LoginItem => "It's taken off your login items after a backup is saved. The app itself isn't touched.",
        Source::Job(Domain::UserAgent) => "The .plist is backed up, then moved to the Trash. If it's running, it's stopped.",
        Source::Job(Domain::Agent) => {
            "The .plist is backed up, then deleted from /Library/LaunchAgents, which asks for your administrator password. If it's running, it's stopped."
        }
        Source::Job(Domain::Daemon) => {
            "The .plist is backed up, then deleted from /Library/LaunchDaemons, which asks for your administrator password. If it's running, it's stopped."
        }
    }
}

fn friendly(err: &str) -> &str {
    if err == crate::mac::CANCELLED { "the password prompt was cancelled" } else { err }
}
