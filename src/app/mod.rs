//! Application state and top-level layout.

mod screens;
mod tabs;
#[cfg(windows)]
mod tools;
mod tree_view;
mod treemap_view;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use eframe::egui::{self, Color32, Key, RichText};

use crate::colors;
use crate::dupes::{DupGroup, DupProgress};
use crate::history::{self, Diff, SnapMeta};
use crate::platform::{self, DriveInfo};
use crate::scan::{self, ScanHandle, ScanOutcome};
use crate::tree::{NodeId, Tree, ROOT};
use crate::treemap::{self, ColorMode, Highlight};
use crate::util::{fmt_count, fmt_duration_ms, fmt_size};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Tree,
    Types,
    Largest,
    Duplicates,
    Junk,
    Changes,
}

/// The top-level pages. Disk usage everywhere; the maintenance tools are
/// Windows-only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Workspace {
    Disk,
    #[cfg(windows)]
    Cleaner,
    #[cfg(windows)]
    Startup,
    #[cfg(windows)]
    Programs,
    #[cfg(windows)]
    Registry,
}

impl Workspace {
    #[cfg(windows)]
    const ALL: [Workspace; 5] =
        [Workspace::Disk, Workspace::Cleaner, Workspace::Startup, Workspace::Programs, Workspace::Registry];

    #[cfg(windows)]
    fn label(self) -> &'static str {
        match self {
            Workspace::Disk => "Disk usage",
            Workspace::Cleaner => "Cleaner",
            Workspace::Startup => "Startup",
            Workspace::Programs => "Programs",
            Workspace::Registry => "Registry",
        }
    }

    /// Name used by `--open=` (so elevating keeps you on the same page).
    fn key(self) -> &'static str {
        match self {
            Workspace::Disk => "disk",
            #[cfg(windows)]
            Workspace::Cleaner => "cleaner",
            #[cfg(windows)]
            Workspace::Startup => "startup",
            #[cfg(windows)]
            Workspace::Programs => "programs",
            #[cfg(windows)]
            Workspace::Registry => "registry",
        }
    }

    fn from_key(key: &str) -> Option<Workspace> {
        #[cfg(windows)]
        return Workspace::ALL.into_iter().find(|w| w.key() == key);
        #[cfg(not(windows))]
        return (key == "disk").then_some(Workspace::Disk);
    }
}

struct JunkState {
    results: Vec<crate::devjunk::Junk>,
    checked: HashSet<NodeId>,
    key: Option<(usize, u64, NodeId)>,
}

/// UI events collected during a frame and applied afterwards, so widgets never
/// need mutable access to the whole app while they're being drawn.
pub enum Action {
    Select(NodeId),
    Reveal(NodeId),
    Zoom(NodeId),
    ZoomOut,
    ToggleExpand(NodeId),
    Open(NodeId),
    ShowInFileManager(NodeId),
    CopyPath(NodeId),
    Delete(Vec<NodeId>),
    ScanPath(String),
    Rescan,
    SetHighlight(Highlight),
}

#[derive(Clone, PartialEq)]
struct RenderKey {
    tree_version: u64,
    tree_ptr: usize,
    root: NodeId,
    size: [usize; 2],
    mode: ColorMode,
    highlight: Highlight,
    diff_ptr: usize,
}

struct LargestState {
    filter: String,
    min_age_days: u32,
    results: Vec<NodeId>,
    key: Option<(usize, u64, NodeId, String, u32)>,
}

struct DupState {
    min_size: u64,
    running: Option<(Arc<DupProgress>, Receiver<Vec<DupGroup>>, Instant)>,
    groups: Vec<DupGroup>,
    checked: HashSet<NodeId>,
    collapsed: HashSet<usize>,
    scope: NodeId,
    searched: bool,
}

struct DeleteJob {
    rx: Receiver<Vec<(NodeId, String, Result<(), String>)>>,
}

/// Folders we didn't look inside (virtual mounts, a second path to a folder
/// counted elsewhere) show as empty, but trashing them would remove real data
/// the user never saw.
fn deletable(tree: &Tree, id: NodeId) -> bool {
    tree.node(id).flags & (crate::tree::flags::MOUNT | crate::tree::flags::SEEN) == 0
}

#[cfg(windows)]
const UNREADABLE_HINT: &str = "Access was denied. Run as administrator to include them.";
#[cfg(target_os = "macos")]
const UNREADABLE_HINT: &str =
    "Access was denied. Give Heft Full Disk Access in System Settings › Privacy & Security to include them.";
#[cfg(not(any(windows, target_os = "macos")))]
const UNREADABLE_HINT: &str = "Permission denied. Scan with sudo (heft --bench PATH) to include them.";

pub struct HeftApp {
    drives: Vec<DriveInfo>,
    last_scans: Vec<Option<i64>>,
    elevated: bool,
    use_mft: bool,
    scan: Option<ScanHandle>,
    error: Option<String>,
    restore_view: Option<String>,

    tree: Option<Arc<Tree>>,
    ext_colors: Arc<Vec<[f32; 3]>>,
    view_root: NodeId,
    selected: Option<NodeId>,
    /// Item under the pointer in the treemap.
    hovered: Option<NodeId>,
    /// Item under the pointer in one of the lists (outlined in the treemap).
    list_hover: Option<NodeId>,
    menu_node: Option<NodeId>,

    expanded: HashSet<NodeId>,
    rows: Vec<(NodeId, u16)>,
    rows_dirty: bool,
    scroll_to_selected: bool,

    worker: treemap::Worker,
    rendered: Option<treemap::Rendered>,
    texture: Option<egui::TextureHandle>,
    render_key: Option<RenderKey>,
    render_seq: u64,
    color_mode: ColorMode,
    highlight: Highlight,

    workspace: Workspace,
    #[cfg(windows)]
    tools: tools::Tools,

    tab: Tab,
    largest: LargestState,
    dupes: DupState,
    junk: JunkState,

    snapshots: Vec<SnapMeta>,
    compare_to: usize,
    diff: Option<Arc<Diff>>,
    diff_job: Option<Receiver<Result<Diff, String>>>,
    hotspots: Vec<(NodeId, i64)>,
    hotspots_key: Option<(u64, NodeId, usize)>,

    confirm_delete: Option<Vec<NodeId>>,
    delete_job: Option<DeleteJob>,
    toast: Option<(String, Instant, bool)>,
    actions: Vec<Action>,
    /// Showing made-up data (`HEFT_DEMO`), so leave scan history alone.
    demo: bool,
}

impl HeftApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Option<String>) -> Self {
        let repaint_ctx = cc.egui_ctx.clone();
        let worker = treemap::Worker::new(move || repaint_ctx.request_repaint());
        let get = |k: &str| cc.storage.and_then(|s| s.get_string(k));
        let color_mode = match get("color_mode").as_deref() {
            Some("category") => ColorMode::Category,
            Some("age") => ColorMode::Age,
            _ => ColorMode::Extension,
        };
        let use_mft = get("use_mft").as_deref() != Some("0");
        let workspace = std::env::args()
            .find_map(|a| a.strip_prefix("--open=").and_then(Workspace::from_key))
            .unwrap_or(Workspace::Disk);
        let elevated = platform::is_elevated();

        let mut app = Self {
            drives: Vec::new(),
            last_scans: Vec::new(),
            elevated,
            use_mft,
            scan: None,
            error: None,
            restore_view: None,
            tree: None,
            ext_colors: Arc::new(Vec::new()),
            view_root: ROOT,
            selected: None,
            hovered: None,
            list_hover: None,
            menu_node: None,
            expanded: HashSet::new(),
            rows: Vec::new(),
            rows_dirty: true,
            scroll_to_selected: false,
            worker,
            rendered: None,
            texture: None,
            render_key: None,
            render_seq: 0,
            color_mode,
            highlight: Highlight::None,
            workspace,
            #[cfg(windows)]
            tools: tools::Tools::new(elevated),
            tab: Tab::Tree,
            largest: LargestState { filter: String::new(), min_age_days: 0, results: Vec::new(), key: None },
            junk: JunkState { results: Vec::new(), checked: HashSet::new(), key: None },
            dupes: DupState {
                min_size: 1 << 20,
                running: None,
                groups: Vec::new(),
                checked: HashSet::new(),
                collapsed: HashSet::new(),
                scope: ROOT,
                searched: false,
            },
            snapshots: Vec::new(),
            compare_to: 0,
            diff: None,
            diff_job: None,
            hotspots: Vec::new(),
            hotspots_key: None,
            confirm_delete: None,
            delete_job: None,
            toast: None,
            actions: Vec::new(),
            demo: false,
        };
        app.refresh_drives();
        if std::env::var_os("HEFT_DEMO").is_some() {
            app.demo = true;
            app.set_tree(crate::demo::tree());
            #[cfg(debug_assertions)]
            app.apply_debug_env(&cc.egui_ctx);
        } else if let Some(path) = initial {
            app.start_scan(&path, &cc.egui_ctx);
        }
        app
    }

    fn refresh_drives(&mut self) {
        self.drives = platform::list_locations();
        self.last_scans = self.drives.iter().map(|d| history::list(&d.root).first().map(|m| m.taken_at)).collect();
    }

    fn start_scan(&mut self, root: &str, ctx: &egui::Context) {
        self.workspace = Workspace::Disk;
        let ctx = ctx.clone();
        self.error = None;
        self.scan = Some(scan::start(root, self.use_mft, move || ctx.request_repaint()));
    }

    fn set_tree(&mut self, t: Tree) {
        let tree = Arc::new(t);
        self.ext_colors = Arc::new(colors::extension_colors(&tree.exts));
        self.view_root = ROOT;
        self.selected = None;
        self.hovered = None;
        self.expanded = HashSet::from([ROOT]);
        self.rows_dirty = true;
        self.render_key = None;
        self.highlight = Highlight::None;
        self.largest.key = None;
        self.junk.key = None;
        self.junk.checked.clear();
        self.dupes.groups.clear();
        self.dupes.checked.clear();
        self.dupes.searched = false;
        self.dupes.running = None;
        self.dupes.scope = ROOT;
        self.diff = None;
        self.diff_job = None;
        self.hotspots.clear();
        self.hotspots_key = None;
        if self.color_mode == ColorMode::Growth {
            self.color_mode = ColorMode::Extension;
        }

        // Previous snapshots of this root, then save this scan as the newest.
        // Demo data never touches the real history.
        self.compare_to = 0;
        if self.demo {
            self.snapshots.clear();
        } else {
            self.snapshots = history::list(&tree.root_path);
            let for_save = tree.clone();
            std::thread::spawn(move || {
                let _ = history::save(&for_save);
            });
        }

        if let Some(path) = self.restore_view.take()
            && let Some(id) = tree.find(&path)
                && tree.node(id).is_dir() {
                    self.view_root = id;
                    self.tree = Some(tree);
                    self.expand_to(id);
                    return;
                }
        self.tree = Some(tree);
    }

    /// Debug builds only: preset UI state from environment variables so
    /// views can be checked without driving the mouse.
    #[cfg(debug_assertions)]
    fn apply_debug_env(&mut self, ctx: &egui::Context) {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let Some(tree) = self.tree.clone() else { return };
        if let Some(t) = env("HEFT_DEBUG_TAB") {
            self.tab = match t.as_str() {
                "types" => Tab::Types,
                "largest" => Tab::Largest,
                "duplicates" => Tab::Duplicates,
                "junk" => Tab::Junk,
                "changes" => Tab::Changes,
                _ => Tab::Tree,
            };
        }
        if let Some(id) = env("HEFT_DEBUG_ZOOM").and_then(|p| tree.find(&p)) {
            self.view_root = id;
            self.expand_to(id);
        }
        if let Some(id) = env("HEFT_DEBUG_SELECT").and_then(|p| tree.find(&p)) {
            self.actions.push(Action::Reveal(id));
        }
        if let Some(m) = env("HEFT_DEBUG_COLOR") {
            self.color_mode = match m.as_str() {
                "category" => ColorMode::Category,
                "age" => ColorMode::Age,
                _ => ColorMode::Extension,
            };
        }
        if env("HEFT_DEBUG_DUPES").is_some() {
            self.start_dupes(ctx);
        }
        if env("HEFT_DEBUG_DIFF").is_some() && !self.snapshots.is_empty() {
            self.start_diff(ctx);
        }
    }

    fn expand_to(&mut self, id: NodeId) {
        if let Some(tree) = &self.tree {
            for a in tree.ancestors(id) {
                if a != id || tree.node(id).is_dir() {
                    self.expanded.insert(a);
                }
            }
            if !tree.node(id).is_dir() {
                self.expanded.remove(&id);
            }
            self.rows_dirty = true;
        }
    }

    fn toast(&mut self, msg: impl Into<String>, is_error: bool) {
        self.toast = Some((msg.into(), Instant::now(), is_error));
    }

    // ------------------------------------------------------------------
    // Background work

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(h) = &self.scan {
            match h.rx.try_recv() {
                Ok(outcome) => {
                    self.scan = None;
                    match outcome {
                        ScanOutcome::Done(t) => {
                            self.set_tree(t);
                            #[cfg(debug_assertions)]
                            self.apply_debug_env(ctx);
                        }
                        ScanOutcome::Cancelled => {}
                        ScanOutcome::Failed(e) => self.error = Some(e),
                    }
                }
                Err(_) => ctx.request_repaint_after(Duration::from_millis(80)),
            }
        }

        while let Ok(r) = self.worker.rx.try_recv() {
            if r.seq > self.rendered.as_ref().map(|x| x.seq).unwrap_or(0) {
                let image = r.image.clone();
                match &mut self.texture {
                    Some(t) => t.set(image, egui::TextureOptions::NEAREST),
                    None => self.texture = Some(ctx.load_texture("treemap", image, egui::TextureOptions::NEAREST)),
                }
                self.rendered = Some(r);
            }
        }

        if let Some((p, rx, _)) = &self.dupes.running {
            match rx.try_recv() {
                Ok(groups) => {
                    let cancelled = p.cancel.load(std::sync::atomic::Ordering::Relaxed);
                    self.dupes.running = None;
                    if !cancelled {
                        self.dupes.groups = groups;
                        self.dupes.checked.clear();
                        self.dupes.collapsed.clear();
                        self.dupes.searched = true;
                    }
                }
                Err(_) => ctx.request_repaint_after(Duration::from_millis(100)),
            }
        }

        if let Some(rx) = &self.diff_job {
            if let Ok(res) = rx.try_recv() {
                self.diff_job = None;
                match res {
                    Ok(d) => {
                        self.diff = Some(Arc::new(d));
                        self.hotspots_key = None;
                        self.color_mode = ColorMode::Growth;
                    }
                    Err(e) => self.toast(format!("Could not load snapshot: {e}"), true),
                }
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }

        if let Some(job) = &self.delete_job {
            if let Ok(results) = job.rx.try_recv() {
                self.delete_job = None;
                self.finish_delete(results);
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
    }

    fn start_delete(&mut self, ids: Vec<NodeId>, ctx: &egui::Context) {
        let Some(tree) = &self.tree else { return };
        let items: Vec<(NodeId, String)> = ids.iter().map(|&id| (id, tree.path(id))).collect();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let res = items
                .into_iter()
                .map(|(id, path)| {
                    let r = trash::delete(&path).map_err(|e| e.to_string());
                    (id, path, r)
                })
                .collect();
            let _ = tx.send(res);
            ctx.request_repaint();
        });
        self.delete_job = Some(DeleteJob { rx });
    }

    fn finish_delete(&mut self, results: Vec<(NodeId, String, Result<(), String>)>) {
        let Some(arc) = self.tree.as_mut() else { return };
        let tree = Arc::make_mut(arc);
        let mut freed = 0u64;
        let mut ok = 0usize;
        let mut failures = Vec::new();
        for (id, path, r) in results {
            match r {
                Ok(()) => {
                    freed += tree.node(id).size;
                    tree.remove(id);
                    ok += 1;
                }
                Err(e) => failures.push(format!("{path}: {e}")),
            }
        }
        let gone = |t: &Tree, id: NodeId| t.ancestors(id).iter().any(|&a| t.is_deleted(a));
        if self.selected.is_some_and(|s| gone(tree, s)) {
            self.selected = None;
        }
        while gone(tree, self.view_root) {
            self.view_root = tree.node(self.view_root).parent;
        }
        for g in &mut self.dupes.groups {
            g.files.retain(|&f| !gone(tree, f));
        }
        self.dupes.groups.retain(|g| g.files.len() > 1);
        self.dupes.checked.retain(|&f| !gone(tree, f));
        self.junk.results.retain(|j| !gone(tree, j.id));
        self.junk.checked.retain(|&f| !gone(tree, f));
        self.rows_dirty = true;
        self.largest.key = None;
        self.hotspots_key = None;

        if failures.is_empty() {
            self.toast(format!("Moved {ok} item(s) to the {}, freed {}", platform::TRASH, fmt_size(freed)), false);
        } else {
            let first = failures.first().cloned().unwrap_or_default();
            self.toast(format!("{ok} moved, {} failed. {first}", failures.len()), true);
        }
    }

    fn start_diff(&mut self, ctx: &egui::Context) {
        let (Some(tree), Some(meta)) = (self.tree.clone(), self.snapshots.get(self.compare_to).cloned()) else {
            return;
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = history::load(&meta).map(|snap| history::compute(&tree, &snap));
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        self.diff_job = Some(rx);
    }

    fn start_dupes(&mut self, ctx: &egui::Context) {
        let Some(tree) = self.tree.clone() else { return };
        let p = Arc::new(DupProgress::default());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (p2, ctx, root, min) = (p.clone(), ctx.clone(), self.view_root, self.dupes.min_size);
        std::thread::spawn(move || {
            let groups = crate::dupes::find(&tree, root, min, &p2);
            let _ = tx.send(groups);
            ctx.request_repaint();
        });
        self.dupes.scope = self.view_root;
        self.dupes.running = Some((p, rx, Instant::now()));
    }

    // ------------------------------------------------------------------
    // Actions

    fn apply_actions(&mut self, ctx: &egui::Context) {
        for action in std::mem::take(&mut self.actions) {
            let Some(tree) = self.tree.clone() else {
                if let Action::ScanPath(p) = action {
                    self.start_scan(&p, ctx);
                }
                continue;
            };
            match action {
                Action::Select(id) => self.selected = Some(id),
                Action::Reveal(id) => {
                    self.selected = Some(id);
                    if id != ROOT {
                        self.expand_to(tree.node(id).parent);
                    }
                    self.scroll_to_selected = true;
                }
                Action::Zoom(id) => {
                    let target = if tree.node(id).is_dir() { id } else { tree.node(id).parent };
                    self.view_root = target;
                    self.expand_to(target);
                }
                Action::ZoomOut => {
                    if self.view_root != ROOT {
                        let child = self.view_root;
                        self.view_root = tree.node(child).parent;
                        self.selected = Some(child);
                    }
                }
                Action::ToggleExpand(id) => {
                    if !self.expanded.remove(&id) {
                        self.expanded.insert(id);
                    }
                    self.rows_dirty = true;
                }
                Action::Open(id) => platform::open_path(&tree.path(id)),
                Action::ShowInFileManager(id) => platform::reveal(&tree.path(id)),
                Action::CopyPath(id) => ctx.copy_text(tree.path(id)),
                Action::Delete(ids) => {
                    let ids: Vec<NodeId> =
                        ids.into_iter().filter(|&id| id != ROOT && !tree.is_deleted(id) && deletable(&tree, id)).collect();
                    if !ids.is_empty() {
                        self.confirm_delete = Some(ids);
                    }
                }
                Action::ScanPath(p) => self.start_scan(&p, ctx),
                Action::Rescan => {
                    if self.view_root != ROOT {
                        self.restore_view = Some(tree.path(self.view_root));
                    }
                    self.start_scan(&tree.root_path.clone(), ctx);
                }
                Action::SetHighlight(h) => {
                    self.highlight = if self.highlight == h { Highlight::None } else { h };
                }
            }
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() || self.confirm_delete.is_some() {
            return;
        }
        let Some(tree) = self.tree.clone() else { return };
        let (up, down, left, right, enter, back, del, esc) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::Enter),
                (i.key_pressed(Key::Backspace) && !i.modifiers.command)
                    || i.pointer.button_pressed(egui::PointerButton::Extra1),
                // ⌘⌫ is "move to trash" on macOS.
                i.key_pressed(Key::Delete) || (i.key_pressed(Key::Backspace) && i.modifiers.mac_cmd),
                i.key_pressed(Key::Escape),
            )
        });
        if back {
            self.actions.push(Action::ZoomOut);
        }
        if esc {
            self.highlight = Highlight::None;
        }
        let Some(sel) = self.selected else {
            if (up || down) && !self.rows.is_empty() {
                self.actions.push(Action::Reveal(self.rows[0].0));
            }
            return;
        };
        if (up || down)
            && let Some(i) = self.rows.iter().position(|r| r.0 == sel) {
                let j = if up { i.saturating_sub(1) } else { (i + 1).min(self.rows.len() - 1) };
                self.actions.push(Action::Reveal(self.rows[j].0));
            }
        if right && tree.node(sel).is_dir() && !self.expanded.contains(&sel) {
            self.actions.push(Action::ToggleExpand(sel));
        }
        if left {
            if self.expanded.contains(&sel) && sel != ROOT {
                self.actions.push(Action::ToggleExpand(sel));
            } else if sel != ROOT {
                self.actions.push(Action::Reveal(tree.node(sel).parent));
            }
        }
        if enter {
            self.actions.push(Action::Zoom(sel));
        }
        if del {
            self.actions.push(Action::Delete(vec![sel]));
        }
    }

    // ------------------------------------------------------------------
    // Shared widgets

    /// Right-click menu for any item.
    fn node_menu(&mut self, ui: &mut egui::Ui, tree: &Tree, id: NodeId) {
        let n = tree.node(id);
        ui.label(RichText::new(tree.name(id)).strong());
        ui.label(RichText::new(format!("{} · {} files", fmt_size(n.size), fmt_count(n.files as u64))).weak());
        ui.separator();
        if n.is_dir() && ui.button("Zoom into folder").clicked() {
            self.actions.push(Action::Zoom(id));
            ui.close();
        }
        if ui.button(format!("Show in {}", platform::FILE_MANAGER)).clicked() {
            self.actions.push(Action::ShowInFileManager(id));
            ui.close();
        }
        if ui.button("↗  Open").clicked() {
            self.actions.push(Action::Open(id));
            ui.close();
        }
        if ui.button("Copy path").clicked() {
            self.actions.push(Action::CopyPath(id));
            ui.close();
        }
        if !n.is_dir() {
            let e = n.ext;
            if ui.button(format!("Highlight .{} files", tree.ext_name(id))).clicked() {
                self.actions.push(Action::SetHighlight(Highlight::Ext(e)));
                ui.close();
            }
        }
        ui.separator();
        let del = ui
            .add_enabled(id != ROOT && deletable(tree, id), egui::Button::new(format!("Move to {}…", platform::TRASH)))
            .on_disabled_hover_text("Not scanned here, so Heft can't tell what deleting it would remove");
        if del.clicked() {
            self.actions.push(Action::Delete(vec![id]));
            ui.close();
        }
    }

    /// Relaunch as administrator, coming back to the same scan and page.
    fn restart_elevated(&mut self, ctx: &egui::Context) {
        let mut args = format!("--open={}", self.workspace.key());
        if let Some(t) = &self.tree {
            // No trailing backslash inside the quotes: `"C:\"` would escape the quote.
            args.push_str(&format!(" \"{}\"", t.root_path.trim_end_matches('\\')));
        }
        if platform::relaunch_elevated(&args) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else {
            self.toast("Elevation was cancelled", true);
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Heft").strong().size(17.0));
            ui.add_space(8.0);

            #[cfg(windows)]
            {
                for ws in Workspace::ALL {
                    let text = if self.workspace == ws { RichText::new(ws.label()).strong() } else { RichText::new(ws.label()) };
                    if ui.add(egui::Button::new(text).selected(self.workspace == ws)).clicked() {
                        self.workspace = ws;
                    }
                }
                ui.separator();
            }
            if self.workspace == Workspace::Disk {
                self.disk_toolbar(ui);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !platform::CAN_ELEVATE {
                    return;
                }
                let why = if self.workspace == Workspace::Disk {
                    "Administrator rights let Heft read the NTFS master file table directly, which is much faster"
                } else {
                    "Administrator rights let Heft clean system locations and change settings for all users"
                };
                if self.elevated {
                    ui.label(RichText::new("Administrator").color(Color32::from_rgb(110, 200, 120)))
                        .on_hover_text("Fast MFT scanning and system-wide cleaning are available");
                } else if ui.button("Restart as administrator").on_hover_text(why).clicked() {
                    let ctx = ui.ctx().clone();
                    self.restart_elevated(&ctx);
                }
                if self.workspace == Workspace::Disk {
                    ui.checkbox(&mut self.use_mft, "Fast scan")
                        .on_hover_text("Read the NTFS master file table directly when possible (needs administrator)");
                }
            });
        });
    }

    /// Scan buttons and treemap coloring.
    fn disk_toolbar(&mut self, ui: &mut egui::Ui) {
        let scanning = self.scan.is_some();
        ui.add_enabled_ui(!scanning, |ui| {
            ui.menu_button("Scan drive", |ui| {
                for d in self.drives.clone() {
                    let label = if d.label.is_empty() { d.kind.to_string() } else { d.label.clone() };
                    let text = format!("{}  {}  ({} free of {})", d.root, label, fmt_size(d.free), fmt_size(d.total));
                    if ui.button(text).clicked() {
                        self.actions.push(Action::ScanPath(d.root.clone()));
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button("⟳  Refresh drive list").clicked() {
                    self.refresh_drives();
                }
            });
            if ui.button("Scan folder…").clicked()
                && let Some(p) = rfd::FileDialog::new().set_title("Choose a folder to scan").pick_folder() {
                    self.actions.push(Action::ScanPath(p.to_string_lossy().into_owned()));
                }
            if self.tree.is_some() && ui.button("⟳  Rescan").on_hover_text("Scan the same location again").clicked() {
                self.actions.push(Action::Rescan);
            }
        });

        if self.tree.is_some() {
            ui.separator();
            ui.label("Color by");
            let before = self.color_mode;
            egui::ComboBox::from_id_salt("color_mode").selected_text(self.color_mode.label()).show_ui(ui, |ui| {
                for m in [ColorMode::Extension, ColorMode::Category, ColorMode::Age] {
                    ui.selectable_value(&mut self.color_mode, m, m.label());
                }
                let growth = ui.add_enabled(
                    self.diff.is_some(),
                    egui::Button::new(ColorMode::Growth.label()).selected(self.color_mode == ColorMode::Growth),
                );
                if growth.clicked() {
                    self.color_mode = ColorMode::Growth;
                }
                if self.diff.is_none() {
                    growth.on_hover_text("Compare with an earlier scan in the Changes tab first");
                }
            });
            if before != self.color_mode {
                ui.ctx().request_repaint();
            }
            if self.highlight != Highlight::None {
                let label = match (self.highlight, &self.tree) {
                    (Highlight::Ext(e), Some(t)) => format!("✖  .{}", t.exts[e as usize].name),
                    (Highlight::Category(c), _) => format!("✖  {}", c.label()),
                    _ => "✖  highlight".into(),
                };
                if ui.button(label).on_hover_text("Clear highlight (Esc)").clicked() {
                    self.highlight = Highlight::None;
                }
            }
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.workspace != Workspace::Disk {
                // The tool pages say what they're doing themselves; only toasts show here.
            } else if let Some(tree) = &self.tree {
                let r = tree.node(ROOT);
                let mode = match tree.info.mode {
                    crate::tree::ScanMode::Mft => RichText::new("MFT scan").color(Color32::from_rgb(250, 200, 70)),
                    crate::tree::ScanMode::Walk => RichText::new("standard scan"),
                };
                ui.label(mode);
                let phases: Vec<String> =
                    tree.info.phases.iter().map(|(n, ms)| format!("{}: {}", n.trim(), fmt_duration_ms(*ms))).collect();
                let scanned_in = if self.demo {
                    String::new()
                } else {
                    format!(" · scanned in {}", fmt_duration_ms(tree.info.duration_ms))
                };
                let summary = ui.label(format!(
                    "{} · {} · {} files{scanned_in}",
                    tree.root_path,
                    fmt_size(r.size),
                    fmt_count(r.files as u64),
                ));
                if !phases.is_empty() {
                    summary.on_hover_text(phases.join("\n"));
                }
                if !self.demo
                    && let Some((total, free)) = platform::free_space(&tree.root_path)
                {
                    ui.label(RichText::new(format!("· {} free of {}", fmt_size(free), fmt_size(total))).weak());
                }
                if tree.info.unreadable_dirs > 0 {
                    ui.label(RichText::new(format!("· {} folders unreadable", fmt_count(tree.info.unreadable_dirs))).weak())
                        .on_hover_text(UNREADABLE_HINT);
                }
                if let Some(note) = &tree.info.note {
                    ui.label(RichText::new(format!("· {note}")).weak());
                }
            } else {
                ui.label(RichText::new("No scan loaded").weak());
            }
            if let Some((msg, at, err)) = &self.toast
                && at.elapsed() < Duration::from_secs(8) {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let color = if *err { ui.visuals().error_fg_color } else { Color32::from_rgb(110, 200, 120) };
                        ui.label(RichText::new(msg).color(color));
                    });
                    ui.ctx().request_repaint_after(Duration::from_secs(1));
                }
        });
    }

    fn confirm_delete_modal(&mut self, ctx: &egui::Context) {
        let Some(ids) = self.confirm_delete.clone() else { return };
        let Some(tree) = self.tree.clone() else { return };
        let total: u64 = ids.iter().map(|&i| tree.node(i).size).sum();
        let files: u64 = ids.iter().map(|&i| tree.node(i).files as u64).sum();
        let protected: Vec<String> =
            ids.iter().map(|&i| tree.path(i)).filter(|p| platform::is_protected_path(p)).collect();
        let network = platform::is_network_path(&tree.root_path);
        let mut close = false;
        let mut go = false;
        let resp = egui::Modal::new(egui::Id::new("confirm_delete")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading(format!("Move to {}?", platform::TRASH));
            ui.add_space(4.0);
            ui.label(format!(
                "{} item(s) · {} · {} file(s)",
                ids.len(),
                fmt_size(total),
                fmt_count(files)
            ));
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                for &i in ids.iter().take(200) {
                    ui.add(egui::Label::new(RichText::new(tree.path(i)).monospace().size(11.5)).truncate());
                }
                if ids.len() > 200 {
                    ui.label(format!("…and {} more", ids.len() - 200));
                }
            });
            if !protected.is_empty() {
                ui.add_space(6.0);
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "⚠ This includes system locations. Deleting them can break Windows or installed programs.",
                );
            }
            if network {
                ui.add_space(6.0);
                ui.colored_label(ui.visuals().error_fg_color, "Network locations have no Recycle Bin, so Heft won't delete here.");
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let label = if protected.is_empty() { format!("Move to {}", platform::TRASH) } else { "Move anyway".into() };
                if ui.add_enabled(!network, egui::Button::new(RichText::new(label).strong())).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if resp.should_close() {
            close = true;
        }
        if go {
            self.confirm_delete = None;
            self.start_delete(ids, ctx);
        } else if close {
            self.confirm_delete = None;
        }
    }
}

impl eframe::App for HeftApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        self.list_hover = None;
        if self.workspace == Workspace::Disk {
            self.handle_keys(&ctx);
        }

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(3.0);
            self.toolbar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));

        #[cfg(windows)]
        if self.workspace != Workspace::Disk {
            self.tools.show(ui, self.workspace);
            for e in self.tools.take_events() {
                match e {
                    tools::Event::Toast(msg, err) => self.toast(msg, err),
                    tools::Event::Scan(path) => self.actions.push(Action::ScanPath(path)),
                    tools::Event::Elevate => self.restart_elevated(&ctx),
                }
            }
            self.apply_actions(&ctx);
            return;
        }

        if self.scan.is_some() {
            egui::CentralPanel::default().show(ui, |ui| self.scanning_screen(ui));
        } else if self.tree.is_some() {
            egui::Panel::left("side").default_size(560.0).min_size(320.0).show(ui, |ui| self.side_panel(ui));
            egui::CentralPanel::default().show(ui, |ui| self.treemap_panel(ui));
        } else {
            egui::CentralPanel::default().show(ui, |ui| self.start_screen(ui));
        }

        self.confirm_delete_modal(&ctx);
        if self.delete_job.is_some() {
            egui::Modal::new(egui::Id::new("deleting")).show(&ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("Moving to the {}…", platform::TRASH));
                });
            });
        }
        self.apply_actions(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let mode = match self.color_mode {
            ColorMode::Category => "category",
            ColorMode::Age => "age",
            _ => "type",
        };
        storage.set_string("color_mode", mode.into());
        storage.set_string("use_mft", if self.use_mft { "1" } else { "0" }.into());
    }
}
