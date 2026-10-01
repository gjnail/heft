//! Heft's menus at the top of the screen (macOS): Heft (About, Settings…,
//! Hide, Quit), File, View, Window and Help, with the usual shortcuts, so
//! Heft works like other Mac apps and not only through its toolbar.
//!
//! Standard items (About, Hide, Quit, Minimize, Close, Full Screen) go to
//! AppKit itself. Items that act on Heft are queued as [`Command`]s, which
//! the app takes on its next frame, the way the menu bar icon talks to it.

use std::cell::OnceCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSMenu, NSMenuDelegate, NSMenuItem};
use objc2_foundation::NSString;

/// Something a menu item asks Heft to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Settings,
    ScanFolder,
    Rescan,
    /// Show the page at this index in the toolbar.
    Page(usize),
    ToggleDarkMode,
    Help,
    ReportProblem,
}

static QUEUE: Mutex<Vec<Command>> = Mutex::new(Vec::new());
static REPAINT: OnceLock<eframe::egui::Context> = OnceLock::new();
/// What the View menu ticks: the page shown and whether dark mode is on.
static PAGE: AtomicUsize = AtomicUsize::new(0);
static DARK: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The menus' target and delegate, kept alive here: menus don't retain
    /// their targets. Main thread only.
    static TARGET: OnceCell<Retained<Target>> = const { OnceCell::new() };
}

/// Tags that tell the View menu's items apart.
const DARK_TAG: isize = 1000;

/// `NSEventModifierFlag…`, for shortcuts with more than ⌘.
const OPTION: usize = 1 << 19;
const COMMAND: usize = 1 << 20;

fn push(c: Command) {
    QUEUE.lock().unwrap().push(c);
    if let Some(ctx) = REPAINT.get() {
        ctx.request_repaint();
    }
}

/// The commands chosen since the last call.
pub fn take() -> Vec<Command> {
    std::mem::take(&mut *QUEUE.lock().unwrap())
}

/// Tell the menus which page is shown and whether dark mode is on, so the
/// View menu ticks the right items.
pub fn set_state(page: usize, dark: bool) {
    PAGE.store(page, Ordering::Relaxed);
    DARK.store(dark, Ordering::Relaxed);
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Target doesn't
    // implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeftAppMenuTarget"]
    struct Target;

    impl Target {
        #[unsafe(method(showSettings:))]
        fn show_settings(&self, _sender: Option<&AnyObject>) {
            push(Command::Settings);
        }

        #[unsafe(method(scanFolder:))]
        fn scan_folder(&self, _sender: Option<&AnyObject>) {
            push(Command::ScanFolder);
        }

        #[unsafe(method(rescan:))]
        fn rescan(&self, _sender: Option<&AnyObject>) {
            push(Command::Rescan);
        }

        #[unsafe(method(showPage:))]
        fn show_page(&self, sender: Option<&NSMenuItem>) {
            if let Some(item) = sender {
                push(Command::Page(item.tag() as usize));
            }
        }

        #[unsafe(method(toggleDarkMode:))]
        fn toggle_dark_mode(&self, _sender: Option<&AnyObject>) {
            push(Command::ToggleDarkMode);
        }

        #[unsafe(method(showHelp:))]
        fn show_help(&self, _sender: Option<&AnyObject>) {
            push(Command::Help);
        }

        #[unsafe(method(reportProblem:))]
        fn report_problem(&self, _sender: Option<&AnyObject>) {
            push(Command::ReportProblem);
        }
    }

    unsafe impl NSObjectProtocol for Target {}

    unsafe impl NSMenuDelegate for Target {
        /// The View menu is about to open: tick the page shown and dark mode.
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            let page = PAGE.load(Ordering::Relaxed) as isize;
            let dark = DARK.load(Ordering::Relaxed);
            for item in menu.itemArray().to_vec() {
                let on = if item.tag() == DARK_TAG { dark } else { item.action() == Some(sel!(showPage:)) && item.tag() == page };
                let state: isize = if on { 1 } else { 0 };
                let _: () = unsafe { msg_send![&item, setState: state] };
            }
        }
    }
);

fn target(mtm: MainThreadMarker) -> Retained<Target> {
    TARGET.with(|t| t.get_or_init(|| unsafe { msg_send![super(Target::alloc(mtm).set_ivars(())), init] }).clone())
}

/// A menu item. With `ours`, its action goes to Heft's target; otherwise to
/// the first responder (the window, or AppKit itself).
fn item(mtm: MainThreadMarker, title: &str, action: Option<Sel>, key: &str, ours: bool) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str(title), action, &NSString::from_str(key))
    };
    if ours {
        let t = target(mtm);
        unsafe { item.setTarget(Some(&*t)) };
    }
    item
}

fn with_modifiers(item: &NSMenuItem, mask: usize) {
    let _: () = unsafe { msg_send![item, setKeyEquivalentModifierMask: mask] };
}

fn submenu(mtm: MainThreadMarker, bar: &NSMenu, title: &str, items: Vec<Option<Retained<NSMenuItem>>>) -> Retained<NSMenu> {
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
    for i in items {
        match i {
            Some(i) => menu.addItem(&i),
            None => menu.addItem(&NSMenuItem::separatorItem(mtm)),
        }
    }
    let holder = item(mtm, title, None, "", false);
    holder.setSubmenu(Some(&menu));
    bar.addItem(&holder);
    menu
}

/// Put Heft's menus in the menu bar, replacing the minimal one it starts
/// with. `pages` are the toolbar's pages, in order. Main thread only; does
/// nothing after the first time.
pub fn install(ctx: &eframe::egui::Context, pages: &[&str]) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    if REPAINT.set(ctx.clone()).is_err() {
        return;
    }
    let app = NSApplication::sharedApplication(mtm);
    let bar = NSMenu::new(mtm);

    let services = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Services"));
    let services_item = item(mtm, "Services", None, "", false);
    services_item.setSubmenu(Some(&services));
    let hide_others = item(mtm, "Hide Others", Some(sel!(hideOtherApplications:)), "h", false);
    with_modifiers(&hide_others, COMMAND | OPTION);
    submenu(
        mtm,
        &bar,
        "Heft",
        vec![
            Some(item(mtm, "About Heft", Some(sel!(orderFrontStandardAboutPanel:)), "", false)),
            None,
            Some(item(mtm, "Settings…", Some(sel!(showSettings:)), ",", true)),
            None,
            Some(services_item),
            None,
            Some(item(mtm, "Hide Heft", Some(sel!(hide:)), "h", false)),
            Some(hide_others),
            Some(item(mtm, "Show All", Some(sel!(unhideAllApplications:)), "", false)),
            None,
            Some(item(mtm, "Quit Heft", Some(sel!(terminate:)), "q", false)),
        ],
    );
    app.setServicesMenu(Some(&services));

    submenu(
        mtm,
        &bar,
        "File",
        vec![
            Some(item(mtm, "Scan Folder…", Some(sel!(scanFolder:)), "o", true)),
            Some(item(mtm, "Rescan", Some(sel!(rescan:)), "r", true)),
            None,
            Some(item(mtm, "Close Window", Some(sel!(performClose:)), "w", false)),
        ],
    );

    let mut view: Vec<Option<Retained<NSMenuItem>>> = Vec::new();
    for (i, name) in pages.iter().enumerate() {
        let key = if i < 9 { (i + 1).to_string() } else { String::new() };
        let it = item(mtm, name, Some(sel!(showPage:)), &key, true);
        it.setTag(i as isize);
        view.push(Some(it));
    }
    view.push(None);
    let dark = item(mtm, "Dark Mode", Some(sel!(toggleDarkMode:)), "", true);
    dark.setTag(DARK_TAG);
    view.push(Some(dark));
    // AppKit adds Enter Full Screen (⌃⌘F) to a menu called View by itself.
    let view = submenu(mtm, &bar, "View", view);
    let t = target(mtm);
    view.setDelegate(Some(ProtocolObject::from_ref(&*t)));

    let window = submenu(
        mtm,
        &bar,
        "Window",
        vec![
            Some(item(mtm, "Minimize", Some(sel!(performMiniaturize:)), "m", false)),
            Some(item(mtm, "Zoom", Some(sel!(performZoom:)), "", false)),
            None,
            Some(item(mtm, "Bring All to Front", Some(sel!(arrangeInFront:)), "", false)),
        ],
    );
    app.setWindowsMenu(Some(&window));

    let help = submenu(
        mtm,
        &bar,
        "Help",
        vec![
            Some(item(mtm, "Heft Guides", Some(sel!(showHelp:)), "?", true)),
            Some(item(mtm, "Report a Problem…", Some(sel!(reportProblem:)), "", true)),
        ],
    );
    app.setHelpMenu(Some(&help));

    app.setMainMenu(Some(&bar));
    #[cfg(debug_assertions)]
    if std::env::var_os("HEFT_DEBUG_MENU").is_some() {
        let _ = std::fs::write(std::env::temp_dir().join("heft-menu.txt"), describe(&bar));
    }
}

/// Debug builds: the menus as text, one item per line, with shortcuts and
/// whether AppKit would enable each.
#[cfg(debug_assertions)]
fn describe(bar: &NSMenu) -> String {
    let mut out = String::new();
    for top in bar.itemArray().to_vec() {
        out.push_str(&format!("{}\n", top.title()));
        let Some(menu) = top.submenu() else { continue };
        menu.update();
        for i in menu.itemArray().to_vec() {
            if i.isSeparatorItem() {
                out.push_str("    ---\n");
                continue;
            }
            let key = i.keyEquivalent().to_string();
            let enabled = if i.isEnabled() { "" } else { "  (disabled)" };
            out.push_str(&format!("    {}{}{enabled}\n", i.title(), if key.is_empty() { String::new() } else { format!("  ⌘{key}") }));
        }
    }
    out
}
