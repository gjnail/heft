//! What starts when you sign in on macOS: login items ("Open at Login"
//! apps), and launch agents and daemons.
//!
//! Turning a launch agent or daemon off uses launchd's own persistent switch
//! (`launchctl disable`), so the .plist is never touched and the change can
//! be undone. Login items have no such switch, so turning one off takes it off
//! the list and Heft remembers it (path, name and whether it opens hidden) to
//! put it back exactly as it was.
//!
//! Login items are read with the old LSSharedFileList API, which still works
//! and needs no permission. System Events, which asks for Automation
//! permission the first time, is the fallback, and is only used when the user
//! asks for it or changes something.

use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use super::broken::{self, path_state, PathState, Target};
use super::{Domain, LaunchJob};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Source {
    LoginItem,
    Job(Domain),
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::LoginItem => "Login item",
            Source::Job(Domain::UserAgent) => "Launch agent (you)",
            Source::Job(Domain::Agent) => "Launch agent (all users)",
            Source::Job(Domain::Daemon) => "Launch daemon",
        }
    }

    /// Turning it on or off asks for the administrator password. Agents in
    /// /Library/LaunchAgents run in your own session, whose switches are
    /// yours to change; daemons run in the system's.
    pub fn toggle_needs_admin(self) -> bool {
        self == Source::Job(Domain::Daemon)
    }

    /// Removing it asks for the administrator password: the file is in /Library.
    pub fn remove_needs_admin(self) -> bool {
        matches!(self, Source::Job(Domain::Agent | Domain::Daemon))
    }
}

/// How a launch job gets started.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Starts {
    AtLogin,
    AtStartup,
    KeptRunning,
    Scheduled,
    OnDemand,
    WhenAsked,
}

impl Starts {
    pub fn of(job: &LaunchJob) -> Starts {
        if job.keep_alive {
            Starts::KeptRunning
        } else if job.run_at_load {
            if job.domain == Domain::Daemon { Starts::AtStartup } else { Starts::AtLogin }
        } else if job.scheduled {
            Starts::Scheduled
        } else if job.on_demand {
            Starts::OnDemand
        } else {
            Starts::WhenAsked
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Starts::AtLogin => "at login",
            Starts::AtStartup => "at startup",
            Starts::KeptRunning => "kept running",
            Starts::Scheduled => "on a schedule",
            Starts::OnDemand => "on demand",
            Starts::WhenAsked => "when an app asks",
        }
    }
}

/// An app on the login items list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginItem {
    pub name: String,
    /// Where the app is. Empty if macOS can't say.
    pub path: String,
    /// Opens with its windows hidden.
    pub hidden: bool,
}

impl LoginItem {
    fn same(&self, other: &LoginItem) -> bool {
        if self.path.is_empty() || other.path.is_empty() { self.name == other.name } else { self.path == other.path }
    }
}

#[derive(Clone, Debug)]
pub enum Item {
    Login(LoginItem),
    Job(LaunchJob),
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub item: Item,
    /// The login item's name, or the job's label.
    pub name: String,
    /// The app a job's program belongs to.
    pub app: Option<String>,
    pub enabled: bool,
    /// The app (login items) or the command line (jobs).
    pub command: String,
    /// The program or app that runs, if known.
    pub target: Option<String>,
    pub state: PathState,
    pub starts: Starts,
    /// Running right now (launch jobs only).
    pub running: bool,
}

impl Entry {
    pub fn source(&self) -> Source {
        match &self.item {
            Item::Login(_) => Source::LoginItem,
            Item::Job(j) => Source::Job(j.domain),
        }
    }

    /// The app's name when the program is inside one, else the item's name.
    pub fn title(&self) -> &str {
        self.app.as_deref().unwrap_or(&self.name)
    }

    /// What Show in Finder points at: the program, or a job's .plist when
    /// the program isn't there.
    pub fn reveal_path(&self) -> Option<String> {
        let target = self.target.clone().filter(|_| self.state == PathState::Exists);
        match &self.item {
            Item::Login(_) => target,
            Item::Job(j) => target.or_else(|| Some(j.plist.to_string_lossy().into_owned())),
        }
    }

    /// What Remove takes away.
    pub fn removal(&self) -> Target {
        match &self.item {
            Item::Login(l) => Target::LoginItem(l.clone(), self.enabled),
            Item::Job(j) => Target::Job(j.clone()),
        }
    }
}

/// Why login items couldn't be listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginError {
    /// Reading them needs System Events, which asks for permission first.
    NeedsPermission,
    /// macOS said no (Automation permission).
    NotAllowed,
    Failed(String),
}

pub struct Listing {
    pub entries: Vec<Entry>,
    pub login_error: Option<LoginError>,
    /// Background items inside app bundles, shown for reference.
    pub bundled: Vec<BundledItem>,
}

/// Everything that starts automatically, enabled first. System Events is
/// only asked when `allow_system_events` is set and the direct read isn't
/// available.
pub fn list(allow_system_events: bool) -> Listing {
    let user = format!("gui/{}", super::uid());
    let user_services = services(&user);
    let system_services = services("system");
    let user_off = super::disabled_overrides(&user);
    let system_off = super::disabled_overrides("system");

    let mut entries = Vec::new();
    let (live, login_error) = match login_items(allow_system_events) {
        Ok(v) => (v, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let mut add_login = |item: LoginItem, enabled: bool| {
        let state = if item.path.is_empty() { PathState::Unknown } else { path_state(&item.path) };
        let command = if item.hidden { format!("{}  (opens hidden)", item.path) } else { item.path.clone() };
        entries.push(Entry {
            name: item.name.clone(),
            app: None,
            enabled,
            command,
            target: (!item.path.is_empty()).then(|| item.path.clone()),
            state,
            starts: Starts::AtLogin,
            running: false,
            item: Item::Login(item),
        });
    };
    // Items turned off in Heft are only shown when the real list is known,
    // so one that was added back elsewhere doesn't show up twice.
    let off = if login_error.is_none() { turned_off() } else { Vec::new() };
    for item in off.into_iter().filter(|o| !live.iter().any(|l| l.same(o))) {
        add_login(item, false);
    }
    for item in live {
        add_login(item, true);
    }

    for job in super::launch_jobs() {
        let (overrides, running) =
            if job.domain == Domain::Daemon { (&system_off, &system_services) } else { (&user_off, &user_services) };
        let enabled = !overrides.get(&job.label).copied().unwrap_or(job.disabled_in_plist);
        let target = job_target(&job);
        let state = target.as_deref().map_or(PathState::Unknown, path_state);
        let app = job
            .program
            .as_deref()
            .and_then(|p| super::enclosing_app(Path::new(p)))
            .and_then(|a| super::bundle_info(&a))
            .map(|b| b.name);
        entries.push(Entry {
            name: job.label.clone(),
            app,
            enabled,
            command: job.command(),
            target,
            state,
            starts: Starts::of(&job),
            running: running.get(&job.label).is_some_and(|pid| pid.is_some()),
            item: Item::Job(job),
        });
    }
    entries.sort_by_key(|e| (!e.enabled, e.title().to_lowercase()));
    Listing { entries, login_error, bundled: bundled_items(&user_services, &system_services) }
}

/// Jobs loaded in a launchd domain (`gui/501`, `system`): label → pid when
/// it's running. Readable without an administrator password.
pub fn services(target: &str) -> HashMap<String, Option<u32>> {
    super::output("/bin/launchctl", &["print", target]).map(|t| parse_services(&t)).unwrap_or_default()
}

/// The `services = { … }` block of `launchctl print`: lines of pid, last
/// exit status and label, like `27319  -  com.example.agent`.
pub fn parse_services(text: &str) -> HashMap<String, Option<u32>> {
    let mut out = HashMap::new();
    let mut inside = false;
    for line in text.lines() {
        let t = line.trim();
        if !inside {
            inside = t == "services = {";
            continue;
        }
        if t == "}" {
            break;
        }
        let mut f = t.split_whitespace();
        let (Some(pid), Some(_status), Some(label)) = (f.next(), f.next(), f.next()) else { continue };
        let Ok(pid) = pid.parse::<u32>() else { continue };
        out.insert(label.to_string(), (pid > 0).then_some(pid));
    }
    out
}

/// Interpreters whose script, not the interpreter itself, is what a job runs.
const INTERPRETERS: [&str; 12] = ["sh", "bash", "zsh", "dash", "ksh", "csh", "tcsh", "perl", "ruby", "node", "osascript", "php"];

/// The file a job really runs: its program, or the script for `/bin/sh
/// script.sh` and the like. `None` when launchd would look the program up
/// on its search path, since then there's no telling which file it means.
pub fn job_target(job: &LaunchJob) -> Option<String> {
    let prog = job.program.as_deref()?;
    if !prog.starts_with('/') {
        return None;
    }
    let name = prog.rsplit('/').next().unwrap_or(prog);
    // ProgramArguments[0] is the program's own name; the rest are arguments.
    let args = job.args.get(1..).unwrap_or(&[]);
    if INTERPRETERS.contains(&name) || name.starts_with("python") {
        for a in args {
            if a == "-c" || a == "-e" {
                break; // An inline script.
            }
            if a.starts_with('-') {
                continue;
            }
            if a.starts_with('/') {
                return Some(a.clone());
            }
            break;
        }
    } else if name == "open"
        && let Some(a) = args.iter().find(|a| a.starts_with('/'))
    {
        return Some(a.clone());
    }
    Some(prog.to_string())
}

// ----------------------------------------------------------------------
// Turning things on and off, and removing them

pub fn set_enabled(e: &Entry, on: bool) -> Result<(), String> {
    match &e.item {
        Item::Login(item) if on => {
            add_login_item(&item.path, item.hidden)?;
            remember_off(item, false)
        }
        Item::Login(item) => {
            // Remember it first, so it can't be lost if the removal half-works.
            remember_off(item, true)?;
            remove_login_item(item).inspect_err(|_| {
                let _ = remember_off(item, false);
            })
        }
        Item::Job(job) => set_job_enabled(job, on),
    }
}

/// launchd's persistent on/off switch. It doesn't start or stop the job
/// now; it decides whether launchd loads it at the next login or startup.
pub fn set_job_enabled(job: &LaunchJob, on: bool) -> Result<(), String> {
    let spec = format!("{}/{}", job.domain.target(), job.label);
    let verb = if on { "enable" } else { "disable" };
    if job.domain == Domain::Daemon {
        let why = format!("turn {} {}", job.label, if on { "on" } else { "off" });
        super::run_as_admin(&format!("/bin/launchctl {verb} {}", super::sh_quote(&spec)), &why).map(|_| ())
    } else {
        launchctl(&[verb, &spec])
    }
}

pub(super) fn launchctl(args: &[&str]) -> Result<(), String> {
    let out = Command::new("/bin/launchctl").args(args).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(if out.stderr.is_empty() { &out.stdout } else { &out.stderr }).trim().to_string();
    Err(if text.is_empty() { "launchctl failed".into() } else { text })
}

/// Remove an entry for good, after saving a backup. Returns the backup.
pub fn remove(e: &Entry) -> Result<PathBuf, String> {
    let report = broken::remove(&[e.removal()], "login-items", &format!("remove {}", e.title()))?;
    if let Some(f) = report.failed.into_iter().next() {
        return Err(f);
    }
    report.backup.ok_or_else(|| "nothing was removed".into())
}

// ----------------------------------------------------------------------
// Login items

/// The login items list. Uses the direct read when it's available; System
/// Events only when `allow_system_events` is set.
pub fn login_items(allow_system_events: bool) -> Result<Vec<LoginItem>, LoginError> {
    if let Some(api) = lsfl() {
        return api.items().ok_or_else(|| LoginError::Failed("macOS didn't return the login items".into()));
    }
    if !allow_system_events {
        return Err(LoginError::NeedsPermission);
    }
    system_events_items().map_err(|e| if e == super::NOT_ALLOWED { LoginError::NotAllowed } else { LoginError::Failed(e) })
}

/// Add an app to the login items. The direct way is checked afterwards and
/// System Events used if it didn't take.
pub fn add_login_item(path: &str, hidden: bool) -> Result<(), String> {
    if path_state(path) != PathState::Exists {
        return Err("the app isn't there any more".into());
    }
    let wanted = LoginItem { name: String::new(), path: path.to_string(), hidden };
    if let Some(api) = lsfl() {
        if api.contains(&wanted) {
            return Ok(());
        }
        let _ = api.add(path, hidden);
        if api.contains(&wanted) {
            return Ok(());
        }
    }
    system_events_add(path, hidden)
}

/// Take an item off the login items list (the app itself isn't touched).
pub fn remove_login_item(item: &LoginItem) -> Result<(), String> {
    if let Some(api) = lsfl() {
        if !api.contains(item) {
            return Ok(());
        }
        let _ = api.remove(item);
        if !api.contains(item) {
            return Ok(());
        }
    }
    system_events_remove(item)
}

fn off_file() -> PathBuf {
    super::data_dir().join("login-items-off.plist")
}

/// Login items turned off in Heft, remembered so they can be turned back on.
pub fn turned_off() -> Vec<LoginItem> {
    read_items(&off_file())
}

/// Remember `item` as turned off, or forget it again.
pub fn remember_off(item: &LoginItem, off: bool) -> Result<(), String> {
    let file = off_file();
    let mut items = read_items(&file);
    items.retain(|i| !i.same(item));
    if off {
        items.push(item.clone());
    }
    write_items(&file, &items)
}

pub fn login_item_value(item: &LoginItem) -> plist::Value {
    let mut d = plist::Dictionary::new();
    d.insert("Name".into(), item.name.clone().into());
    d.insert("Path".into(), item.path.clone().into());
    d.insert("Hidden".into(), item.hidden.into());
    plist::Value::Dictionary(d)
}

pub fn login_item_from(v: &plist::Value) -> Option<LoginItem> {
    let d = v.as_dictionary()?;
    let get = |k: &str| d.get(k).and_then(|v| v.as_string()).unwrap_or("").to_string();
    let item = LoginItem { name: get("Name"), path: get("Path"), hidden: super::plist_bool(d, "Hidden").unwrap_or(false) };
    (!item.name.is_empty() || !item.path.is_empty()).then_some(item)
}

fn read_items(file: &Path) -> Vec<LoginItem> {
    super::read_plist(file)
        .and_then(|v| v.as_array().map(|a| a.iter().filter_map(login_item_from).collect()))
        .unwrap_or_default()
}

fn write_items(file: &Path, items: &[LoginItem]) -> Result<(), String> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    plist::Value::Array(items.iter().map(login_item_value).collect()).to_file_xml(file).map_err(|e| e.to_string())
}

// System Events: works everywhere, but asks for Automation permission once.

fn system_events_items() -> Result<Vec<LoginItem>, String> {
    let script = "set out to \"\"\n\
        tell application \"System Events\"\n\
        repeat with i in every login item\n\
        set p to \"\"\n\
        try\n\
        set p to path of i\n\
        end try\n\
        set out to out & (name of i) & tab & p & tab & ((hidden of i) as text) & linefeed\n\
        end repeat\n\
        end tell\n\
        return out";
    super::osascript(script).map(|t| parse_system_events(&t))
}

/// Lines of name, path and hidden, separated by tabs.
pub fn parse_system_events(text: &str) -> Vec<LoginItem> {
    text.split(['\n', '\r'])
        .filter_map(|l| {
            let mut f = l.split('\t');
            let name = f.next()?.trim().to_string();
            let path = f.next().unwrap_or("").trim();
            let path = if path == "missing value" { "" } else { path };
            let hidden = f.next().is_some_and(|h| h.trim() == "true");
            (!name.is_empty()).then(|| LoginItem { name, path: path.trim_end_matches('/').to_string(), hidden })
        })
        .collect()
}

fn system_events_add(path: &str, hidden: bool) -> Result<(), String> {
    super::osascript(&format!(
        "tell application \"System Events\" to make login item at end with properties {{path:{}, hidden:{hidden}}}",
        super::applescript_quote(path)
    ))
    .map(|_| ())
}

fn system_events_remove(item: &LoginItem) -> Result<(), String> {
    let (key, value) = if item.path.is_empty() { ("name", &item.name) } else { ("path", &item.path) };
    super::osascript(&format!(
        "tell application \"System Events\" to delete (every login item whose {key} is {})",
        super::applescript_quote(value)
    ))
    .map(|_| ())
}

// The LSSharedFileList API: deprecated since macOS 10.11, yet still how the
// login items list is read and written underneath. Looked up at run time so
// Heft keeps working, through System Events, if Apple ever removes it.

mod cf {
    use std::ffi::{c_char, c_void, CStr};

    pub type Ref = *const c_void;
    const UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub fn CFRelease(cf: Ref);
        fn CFGetTypeID(cf: Ref) -> usize;
        pub fn CFArrayGetCount(a: Ref) -> isize;
        pub fn CFArrayGetValueAtIndex(a: Ref, i: isize) -> Ref;
        fn CFArrayCreate(alloc: Ref, values: *const Ref, n: isize, callbacks: *const c_void) -> Ref;
        pub fn CFDictionaryCreate(alloc: Ref, keys: *const Ref, values: *const Ref, n: isize, kcb: *const c_void, vcb: *const c_void) -> Ref;
        fn CFDictionaryGetValue(d: Ref, key: Ref) -> Ref;
        fn CFStringGetLength(s: Ref) -> isize;
        fn CFStringGetMaximumSizeForEncoding(len: isize, enc: u32) -> isize;
        fn CFStringGetCString(s: Ref, buf: *mut c_char, size: isize, enc: u32) -> u8;
        fn CFStringGetTypeID() -> usize;
        fn CFBooleanGetValue(b: Ref) -> u8;
        fn CFBooleanGetTypeID() -> usize;
        fn CFDataGetTypeID() -> usize;
        fn CFURLGetFileSystemRepresentation(url: Ref, resolve: u8, buf: *mut u8, len: isize) -> u8;
        pub fn CFURLCreateFromFileSystemRepresentation(alloc: Ref, buf: *const u8, len: isize, is_dir: u8) -> Ref;
        fn CFURLCreateResourcePropertiesForKeysFromBookmarkData(alloc: Ref, keys: Ref, data: Ref) -> Ref;
        fn CFDataCreate(alloc: Ref, bytes: *const u8, len: isize) -> Ref;
        fn CFURLCreateByResolvingBookmarkData(alloc: Ref, data: Ref, options: usize, relative: Ref, keys: Ref, stale: *mut u8, error: *mut Ref) -> Ref;
        pub static kCFBooleanTrue: Ref;
        pub static kCFBooleanFalse: Ref;
        static kCFURLPathKey: Ref;
        static kCFTypeArrayCallBacks: [u8; 0];
        pub static kCFTypeDictionaryKeyCallBacks: [u8; 0];
        pub static kCFTypeDictionaryValueCallBacks: [u8; 0];
    }

    /// A reference Heft owns, released when dropped.
    pub struct Owned(pub Ref);

    impl Owned {
        pub fn new(r: Ref) -> Option<Owned> {
            (!r.is_null()).then_some(Owned(r))
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0) }
        }
    }

    pub fn string(s: Ref) -> Option<String> {
        unsafe {
            if s.is_null() || CFGetTypeID(s) != CFStringGetTypeID() {
                return None;
            }
            let size = CFStringGetMaximumSizeForEncoding(CFStringGetLength(s), UTF8) + 1;
            let mut buf = vec![0 as c_char; size.max(1) as usize];
            if CFStringGetCString(s, buf.as_mut_ptr(), size, UTF8) == 0 {
                return None;
            }
            Some(CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
        }
    }

    pub fn boolean(b: Ref) -> bool {
        unsafe { !b.is_null() && CFGetTypeID(b) == CFBooleanGetTypeID() && CFBooleanGetValue(b) != 0 }
    }

    pub fn url_path(url: Ref) -> Option<String> {
        let mut buf = [0u8; 4096];
        unsafe {
            if CFURLGetFileSystemRepresentation(url, 1, buf.as_mut_ptr(), buf.len() as isize) == 0 {
                return None;
            }
            Some(CStr::from_ptr(buf.as_ptr().cast()).to_string_lossy().into_owned())
        }
    }

    /// Where a bookmark leads now, found the way Finder would (following a
    /// moved item), without asking the user anything or mounting disks.
    pub fn resolve_bookmark(bytes: &[u8]) -> Option<String> {
        // kCFURLBookmarkResolutionWithoutUIMask | …WithoutMountingMask
        const QUIETLY: usize = (1 << 8) | (1 << 9);
        unsafe {
            let data = Owned::new(CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize))?;
            let mut stale = 0u8;
            let url = Owned::new(CFURLCreateByResolvingBookmarkData(
                std::ptr::null(),
                data.0,
                QUIETLY,
                std::ptr::null(),
                std::ptr::null(),
                &mut stale,
                std::ptr::null_mut(),
            ))?;
            url_path(url.0)
        }
    }

    /// The path a bookmark was made for, without resolving it (so it works
    /// when the file is gone).
    pub fn bookmark_path(data: Ref) -> Option<String> {
        unsafe {
            if CFGetTypeID(data) != CFDataGetTypeID() {
                return None;
            }
            let key = kCFURLPathKey;
            let keys = Owned::new(CFArrayCreate(std::ptr::null(), &key, 1, (&raw const kCFTypeArrayCallBacks).cast()))?;
            let props = Owned::new(CFURLCreateResourcePropertiesForKeysFromBookmarkData(std::ptr::null(), keys.0, data))?;
            string(CFDictionaryGetValue(props.0, key))
        }
    }
}

type ItemRef = *const c_void;

struct Lsfl {
    create: unsafe extern "C" fn(cf::Ref, cf::Ref, cf::Ref) -> cf::Ref,
    snapshot: unsafe extern "C" fn(cf::Ref, *mut u32) -> cf::Ref,
    display_name: unsafe extern "C" fn(ItemRef) -> cf::Ref,
    resolve: unsafe extern "C" fn(ItemRef, u32, *mut cf::Ref) -> cf::Ref,
    property: unsafe extern "C" fn(ItemRef, cf::Ref) -> cf::Ref,
    /// Private, so optional: only used for items whose app is gone.
    bookmark: Option<unsafe extern "C" fn(ItemRef, cf::Ref, cf::Ref) -> cf::Ref>,
    insert: unsafe extern "C" fn(cf::Ref, ItemRef, cf::Ref, cf::Ref, cf::Ref, cf::Ref, cf::Ref) -> cf::Ref,
    remove: unsafe extern "C" fn(cf::Ref, ItemRef) -> i32,
    login_items: cf::Ref,
    hidden_key: cf::Ref,
    last: ItemRef,
}

// The pointers are immutable constants and functions from a system framework.
unsafe impl Send for Lsfl {}
unsafe impl Sync for Lsfl {}

/// Resolve without asking the user anything or mounting disks.
const RESOLVE_QUIETLY: u32 = 1 | 2;

fn lsfl() -> Option<&'static Lsfl> {
    static API: OnceLock<Option<Lsfl>> = OnceLock::new();
    API.get_or_init(|| unsafe { load_lsfl() }).as_ref()
}

unsafe fn load_lsfl() -> Option<Lsfl> {
    unsafe {
        let h = libc::dlopen(c"/System/Library/Frameworks/CoreServices.framework/CoreServices".as_ptr(), libc::RTLD_LAZY);
        if h.is_null() {
            return None;
        }
        let data = |name: &CStr| {
            let p = libc::dlsym(h, name.as_ptr()) as *const cf::Ref;
            if p.is_null() { None } else { Some(*p).filter(|v| !v.is_null()) }
        };
        Some(Lsfl {
            create: func(h, c"LSSharedFileListCreate")?,
            snapshot: func(h, c"LSSharedFileListCopySnapshot")?,
            display_name: func(h, c"LSSharedFileListItemCopyDisplayName")?,
            resolve: func(h, c"LSSharedFileListItemCopyResolvedURL")?,
            property: func(h, c"LSSharedFileListItemCopyProperty")?,
            bookmark: func(h, c"LSSharedFileListItemCopyBookmarkData"),
            insert: func(h, c"LSSharedFileListInsertItemURL")?,
            remove: func(h, c"LSSharedFileListItemRemove")?,
            login_items: data(c"kLSSharedFileListSessionLoginItems")?,
            hidden_key: data(c"kLSSharedFileListLoginItemHidden")?,
            last: data(c"kLSSharedFileListItemLast")?,
        })
    }
}

/// A function from a loaded library. `T` must be an `extern "C" fn` type.
unsafe fn func<T: Copy>(h: *mut c_void, name: &CStr) -> Option<T> {
    debug_assert_eq!(size_of::<T>(), size_of::<*mut c_void>());
    let p = unsafe { libc::dlsym(h, name.as_ptr()) };
    (!p.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
}

impl Lsfl {
    fn list(&self) -> Option<cf::Owned> {
        cf::Owned::new(unsafe { (self.create)(std::ptr::null(), self.login_items, std::ptr::null()) })
    }

    /// Call `f` with each item of a fresh snapshot of the list.
    fn each(&self, mut f: impl FnMut(&cf::Owned, ItemRef)) -> Option<()> {
        let list = self.list()?;
        let mut seed = 0u32;
        let snap = cf::Owned::new(unsafe { (self.snapshot)(list.0, &mut seed) })?;
        for i in 0..unsafe { cf::CFArrayGetCount(snap.0) } {
            f(&list, unsafe { cf::CFArrayGetValueAtIndex(snap.0, i) });
        }
        Some(())
    }

    fn items(&self) -> Option<Vec<LoginItem>> {
        let mut out = Vec::new();
        self.each(|_, item| out.push(self.describe(item)))?;
        Some(out)
    }

    fn contains(&self, wanted: &LoginItem) -> bool {
        self.items().is_some_and(|v| v.iter().any(|i| i.same(wanted)))
    }

    fn describe(&self, item: ItemRef) -> LoginItem {
        let path = self.item_path(item);
        let name = cf::Owned::new(unsafe { (self.display_name)(item) })
            .and_then(|s| cf::string(s.0))
            .or_else(|| Path::new(&path).file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let hidden = cf::Owned::new(unsafe { (self.property)(item, self.hidden_key) }).is_some_and(|b| cf::boolean(b.0));
        LoginItem { name: name.trim_end_matches(".app").to_string(), path, hidden }
    }

    /// Where the item is: found the way Finder would (following a moved
    /// app), without mounting anything; else the path its bookmark was made
    /// for, which is what's left when the app is gone.
    fn item_path(&self, item: ItemRef) -> String {
        if let Some(url) = cf::Owned::new(unsafe { (self.resolve)(item, RESOLVE_QUIETLY, std::ptr::null_mut()) })
            && let Some(p) = cf::url_path(url.0)
        {
            return p.trim_end_matches('/').to_string();
        }
        self.bookmark
            .and_then(|f| cf::Owned::new(unsafe { f(item, std::ptr::null(), std::ptr::null()) }))
            .and_then(|data| cf::bookmark_path(data.0))
            .map(|p| p.trim_end_matches('/').to_string())
            .unwrap_or_default()
    }

    fn remove(&self, wanted: &LoginItem) -> Result<(), String> {
        let mut result = Ok(());
        self.each(|list, item| {
            if result.is_ok() && self.describe(item).same(wanted) {
                let rc = unsafe { (self.remove)(list.0, item) };
                if rc != 0 {
                    result = Err(format!("macOS returned error {rc}"));
                }
            }
        })
        .ok_or("macOS didn't return the login items")?;
        result
    }

    fn add(&self, path: &str, hidden: bool) -> Result<(), String> {
        let list = self.list().ok_or("macOS didn't return the login items")?;
        unsafe {
            let url = cf::Owned::new(cf::CFURLCreateFromFileSystemRepresentation(std::ptr::null(), path.as_ptr(), path.len() as isize, 1))
                .ok_or("not a valid path")?;
            let value = if hidden { cf::kCFBooleanTrue } else { cf::kCFBooleanFalse };
            let props = cf::Owned::new(cf::CFDictionaryCreate(
                std::ptr::null(),
                &self.hidden_key,
                &value,
                1,
                (&raw const cf::kCFTypeDictionaryKeyCallBacks).cast(),
                (&raw const cf::kCFTypeDictionaryValueCallBacks).cast(),
            ))
            .ok_or("out of memory")?;
            let item = (self.insert)(list.0, self.last, std::ptr::null(), std::ptr::null(), url.0, props.0, std::ptr::null());
            match cf::Owned::new(item) {
                Some(_) => Ok(()),
                None => Err("macOS didn't add it".into()),
            }
        }
    }
}

// ----------------------------------------------------------------------
// Who made it: the code signature

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signer {
    /// A developer certificate: the company or person, and their team ID.
    Developer { name: String, team: Option<String> },
    Apple,
    AppStore,
    /// Signed without an identity ("ad hoc"), or not at all.
    Unsigned,
}

impl Signer {
    pub fn text(&self) -> &str {
        match self {
            Signer::Developer { name, .. } => name,
            Signer::Apple => "Apple",
            Signer::AppStore => "App Store",
            Signer::Unsigned => "not signed",
        }
    }

    pub fn hover(&self) -> String {
        match self {
            Signer::Developer { name, team: Some(t) } => format!("Signed by {name} (team {t})"),
            Signer::Developer { name, team: None } => format!("Signed by {name}"),
            Signer::Apple => "Part of macOS, signed by Apple".into(),
            Signer::AppStore => "Signed by the App Store".into(),
            Signer::Unsigned => "No developer signature".into(),
        }
    }
}

/// Who signed the program or app at `path`, from `codesign -dv`. Cached,
/// since it takes a moment; call it off the UI thread.
pub fn signer(path: &str) -> Option<Signer> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Signer>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(s) = cache.lock().unwrap().get(path) {
        return s.clone();
    }
    let s = Command::new("/usr/bin/codesign")
        .args(["-dv", "--verbose=2", path])
        .output()
        .ok()
        .and_then(|o| parse_codesign(&String::from_utf8_lossy(&o.stderr)));
    cache.lock().unwrap().insert(path.to_string(), s.clone());
    s
}

/// Read `codesign -dv --verbose=2` output (it prints to stderr).
pub fn parse_codesign(text: &str) -> Option<Signer> {
    if text.contains("is not signed at all") {
        return Some(Signer::Unsigned);
    }
    let field = |k: &str| text.lines().find_map(|l| l.trim().strip_prefix(k)).map(str::trim);
    if field("Signature=") == Some("adhoc") {
        return Some(Signer::Unsigned);
    }
    let authority = field("Authority=")?;
    if authority.contains("Software Signing") {
        return Some(Signer::Apple);
    }
    if authority == "Apple Mac OS Application Signing" {
        return Some(Signer::AppStore);
    }
    let team = field("TeamIdentifier=").filter(|t| *t != "not set").map(str::to_string);
    let name = ["Developer ID Application: ", "Apple Development: ", "Apple Distribution: ", "Mac Developer: ", "3rd Party Mac Developer Application: "]
        .iter()
        .find_map(|p| authority.strip_prefix(p))
        .unwrap_or(authority);
    // Certificate names end with the team ID in brackets.
    let name = match name.rsplit_once(" (") {
        Some((n, t)) if t.ends_with(')') && !n.is_empty() => n,
        _ => name,
    };
    Some(Signer::Developer { name: name.to_string(), team })
}

// ----------------------------------------------------------------------
// Background items inside apps

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BundledKind {
    Agent,
    Daemon,
    LoginItem,
}

impl BundledKind {
    pub fn label(self) -> &'static str {
        match self {
            BundledKind::Agent => "Background agent",
            BundledKind::Daemon => "Background daemon",
            BundledKind::LoginItem => "Helper app",
        }
    }
}

/// A launch agent, daemon or helper app shipped inside an app, which the app
/// can switch on itself. Only macOS knows which are allowed to run, and it
/// doesn't tell other apps without an administrator password; "active" means
/// launchd has it loaded right now.
#[derive(Clone, Debug)]
pub struct BundledItem {
    pub app: String,
    pub kind: BundledKind,
    pub label: String,
    /// The helper's .plist or app.
    pub path: PathBuf,
    pub active: bool,
}

fn app_bundles() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(h) = crate::platform::home_dir() {
        roots.push(Path::new(&h).join("Applications"));
    }
    let is_app = |p: &Path| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("app"));
    let mut out = Vec::new();
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        for p in rd.flatten().map(|e| e.path()) {
            if is_app(&p) {
                out.push(p);
            } else if p.is_dir()
                && let Ok(sub) = std::fs::read_dir(&p)
            {
                // One level of folders, like /Applications/Utilities.
                out.extend(sub.flatten().map(|e| e.path()).filter(|p| is_app(p)));
            }
        }
    }
    out
}

pub fn bundled_items(user: &HashMap<String, Option<u32>>, system: &HashMap<String, Option<u32>>) -> Vec<BundledItem> {
    let mut out = Vec::new();
    for app in app_bundles() {
        let lib = app.join("Contents/Library");
        if !lib.is_dir() {
            continue;
        }
        let app_name = super::bundle_info(&app).map(|b| b.name).unwrap_or_default();
        for (folder, kind, domain) in [("LaunchAgents", BundledKind::Agent, Domain::Agent), ("LaunchDaemons", BundledKind::Daemon, Domain::Daemon)] {
            let Ok(rd) = std::fs::read_dir(lib.join(folder)) else { continue };
            for p in rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "plist")) {
                let Some(job) = super::parse_launch_job(&p, domain) else { continue };
                let loaded = if kind == BundledKind::Daemon { system } else { user };
                out.push(BundledItem {
                    app: app_name.clone(),
                    kind,
                    active: loaded.contains_key(&job.label),
                    label: job.label,
                    path: p,
                });
            }
        }
        let Ok(rd) = std::fs::read_dir(lib.join("LoginItems")) else { continue };
        for p in rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "app")) {
            let Some(b) = super::bundle_info(&p) else { continue };
            let label = b.id.unwrap_or(b.name);
            out.push(BundledItem {
                app: app_name.clone(),
                kind: BundledKind::LoginItem,
                active: user.contains_key(&label),
                label,
                path: p,
            });
        }
    }
    out.sort_by_key(|b| (!b.active, b.app.to_lowercase(), b.label.clone()));
    out
}

/// Where a bookmark (an alias Finder or the Dock saved) leads now, if
/// anywhere: followed quietly, without prompts or mounting disks.
pub fn resolve_bookmark(bytes: &[u8]) -> Option<String> {
    cf::resolve_bookmark(bytes)
}

// ----------------------------------------------------------------------
// Everything in System Settings › Login Items & Extensions

/// One item from macOS's own list of login and background items (Background
/// Task Management), which only root can read, with `sfltool dumpbtm`.
/// Other apps can't switch these; System Settings can.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundItem {
    pub name: String,
    /// From the code signature, when it has one.
    pub developer: Option<String>,
    /// "app", "login item", "agent", "legacy daemon", …
    pub kind: String,
    /// "Allow in the Background" is on.
    pub allowed: bool,
    /// The program, or the .plist or app it comes from.
    pub path: Option<String>,
    /// The app it belongs to.
    pub parent: Option<String>,
}

/// Every login and background item macOS keeps track of, for every user.
/// Asks for the administrator password; blocks until it's answered.
pub fn background_items() -> Result<Vec<BackgroundItem>, String> {
    let out = super::run_as_admin("/usr/bin/sfltool dumpbtm", "list every login and background item macOS knows about")?;
    let items = parse_dumpbtm(&out);
    if items.is_empty() && !out.contains("Records") {
        return Err("macOS didn't list anything".into());
    }
    Ok(items)
}

/// Parse `sfltool dumpbtm`: records headed `#1:`, then `Key: value` lines
/// (`Name`, `Developer Name`, `Type: legacy agent (0x10008)`,
/// `Disposition: [enabled, allowed, visible, notified] (0xb)`,
/// `Executable Path`, `URL`, `Parent Identifier`, …). `developer` records
/// only group the others, and the same item can be listed for several
/// users, so both are left out. `do shell script` ends lines with `\r`.
pub fn parse_dumpbtm(text: &str) -> Vec<BackgroundItem> {
    fn finish(cur: &mut Option<HashMap<String, String>>, out: &mut Vec<BackgroundItem>) {
        let Some(f) = cur.take() else { return };
        let get = |k: &str| f.get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty() && v != "(null)");
        let Some(name) = get("Name") else { return };
        // "legacy agent (0x10008)" → "legacy agent"
        let kind = get("Type").map(|t| t.split(" (").next().unwrap_or(&t).trim().to_string()).unwrap_or_default();
        if kind == "developer" {
            return;
        }
        let disposition = get("Disposition").unwrap_or_default();
        let words: Vec<&str> = disposition.trim_start_matches('[').split(']').next().unwrap_or("").split([',', ' ']).collect();
        let path = get("Executable Path").or_else(|| {
            get("URL").map(|u| {
                let p = u.strip_prefix("file://").unwrap_or(&u).replace("%20", " ");
                p.trim_end_matches('/').to_string()
            })
        });
        let parent = get("Parent Identifier").filter(|p| p != "Unknown Developer" && !p.starts_with("(embedded"));
        let item = BackgroundItem { name, developer: get("Developer Name"), kind, allowed: words.contains(&"allowed"), path, parent };
        if !out.contains(&item) {
            out.push(item);
        }
    }
    let mut out = Vec::new();
    let mut cur: Option<HashMap<String, String>> = None;
    for line in text.split(['\r', '\n']) {
        let t = line.trim();
        // A record starts with `#12:` alone; `#1: <id>` lines list embedded items.
        if t.starts_with('#') && t.ends_with(':') && t[1..t.len() - 1].chars().all(|c| c.is_ascii_digit()) {
            finish(&mut cur, &mut out);
            cur = Some(HashMap::new());
            continue;
        }
        if let Some(f) = cur.as_mut()
            && let Some((k, v)) = t.split_once(": ")
            && !k.starts_with('#')
        {
            f.entry(k.trim().to_string()).or_insert_with(|| v.to_string());
        }
    }
    finish(&mut cur, &mut out);
    out.sort_by_key(|i| (i.developer.clone().unwrap_or_default().to_lowercase(), i.name.to_lowercase()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dumpbtm_output() {
        let text = "========================\r Records for UID 501 : 2F3C\r========================\r\r ServiceManagement Records:\r\r #1:\r\
                 UUID: 7A2E\r                 Name: Dropbox\r       Developer Name: Dropbox, Inc.\r\
                 Type: app (0x2)\r          Disposition: [enabled, allowed, visible, notified] (0xb)\r\
                 URL: file:///Applications/Dropbox.app/\r    Embedded Item Identifiers:\r        #1: 16.com.getdropbox.dropbox.garcon\r\
             #2:\r                 Name: Wandering WiFi LLC\r       Developer Name: Wandering WiFi LLC\r\
                 Type: developer (0x20)\r          Disposition: [disabled, allowed, visible, not notified] (2)\r\
             #3:\r                 Name: com.microsoft.autoupdate.helper\r       Developer Name: Microsoft AutoUpdate\r\
                 Type: curated legacy daemon (0x90010)\r          Disposition: [enabled, disallowed, visible, notified] (9)\r\
                 URL: file:///Library/LaunchDaemons/com.microsoft.autoupdate.helper.plist\r\
      Executable Path: /Library/PrivilegedHelperTools/com.microsoft.autoupdate.helper\r\
    Parent Identifier: Microsoft AutoUpdate\r\
             #4:\r                 Name: Dropbox\r       Developer Name: Dropbox, Inc.\r\
                 Type: app (0x2)\r          Disposition: [enabled, allowed, visible, notified] (0xb)\r\
                 URL: file:///Applications/Dropbox.app/\r";
        let items = parse_dumpbtm(text);
        assert_eq!(items.len(), 2, "{items:#?}");
        assert_eq!(
            items[0],
            BackgroundItem {
                name: "Dropbox".into(),
                developer: Some("Dropbox, Inc.".into()),
                kind: "app".into(),
                allowed: true,
                path: Some("/Applications/Dropbox.app".into()),
                parent: None,
            }
        );
        let ms = &items[1];
        assert_eq!(ms.kind, "curated legacy daemon");
        assert!(!ms.allowed, "disallowed");
        assert_eq!(ms.path.as_deref(), Some("/Library/PrivilegedHelperTools/com.microsoft.autoupdate.helper"));
        assert_eq!(ms.parent.as_deref(), Some("Microsoft AutoUpdate"));
    }

    fn job(program: &str, args: &[&str]) -> LaunchJob {
        LaunchJob {
            plist: PathBuf::from("/tmp/x.plist"),
            domain: Domain::UserAgent,
            label: "com.heftdev.test".into(),
            program: Some(program.into()),
            args: args.iter().map(|s| s.to_string()).collect(),
            run_at_load: false,
            keep_alive: false,
            scheduled: false,
            on_demand: false,
            disabled_in_plist: false,
            bundles: Vec::new(),
        }
    }

    #[test]
    fn job_targets() {
        assert_eq!(job_target(&job("/Applications/A.app/Contents/MacOS/a", &[])).as_deref(), Some("/Applications/A.app/Contents/MacOS/a"));
        assert_eq!(job_target(&job("/bin/sh", &["/bin/sh", "/Users/x/run.sh", "go"])).as_deref(), Some("/Users/x/run.sh"));
        assert_eq!(job_target(&job("/bin/sh", &["/bin/sh", "-c", "/gone/x"])).as_deref(), Some("/bin/sh"));
        assert_eq!(job_target(&job("/usr/bin/python3", &["python3", "-u", "/opt/x/main.py"])).as_deref(), Some("/opt/x/main.py"));
        assert_eq!(job_target(&job("/usr/bin/python3", &["python3", "main.py"])).as_deref(), Some("/usr/bin/python3"));
        assert_eq!(job_target(&job("/usr/bin/open", &["open", "-a", "/Applications/B.app"])).as_deref(), Some("/Applications/B.app"));
        assert_eq!(job_target(&job("node", &["node", "/x.js"])), None, "PATH lookups can't be judged");
    }

    #[test]
    fn how_jobs_start() {
        let mut j = job("/x", &[]);
        assert_eq!(Starts::of(&j), Starts::WhenAsked);
        j.on_demand = true;
        assert_eq!(Starts::of(&j), Starts::OnDemand);
        j.scheduled = true;
        assert_eq!(Starts::of(&j), Starts::Scheduled);
        j.run_at_load = true;
        assert_eq!(Starts::of(&j), Starts::AtLogin);
        j.domain = Domain::Daemon;
        assert_eq!(Starts::of(&j), Starts::AtStartup);
        j.keep_alive = true;
        assert_eq!(Starts::of(&j), Starts::KeptRunning);
    }

    #[test]
    fn launchctl_services() {
        let text = "gui/501 = {\n\ttype = login\n\tservices = {\n\t\t   80870   (pe) \tcom.apple.x.A5EB\n\t\t       0      - \tcom.dropbox.DropboxUpdater.wake\n\t\t   27319      - \tcom.riftbound.bot\n\t}\n\tdisabled services = {\n\t\t\"com.a\" => enabled\n\t}\n}\n";
        let s = parse_services(text);
        assert_eq!(s.len(), 3);
        assert_eq!(s["com.riftbound.bot"], Some(27319));
        assert_eq!(s["com.dropbox.DropboxUpdater.wake"], None);
        assert!(!s.contains_key("\"com.a\""));
    }

    #[test]
    fn codesign_output() {
        let dropbox = "Executable=/Applications/Dropbox.app/Contents/MacOS/Dropbox\nIdentifier=com.getdropbox.dropbox\n\
            Format=app bundle with Mach-O thin (arm64)\nSignature size=9146\n\
            Authority=Developer ID Application: Dropbox, Inc. (G7HH3F8CAK)\nAuthority=Developer ID Certification Authority\n\
            Authority=Apple Root CA\nTeamIdentifier=G7HH3F8CAK\n";
        assert_eq!(parse_codesign(dropbox), Some(Signer::Developer { name: "Dropbox, Inc.".into(), team: Some("G7HH3F8CAK".into()) }));
        let ls = "Executable=/bin/ls\nIdentifier=com.apple.ls\nPlatform identifier=26\nAuthority=macOS Software Signing\n\
            Authority=Apple Code Signing Certification Authority\nTeamIdentifier=not set\n";
        assert_eq!(parse_codesign(ls), Some(Signer::Apple));
        let store = "Authority=Apple Mac OS Application Signing\nAuthority=Apple Worldwide Developer Relations Certification Authority\nTeamIdentifier=S8EX82NJP6\n";
        assert_eq!(parse_codesign(store), Some(Signer::AppStore));
        assert_eq!(parse_codesign("Executable=/x\nSignature=adhoc\nTeamIdentifier=not set\n"), Some(Signer::Unsigned));
        assert_eq!(parse_codesign("/tmp/x: code object is not signed at all\n"), Some(Signer::Unsigned));
        assert_eq!(parse_codesign("/tmp/nope: No such file or directory\n"), None);
    }

    #[test]
    fn system_events_output() {
        let items = parse_system_events("Dropbox\t/Applications/Dropbox.app/\tfalse\rMail\t/System/Applications/Mail.app\ttrue\rGone\tmissing value\tfalse\r");
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], LoginItem { name: "Dropbox".into(), path: "/Applications/Dropbox.app".into(), hidden: false });
        assert!(items[1].hidden);
        assert_eq!(items[2].path, "");
    }

    #[test]
    fn remembers_turned_off_items() {
        let dir = std::env::temp_dir().join(format!("heft-loginoff-{}", std::process::id()));
        let file = dir.join("login-items-off.plist");
        let a = LoginItem { name: "A".into(), path: "/Applications/A.app".into(), hidden: true };
        let b = LoginItem { name: "B".into(), path: "/Applications/B.app".into(), hidden: false };
        write_items(&file, &[a.clone(), b.clone()]).unwrap();
        assert_eq!(read_items(&file), vec![a.clone(), b]);
        assert!(read_items(&dir.join("nope.plist")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Read-only: the login items list and launchd state on this Mac.
    #[test]
    fn lists_without_changing_anything() {
        let l = list(false);
        for e in &l.entries {
            assert!(!e.name.is_empty() || !e.command.is_empty());
        }
        // The direct read works on the macOS versions Heft is tested on.
        assert!(lsfl().is_some(), "LSSharedFileList is missing");
        assert_ne!(l.login_error, Some(LoginError::NotAllowed));
    }
}
