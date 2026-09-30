//! Apps workspace (macOS): sizes, uninstalling (with a leftover check
//! afterwards) and updates through Homebrew.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use eframe::egui::{self, vec2, Align, Layout, RichText};

use super::{big_button, card, heading, Cx, Event, AMBER, GREEN};
use crate::mac::apps::brew::{self, Brew, Cask, Upgrade};
use crate::mac::apps::orphans::{self, Orphan};
use crate::mac::apps::updaters::{self, FeedUpdate, StoreUpdate, Updater};
use crate::mac::apps::{self, App, Identity, Leftover, Signature, Uninstaller};
use crate::platform;
use crate::util::{fmt_count, fmt_size};

const NO_BREW: &str = "Homebrew isn't installed. Get it from brew.sh to update apps from here.";
const APP_STORE_UPDATES: &str = "macappstore://showUpdatesPage";
const SOFTWARE_UPDATE: &str = "x-apple.systempreferences:com.apple.Software-Update-Settings.extension";
/// The app was opened again before it could be moved.
const OPEN: &str = "open";

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Sort {
    #[default]
    Size,
    Name,
    Added,
    LastOpened,
    Publisher,
}

#[derive(Clone)]
enum Method {
    /// The app's own uninstaller.
    Uninstaller(Uninstaller),
    /// `brew uninstall --cask` in Terminal.
    Homebrew(Brew, String),
    Trash,
}

enum Outcome {
    /// The app is still there: cancelled, failed, or still running.
    StillInstalled,
    Failed(String),
    Removed(Vec<Leftover>),
}

struct Uninstalling {
    app: App,
    method: Method,
    rx: Receiver<Outcome>,
    stop: Arc<AtomicBool>,
    started: Instant,
}

struct Confirm {
    app: App,
    running: bool,
    admin: bool,
}

/// What checking for updates found: Homebrew's upgrades and installed
/// casks, App Store updates through `mas` (when it's installed), and newer
/// versions in Sparkle apps' feeds (when that's turned on).
struct Checked {
    brew: Result<(Vec<Upgrade>, Vec<Cask>), String>,
    store: Option<Result<Vec<StoreUpdate>, String>>,
    feeds: Vec<FeedUpdate>,
}

struct Leftovers {
    app: App,
    items: Vec<Leftover>,
    checked: HashSet<usize>,
}

/// What apps deleted earlier left behind, by app. Nothing is ticked.
struct Orphans {
    list: Vec<Orphan>,
    checked: HashSet<usize>,
    open: HashSet<usize>,
}

#[derive(Default)]
pub struct State {
    apps: Vec<App>,
    loading: Option<Receiver<Vec<App>>>,
    sizes: Option<Receiver<(PathBuf, u64)>>,
    signatures: Option<Receiver<(PathBuf, Signature)>>,
    loaded: bool,
    filter: String,
    sort: Sort,
    brew: Option<Brew>,
    upgrades: Option<Result<Vec<Upgrade>, String>>,
    /// App Store updates, when `mas` is installed.
    store: Option<Result<Vec<StoreUpdate>, String>>,
    /// Sparkle apps whose own feed lists a newer version.
    feeds: Vec<FeedUpdate>,
    mas: Option<PathBuf>,
    upgrades_job: Option<Receiver<Checked>>,
    /// Casks as `brew info` reported them, which refine the Caskroom's.
    casks: Vec<Cask>,
    /// App bundle → index into `upgrades`.
    badges: HashMap<PathBuf, usize>,
    show_updates: bool,
    confirm_uninstall: Option<Confirm>,
    confirm_update_all: bool,
    uninstalling: Option<Uninstalling>,
    leftovers: Option<Leftovers>,
    orphans: Option<Orphans>,
    orphans_job: Option<Receiver<Vec<Orphan>>>,
    trash_job: Option<Receiver<(u64, Vec<String>)>>,
}

/// Why an app couldn't be moved, and what to do about it. An app you own
/// that macOS still won't let Heft move is protected by App Management.
fn trash_failure(name: &str, e: &str) -> String {
    if e == OPEN {
        return format!("{name} is open. Quit it first, then try again.");
    }
    let lower = e.to_lowercase();
    let blocked = lower.contains("permission") || lower.contains("not permitted");
    if blocked {
        format!(
            "{name} couldn't be moved to the Trash: {e}. If macOS blocked Heft, allow it under App Management in \
             System Settings › Privacy & Security, or drag {name} to the Trash in Finder."
        )
    } else {
        format!("{name} couldn't be moved to the Trash: {e}. You can drag it to the Trash in Finder instead.")
    }
}

/// Record what Heft moved to the Trash, and where it landed, so the Removed
/// page can list it and put it back.
fn log_removed(items: &[(PathBuf, u64, bool, Option<PathBuf>)]) {
    let now = platform::now_unix();
    let logged: Vec<crate::trashlog::Removed> = items
        .iter()
        .map(|(p, size, is_dir, landed)| {
            let (trashed_at, trash_id) = landed.as_deref().map_or((None, 0), |at| {
                let (at, id) = crate::trashlog::located(at);
                (Some(at), id)
            });
            crate::trashlog::Removed {
                when: now,
                size: *size,
                is_dir: *is_dir,
                path: p.to_string_lossy().into_owned(),
                trashed_at,
                trash_id,
                restored: false,
            }
        })
        .collect();
    crate::trashlog::record(&logged);
}

impl State {
    fn reload(&mut self, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(apps::list());
            ctx.request_repaint();
        });
        self.loading = Some(rx);
        self.brew = brew::find();
        self.mas = updaters::find_mas();
    }

    /// Measure bundles in the background, largest (by Spotlight's size) first.
    fn measure(&mut self, ctx: &egui::Context) {
        let mut jobs: Vec<(PathBuf, u64)> = self.apps.iter().map(|a| (a.path.clone(), a.estimated)).collect();
        jobs.sort_by_key(|j| std::cmp::Reverse(j.1));
        let (tx, rx) = crossbeam_channel::unbounded();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            use rayon::prelude::*;
            jobs.par_iter().for_each(|(path, _)| {
                let _ = tx.send((path.clone(), apps::size_on_disk(path)));
                ctx.request_repaint();
            });
        });
        self.sizes = Some(rx);
    }

    /// Read code signatures (the publisher) in the background.
    fn read_signatures(&mut self, ctx: &egui::Context) {
        let paths: Vec<PathBuf> = self.apps.iter().map(|a| a.path.clone()).collect();
        let (tx, rx) = crossbeam_channel::unbounded();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            use rayon::prelude::*;
            paths.par_iter().for_each(|p| {
                let _ = tx.send((p.clone(), apps::signature(p)));
                ctx.request_repaint();
            });
        });
        self.signatures = Some(rx);
    }

    fn check_updates(&mut self, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (brew, mas, ctx) = (self.brew.clone(), self.mas.clone(), ctx.clone());
        // Sparkle apps with a feed, not already handled by Homebrew.
        let feeds: Vec<(PathBuf, String, String)> = if updaters::check_feeds() {
            self.apps
                .iter()
                .filter(|a| a.cask.is_none() && !a.is_from_app_store())
                .filter_map(|a| match &a.updater {
                    Some(Updater::Sparkle { feed: Some(f) }) => Some((a.path.clone(), a.name.clone(), f.clone())),
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        };
        std::thread::spawn(move || {
            let brew = match brew {
                Some(b) => brew::check(&b),
                None => Err(NO_BREW.into()),
            };
            let store = mas.map(|m| updaters::mas_outdated(&m));
            let feeds = updaters::check_feeds_of(&feeds);
            let _ = tx.send(Checked { brew, store, feeds });
            ctx.request_repaint();
        });
        self.upgrades_job = Some(rx);
    }

    /// Updates found by every source.
    fn update_count(&self) -> usize {
        let brew = self.upgrades.as_ref().and_then(|u| u.as_ref().ok()).map_or(0, Vec::len);
        let store = self.store.as_ref().and_then(|u| u.as_ref().ok()).map_or(0, Vec::len);
        brew + store + self.feeds.len()
    }

    /// Match Homebrew's outdated casks to apps through the apps each cask
    /// installed.
    fn match_upgrades(&mut self) {
        self.badges.clear();
        for a in &mut self.apps {
            if a.cask.is_none()
                && let Some(c) = brew::cask_for(&self.casks, &a.path, a.link.as_deref())
            {
                a.cask = Some(c.token.clone());
                if a.source == apps::Source::Other {
                    a.source = apps::Source::Homebrew;
                }
            }
        }
        let Some(Ok(ups)) = &self.upgrades else { return };
        for a in &self.apps {
            if let Some(token) = &a.cask
                && let Some(i) = ups.iter().position(|u| u.cask && u.name == *token)
            {
                self.badges.insert(a.path.clone(), i);
            }
        }
    }

    fn start_uninstall(&mut self, app: App, method: Method, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let stop = Arc::new(AtomicBool::new(false));
        let (a, m, stop2, ctx) = (app.clone(), method.clone(), stop.clone(), ctx.clone());
        std::thread::spawn(move || {
            let wait = |secs: u64| {
                let deadline = Instant::now() + Duration::from_secs(secs);
                while apps::still_installed(&a) && Instant::now() < deadline && !stop2.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(700));
                }
            };
            let failed = match &m {
                // It may have been opened while the dialog was up.
                Method::Trash if apps::is_running(&a) => Some(OPEN.to_string()),
                Method::Trash => {
                    let size = a.size();
                    match apps::trash(std::slice::from_ref(&a.path), &format!("move {} to the Trash", a.name)).pop() {
                        Some((_, Err(e))) => Some(e),
                        landed => {
                            let landed = landed.and_then(|(_, r)| r.ok().flatten());
                            log_removed(&[(a.path.clone(), size, true, landed)]);
                            None
                        }
                    }
                }
                // Some uninstallers hand off to a helper and quit at once,
                // so give the app a while to disappear.
                Method::Uninstaller(u) => apps::run_uninstaller(u).err().or_else(|| {
                    wait(120);
                    None
                }),
                // Homebrew runs in Terminal and may ask for a password there.
                Method::Homebrew(b, token) => brew::uninstall(b, token).err().or_else(|| {
                    wait(900);
                    None
                }),
            };
            let outcome = if let Some(e) = failed {
                Outcome::Failed(e)
            } else if apps::still_installed(&a) {
                Outcome::StillInstalled
            } else {
                // Checked against what's still installed now, so nothing
                // another app uses is offered.
                let mut others: Vec<Identity> = apps::list().iter().map(App::identity).collect();
                others.extend(apps::system_apps());
                Outcome::Removed(apps::find_leftovers(&a, &others))
            };
            let _ = tx.send(outcome);
            ctx.request_repaint();
        });
        self.uninstalling = Some(Uninstalling { app, method, rx, stop, started: Instant::now() });
    }

    /// Look for what deleted apps left in `~/Library`, in the background.
    fn find_orphans(&mut self, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(orphans::find(crate::mac::has_full_disk_access() != Some(false)));
            ctx.request_repaint();
        });
        self.orphans_job = Some(rx);
    }

    /// Move leftovers to the Trash in the background, and log them for the
    /// Removed page. `whose` finishes "move what … left behind".
    fn start_trash(&mut self, items: Vec<(PathBuf, u64)>, whose: &str, ctx: &egui::Context) {
        let why = format!("move what {whose} left behind to the Trash");
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            let paths: Vec<PathBuf> = items.iter().map(|(p, _)| p.clone()).collect();
            let dirs: HashSet<PathBuf> =
                paths.iter().filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())).cloned().collect();
            let results = apps::trash(&paths, &why);
            let mut moved = Vec::new();
            let mut failures = Vec::new();
            for (p, r) in results {
                let size = items.iter().find(|(q, _)| *q == p).map_or(0, |(_, s)| *s);
                match r {
                    Ok(landed) => moved.push((p.clone(), size, dirs.contains(&p), landed)),
                    Err(e) => failures.push(format!("{}: {e}", p.display())),
                }
            }
            log_removed(&moved);
            let _ = tx.send((moved.iter().map(|m| m.1).sum(), failures));
            ctx2.request_repaint();
        });
        self.trash_job = Some(rx);
    }

    fn poll(&mut self, ctx: &egui::Context, cx: &mut Cx) {
        let mut repaint = false;
        if let Some(rx) = &self.orphans_job {
            match rx.try_recv() {
                Ok(list) => {
                    self.orphans_job = None;
                    if list.is_empty() {
                        cx.toast("No leftovers from deleted apps turned up.", false);
                    } else {
                        self.orphans = Some(Orphans { list, checked: HashSet::new(), open: HashSet::new() });
                    }
                }
                Err(_) => repaint = true,
            }
        }
        if let Some(rx) = &self.loading {
            match rx.try_recv() {
                Ok(v) => {
                    self.apps = v;
                    self.loading = None;
                    self.match_upgrades();
                    self.measure(ctx);
                    self.read_signatures(ctx);
                }
                Err(_) => repaint = true,
            }
        }
        if let Some(rx) = &self.sizes {
            for (path, size) in rx.try_iter() {
                if let Some(a) = self.apps.iter_mut().find(|a| a.path == path) {
                    a.measured = Some(size);
                }
            }
        }
        if let Some(rx) = &self.signatures {
            for (path, sig) in rx.try_iter() {
                if let Some(a) = self.apps.iter_mut().find(|a| a.path == path) {
                    a.signature = Some(sig);
                }
            }
        }
        if let Some(rx) = &self.upgrades_job {
            match rx.try_recv() {
                Ok(found) => {
                    let r = found.brew.map(|(ups, casks)| {
                        self.casks = casks;
                        ups
                    });
                    self.upgrades = Some(r);
                    self.store = found.store;
                    self.feeds = found.feeds;
                    self.show_updates = self.update_count() > 0;
                    self.upgrades_job = None;
                    self.match_upgrades();
                }
                Err(_) => repaint = true,
            }
        }
        if let Some(u) = &self.uninstalling {
            match u.rx.try_recv() {
                Ok(Outcome::StillInstalled) => {
                    cx.toast(format!("{} is still installed.", u.app.name), false);
                    self.uninstalling = None;
                    self.reload(ctx);
                }
                Ok(Outcome::Failed(e)) => {
                    let name = u.app.name.clone();
                    match u.method {
                        Method::Trash if e == "cancelled" => cx.toast(format!("Cancelled. {name} is still installed."), false),
                        Method::Trash => cx.toast(trash_failure(&name, &e), true),
                        Method::Uninstaller(_) => cx.toast(format!("Could not open the uninstaller: {e}"), true),
                        Method::Homebrew(..) => cx.toast(format!("Could not start Homebrew: {e}"), true),
                    }
                    self.uninstalling = None;
                }
                Ok(Outcome::Removed(items)) => {
                    let app = u.app.clone();
                    self.uninstalling = None;
                    if items.is_empty() {
                        cx.toast(format!("{} was uninstalled. Nothing was left behind.", app.name), false);
                    } else {
                        let checked = (0..items.len()).filter(|&i| items[i].confident).collect();
                        self.leftovers = Some(Leftovers { app, items, checked });
                    }
                    self.reload(ctx);
                }
                Err(_) => repaint = true,
            }
        }
        if let Some(rx) = &self.trash_job {
            match rx.try_recv() {
                Ok((bytes, failures)) => {
                    match failures.first() {
                        None => cx.toast(format!("Moved leftovers to the Trash ({})", fmt_size(bytes)), false),
                        Some(first) => cx.toast(format!("Some leftovers couldn't be moved: {first}"), true),
                    }
                    self.trash_job = None;
                }
                Err(_) => repaint = true,
            }
        }
        if repaint {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, mut cx: Cx) {
        let ctx = ui.ctx().clone();
        if !self.loaded {
            self.loaded = true;
            self.reload(&ctx);
            if cfg!(debug_assertions) && std::env::var_os("HEFT_DEBUG_ORPHANS").is_some() {
                self.find_orphans(&ctx);
            }
        }
        self.poll(&ctx, &mut cx);

        egui::CentralPanel::default().show(ui, |ui| {
            heading(
                ui,
                "Apps",
                "Every app you installed, with the space it really takes. Uninstalling moves the app to the Trash or runs its own uninstaller, then Heft checks what it left behind.",
            );
            self.summary(ui, &ctx);
            ui.add_space(8.0);
            if self.show_updates {
                self.updates_list(ui, &mut cx);
                ui.add_space(8.0);
            }
            self.app_list(ui, &mut cx);
        });
        self.modals(&ctx, &mut cx);
    }

    fn summary(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let total: u64 = self.apps.iter().map(App::size).sum();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{} apps", self.apps.len())).size(18.0).strong());
                        if self.loading.is_some() {
                            ui.spinner();
                        }
                    });
                    ui.label(RichText::new(format!("{} in total", fmt_size(total))).weak());
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let checking = self.upgrades_job.is_some();
                    let found = self.update_count();
                    let label = if found > 0 { format!("{found} updates") } else { "Check for updates".into() };
                    let mut asks = vec!["Homebrew"];
                    if self.mas.is_some() {
                        asks.push("the App Store (through mas)");
                    }
                    if updaters::check_feeds() {
                        asks.push("apps' own update feeds");
                    }
                    let hover = format!("Asks {} which apps and packages have updates", asks.join(", "));
                    let b = big_button(ui, &label, !checking).on_hover_text(hover);
                    if b.clicked() {
                        if found > 0 {
                            self.show_updates = !self.show_updates;
                        } else {
                            self.check_updates(ctx);
                        }
                    }
                    if checking {
                        ui.spinner();
                        ui.label(RichText::new("Checking for updates…").weak());
                    }
                    if ui.add_enabled(self.loading.is_none(), egui::Button::new("⟳  Refresh")).clicked() {
                        self.reload(ctx);
                    }
                    let leftovers = ui
                        .add_enabled(self.orphans_job.is_none(), egui::Button::new("Leftovers of deleted apps…"))
                        .on_hover_text("Data in your Library folder from apps that are no longer installed");
                    if leftovers.clicked() {
                        self.find_orphans(ctx);
                    }
                });
            });
            if let Some(Err(e)) = &self.upgrades {
                ui.horizontal(|ui| {
                    ui.colored_label(AMBER, e);
                    if self.brew.is_none() && ui.small_button("brew.sh").clicked() {
                        crate::mac::open_url("https://brew.sh");
                    }
                });
            }
            if let Some(Ok(u)) = &self.upgrades
                && u.is_empty()
            {
                ui.colored_label(GREEN, "Everything Homebrew knows about is up to date.");
            }
            if let Some(Err(e)) = &self.store {
                ui.colored_label(AMBER, format!("The App Store didn't answer through mas: {e}"));
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new("Apps that come with macOS aren't listed: they're part of the system and update with it.")
                        .weak()
                        .size(11.5),
                );
                if ui.small_button("Software Update").on_hover_text("Open Software Update in System Settings").clicked() {
                    crate::mac::open_url(SOFTWARE_UPDATE);
                }
                if self.apps.iter().any(App::is_from_app_store)
                    && ui.small_button("App Store updates").on_hover_text("Apps from the App Store update there").clicked()
                {
                    crate::mac::open_url(APP_STORE_UPDATES);
                }
            });
            if let Some(u) = &self.uninstalling {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    let secs = u.started.elapsed().as_secs();
                    match &u.method {
                        Method::Trash => {
                            ui.label(format!("Moving {} to the Trash…", u.app.name));
                        }
                        Method::Uninstaller(_) => {
                            ui.label(format!("Waiting for the {} uninstaller… ({secs}s)", u.app.name));
                        }
                        Method::Homebrew(..) => {
                            ui.label(format!("Waiting for Homebrew to uninstall {} in Terminal… ({secs}s)", u.app.name));
                        }
                    }
                    if !matches!(u.method, Method::Trash) && ui.small_button("Stop waiting").clicked() {
                        u.stop.store(true, Ordering::Relaxed);
                    }
                });
            }
            if self.trash_job.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Moving leftovers to the Trash…");
                });
            }
            if self.orphans_job.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Looking for what deleted apps left behind…");
                });
            }
        });
    }

    /// What an update row is called: the app a cask installed, else the
    /// package's own name.
    fn upgrade_label(&self, u: &Upgrade) -> String {
        if u.cask
            && let Some(a) = self.apps.iter().find(|a| a.cask.as_deref() == Some(u.name.as_str()))
        {
            return a.name.clone();
        }
        u.name.clone()
    }

    fn start_upgrade(&self, u: &Upgrade, cx: &mut Cx) {
        let Some(b) = &self.brew else { return };
        match brew::upgrade(b, u) {
            Ok(()) => cx.toast(format!("Updating {} in Terminal", self.upgrade_label(u)), false),
            Err(e) => cx.toast(format!("Could not start Homebrew: {e}"), true),
        }
    }

    fn updates_list(&mut self, ui: &mut egui::Ui, cx: &mut Cx) {
        let ups = self.upgrades.as_ref().and_then(|u| u.as_ref().ok()).cloned().unwrap_or_default();
        let store = self.store.as_ref().and_then(|u| u.as_ref().ok()).cloned().unwrap_or_default();
        let feeds = self.feeds.clone();
        if ups.is_empty() && store.is_empty() && feeds.is_empty() {
            return;
        }
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} updates available", ups.len() + store.len() + feeds.len())).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if !ups.is_empty() && ui.button("Update all…").on_hover_text("Runs brew upgrade --greedy in Terminal").clicked() {
                        self.confirm_update_all = true;
                    }
                });
            });
            if !ups.is_empty() {
                ui.label(RichText::new("Updates run in Terminal so you can see what Homebrew does and answer its questions.").weak().size(11.5));
            }
            egui::ScrollArea::vertical().id_salt("upgrades").max_height(200.0).show(ui, |ui| {
                egui::Grid::new("upgrade_grid").num_columns(4).striped(true).spacing([16.0, 4.0]).show(ui, |ui| {
                    for u in &ups {
                        ui.horizontal(|ui| {
                            ui.label(self.upgrade_label(u));
                            if !u.cask {
                                ui.label(RichText::new("command-line package").weak().size(11.0));
                            }
                        });
                        ui.label(RichText::new(brew::short_version(&u.installed)).weak());
                        ui.label(RichText::new(format!("to {}", brew::short_version(&u.available))).color(GREEN));
                        let b = ui.add_enabled(!u.pinned, egui::Button::new("Update"));
                        if b.on_disabled_hover_text("Pinned in Homebrew. Unpin it with brew unpin first.").clicked() {
                            self.start_upgrade(u, cx);
                        }
                        ui.end_row();
                    }
                    for u in &store {
                        ui.horizontal(|ui| {
                            ui.label(&u.name);
                            ui.label(RichText::new("App Store").weak().size(11.0));
                        });
                        ui.label(RichText::new(&u.installed).weak());
                        ui.label(RichText::new(format!("to {}", u.available)).color(GREEN));
                        if ui.button("Update").on_hover_text("Runs mas upgrade in Terminal").clicked()
                            && let Some(m) = &self.mas
                        {
                            match updaters::mas_upgrade(m, Some(u.id)) {
                                Ok(()) => cx.toast(format!("Updating {} in Terminal", u.name), false),
                                Err(e) => cx.toast(format!("Could not start mas: {e}"), true),
                            }
                        }
                        ui.end_row();
                    }
                    for u in &feeds {
                        ui.horizontal(|ui| {
                            ui.label(&u.name);
                            ui.label(RichText::new("its own updater").weak().size(11.0));
                        });
                        ui.label(RichText::new(&u.installed).weak());
                        ui.label(RichText::new(format!("to {}", u.available)).color(GREEN));
                        if ui.button("Open to update").on_hover_text("Opens the app; use its Check for Updates").clicked()
                            && let Err(e) = updaters::open_to_update(&u.app)
                        {
                            cx.toast(format!("Could not open {}: {e}", u.name), true);
                        }
                        ui.end_row();
                    }
                });
            });
        });
    }

    fn app_list(&mut self, ui: &mut egui::Ui, cx: &mut Cx) {
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Search name or publisher").desired_width(220.0));
            ui.label("Sort by");
            egui::ComboBox::from_id_salt("app_sort")
                .selected_text(match self.sort {
                    Sort::Size => "Size",
                    Sort::Name => "Name",
                    Sort::Added => "Date added",
                    Sort::LastOpened => "Least recently opened",
                    Sort::Publisher => "Publisher",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.sort, Sort::Size, "Size");
                    ui.selectable_value(&mut self.sort, Sort::Name, "Name");
                    ui.selectable_value(&mut self.sort, Sort::Added, "Date added");
                    ui.selectable_value(&mut self.sort, Sort::LastOpened, "Least recently opened");
                    ui.selectable_value(&mut self.sort, Sort::Publisher, "Publisher");
                });
            if self.sizes.is_some() && self.apps.iter().any(|a| a.measured.is_none()) {
                ui.spinner();
                ui.label(RichText::new("measuring apps…").weak());
            }
        });
        ui.add_space(4.0);

        let filter = self.filter.to_lowercase();
        let publishers: Vec<String> = self.apps.iter().map(App::publisher).collect();
        let mut order: Vec<usize> = (0..self.apps.len())
            .filter(|&i| {
                let a = &self.apps[i];
                filter.is_empty()
                    || a.name.to_lowercase().contains(&filter)
                    || publishers[i].to_lowercase().contains(&filter)
                    || a.id.as_ref().is_some_and(|id| id.to_lowercase().contains(&filter))
            })
            .collect();
        let aps = &self.apps;
        match self.sort {
            Sort::Size => order.sort_by_key(|&i| std::cmp::Reverse(aps[i].size())),
            Sort::Name => order.sort_by_key(|&i| aps[i].name.to_lowercase()),
            Sort::Added => order.sort_by_key(|&i| std::cmp::Reverse(aps[i].added)),
            // Never-opened (or not indexed) apps last: Spotlight may just not know.
            Sort::LastOpened => order.sort_by_key(|&i| (aps[i].last_used.is_none(), aps[i].last_used)),
            Sort::Publisher => order.sort_by_key(|&i| (publishers[i].to_lowercase(), aps[i].name.to_lowercase())),
        }

        let busy = self.uninstalling.is_some();
        egui::ScrollArea::vertical().id_salt("app_rows").auto_shrink([false, false]).show_rows(ui, 46.0, order.len(), |ui, range| {
            for &i in &order[range] {
                let a = self.apps[i].clone();
                let upgrade = self.badges.get(&a.path).and_then(|&u| self.upgrades.as_ref()?.as_ref().ok()?.get(u).cloned());
                ui.horizontal(|ui| {
                    ui.set_height(42.0);
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&a.name).strong());
                            ui.label(RichText::new(&a.version).weak());
                            if let Some(u) = &upgrade {
                                ui.label(RichText::new(format!("⬆ {}", brew::short_version(&u.available))).color(GREEN))
                                    .on_hover_text("An update is available through Homebrew");
                            }
                            if let Some(f) = self.feeds.iter().find(|f| f.app == a.path) {
                                ui.label(RichText::new(format!("⬆ {}", f.available)).color(GREEN))
                                    .on_hover_text("Its own update feed lists a newer version. Open the app to update it");
                            }
                            if a.is_from_app_store() {
                                ui.label(RichText::new("App Store").weak().size(11.0));
                            } else if let Some(u) = a.updater.as_ref().filter(|_| a.cask.is_none()) {
                                ui.label(RichText::new(u.label()).weak().size(11.0)).on_hover_text(u.hover());
                            } else if a.cask.is_some() {
                                ui.label(RichText::new("Homebrew").weak().size(11.0)).on_hover_text(format!(
                                    "Installed with Homebrew (cask {})",
                                    a.cask.as_deref().unwrap_or_default()
                                ));
                            }
                        });
                        let mut meta = Vec::new();
                        if !publishers[i].is_empty() {
                            meta.push(publishers[i].clone());
                        }
                        if a.added > 0 {
                            meta.push(format!("added {}", platform::fmt_date(a.added)));
                        }
                        if let Some(t) = a.last_used {
                            meta.push(format!("opened {}", platform::fmt_date(t)));
                        }
                        meta.push(a.shown_path().to_string_lossy().into_owned());
                        ui.add(egui::Label::new(RichText::new(meta.join("  ·  ")).weak().size(11.5)).truncate());
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.menu_button("☰", |ui| self.app_menu(ui, &a, upgrade.as_ref(), cx));
                        let b = ui.add_enabled(!busy, egui::Button::new("Uninstall").min_size(vec2(84.0, 24.0)));
                        if b.clicked() {
                            let running = apps::is_running(&a);
                            self.confirm_uninstall = Some(Confirm { admin: a.needs_admin(), running, app: a.clone() });
                        }
                        let size = match (a.measured, a.estimated) {
                            (Some(m), _) if m > 0 => RichText::new(fmt_size(m)).strong(),
                            (_, e) if e > 0 => RichText::new(format!("~{}", fmt_size(e))),
                            _ => RichText::new("-").weak(),
                        };
                        ui.label(size).on_hover_text(if a.measured.is_some_and(|m| m > 0) {
                            "Measured space the app takes on disk"
                        } else {
                            "Size from Spotlight, until Heft measures it"
                        });
                    });
                });
                ui.separator();
            }
        });
    }

    fn app_menu(&mut self, ui: &mut egui::Ui, a: &App, upgrade: Option<&Upgrade>, cx: &mut Cx) {
        if let Some(u) = upgrade
            && ui.add_enabled(!u.pinned, egui::Button::new(format!("Update to {}", brew::short_version(&u.available)))).clicked()
        {
            self.start_upgrade(u, cx);
            ui.close();
        }
        if ui.button(format!("Show in {}", platform::FILE_MANAGER)).clicked() {
            platform::reveal(&a.shown_path().to_string_lossy());
            ui.close();
        }
        if ui.button("Show in disk map").on_hover_text("Scan the app in Disk usage").clicked() {
            cx.events.push(Event::Scan(a.path.to_string_lossy().into_owned()));
            ui.close();
        }
        if ui.button("Copy path").clicked() {
            ui.ctx().copy_text(a.path.to_string_lossy().into_owned());
            ui.close();
        }
        if let Some(id) = &a.id
            && ui.button("Copy bundle ID").clicked()
        {
            ui.ctx().copy_text(id.clone());
            ui.close();
        }
        if a.is_from_app_store() && ui.button("App Store updates").clicked() {
            crate::mac::open_url(APP_STORE_UPDATES);
            ui.close();
        }
        if let Some(u) = &a.updater
            && ui.button("Open to update").on_hover_text(u.hover()).clicked()
        {
            if let Err(e) = updaters::open_to_update(&a.path) {
                cx.toast(format!("Could not open {}: {e}", a.name), true);
            }
            ui.close();
        }
    }

    fn modals(&mut self, ctx: &egui::Context, cx: &mut Cx) {
        if self.confirm_update_all {
            let n = self.upgrades.as_ref().and_then(|u| u.as_ref().ok()).map_or(0, |u| u.len());
            let mut choice = None;
            let resp = egui::Modal::new(egui::Id::new("confirm_update_all")).show(ctx, |ui| {
                ui.set_width(460.0);
                ui.heading(format!("Update {n} packages?"));
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Homebrew downloads and installs each update in a Terminal window. Some may ask for your password \
                         there. Quit the apps you're updating first.",
                    )
                    .weak(),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Update all").strong()).clicked() {
                        choice = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(false);
                    }
                });
            });
            if choice == Some(true) {
                self.confirm_update_all = false;
                if let Some(b) = &self.brew {
                    match brew::upgrade_all(b) {
                        Ok(()) => cx.toast("Updating in Terminal. Check for updates again when it's done.", false),
                        Err(e) => cx.toast(format!("Could not start Homebrew: {e}"), true),
                    }
                }
            } else if choice == Some(false) || resp.should_close() {
                self.confirm_update_all = false;
            }
        }

        if let Some(c) = &mut self.confirm_uninstall {
            let app = c.app.clone();
            let mut choice: Option<Option<Method>> = None;
            let cask = app.cask.clone().zip(self.brew.clone());
            let resp = egui::Modal::new(egui::Id::new("confirm_uninstall")).show(ctx, |ui| {
                ui.set_width(480.0);
                ui.heading(format!("Uninstall {}?", app.name));
                ui.add_space(2.0);
                let where_ = match &app.link {
                    Some(l) => format!("{} (linked from {})", app.path.display(), l.display()),
                    None => app.path.display().to_string(),
                };
                ui.add(egui::Label::new(RichText::new(where_).monospace().size(11.5).weak()).truncate());
                ui.add_space(6.0);
                if c.running {
                    ui.colored_label(AMBER, format!("{} is open. Quit it first, then try again.", app.name));
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Check again").strong()).clicked() {
                            c.running = apps::is_running(&app);
                        }
                        if ui.button("Cancel").clicked() {
                            choice = Some(None);
                        }
                    });
                    return;
                }
                let trash_label = if let Some(u) = &app.uninstaller {
                    ui.label(
                        RichText::new(format!(
                            "{} comes with its own uninstaller ({}), which can also remove the helpers and settings it installed. \
                             Heft opens it, and when it's done, looks for anything left behind.",
                            app.name,
                            u.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
                        ))
                        .weak(),
                    );
                    "Move to Trash instead"
                } else if let Some((token, _)) = &cask {
                    ui.label(
                        RichText::new(format!(
                            "{} was installed with Homebrew (cask {token}). Uninstalling it with Homebrew also runs the cask's own \
                             uninstall steps. It runs in Terminal, so you can see what it does. Then Heft looks for anything left behind.",
                            app.name
                        ))
                        .weak(),
                    );
                    "Move to Trash instead"
                } else {
                    ui.label(
                        RichText::new(format!(
                            "Heft moves {} to the Trash, then looks for settings, caches and other files it left behind.",
                            app.name
                        ))
                        .weak(),
                    );
                    "Move to Trash"
                };
                if c.admin {
                    ui.label(
                        RichText::new("It was installed for all users, so macOS will ask for your administrator password to move it.")
                            .weak()
                            .size(11.5),
                    );
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if let Some(u) = &app.uninstaller
                        && ui.button(RichText::new("Open its uninstaller").strong()).clicked()
                    {
                        choice = Some(Some(Method::Uninstaller(u.clone())));
                    }
                    if app.uninstaller.is_none()
                        && let Some((token, b)) = &cask
                        && ui.button(RichText::new("Uninstall with Homebrew").strong()).clicked()
                    {
                        choice = Some(Some(Method::Homebrew(b.clone(), token.clone())));
                    }
                    let plain = app.uninstaller.is_none() && cask.is_none();
                    let text = if plain { RichText::new(trash_label).strong() } else { RichText::new(trash_label) };
                    if ui.button(text).clicked() {
                        choice = Some(Some(Method::Trash));
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(None);
                    }
                });
            });
            match choice {
                Some(Some(method)) => {
                    self.confirm_uninstall = None;
                    self.start_uninstall(app, method, ctx);
                }
                Some(None) => self.confirm_uninstall = None,
                None if resp.should_close() => self.confirm_uninstall = None,
                None => {}
            }
        }

        let mut close = false;
        let mut trash: Option<Vec<(PathBuf, u64)>> = None;
        if let Some(l) = &mut self.leftovers {
            let resp = egui::Modal::new(egui::Id::new("leftovers")).show(ctx, |ui| {
                ui.set_width(600.0);
                ui.heading(format!("{} left some things behind", l.app.name));
                ui.add_space(4.0);
                ui.label(RichText::new("Pick what to move to the Trash. Items that only share the app's name are unticked, so check them first.").weak());
                ui.add_space(6.0);
                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                    for (i, item) in l.items.iter().enumerate() {
                        ui.horizontal(|ui| {
                            let mut on = l.checked.contains(&i);
                            if ui.checkbox(&mut on, "").changed() {
                                if on {
                                    l.checked.insert(i);
                                } else {
                                    l.checked.remove(&i);
                                }
                            }
                            if item.measured {
                                ui.label(RichText::new(fmt_size(item.size)).strong());
                            } else {
                                ui.label(RichText::new("size?").strong().weak()).on_hover_text(
                                    "Another app's data. macOS only lets Heft look inside with Full Disk Access, so it wasn't measured.",
                                );
                            }
                            ui.vertical(|ui| {
                                ui.add(egui::Label::new(RichText::new(item.path.to_string_lossy()).monospace().size(11.5)).truncate());
                                ui.label(RichText::new(item.reason).weak().size(11.0));
                            });
                        });
                    }
                });
                let total: u64 = l.checked.iter().map(|&i| l.items[i].size).sum();
                let unmeasured = l.checked.iter().any(|&i| !l.items[i].measured);
                if l.checked.iter().any(|&i| l.items[i].admin) {
                    ui.add_space(4.0);
                    ui.label(RichText::new("Some of these belong to the system, so macOS will ask for your administrator password.").weak().size(11.5));
                }
                if l.checked.iter().any(|&i| l.items[i].reason.starts_with("launch")) {
                    ui.label(RichText::new("Launch agents and daemons are stopped too, so what they run doesn't keep going until you restart.").weak().size(11.5));
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let what = if unmeasured { "them".to_string() } else { fmt_size(total) };
                    let b = ui.add_enabled(!l.checked.is_empty(), egui::Button::new(RichText::new(format!("Move {what} to the Trash")).strong()));
                    if b.clicked() {
                        let mut picked: Vec<usize> = l.checked.iter().copied().collect();
                        picked.sort_unstable();
                        trash = Some(picked.iter().map(|&i| (l.items[i].path.clone(), l.items[i].size)).collect());
                    }
                    if ui.button("Keep everything").clicked() {
                        close = true;
                    }
                });
                ui.label(RichText::new(format!("{} items found", fmt_count(l.items.len() as u64))).weak().size(11.0));
            });
            if resp.should_close() {
                close = true;
            }
        }
        if let Some(items) = trash {
            let name = self.leftovers.as_ref().map(|l| l.app.name.clone()).unwrap_or_default();
            self.start_trash(items, &name, ctx);
            close = true;
        }
        if close {
            self.leftovers = None;
        }
        self.orphans_modal(ctx);
    }

    fn orphans_modal(&mut self, ctx: &egui::Context) {
        let Some(o) = &mut self.orphans else { return };
        let mut close = false;
        let mut trash: Option<Vec<(PathBuf, u64)>> = None;
        let resp = egui::Modal::new(egui::Id::new("orphans")).show(ctx, |ui| {
            ui.set_width(640.0);
            ui.heading("Leftovers of deleted apps");
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "Data in your Library folder from apps that are no longer installed: their data containers, and files \
                     named after the same app. Only apps macOS no longer knows anywhere on this Mac are listed. Nothing is \
                     ticked, so check each one first: an app you still use from a disk that isn't connected shows up here too.",
                )
                .weak(),
            );
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                for (i, orphan) in o.list.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let mut on = o.checked.contains(&i);
                        if ui.checkbox(&mut on, "").changed() {
                            if on {
                                o.checked.insert(i);
                            } else {
                                o.checked.remove(&i);
                            }
                        }
                        let size = if orphan.items.iter().all(|l| l.measured) {
                            fmt_size(orphan.size())
                        } else if orphan.size() > 0 {
                            format!("{}+", fmt_size(orphan.size()))
                        } else {
                            "size?".to_string()
                        };
                        ui.label(RichText::new(size).strong());
                        let open = o.open.contains(&i);
                        let arrow = if open { "⏷" } else { "⏵" };
                        let title = ui.add(egui::Button::new(format!("{arrow} {}", orphan.title())).frame(false));
                        if title.clicked() && !o.open.remove(&i) {
                            o.open.insert(i);
                        }
                        if orphan.name.is_some() {
                            ui.label(RichText::new(&orphan.id).weak().size(11.5));
                        }
                        ui.label(RichText::new(format!("{} item(s)", orphan.items.len())).weak().size(11.5));
                    });
                    if o.open.contains(&i) {
                        ui.indent(("orphan", i), |ui| {
                            for l in &orphan.items {
                                ui.horizontal(|ui| {
                                    let size = if l.measured { fmt_size(l.size) } else { "?".into() };
                                    ui.label(RichText::new(size).monospace().size(11.0));
                                    ui.add(egui::Label::new(RichText::new(l.path.to_string_lossy()).monospace().size(11.0).weak()).truncate());
                                });
                            }
                        });
                    }
                }
            });
            let picked: Vec<&Orphan> = o.checked.iter().map(|&i| &o.list[i]).collect();
            let total: u64 = picked.iter().map(|p| p.size()).sum();
            if o.list.iter().any(|p| !p.measured()) {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Sizes marked ? are other apps' data, which macOS only lets Heft measure with Full Disk Access. \
                         Without it, macOS may also refuse to move them.",
                    )
                    .weak()
                    .size(11.5),
                );
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let label = if picked.is_empty() || !picked.iter().all(|p| p.measured()) {
                    "Move to the Trash".to_string()
                } else {
                    format!("Move {} to the Trash", fmt_size(total))
                };
                if ui.add_enabled(!picked.is_empty(), egui::Button::new(RichText::new(label).strong())).clicked() {
                    trash = Some(picked.iter().flat_map(|p| p.items.iter().map(|l| (l.path.clone(), l.size))).collect());
                }
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        });
        if resp.should_close() {
            close = true;
        }
        if let Some(items) = trash {
            self.start_trash(items, "deleted apps", ctx);
            close = true;
        }
        if close {
            self.orphans = None;
        }
    }
}
