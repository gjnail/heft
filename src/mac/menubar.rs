//! The menu bar icon (macOS). With it, Heft can keep watching free space
//! after its window is closed and start at login, hidden there until a drive
//! gets full.
//!
//! AppKit only works on the main thread, which is where eframe runs the app,
//! so the icon is made and changed from there. eframe stops running the app
//! while the window is hidden, so the menu brings the window back itself and
//! then tells the app, like the notification-area icon on Windows.

use std::cell::{OnceCell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBitmapImageRep, NSDeviceRGBColorSpace, NSImage, NSMenu, NSMenuDelegate,
    NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSWindow,
};
use objc2_foundation::{NSAppleEventManager, NSSize, NSString};

use super::{Domain, parse_launch_job};
use crate::monitor;
use crate::platform::{self, DriveInfo};
use crate::util::fmt_size;

/// The icon is in the menu bar.
static RUNNING: AtomicBool = AtomicBool::new(false);
/// The menu brought the window back; the app syncs its own state.
static SHOWN: AtomicBool = AtomicBool::new(false);
/// What the last free space check looked at, for the menu's drive lines.
static WATCHED: Mutex<Watched> = Mutex::new(Watched::NotYet);

thread_local! {
    /// The status item; it keeps its menu alive. Main thread only.
    static ITEM: RefCell<Option<Retained<NSStatusItem>>> = const { RefCell::new(None) };
    /// The menu's target and delegate. Made once and kept, because menus
    /// and the Apple event manager don't keep it alive themselves.
    static TARGET: OnceCell<Retained<Target>> = const { OnceCell::new() };
}

#[derive(Clone)]
enum Watched {
    /// No check yet.
    NotYet,
    /// Alerts are off, so nothing is checked.
    Off,
    /// (root, name) of each drive checked, and the free space limit.
    Drives(Vec<(String, String)>, u64),
}

pub fn running() -> bool {
    RUNNING.load(Ordering::Acquire)
}

pub fn take_shown() -> bool {
    SHOWN.swap(false, Ordering::AcqRel)
}

/// Show the icon (does nothing if it's already there). Main thread only.
pub fn start() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    if running() {
        return;
    }
    let target = target(mtm);
    let menu = NSMenu::new(mtm);
    menu.setDelegate(Some(ProtocolObject::from_ref(&*target)));
    fill_menu(&menu, &target);
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(mtm) {
        button.setImage(Some(&icon()));
        button.setToolTip(Some(&NSString::from_str("Heft: watching free space")));
    }
    item.setMenu(Some(&menu));
    ITEM.with_borrow_mut(|i| *i = Some(item));
    RUNNING.store(true, Ordering::Release);
}

/// Remove the icon. Main thread only.
pub fn stop() {
    if MainThreadMarker::new().is_none() {
        return;
    }
    if let Some(item) = ITEM.with_borrow_mut(Option::take) {
        NSStatusBar::systemStatusBar().removeStatusItem(&item);
    }
    RUNNING.store(false, Ordering::Release);
}

/// Called after each free space check with the drives it looked at and the
/// limit, or `None` when alerts are off.
pub fn set_watched(drives: Option<(&[DriveInfo], u64)>) {
    *WATCHED.lock().unwrap() = match drives {
        Some((drives, limit)) => {
            Watched::Drives(drives.iter().map(|d| (d.root.clone(), monitor::display_name(d))).collect(), limit)
        }
        None => Watched::Off,
    };
}

// ----------------------------------------------------------------------
// The window and the Dock

/// Heft's main window. The status item and menus have windows of their own,
/// so it's picked by its title, like on Windows.
fn main_windows(mtm: MainThreadMarker) -> Vec<Retained<NSWindow>> {
    NSApplication::sharedApplication(mtm).windows().to_vec().into_iter().filter(|w| w.title().to_string() == "Heft").collect()
}

/// Bring the window back, with Heft back in the Dock and the app switcher.
pub fn show_window() {
    if let Some(mtm) = MainThreadMarker::new() {
        show(mtm);
    }
}

fn show(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.unhide(None);
    for w in main_windows(mtm) {
        if w.isMiniaturized() {
            w.deminiaturize(None);
        }
        w.makeKeyAndOrderFront(None);
    }
    #[allow(deprecated)] // its replacement, `activate`, needs macOS 14
    app.activateIgnoringOtherApps(true);
    SHOWN.store(true, Ordering::Release);
}

/// Hide the window and take Heft out of the Dock and the app switcher, so
/// only the menu bar icon is left (the taskbar button going away, on
/// Windows). Main thread only.
pub fn hide_window() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    for w in main_windows(mtm) {
        w.orderOut(None);
    }
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    // Hand the keyboard to the app behind instead of keeping it with no window.
    if app.isActive() {
        app.hide(None);
    }
}

/// Started at login (`--tray`): keep Heft out of the Dock and leave the
/// keyboard with whatever you're doing from the start. The window is hidden
/// after its first frame, once the icon is there.
pub fn launch_options(mut options: eframe::NativeOptions, tray: bool) -> eframe::NativeOptions {
    if tray {
        options.event_loop_builder = Some(Box::new(|b| {
            use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
            b.with_activation_policy(ActivationPolicy::Accessory).with_activate_ignoring_other_apps(false);
        }));
    }
    options
}

// ----------------------------------------------------------------------
// The menu

/// `kCoreEventClass` and `kAEReopenApplication`: Heft opened again from the
/// Dock, Finder or Spotlight while it's running.
const CORE_EVENT: u32 = u32::from_be_bytes(*b"aevt");
const REOPEN: u32 = u32::from_be_bytes(*b"rapp");

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Target doesn't
    // implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeftMenuBarTarget"]
    struct Target;

    impl Target {
        #[unsafe(method(openHeft:))]
        fn open_heft(&self, _sender: Option<&AnyObject>) {
            show(self.mtm());
        }

        #[unsafe(method(quitHeft:))]
        fn quit_heft(&self, _sender: Option<&AnyObject>) {
            // The same way out as Cmd-Q, which eframe saves the settings on.
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }

        /// Without this, opening Heft again while it's hidden in the menu
        /// bar would do nothing you could see.
        #[unsafe(method(reopen:withReplyEvent:))]
        fn reopen(&self, _event: Option<&AnyObject>, _reply: Option<&AnyObject>) {
            show(self.mtm());
        }
    }

    unsafe impl NSObjectProtocol for Target {}

    unsafe impl NSMenuDelegate for Target {
        /// The menu is about to open: refresh the free space lines.
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            fill_menu(menu, self);
        }
    }
);

fn target(mtm: MainThreadMarker) -> Retained<Target> {
    TARGET.with(|t| {
        t.get_or_init(|| {
            let target: Retained<Target> = unsafe { msg_send![super(Target::alloc(mtm).set_ivars(())), init] };
            let events = NSAppleEventManager::sharedAppleEventManager();
            // Raw, because the typed method needs the CoreServices bindings.
            let _: () = unsafe {
                msg_send![
                    &events,
                    setEventHandler: &*target,
                    andSelector: sel!(reopen:withReplyEvent:),
                    forEventClass: CORE_EVENT,
                    andEventID: REOPEN
                ]
            };
            target
        })
        .clone()
    })
}

/// Open Heft, a line per watched drive with its free space, and Quit Heft.
fn fill_menu(menu: &NSMenu, target: &Target) {
    let mtm = target.mtm();
    let add = |title: &str, action: Option<Sel>| {
        let title = NSString::from_str(title);
        let item =
            unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &title, action, &NSString::new()) };
        if action.is_some() {
            unsafe { item.setTarget(Some(target)) };
        }
        // Lines without an action are shown dimmed, as information.
        menu.addItem(&item);
    };
    menu.removeAllItems();
    add("Open Heft", Some(sel!(openHeft:)));
    let lines = drive_lines();
    if !lines.is_empty() {
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        for line in &lines {
            add(line, None);
        }
    }
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    add("Quit Heft", Some(sel!(quitHeft:)));
}

/// The watched drives' free space, read again now. Only local disks are
/// watched, so this can't hang on a network server.
fn drive_lines() -> Vec<String> {
    let watched = WATCHED.lock().unwrap().clone();
    match watched {
        Watched::NotYet => Vec::new(),
        Watched::Off => vec!["Free space alerts are off".into()],
        Watched::Drives(drives, limit) => drives
            .iter()
            .filter_map(|(root, name)| {
                let (total, free) = platform::free_space(root)?;
                (total > 0).then(|| drive_line(name, free, total, limit))
            })
            .collect(),
    }
}

/// "System: 65.3 GB free of 460 GB", or a warning when it's almost full.
fn drive_line(name: &str, free: u64, total: u64, limit: u64) -> String {
    if free < monitor::threshold(total, limit) {
        format!("⚠ {name} is almost full: {} free of {}", fmt_size(free), fmt_size(total))
    } else {
        format!("{name}: {} free of {}", fmt_size(free), fmt_size(total))
    }
}

// ----------------------------------------------------------------------
// The icon

/// Size of the menu bar image, in points.
const ICON_POINTS: usize = 18;

/// The glyph's blocks, (x0, y0, x1, y1) in points from the top left: a tall
/// block, one beside it and two small ones under that, like the app icon.
const BLOCKS: [(f32, f32, f32, f32); 4] =
    [(2.0, 2.0, 9.0, 16.0), (10.0, 2.0, 16.0, 10.0), (10.0, 11.0, 13.0, 16.0), (14.0, 11.0, 16.0, 16.0)];

/// The app icon's treemap in one color, as a template image so macOS tints
/// it to match the menu bar, drawn for 1x and 2x screens.
fn icon() -> Retained<NSImage> {
    let size = NSSize::new(ICON_POINTS as f64, ICON_POINTS as f64);
    let image = NSImage::initWithSize(NSImage::alloc(), size);
    for scale in [1, 2] {
        if let Some(rep) = bitmap(scale, size) {
            image.addRepresentation(&rep);
        }
    }
    image.setTemplate(true);
    image.setAccessibilityDescription(Some(&NSString::from_str("Heft")));
    image
}

fn bitmap(scale: usize, size: NSSize) -> Option<Retained<NSBitmapImageRep>> {
    let px = ICON_POINTS * scale;
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            px as isize,
            px as isize,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            (px * 4) as isize,
            32,
        )
    }?;
    let data = rep.bitmapData();
    if data.is_null() {
        return None;
    }
    // Black with the glyph's coverage as alpha (premultiplied, so black stays 0).
    let pixels = unsafe { std::slice::from_raw_parts_mut(data, px * px * 4) };
    for (p, a) in pixels.as_chunks_mut::<4>().0.iter_mut().zip(glyph(scale)) {
        *p = [0, 0, 0, a];
    }
    rep.setSize(size);
    Some(rep)
}

/// Coverage of each pixel at `scale` pixels per point, rows from the top,
/// with slightly rounded corners, smoothed by sampling each pixel 4×4 times.
fn glyph(scale: usize) -> Vec<u8> {
    const RADIUS: f32 = 1.0;
    let px = ICON_POINTS * scale;
    let inside = |x: f32, y: f32| {
        BLOCKS.iter().any(|&(x0, y0, x1, y1)| {
            if x < x0 || x >= x1 || y < y0 || y >= y1 {
                return false;
            }
            let (cx, cy) = (x.clamp(x0 + RADIUS, x1 - RADIUS), y.clamp(y0 + RADIUS, y1 - RADIUS));
            (x - cx).powi(2) + (y - cy).powi(2) <= RADIUS * RADIUS
        })
    };
    (0..px * px)
        .map(|i| {
            let (x, y) = ((i % px) as f32, (i / px) as f32);
            let hits = (0..16)
                .filter(|k| {
                    let (sx, sy) = ((k % 4) as f32 + 0.5, (k / 4) as f32 + 0.5);
                    inside((x + sx / 4.0) / scale as f32, (y + sy / 4.0) / scale as f32)
                })
                .count();
            (hits * 255 / 16) as u8
        })
        .collect()
}

// ----------------------------------------------------------------------
// Starting at login

/// The launch agent that starts Heft at login, as `<label>.plist` in
/// ~/Library/LaunchAgents.
pub const AGENT_LABEL: &str = "io.github.gjnail.heft";

fn agent_file(dir: &Path) -> PathBuf {
    dir.join(format!("{AGENT_LABEL}.plist"))
}

/// Heft is set to start, in the menu bar, when you log in.
pub fn starts_at_login() -> bool {
    Domain::UserAgent.folder().is_some_and(|dir| agent_starts_heft(&dir))
}

fn agent_starts_heft(dir: &Path) -> bool {
    parse_launch_job(&agent_file(dir), Domain::UserAgent)
        .is_some_and(|j| j.label == AGENT_LABEL && j.args.iter().any(|a| a == "--tray"))
}

/// A launch agent rather than `SMAppService`: it works for the app bundle
/// and a bare binary alike, can pass `--tray`, doesn't depend on how the app
/// is signed, and turning it off is just removing the file.
pub fn set_start_at_login(on: bool) -> Result<(), String> {
    let dir = Domain::UserAgent.folder().ok_or("your home folder wasn't found")?;
    if !on {
        return remove_agent(&dir);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // Apps opened straight from Downloads or a disk image run from a
    // temporary copy that's gone after a restart.
    if exe.to_string_lossy().contains("/AppTranslocation/") {
        return Err("Heft is running from a temporary copy. Move it to the Applications folder and open it from there first".into());
    }
    write_agent(&dir, &exe, super::notify::bundle_id().as_deref())
}

/// Heft (inside Heft.app when it's bundled) with `--tray`, started when you
/// log in to the desktop, and never restarted after it quits.
fn agent_plist(exe: &Path, bundle_id: Option<&str>) -> plist::Value {
    let mut d = plist::Dictionary::new();
    d.insert("Label".into(), AGENT_LABEL.into());
    d.insert("ProgramArguments".into(), vec![plist::Value::from(exe.to_string_lossy().into_owned()), "--tray".into()].into());
    d.insert("RunAtLoad".into(), true.into());
    // Not for ssh or other sessions without a menu bar.
    d.insert("LimitLoadToSessionType".into(), "Aqua".into());
    if let Some(id) = bundle_id {
        // System Settings › General › Login Items then shows it as Heft, with its icon.
        d.insert("AssociatedBundleIdentifiers".into(), vec![plist::Value::from(id)].into());
    }
    d.into()
}

/// After Heft.app was moved, point Start at login at its new place.
pub fn follow_move() {
    let Some(dir) = Domain::UserAgent.folder() else { return };
    let Some(job) = parse_launch_job(&agent_file(&dir), Domain::UserAgent) else { return };
    if agent_starts_heft(&dir)
        && let Some(exe) = super::moved_heft(&job)
    {
        let _ = write_agent(&dir, &exe, super::notify::bundle_id().as_deref());
    }
}

fn write_agent(dir: &Path, exe: &Path, bundle_id: Option<&str>) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    // Written beside it and renamed over it, so a link in its place is
    // replaced rather than written through, and launchd never sees half a file.
    let tmp = dir.join(format!(".{AGENT_LABEL}.plist.tmp"));
    let _ = std::fs::remove_file(&tmp);
    agent_plist(exe, bundle_id).to_file_xml(&tmp).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, agent_file(dir)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

fn remove_agent(dir: &Path) -> Result<(), String> {
    match std::fs::remove_file(agent_file(dir)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
        _ => Ok(()),
    }
}

// ----------------------------------------------------------------------
// Self-test

/// Debug builds: choose Open Heft in the menu, close the window, open it
/// again, close it again, open Heft again the way the Dock does, close it,
/// and choose Quit Heft, logging after each step whether the window is
/// visible and Heft is in the Dock to heft-tray-selftest.txt in the temp
/// folder.
#[cfg(debug_assertions)]
pub fn self_test() {
    use dispatch2::run_on_main;
    std::thread::spawn(|| {
        let pause = || std::thread::sleep(std::time::Duration::from_secs(2));
        let mut log = String::new();
        let mut note = |step: &str| {
            log.push_str(&format!("{step}: {}\n", run_on_main(describe)));
            let _ = std::fs::write(std::env::temp_dir().join("heft-tray-selftest.txt"), &log);
        };
        let choose = |title: &'static str| run_on_main(move |mtm| choose(mtm, title));
        let close = || run_on_main(|mtm| main_windows(mtm).iter().for_each(|w| w.performClose(None)));
        std::thread::sleep(std::time::Duration::from_secs(3));
        note("started with --tray");
        choose("Open Heft");
        pause();
        note("chose Open Heft");
        close();
        pause();
        note("closed the window");
        choose("Open Heft");
        pause();
        note("chose Open Heft again");
        close();
        pause();
        note("closed it again");
        run_on_main(|mtm| {
            let _: () = unsafe { msg_send![&*target(mtm), reopen: None::<&AnyObject>, withReplyEvent: None::<&AnyObject>] };
        });
        pause();
        note("opened Heft again as the Dock does");
        close();
        pause();
        note("closed it");
        note("quitting");
        choose("Quit Heft");
    });
}

/// Pick a menu item the way a click does, through AppKit's target/action.
#[cfg(debug_assertions)]
fn choose(mtm: MainThreadMarker, title: &str) {
    let Some(menu) = ITEM.with_borrow(|i| i.as_ref().and_then(|i| i.menu(mtm))) else { return };
    fill_menu(&menu, &target(mtm));
    let items = menu.itemArray().to_vec();
    if let Some(i) = items.iter().position(|it| it.title().to_string() == title) {
        menu.performActionForItemAtIndex(i as isize);
    }
}

#[cfg(debug_assertions)]
fn describe(mtm: MainThreadMarker) -> String {
    let app = NSApplication::sharedApplication(mtm);
    let visible = main_windows(mtm).iter().any(|w| w.isVisible());
    let dock = app.activationPolicy() == NSApplicationActivationPolicy::Regular;
    let (icon, menu) = ITEM.with_borrow(|i| {
        let Some(item) = i else { return ("none".to_string(), String::new()) };
        let at = item.button(mtm).and_then(|b| b.window()).map(|w| w.frame()).map_or("?".into(), |f| {
            format!("{}x{} at ({}, {})", f.size.width, f.size.height, f.origin.x, f.origin.y)
        });
        let menu = item.menu(mtm).map_or(String::new(), |m| {
            fill_menu(&m, &target(mtm));
            let titles: Vec<String> = m.itemArray().to_vec().iter().map(|it| it.title().to_string()).collect();
            titles.join(" | ")
        });
        (at, menu)
    });
    format!(
        "icon={icon} window visible={visible} in Dock={dock} active={} shown flag={} menu=[{menu}]",
        app.isActive(),
        SHOWN.load(Ordering::Acquire)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_agent_round_trip() {
        let dir = std::env::temp_dir().join(format!("heft-agent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!agent_starts_heft(&dir));
        remove_agent(&dir).unwrap(); // nothing to remove is fine

        let exe = Path::new("/Applications/Heft.app/Contents/MacOS/heft");
        write_agent(&dir, exe, Some("local.heft.Heft")).unwrap();
        assert!(agent_starts_heft(&dir));
        let job = parse_launch_job(&agent_file(&dir), Domain::UserAgent).unwrap();
        assert_eq!(job.label, AGENT_LABEL);
        assert_eq!(job.args, ["/Applications/Heft.app/Contents/MacOS/heft", "--tray"]);
        assert!(job.run_at_load && !job.keep_alive && !job.scheduled && !job.disabled_in_plist);
        assert_eq!(job.bundles, ["local.heft.Heft"]);
        let plist = super::super::read_plist(&agent_file(&dir)).unwrap();
        let dict = plist.as_dictionary().unwrap();
        assert_eq!(super::super::plist_str(dict, "LimitLoadToSessionType").as_deref(), Some("Aqua"));
        let text = std::fs::read_to_string(agent_file(&dir)).unwrap();
        assert!(text.starts_with("<?xml") && text.contains("<key>RunAtLoad</key>"), "{text}");

        // A link in its place is replaced, not written through.
        let elsewhere = dir.join("elsewhere.txt");
        std::fs::write(&elsewhere, "keep").unwrap();
        std::fs::remove_file(agent_file(&dir)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, agent_file(&dir)).unwrap();
        write_agent(&dir, Path::new("/Users/someone/bin/heft"), None).unwrap();
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "keep");
        assert!(!agent_file(&dir).is_symlink());
        let job = parse_launch_job(&agent_file(&dir), Domain::UserAgent).unwrap();
        assert_eq!(job.args, ["/Users/someone/bin/heft", "--tray"]);
        assert!(job.bundles.is_empty());
        assert!(!dir.join(format!(".{AGENT_LABEL}.plist.tmp")).exists());

        remove_agent(&dir).unwrap();
        assert!(!agent_file(&dir).exists() && elsewhere.exists());
        assert!(!agent_starts_heft(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Writes and removes the real launch agent (skipped if one is already
    /// there). macOS may show a "Background Items Added" notice.
    #[test]
    #[ignore]
    fn start_at_login_round_trip() {
        let file = agent_file(&Domain::UserAgent.folder().unwrap());
        if file.exists() {
            return;
        }
        set_start_at_login(true).unwrap();
        assert!(starts_at_login());
        set_start_at_login(false).unwrap();
        assert!(!starts_at_login() && !file.exists());
    }

    #[test]
    fn someone_elses_file_isnt_ours() {
        let dir = std::env::temp_dir().join(format!("heft-agent-other-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Right name, but no --tray: not what the setting writes.
        std::fs::write(
            agent_file(&dir),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>Label</key><string>{AGENT_LABEL}</string>
<key>ProgramArguments</key><array><string>/usr/local/bin/heft</string></array></dict></plist>"#
            ),
        )
        .unwrap();
        assert!(!agent_starts_heft(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The startup disk is a snapshot mount since macOS 11, and snapshots
    /// are otherwise left out of the drive list.
    #[test]
    fn the_startup_disk_is_watched() {
        assert!(platform::list_drives().iter().any(|d| d.root == "/" && d.kind == "Local disk"));
    }

    #[test]
    fn drive_lines_flag_almost_full_drives() {
        const GB: u64 = 1 << 30;
        assert_eq!(drive_line("System", 65 * GB, 460 * GB, 10 * GB), "System: 65.0 GB free of 460 GB");
        assert_eq!(
            drive_line("System", 4 * GB, 460 * GB, 10 * GB),
            "⚠ System is almost full: 4.00 GB free of 460 GB"
        );
        // Small drives use a tenth of their size, like the alerts.
        assert!(!drive_line("Stick", 4 * GB, 32 * GB, 10 * GB).contains("almost full"));
    }

    #[test]
    fn glyph_has_gaps_between_blocks() {
        for scale in [1, 2] {
            let g = glyph(scale);
            let px = ICON_POINTS * scale;
            let at = |x: f32, y: f32| g[(y * scale as f32) as usize * px + (x * scale as f32) as usize];
            assert_eq!(g.len(), px * px);
            assert_eq!(at(0.5, 0.5), 0, "margin");
            assert_eq!(at(5.0, 9.0), 255, "inside the tall block");
            assert_eq!(at(9.5, 5.0), 0, "gap between the tall block and the next");
            assert_eq!(at(13.0, 10.5), 0, "gap under the top right block");
            assert_eq!(at(13.5, 13.0), 0, "gap between the small blocks");
            assert_eq!(at(14.5, 13.0), 255, "inside the smallest block");
        }
    }
}
