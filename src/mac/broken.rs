//! Broken items on macOS, the counterpart of the Windows registry check:
//! launch agents and daemons, login items, Dock icons, command-line links,
//! "Open With" registrations and package receipts that point at apps and
//! files that no longer exist.
//!
//! Like the registry check it's deliberately narrow. An item only counts when
//! the file it names is provably gone: never for disks that aren't connected
//! (anything under /Volumes), network folders, or places Heft isn't allowed to
//! look. Everything is backed up before it's changed (here and on the Login
//! items page), and the Backups list puts it back. Changes that need root are
//! gathered into one administrator password prompt.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use super::startup::{self, LoginItem};
use super::{sh_quote, Domain, LaunchJob};

// ----------------------------------------------------------------------
// Is it really gone?

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathState {
    Exists,
    Missing,
    /// Can't tell safely: a relative name, a disk that isn't connected, a
    /// folder Heft can't read, …
    Unknown,
}

/// Whether a path points at something that exists. Only says `Missing` when
/// it can be sure.
pub fn path_state(raw: &str) -> PathState {
    state_of(Path::new(raw.trim()), 0)
}

fn state_of(p: &Path, depth: u8) -> PathState {
    if depth > 8 || !p.is_absolute() || p.as_os_str().is_empty() || elsewhere(p) {
        return PathState::Unknown;
    }
    match std::fs::metadata(p) {
        Ok(_) => return PathState::Exists,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        // No permission (other users' folders, privacy-protected ones) isn't "gone".
        Err(_) => return PathState::Unknown,
    }
    // Something along the way is missing. If it's a link that leads to
    // another disk, that disk may just not be connected.
    for anc in p.ancestors() {
        match std::fs::symlink_metadata(anc) {
            Ok(m) if m.file_type().is_symlink() => {
                let Ok(target) = std::fs::read_link(anc) else { return PathState::Unknown };
                let base = anc.parent().unwrap_or(Path::new("/"));
                let rest = p.strip_prefix(anc).unwrap_or(Path::new(""));
                let next = if rest.as_os_str().is_empty() { base.join(target) } else { base.join(target).join(rest) };
                return state_of(&next, depth + 1);
            }
            // A real folder that exists, and the next part isn't in it.
            Ok(_) => return PathState::Missing,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return PathState::Unknown,
        }
    }
    PathState::Unknown
}

/// On a disk that comes and goes (external drives, disk images, network
/// shares) or one macOS manages itself. What's missing there may just be
/// somewhere Heft can't see right now.
fn elsewhere(p: &Path) -> bool {
    let n = normalize(p);
    let s = n.to_string_lossy();
    let under = |root: &str| s == root || s.starts_with(&format!("{root}/"));
    ["/Volumes", "/Network", "/net", "/home", "/private/var/automount", "/automount"].iter().any(|r| under(r))
        || (under("/System/Volumes") && !under("/System/Volumes/Data"))
}

/// `.` and `..` resolved without touching the disk.
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// Whether the current user can create and delete files in `dir`.
fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return false };
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

// ----------------------------------------------------------------------
// What's checked

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum Category {
    LaunchJobs,
    LoginItems,
    Dock,
    Links,
    OpenWith,
    Receipts,
}

impl Category {
    pub const ALL: [Category; 6] =
        [Category::LaunchJobs, Category::LoginItems, Category::Dock, Category::Links, Category::OpenWith, Category::Receipts];

    pub fn label(self) -> &'static str {
        match self {
            Category::LaunchJobs => "Launch agents and daemons for missing programs",
            Category::LoginItems => "Login items for missing apps",
            Category::Dock => "Dock icons for missing apps",
            Category::Links => "Command-line links to missing files",
            Category::OpenWith => "Open With entries for missing apps",
            Category::Receipts => "Package receipts for removed software",
        }
    }

    /// Ticked after a scan: the kinds with a visible effect (macOS failing
    /// to start something at every login or startup). The rest is inert
    /// clutter that's listed but left for the user to choose.
    pub fn default_on(self) -> bool {
        matches!(self, Category::LaunchJobs | Category::LoginItems | Category::Dock)
    }

    pub fn about(self) -> &'static str {
        match self {
            Category::LaunchJobs => {
                "Launch agents and daemons whose program no longer exists; macOS tries and fails to start them at every login or startup."
            }
            Category::LoginItems => "Apps set to open at login that no longer exist.",
            Category::Dock => {
                "Apps and folders kept in the Dock that no longer exist, which the Dock shows with a question mark. Removing them restarts the Dock, which takes a moment."
            }
            Category::Links => {
                "Commands in /usr/local/bin, ~/.local/bin and ~/bin that point at files that are gone, usually left by apps you deleted. Links Homebrew, MacPorts or Nix manage are left to them."
            }
            Category::OpenWith => {
                "Apps Launch Services still has registered at a place where they no longer exist. They can linger in Open With menus. Harmless, but clutter."
            }
            Category::Receipts => {
                "Installer package records whose files have all been deleted. Forgetting one only removes the record; no files are touched."
            }
        }
    }
}

/// Something Heft can remove, with what it takes to do it and undo it.
#[derive(Clone, Debug)]
pub enum Target {
    Job(LaunchJob),
    /// A login item, and whether it's on the real list (`true`) or only
    /// remembered by Heft as turned off.
    LoginItem(LoginItem, bool),
    /// A symbolic link and where it points.
    Link { path: PathBuf, target: String },
    /// A Launch Services registration of an app at this path.
    OpenWith(String),
    /// A package receipt, by package id.
    Receipt(String),
    /// A Dock icon: which list it's in, its id and the address it opens.
    DockTile { key: String, guid: Option<i64>, url: String, label: String },
}

#[derive(Clone, Debug)]
pub struct Issue {
    pub category: Category,
    /// What it refers to (the missing file or app), or the package.
    pub detail: String,
    /// Where the entry lives.
    pub location: String,
    pub target: Target,
}

impl Issue {
    pub fn needs_admin(&self) -> bool {
        self.target.needs_admin()
    }
}

/// Everything broken. Read-only; takes a few seconds (Launch Services and
/// the package database are the slow parts).
pub fn scan() -> Vec<Issue> {
    let mut out = Vec::new();
    std::thread::scope(|s| {
        let parts = [
            s.spawn(launch_job_issues),
            s.spawn(login_item_issues),
            s.spawn(dock_issues),
            s.spawn(link_issues),
            s.spawn(open_with_issues),
            s.spawn(receipt_issues),
        ];
        for p in parts {
            out.extend(p.join().unwrap_or_default());
        }
    });
    out
}

fn launch_job_issues() -> Vec<Issue> {
    super::launch_jobs()
        .into_iter()
        .filter_map(|job| {
            let target = startup::job_target(&job)?;
            (path_state(&target) == PathState::Missing).then(|| Issue {
                category: Category::LaunchJobs,
                detail: target,
                location: job.plist.to_string_lossy().into_owned(),
                target: Target::Job(job),
            })
        })
        .collect()
}

fn login_item_issues() -> Vec<Issue> {
    // Never System Events here: a scan mustn't ask for permission.
    let Ok(live) = startup::login_items(false) else { return Vec::new() };
    let gone = |i: &LoginItem| !i.path.is_empty() && path_state(&i.path) == PathState::Missing;
    let mut out: Vec<Issue> = live
        .into_iter()
        .filter(gone)
        .map(|i| Issue {
            category: Category::LoginItems,
            detail: i.path.clone(),
            location: format!("Login items › {}", i.name),
            target: Target::LoginItem(i, true),
        })
        .collect();
    out.extend(startup::turned_off().into_iter().filter(gone).map(|i| Issue {
        category: Category::LoginItems,
        detail: i.path.clone(),
        location: format!("Turned off in Heft › {}", i.name),
        target: Target::LoginItem(i, false),
    }));
    out
}

// The Dock

/// The Dock's lists of icons: kept apps, kept folders and files, recent apps.
const DOCK_LISTS: [&str; 3] = ["persistent-apps", "persistent-others", "recent-apps"];

/// The Dock's settings as it has them now (`defaults export`), not the
/// file, which it may not have written yet.
fn dock_prefs() -> Option<plist::Value> {
    let out = Command::new("/usr/bin/defaults").args(["export", "com.apple.dock", "-"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    plist::Value::from_reader_xml(&out.stdout[..]).ok()
}

/// A Dock icon's file: the address it opens, the path, its bookmark and its label.
struct Tile {
    guid: Option<i64>,
    url: String,
    path: String,
    book: Option<Vec<u8>>,
    label: String,
}

fn tile(v: &plist::Value) -> Option<Tile> {
    let d = v.as_dictionary()?;
    let data = d.get("tile-data")?.as_dictionary()?;
    let file = data.get("file-data")?.as_dictionary()?;
    let url = file.get("_CFURLString")?.as_string()?.to_string();
    // Type 15 is a file:// address, 0 a plain path.
    let path = match url.strip_prefix("file://") {
        Some(rest) => percent_decode(rest),
        None => url.clone(),
    };
    Some(Tile {
        guid: d.get("GUID").and_then(|g| g.as_signed_integer()),
        path: path.trim_end_matches('/').to_string(),
        book: data.get("book").and_then(|b| b.as_data()).map(<[u8]>::to_vec),
        label: data.get("file-label").and_then(|l| l.as_string()).unwrap_or_default().to_string(),
        url,
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Gone for sure: the path is missing, and the bookmark the Dock also keeps
/// (which follows a moved app) doesn't lead anywhere that exists either.
fn tile_gone(t: &Tile, resolve: impl Fn(&[u8]) -> Option<String>) -> bool {
    path_state(&t.path) == PathState::Missing
        && t.book.as_deref().and_then(resolve).is_none_or(|p| path_state(&p) != PathState::Exists)
}

fn dock_issues_in(prefs: &plist::Value, resolve: impl Fn(&[u8]) -> Option<String>) -> Vec<Issue> {
    let Some(d) = prefs.as_dictionary() else { return Vec::new() };
    let mut out = Vec::new();
    for key in DOCK_LISTS {
        for t in d.get(key).and_then(|v| v.as_array()).into_iter().flatten().filter_map(tile) {
            if tile_gone(&t, &resolve) {
                out.push(Issue {
                    category: Category::Dock,
                    detail: t.path.clone(),
                    location: format!("Dock › {}", if t.label.is_empty() { &t.path } else { &t.label }),
                    target: Target::DockTile { key: key.to_string(), guid: t.guid, url: t.url, label: t.label },
                });
            }
        }
    }
    out
}

fn dock_issues() -> Vec<Issue> {
    dock_prefs().map(|p| dock_issues_in(&p, startup::resolve_bookmark)).unwrap_or_default()
}

/// Where a Dock icon is in its list, found by its id and address.
fn find_tile(list: &[plist::Value], guid: Option<i64>, url: &str) -> Option<usize> {
    list.iter().position(|v| tile(v).is_some_and(|t| t.url == url && (guid.is_none() || t.guid == guid)))
}

/// Change the Dock's settings and hand them back to it. The Dock only sees
/// the change once it restarts (see [`restart_dock`]).
fn edit_dock(f: impl FnOnce(&mut plist::Dictionary) -> Result<(), String>) -> Result<(), String> {
    let mut prefs = dock_prefs().ok_or("the Dock's settings couldn't be read")?;
    f(prefs.as_dictionary_mut().ok_or("the Dock's settings aren't what Heft expected")?)?;
    let file = std::env::temp_dir().join(format!("heft-dock-{}-{}.plist", std::process::id(), crate::platform::now_unix()));
    prefs.to_file_xml(&file).map_err(|e| e.to_string())?;
    let out = Command::new("/usr/bin/defaults").args(["import", "com.apple.dock"]).arg(&file).output();
    let _ = std::fs::remove_file(&file);
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Restart the Dock so it reads its settings again, as `killall Dock` does.
fn restart_dock() {
    let _ = Command::new("/usr/bin/killall").arg("Dock").status();
}

/// Take icons out of a list, returning each with where it was.
fn take_tiles(list: &mut Vec<plist::Value>, wanted: &[(Option<i64>, String)]) -> Vec<(usize, plist::Value)> {
    let mut taken = Vec::new();
    for (guid, url) in wanted {
        if let Some(i) = find_tile(list, *guid, url) {
            taken.push((i, list.remove(i)));
        }
    }
    taken
}

/// Folders of command-line tools that installers link into.
fn link_folders() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/usr/local/bin"), PathBuf::from("/usr/local/sbin")];
    if let Some(h) = crate::platform::home_dir() {
        v.push(Path::new(&h).join(".local/bin"));
        v.push(Path::new(&h).join("bin"));
    }
    v
}

/// Links a package manager keeps; `brew cleanup` and friends deal with those.
fn managed_elsewhere(target: &Path) -> bool {
    let s = normalize(target).to_string_lossy().into_owned();
    ["/opt/homebrew/", "/usr/local/Homebrew/", "/usr/local/Cellar/", "/usr/local/Caskroom/", "/usr/local/opt/", "/opt/local/", "/nix/", "/home/linuxbrew/"]
        .iter()
        .any(|p| s.starts_with(p))
        || s.contains("/Cellar/")
        || s.contains("/Caskroom/")
}

fn link_issues() -> Vec<Issue> {
    link_folders().iter().flat_map(|dir| broken_links_in(dir)).collect()
}

fn broken_links_in(dir: &Path) -> Vec<Issue> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let path = e.path();
        if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        let Ok(target) = std::fs::read_link(&path) else { continue };
        let resolved = if target.is_absolute() { target.clone() } else { dir.join(&target) };
        if managed_elsewhere(&resolved) || path_state(&resolved.to_string_lossy()) != PathState::Missing {
            continue;
        }
        out.push(Issue {
            category: Category::Links,
            detail: normalize(&resolved).to_string_lossy().into_owned(),
            location: path.to_string_lossy().into_owned(),
            target: Target::Link { path, target: target.to_string_lossy().into_owned() },
        });
    }
    out.sort_by(|a, b| a.location.cmp(&b.location));
    out
}

const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

/// An app Launch Services has on record.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::mac) struct Registered {
    pub path: String,
    pub id: Option<String>,
    pub name: Option<String>,
    /// Bundle ids of its extensions.
    pub plugins: Vec<String>,
    pub app: bool,
    /// On the startup disk, which is mounted.
    pub on_startup_disk: bool,
    /// On a disk that's mounted now.
    pub mounted: bool,
}

/// Every app Launch Services has on record, including ones since deleted
/// and ones on disks that aren't connected. Takes a second or two.
pub(in crate::mac) fn registered_apps() -> Vec<Registered> {
    // Newer macOS can dump just the bundle table, which is twice as fast.
    let text = super::output(LSREGISTER, &["-dump", "Bundle"])
        .filter(|t| t.contains("\nbundle id:"))
        .or_else(|| super::output(LSREGISTER, &["-dump"]))
        .unwrap_or_default();
    parse_lsregister(&text)
}

/// Parse `lsregister -dump`: records separated by lines of dashes; bundles
/// start with `bundle id:`, and apps have `class: kLSBundleClassApplication`.
/// Values often end with an internal id like ` (0x1938)`.
fn parse_lsregister(text: &str) -> Vec<Registered> {
    fn value(v: &str) -> &str {
        let v = v.trim();
        match v.rfind(" (0x") {
            Some(i) if v.ends_with(')') => &v[..i],
            _ => v,
        }
    }
    let mut out = Vec::new();
    let mut cur: Option<(Registered, bool)> = None;
    let mut flush = |cur: &mut Option<(Registered, bool)>| {
        if let Some((r, mounted)) = cur.take()
            && !r.path.is_empty()
        {
            out.push(Registered { on_startup_disk: r.on_startup_disk && mounted, mounted, ..r });
        }
    };
    for line in text.lines() {
        if line.len() >= 20 && line.bytes().all(|b| b == b'-') {
            flush(&mut cur);
            continue;
        }
        // Nested blocks (Info.plist contents and so on) are indented.
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((key, v)) = line.split_once(':') else { continue };
        match key {
            "bundle id" => {
                flush(&mut cur);
                cur = Some((
                    Registered {
                        path: String::new(),
                        id: None,
                        name: None,
                        plugins: Vec::new(),
                        app: false,
                        on_startup_disk: false,
                        mounted: true,
                    },
                    true,
                ));
            }
            _ => {
                let Some((r, mounted)) = cur.as_mut() else { continue };
                match key {
                    "path" => r.path = value(v).to_string(),
                    "identifier" => r.id = Some(value(v).to_string()),
                    "name" => r.name = Some(value(v).to_string()).filter(|n| !n.is_empty()),
                    "plugin Identifiers" => {
                        r.plugins = v.split(',').map(|p| value(p).to_string()).filter(|p| !p.is_empty()).collect();
                    }
                    "class" => r.app = v.contains("kLSBundleClassApplication"),
                    "container" => r.on_startup_disk = value(v) == "/",
                    "mount state" => *mounted = value(v) == "mounted",
                    _ => {}
                }
            }
        }
    }
    flush(&mut cur);
    out
}

fn open_with_issues() -> Vec<Issue> {
    let mut seen = HashSet::new();
    registered_apps()
        .into_iter()
        .filter(|r| r.app && r.on_startup_disk && seen.insert(r.path.clone()))
        .filter(|r| path_state(&r.path) == PathState::Missing)
        .map(|r| Issue {
            category: Category::OpenWith,
            detail: r.path.clone(),
            location: format!("Launch Services › {}", r.id.as_deref().unwrap_or("no bundle id")),
            target: Target::OpenWith(r.path),
        })
        .collect()
}

const RECEIPTS: &str = "/var/db/receipts";

/// Package ids pkgutil can put in a file name without surprises.
fn safe_package_id(id: &str) -> bool {
    !id.is_empty() && !id.starts_with('.') && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b))
}

fn receipt_issues() -> Vec<Issue> {
    use rayon::prelude::*;
    let Some(text) = super::output("/usr/sbin/pkgutil", &["--pkgs"]) else { return Vec::new() };
    let ids: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|id| safe_package_id(id) && !id.to_ascii_lowercase().starts_with("com.apple."))
        .map(str::to_string)
        .collect();
    let mut out: Vec<Issue> = ids.par_iter().filter_map(|id| stale_receipt(id)).collect();
    out.sort_by(|a, b| a.detail.cmp(&b.detail));
    out
}

fn stale_receipt(id: &str) -> Option<Issue> {
    let info = super::read_plist(&Path::new(RECEIPTS).join(format!("{id}.plist")));
    let dict = info.as_ref().and_then(|v| v.as_dictionary());
    let prefix = dict.and_then(|d| super::plist_str(d, "InstallPrefixPath")).unwrap_or_default();
    let version = dict.and_then(|d| super::plist_str(d, "PackageVersion")).unwrap_or_default();
    let base = Path::new("/").join(prefix.trim_start_matches(['/', '.']));
    let files = super::output("/usr/sbin/pkgutil", &["--only-files", "--files", id])?;
    let files: Vec<&str> = files.lines().filter(|l| !l.trim().is_empty()).collect();
    let dirs = || super::output("/usr/sbin/pkgutil", &["--only-dirs", "--files", id]).map(|t| t.lines().map(str::to_string).collect()).unwrap_or_default();
    if !payload_gone(&base, &files, dirs, |p| path_state(&p.to_string_lossy())) {
        return None;
    }
    Some(Issue {
        category: Category::Receipts,
        detail: format!("{id} {version} · {} files, all gone", crate::util::fmt_count(files.len() as u64)),
        location: format!("{RECEIPTS}/{id}.plist"),
        target: Target::Receipt(id.to_string()),
    })
}

/// Bundle folders: if one is still there, so is the software.
const BUNDLES: [&str; 17] = [
    "app", "bundle", "component", "vst", "vst3", "aaxplugin", "clap", "plugin", "framework", "kext", "prefpane",
    "appex", "dext", "systemextension", "qlgenerator", "mdimporter", "saver",
];

/// Whether everything a package installed is gone. Its plain folders don't
/// count (they're shared, like /Library/Audio/Plug-Ins), but bundles like
/// Foo.app or Foo.vst3 do. Packages that unpack into a temporary folder and
/// move things with a script can't be judged, so they never count.
fn payload_gone(base: &Path, files: &[&str], dirs: impl FnOnce() -> Vec<String>, state: impl Fn(&Path) -> PathState) -> bool {
    if files.is_empty() {
        return false;
    }
    let temp = ["/tmp/", "/private/tmp/", "/var/tmp/", "/private/var/tmp/", "/var/folders/", "/private/var/folders/"];
    for f in files {
        let full = normalize(&base.join(f.trim_start_matches("./")));
        if temp.iter().any(|t| full.to_string_lossy().starts_with(t)) || state(&full) != PathState::Missing {
            return false;
        }
    }
    dirs().iter().all(|d| {
        let is_bundle = Path::new(d).extension().is_some_and(|e| BUNDLES.contains(&e.to_string_lossy().to_ascii_lowercase().as_str()));
        !is_bundle || state(&base.join(d.trim_start_matches("./"))) == PathState::Missing
    })
}

// ----------------------------------------------------------------------
// Removing, with a backup first

impl Target {
    pub fn describe(&self) -> String {
        match self {
            Target::Job(j) => format!("{} {}", job_kind(j.domain), j.label),
            Target::LoginItem(i, _) => format!("Login item {}", i.name),
            Target::Link { path, .. } => format!("Link {}", path.display()),
            Target::OpenWith(p) => format!("Open With entry for {p}"),
            Target::Receipt(id) => format!("Receipt {id}"),
            Target::DockTile { label, url, .. } => format!("Dock icon {}", if label.is_empty() { url } else { label }),
        }
    }

    pub fn needs_admin(&self) -> bool {
        match self {
            Target::Job(j) => j.domain != Domain::UserAgent,
            Target::Link { path, .. } => !path.parent().is_some_and(writable),
            Target::Receipt(_) => true,
            Target::LoginItem(..) | Target::OpenWith(_) | Target::DockTile { .. } => false,
        }
    }

    /// Everything needed to put it back.
    fn save(&self) -> Result<Saved, String> {
        Ok(match self {
            Target::Job(j) => Saved::Job {
                path: j.plist.clone(),
                label: j.label.clone(),
                domain: j.domain,
                program: j.command(),
                contents: std::fs::read(&j.plist).map_err(|e| e.to_string())?,
            },
            Target::LoginItem(i, on) => Saved::LoginItem { item: i.clone(), on: *on },
            Target::Link { path, target } => {
                let now = std::fs::read_link(path).map_err(|e| e.to_string())?;
                if now.to_string_lossy() != target.as_str() {
                    return Err("the link changed since the scan".into());
                }
                Saved::Link { path: path.clone(), target: target.clone() }
            }
            Target::OpenWith(p) => Saved::OpenWith { path: p.clone() },
            Target::Receipt(id) => {
                if !safe_package_id(id) {
                    return Err("unexpected package id".into());
                }
                let mut files = Vec::new();
                for ext in ["plist", "bom"] {
                    let name = format!("{id}.{ext}");
                    match std::fs::read(Path::new(RECEIPTS).join(&name)) {
                        Ok(b) => files.push((name, b)),
                        Err(e) if ext == "plist" => return Err(e.to_string()),
                        Err(_) => {}
                    }
                }
                Saved::Receipt { id: id.clone(), files }
            }
            Target::DockTile { key, guid, url, label } => {
                let prefs = dock_prefs().ok_or("the Dock's settings couldn't be read")?;
                let list = prefs.as_dictionary().and_then(|d| d.get(key)).and_then(|v| v.as_array()).ok_or("it's no longer in the Dock")?;
                let index = find_tile(list, *guid, url).ok_or("it's no longer in the Dock")?;
                Saved::DockTile { key: key.clone(), index, tile: list[index].clone(), label: label.clone(), url: url.clone() }
            }
        })
    }

    /// The part that runs as root, if any.
    fn admin_command(&self) -> Result<Option<String>, String> {
        Ok(match self {
            Target::Job(j) if j.domain != Domain::UserAgent => {
                if j.plist.parent() != j.domain.folder().as_deref() {
                    return Err("unexpected location".into());
                }
                let rm = format!("/bin/rm -f -- {}", sh_quote(&j.plist.to_string_lossy()));
                Some(if j.domain == Domain::Daemon {
                    format!("/bin/launchctl bootout {} 2>/dev/null; {rm}", sh_quote(&format!("system/{}", j.label)))
                } else {
                    rm
                })
            }
            Target::Link { path, .. } if self.needs_admin() => {
                let p = sh_quote(&path.to_string_lossy());
                Some(format!("if [ -L {p} ]; then /bin/rm -f -- {p}; else echo 'it is no longer a link' >&2; false; fi"))
            }
            Target::Receipt(id) => Some(format!("/usr/sbin/pkgutil --forget {}", sh_quote(id))),
            _ => None,
        })
    }

    /// The part Heft does itself, after the root part succeeded.
    fn user_step(&self) -> Result<(), String> {
        match self {
            Target::Job(j) => {
                if j.domain == Domain::UserAgent {
                    crate::platform::move_to_trash(&j.plist)?;
                }
                if j.domain != Domain::Daemon {
                    // Stop it if it's loaded in this session; fine if it isn't.
                    let _ = startup::launchctl(&["bootout", &format!("gui/{}/{}", super::uid(), j.label)]);
                }
                Ok(())
            }
            Target::LoginItem(i, true) => startup::remove_login_item(i),
            Target::LoginItem(i, false) => startup::remember_off(i, false),
            Target::Link { path, .. } if !self.needs_admin() => {
                if !std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
                    return Err("it is no longer a link".into());
                }
                std::fs::remove_file(path).map_err(|e| e.to_string())
            }
            Target::OpenWith(p) => {
                let out = Command::new(LSREGISTER).args(["-u", p]).output().map_err(|e| e.to_string())?;
                if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
            }
            // The Dock is restarted once, after all of them (see `remove`).
            Target::DockTile { key, guid, url, .. } => edit_dock(|d| {
                let list = d.get_mut(key).and_then(|v| v.as_array_mut()).ok_or("it's no longer in the Dock")?;
                if take_tiles(list, &[(*guid, url.clone())]).is_empty() {
                    return Err("it's no longer in the Dock".into());
                }
                Ok(())
            }),
            _ => Ok(()),
        }
    }
}

fn job_kind(d: Domain) -> &'static str {
    match d {
        Domain::UserAgent | Domain::Agent => "Launch agent",
        Domain::Daemon => "Launch daemon",
    }
}

#[derive(Debug, Default)]
pub struct FixReport {
    pub fixed: usize,
    pub failed: Vec<String>,
    pub backup: Option<PathBuf>,
}

/// Fix the given issues: back up, then remove.
pub fn fix(issues: &[Issue]) -> Result<FixReport, String> {
    let targets: Vec<Target> = issues.iter().map(|i| i.target.clone()).collect();
    remove(&targets, "broken-items", "remove broken items")
}

/// Back up, then remove. Nothing is changed if the backup can't be written;
/// an item that can't be backed up is left alone. `why` finishes "Heft wants
/// to …" in the password prompt, if one is needed.
pub fn remove(targets: &[Target], prefix: &str, why: &str) -> Result<FixReport, String> {
    let mut report = FixReport::default();
    let mut saved = Vec::new();
    let mut ready = Vec::new();
    for t in targets {
        match t.save() {
            Ok(s) => {
                saved.push(s);
                ready.push(t);
            }
            Err(e) => report.failed.push(format!("{}: couldn't back it up ({e}), so it was left alone", t.describe())),
        }
    }
    if saved.is_empty() {
        return Ok(report);
    }
    let file = super::new_backup_path(prefix, "plist");
    write_backup(&file, &saved).map_err(|e| format!("could not save a backup, nothing was changed: {e}"))?;
    report.backup = Some(file);
    let results = execute(&ready, |t| t.admin_command(), |t| t.user_step(), why);
    let mut dock = false;
    for (t, r) in ready.iter().zip(results) {
        match r {
            Ok(()) => {
                report.fixed += 1;
                dock |= matches!(t, Target::DockTile { .. });
            }
            Err(e) => report.failed.push(format!("{}: {e}", t.describe())),
        }
    }
    if dock {
        restart_dock();
    }
    Ok(report)
}

/// Run every item's root part under one password prompt, then the user
/// parts of the items whose root part worked (or that had none).
fn execute<T>(
    items: &[T],
    admin: impl Fn(&T) -> Result<Option<String>, String>,
    user: impl Fn(&T) -> Result<(), String>,
    why: &str,
) -> Vec<Result<(), String>> {
    let mut results: Vec<Option<Result<(), String>>> = items.iter().map(|_| None).collect();
    let mut commands = Vec::new();
    for (i, t) in items.iter().enumerate() {
        match admin(t) {
            Ok(Some(c)) => commands.push((i, c)),
            Ok(None) => {}
            Err(e) => results[i] = Some(Err(e)),
        }
    }
    if !commands.is_empty() {
        match super::run_as_admin(&admin_script(&commands), why) {
            Ok(out) => {
                let mut done = parse_admin_output(&out);
                for (i, _) in &commands {
                    let r = done.remove(i).unwrap_or_else(|| Err("it didn't run".into()));
                    if r.is_err() {
                        results[*i] = Some(r);
                    }
                }
            }
            Err(e) => {
                let e = if e == super::CANCELLED { "the password prompt was cancelled".to_string() } else { e };
                for (i, _) in &commands {
                    results[*i] = Some(Err(e.clone()));
                }
            }
        }
    }
    items.iter().zip(results).map(|(t, r)| r.unwrap_or_else(|| user(t))).collect()
}

/// One shell script for several root commands. Each reports on its own line
/// (`heft-ok 3`, `heft-fail 4 <first line of its error>`), so one failure
/// doesn't hide the others' results.
fn admin_script(commands: &[(usize, String)]) -> String {
    commands
        .iter()
        .map(|(i, c)| {
            format!("if out=$( {{ {c} ; }} 2>&1 ); then echo 'heft-ok {i}'; else echo \"heft-fail {i} $(printf '%s' \"$out\" | /usr/bin/head -n 1)\"; fi")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn parse_admin_output(text: &str) -> HashMap<usize, Result<(), String>> {
    text.split(['\r', '\n'])
        .filter_map(|l| {
            let l = l.trim();
            if let Some(i) = l.strip_prefix("heft-ok ") {
                return Some((i.trim().parse().ok()?, Ok(())));
            }
            let rest = l.strip_prefix("heft-fail ")?;
            let (i, msg) = rest.split_once(' ').unwrap_or((rest, ""));
            let msg = msg.trim();
            Some((i.parse().ok()?, Err(if msg.is_empty() { "it failed".to_string() } else { msg.to_string() })))
        })
        .collect()
}

// ----------------------------------------------------------------------
// Backups

/// One thing Heft changed, with what it takes to put it back.
#[derive(Clone, Debug, PartialEq)]
pub enum Saved {
    /// A launch agent's or daemon's .plist.
    Job { path: PathBuf, label: String, domain: Domain, program: String, contents: Vec<u8> },
    /// A login item, on the real list (`on`) or remembered as turned off.
    LoginItem { item: LoginItem, on: bool },
    Link { path: PathBuf, target: String },
    /// An Open With registration. Nothing to restore: macOS registers an app
    /// again by itself when it shows up.
    OpenWith { path: String },
    /// A package receipt's files from /var/db/receipts.
    Receipt { id: String, files: Vec<(String, Vec<u8>)> },
    /// A Dock icon exactly as the Dock had it, and where.
    DockTile { key: String, index: usize, tile: plist::Value, label: String, url: String },
}

const MARKER: &str = "HeftBackup";

fn domain_key(d: Domain) -> &'static str {
    match d {
        Domain::UserAgent => "UserAgent",
        Domain::Agent => "Agent",
        Domain::Daemon => "Daemon",
    }
}

impl Saved {
    fn to_value(&self) -> plist::Value {
        let mut d = plist::Dictionary::new();
        let mut put = |k: &str, v: plist::Value| {
            d.insert(k.to_string(), v);
        };
        match self {
            Saved::Job { path, label, domain, program, contents } => {
                put("Kind", "LaunchJob".into());
                put("Path", path.to_string_lossy().into_owned().into());
                put("Label", label.clone().into());
                put("Domain", domain_key(*domain).into());
                put("Program", program.clone().into());
                put("Contents", plist::Value::Data(contents.clone()));
            }
            Saved::LoginItem { item, on } => {
                put("Kind", "LoginItem".into());
                put("Item", startup::login_item_value(item));
                put("On", (*on).into());
            }
            Saved::Link { path, target } => {
                put("Kind", "Link".into());
                put("Path", path.to_string_lossy().into_owned().into());
                put("Target", target.clone().into());
            }
            Saved::OpenWith { path } => {
                put("Kind", "OpenWith".into());
                put("Path", path.clone().into());
            }
            Saved::DockTile { key, index, tile, label, url } => {
                put("Kind", "DockTile".into());
                put("Key", key.clone().into());
                put("Index", (*index as i64).into());
                put("Tile", tile.clone());
                put("Label", label.clone().into());
                put("Url", url.clone().into());
            }
            Saved::Receipt { id, files } => {
                put("Kind", "Receipt".into());
                put("Id", id.clone().into());
                let mut f = plist::Dictionary::new();
                for (name, bytes) in files {
                    f.insert(name.clone(), plist::Value::Data(bytes.clone()));
                }
                put("Files", plist::Value::Dictionary(f));
            }
        }
        plist::Value::Dictionary(d)
    }

    fn from_value(v: &plist::Value) -> Option<Saved> {
        let d = v.as_dictionary()?;
        let s = |k: &str| d.get(k).and_then(|v| v.as_string()).map(str::to_string);
        Some(match s("Kind")?.as_str() {
            "LaunchJob" => Saved::Job {
                path: PathBuf::from(s("Path")?),
                label: s("Label")?,
                domain: match s("Domain")?.as_str() {
                    "UserAgent" => Domain::UserAgent,
                    "Agent" => Domain::Agent,
                    "Daemon" => Domain::Daemon,
                    _ => return None,
                },
                program: s("Program").unwrap_or_default(),
                contents: d.get("Contents")?.as_data()?.to_vec(),
            },
            "LoginItem" => Saved::LoginItem {
                item: startup::login_item_from(d.get("Item")?)?,
                on: super::plist_bool(d, "On").unwrap_or(true),
            },
            "Link" => Saved::Link { path: PathBuf::from(s("Path")?), target: s("Target")? },
            "OpenWith" => Saved::OpenWith { path: s("Path")? },
            "DockTile" => Saved::DockTile {
                key: s("Key").filter(|k| DOCK_LISTS.contains(&k.as_str()))?,
                index: d.get("Index").and_then(|v| v.as_signed_integer()).and_then(|i| usize::try_from(i).ok()).unwrap_or(usize::MAX),
                tile: d.get("Tile").filter(|t| tile(t).is_some())?.clone(),
                label: s("Label").unwrap_or_default(),
                url: s("Url")?,
            },
            "Receipt" => Saved::Receipt {
                id: s("Id")?,
                files: d
                    .get("Files")?
                    .as_dictionary()?
                    .iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_data()?.to_vec())))
                    .collect(),
            },
            _ => return None,
        })
    }

    pub fn describe(&self) -> String {
        match self {
            Saved::Job { label, domain, program, .. } => format!("{} {label} ({program})", job_kind(*domain)),
            Saved::LoginItem { item, on: true } => format!("Login item {} ({})", item.name, item.path),
            Saved::LoginItem { item, on: false } => format!("Login item {}, turned off ({})", item.name, item.path),
            Saved::Link { path, target } => format!("Link {} → {target}", path.display()),
            Saved::OpenWith { path } => format!("Open With entry for {path}"),
            Saved::Receipt { id, .. } => format!("Package receipt {id}"),
            Saved::DockTile { label, url, .. } => format!("Dock icon {}", if label.is_empty() { url } else { label }),
        }
    }

    pub fn needs_admin(&self) -> bool {
        match self {
            Saved::Job { domain, .. } => *domain != Domain::UserAgent,
            Saved::Link { path, .. } => !path.parent().is_some_and(writable),
            Saved::Receipt { .. } => true,
            Saved::LoginItem { .. } | Saved::OpenWith { .. } | Saved::DockTile { .. } => false,
        }
    }

    /// Why it can't be put back, if it can't.
    pub fn blocker(&self) -> Option<String> {
        let taken = |p: &Path| std::fs::symlink_metadata(p).is_ok();
        match self {
            Saved::Job { path, domain, .. } => {
                if path.parent() != domain.folder().as_deref() || path.extension().is_none_or(|e| e != "plist") {
                    Some("it isn't in a launch agents or daemons folder".into())
                } else if taken(path) {
                    Some("a file with that name is already there".into())
                } else {
                    None
                }
            }
            Saved::LoginItem { item, on: true } => {
                if path_state(&item.path) != PathState::Exists {
                    Some("the app isn't there, so it can't open at login until it's reinstalled".into())
                } else if startup::login_items(false).is_ok_and(|v| v.iter().any(|i| i.path == item.path)) {
                    Some("it's already a login item".into())
                } else {
                    None
                }
            }
            Saved::LoginItem { .. } => None,
            Saved::Link { path, .. } => {
                if !path.parent().is_some_and(|d| link_folders().iter().any(|f| f == d)) {
                    Some("it isn't in a folder Heft manages links in".into())
                } else if taken(path) {
                    Some("something with that name is already there".into())
                } else {
                    None
                }
            }
            Saved::OpenWith { .. } => Some("nothing to restore: macOS registers the app again by itself if it comes back".into()),
            Saved::DockTile { key, url, .. } => {
                let there = dock_prefs()
                    .and_then(|p| p.as_dictionary().and_then(|d| d.get(key)).and_then(|v| v.as_array()).map(|l| find_tile(l, None, url).is_some()));
                match there {
                    None => Some("the Dock's settings couldn't be read".into()),
                    Some(true) => Some("it's already in the Dock".into()),
                    Some(false) => None,
                }
            }
            Saved::Receipt { id, files } => {
                if !safe_package_id(id) || files.iter().any(|(n, _)| n != &format!("{id}.plist") && n != &format!("{id}.bom")) {
                    Some("unexpected receipt files".into())
                } else if taken(&Path::new(RECEIPTS).join(format!("{id}.plist"))) {
                    Some("the receipt is already there".into())
                } else {
                    None
                }
            }
        }
    }

    fn admin_command(&self, stage: &Result<Stage, String>) -> Result<Option<String>, String> {
        if !self.needs_admin() {
            return Ok(None);
        }
        Ok(Some(match self {
            Saved::Job { path, contents, .. } => stage.as_ref().map_err(Clone::clone)?.put(contents, path)?,
            Saved::Link { path, target } => {
                let p = sh_quote(&path.to_string_lossy());
                format!("if [ -e {p} ] || [ -L {p} ]; then echo 'something is already there' >&2; false; else /bin/ln -s {} {p}; fi", sh_quote(target))
            }
            Saved::Receipt { files, .. } => {
                let stage = stage.as_ref().map_err(Clone::clone)?;
                let parts: Result<Vec<String>, String> =
                    files.iter().map(|(name, bytes)| stage.put(bytes, &Path::new(RECEIPTS).join(name))).collect();
                parts?.iter().map(|c| format!("{{ {c} ; }}")).collect::<Vec<_>>().join(" && ")
            }
            _ => return Ok(None),
        }))
    }

    fn user_step(&self) -> Result<(), String> {
        if self.needs_admin() {
            return Ok(());
        }
        match self {
            Saved::Job { path, contents, .. } => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                }
                let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| e.to_string())?;
                f.write_all(contents).map_err(|e| e.to_string())
            }
            Saved::LoginItem { item, on: true } => startup::add_login_item(&item.path, item.hidden),
            Saved::LoginItem { item, on: false } => startup::remember_off(item, true),
            Saved::Link { path, target } => std::os::unix::fs::symlink(target, path).map_err(|e| e.to_string()),
            // The Dock is restarted once, after all of them (see `restore`).
            Saved::DockTile { key, index, tile, .. } => edit_dock(|d| {
                if !d.contains_key(key) {
                    d.insert(key.clone(), plist::Value::Array(Vec::new()));
                }
                let list = d.get_mut(key).and_then(|v| v.as_array_mut()).ok_or("the Dock's settings aren't what Heft expected")?;
                list.insert((*index).min(list.len()), tile.clone());
                Ok(())
            }),
            _ => Ok(()),
        }
    }
}

fn write_backup(path: &Path, items: &[Saved]) -> Result<(), String> {
    let mut d = plist::Dictionary::new();
    // First, so the Backups list can recognize the file from its start.
    d.insert(MARKER.into(), plist::Value::Integer(1.into()));
    d.insert("Created".into(), plist::Value::Integer(crate::platform::now_unix().into()));
    d.insert("Items".into(), plist::Value::Array(items.iter().map(Saved::to_value).collect()));
    plist::Value::Dictionary(d).to_file_xml(path).map_err(|e| e.to_string())
}

/// What a backup file holds.
pub fn read_backup(path: &Path) -> Result<Vec<Saved>, String> {
    let v = super::read_plist(path).ok_or("Heft can't read this file")?;
    let d = v.as_dictionary().filter(|d| d.contains_key(MARKER)).ok_or("this isn't a Heft backup")?;
    Ok(d.get("Items").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(Saved::from_value).collect()).unwrap_or_default())
}

#[derive(Clone, Debug)]
pub struct Backup {
    pub path: PathBuf,
    pub modified: i64,
    pub size: u64,
}

/// Saved backups, newest first.
pub fn backups() -> Vec<Backup> {
    backups_in(&super::backup_dir())
}

fn backups_in(dir: &Path) -> Vec<Backup> {
    use std::io::Read;
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let is_ours = |p: &Path| {
        let mut head = [0u8; 1024];
        let n = std::fs::File::open(p).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
        String::from_utf8_lossy(&head[..n]).contains(&format!("<key>{MARKER}</key>"))
    };
    let mut v: Vec<Backup> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "plist") && is_ours(&e.path()))
        .filter_map(|e| {
            let md = e.metadata().ok()?;
            let modified = md.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
            Some(Backup { path: e.path(), modified, size: md.len() })
        })
        .collect();
    v.sort_by_key(|b| std::cmp::Reverse(b.modified));
    v
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    pub restored: usize,
    pub failed: Vec<String>,
}

/// Put back what a backup holds, except items with a [`Saved::blocker`].
pub fn restore(items: &[Saved]) -> RestoreReport {
    let ready: Vec<&Saved> = items.iter().filter(|s| s.blocker().is_none()).collect();
    let stage = if ready.iter().any(|s| s.needs_admin()) { Stage::new() } else { Err("not needed".into()) };
    let results = execute(&ready, |s| s.admin_command(&stage), |s| s.user_step(), "put back items from a backup");
    let mut report = RestoreReport::default();
    let mut dock = false;
    for (s, r) in ready.iter().zip(results) {
        match r {
            Ok(()) => {
                report.restored += 1;
                dock |= matches!(s, Saved::DockTile { .. });
            }
            Err(e) => report.failed.push(format!("{}: {e}", s.describe())),
        }
    }
    if dock {
        restart_dock();
    }
    report
}

/// Files for the administrator command to put in place. They're written to
/// a private temporary folder, and checked as root against a SHA-256 of what
/// Heft meant to write before they're moved into place, so nothing can be
/// swapped in between.
struct Stage {
    dir: PathBuf,
    count: std::cell::Cell<usize>,
}

impl Stage {
    fn new() -> Result<Stage, String> {
        use std::os::unix::fs::DirBuilderExt;
        let dir = std::env::temp_dir().join(format!("heft-restore-{}-{}", std::process::id(), crate::platform::now_unix()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|e| e.to_string())?;
        Ok(Stage { dir, count: std::cell::Cell::new(0) })
    }

    /// A shell command that copies `bytes` to `dest` as root, owned by root
    /// and readable by everyone, unless something is already there.
    fn put(&self, bytes: &[u8], dest: &Path) -> Result<String, String> {
        let n = self.count.get();
        self.count.set(n + 1);
        let src = self.dir.join(format!("{n}.part"));
        std::fs::write(&src, bytes).map_err(|e| e.to_string())?;
        let hash = sha256(bytes)?;
        let d = sh_quote(&dest.to_string_lossy());
        let t = sh_quote(&format!("{}.heft-new", dest.to_string_lossy()));
        let s = sh_quote(&src.to_string_lossy());
        Ok(format!(
            "if [ -e {d} ] || [ -L {d} ]; then echo 'a file with that name is already there' >&2; false; \
             elif (umask 077; /bin/cp -X {s} {t}) && [ \"$(/usr/bin/shasum -a 256 < {t} | /usr/bin/cut -c1-64)\" = {hash} ]; \
             then /usr/sbin/chown root:wheel {t} && /bin/chmod 644 {t} && /bin/mv -f {t} {d}; \
             else /bin/rm -f {t}; echo 'the copy did not match the backup' >&2; false; fi"
        ))
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// SHA-256 of `bytes`, in hex, from the same tool the root side checks with.
fn sha256(bytes: &[u8]) -> Result<String, String> {
    let mut child = Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    // shasum reads everything before it writes, so this can't deadlock.
    child.stdin.take().ok_or("no stdin")?.write_all(bytes).map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.get(..64).filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit())).map(str::to_string).ok_or_else(|| "shasum failed".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("heft-broken-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn provably_missing() {
        let d = temp("state");
        let file = d.join("there.app");
        std::fs::write(&file, b"x").unwrap();
        let s = |p: &Path| path_state(&p.to_string_lossy());
        assert_eq!(s(&file), PathState::Exists);
        assert_eq!(s(&d.join("gone.app")), PathState::Missing);
        assert_eq!(s(&d.join("gone/deeper/x")), PathState::Missing);
        assert_eq!(path_state("relative/x"), PathState::Unknown);
        assert_eq!(path_state(""), PathState::Unknown);
        assert_eq!(path_state("/Volumes/Heft No Such Disk/A.app"), PathState::Unknown, "disks come and go");
        assert_eq!(path_state("/Users/../Volumes/Heft No Such Disk/A.app"), PathState::Unknown);
        assert_eq!(path_state("/Network/Servers/x/A.app"), PathState::Unknown);

        // A link to a missing file is missing; a link to a disk that isn't there isn't.
        let dangling = d.join("dangling");
        std::os::unix::fs::symlink(d.join("nowhere"), &dangling).unwrap();
        assert_eq!(s(&dangling), PathState::Missing);
        let to_disk = d.join("apps");
        std::os::unix::fs::symlink("/Volumes/Heft No Such Disk/Apps", &to_disk).unwrap();
        assert_eq!(s(&to_disk.join("A.app")), PathState::Unknown);
        let relative = d.join("rel");
        std::os::unix::fs::symlink("../heft-no-such-sibling", &relative).unwrap();
        assert_eq!(s(&relative.join("x")), PathState::Missing);

        // A folder Heft can't read isn't evidence of anything.
        use std::os::unix::fs::PermissionsExt;
        let locked = d.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if !crate::platform::is_elevated() {
            assert_eq!(s(&locked.join("A.app")), PathState::Unknown);
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn links_to_missing_files() {
        let d = temp("links");
        std::fs::write(d.join("real"), b"x").unwrap();
        std::os::unix::fs::symlink(d.join("real"), d.join("ok")).unwrap();
        std::os::unix::fs::symlink(d.join("gone-tool"), d.join("gone")).unwrap();
        std::os::unix::fs::symlink("../Cellar/foo/1.0/bin/foo", d.join("brew")).unwrap();
        std::os::unix::fs::symlink("/opt/homebrew/bin/bar", d.join("brew2")).unwrap();
        std::os::unix::fs::symlink("/Volumes/Heft No Such Disk/tool", d.join("external")).unwrap();
        std::fs::write(d.join("plain"), b"x").unwrap();
        let found = broken_links_in(&d);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].location, d.join("gone").to_string_lossy());
        assert!(!found[0].needs_admin(), "a folder you own needs no password");
        assert!(managed_elsewhere(Path::new("/usr/local/bin/../Cellar/x/bin/x")));
        assert!(!managed_elsewhere(Path::new("/Applications/Foo.app/Contents/MacOS/foo")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn lsregister_dump() {
        let text = "Checking data integrity...\n...done.\nStatus: Preferences are loaded.\n\
--------------------------------------------------------------------------------\n\
bundle id:                  GoogleUpdater (0x980)\n\
Bundle node not found on disk: Error Domain=NSOSStatusErrorDomain Code=-43 \"fnfErr: File not found\"\n\
container:                  / (0x4)\n\
mount state:                mounted\n\
path:                       /Users/x/Library/Application Support/Google/GoogleUpdater/151.0/GoogleUpdater.app (0x1938)\n\
name:                       GoogleUpdater\n\
identifier:                 com.google.GoogleUpdater\n\
plugin Identifiers:         com.google.GoogleUpdater.a, com.google.GoogleUpdater.b (0x44)\n\
infoDictionary:             13 values (25336 (0x62f8))\n\
                            {\n\
                                path = \"/nested/should/be/ignored\";\n\
                            }\n\
class:                      kLSBundleClassApplication (0x2)\n\
--------------------------------------------------------------------------------\n\
bundle id:                  RobloxStudioInstaller (0x21cc)\n\
container:                  /Volumes/RobloxStudioInstaller (0x1c)\n\
mount state:                not mounted\n\
path:                       /Volumes/RobloxStudioInstaller/RobloxStudioInstaller.app (0x21cc)\n\
class:                      kLSBundleClassApplication (0x2)\n\
--------------------------------------------------------------------------------\n\
bundle id:                  CoreTypes (0x4)\n\
container:                  / (0x4)\n\
path:                       /System/Library/CoreServices/CoreTypes.bundle (0x10)\n\
class:                      kLSBundleClassCoreTypes (0xb)\n\
--------------------------------------------------------------------------------\n\
claim id:                   something (0x5)\n\
path:                       /not/a/bundle\n";
        let r = parse_lsregister(text);
        assert_eq!(r.len(), 3);
        assert_eq!(
            r[0],
            Registered {
                path: "/Users/x/Library/Application Support/Google/GoogleUpdater/151.0/GoogleUpdater.app".into(),
                id: Some("com.google.GoogleUpdater".into()),
                name: Some("GoogleUpdater".into()),
                plugins: vec!["com.google.GoogleUpdater.a".into(), "com.google.GoogleUpdater.b".into()],
                app: true,
                on_startup_disk: true,
                mounted: true,
            }
        );
        assert!(r[1].app && !r[1].on_startup_disk && !r[1].mounted, "unmounted disk images don't count");
        assert!(!r[2].app);
    }

    #[test]
    fn package_payloads() {
        // What's on the disk, as far as this test is concerned.
        let there = ["/Library/Plug-Ins/present.txt", "/Library/Plug-Ins/Kept.vst3"];
        let state = |p: &Path| if there.iter().any(|t| Path::new(t) == p) { PathState::Exists } else { PathState::Missing };
        let base = Path::new("/Library");
        let none = || Vec::new();
        assert!(payload_gone(base, &["Plug-Ins/Gone.vst3/Contents/Info.plist", "./gone.txt"], none, state));
        assert!(!payload_gone(base, &["Plug-Ins/Gone.vst3/x", "Plug-Ins/present.txt"], none, state), "one file left is enough");
        assert!(!payload_gone(base, &[], none, state), "nothing to judge");
        assert!(
            !payload_gone(base, &["Plug-Ins/Kept.vst3/Contents/MacOS/gone"], || vec!["Plug-Ins".into(), "Plug-Ins/Kept.vst3".into()], state),
            "the bundle is still there"
        );
        assert!(
            payload_gone(base, &["Plug-Ins/Gone.vst3/x"], || vec!["Plug-Ins".into(), "Plug-Ins/Gone.vst3".into()], state),
            "shared folders don't count"
        );
        assert!(!payload_gone(Path::new("/"), &["private/tmp/payload.dmg"], none, state), "temporary payloads can't be judged");
        let unknown = |_: &Path| PathState::Unknown;
        assert!(!payload_gone(base, &["Plug-Ins/Gone.vst3/x"], none, unknown), "only provably missing files count");
        assert!(safe_package_id("com.fabfilter.Pro-Q.3"));
        assert!(!safe_package_id("../etc/x") && !safe_package_id("a b"));
    }

    #[test]
    fn admin_batch() {
        let script = admin_script(&[(0, "/bin/rm -f -- '/x'".into()), (2, "false".into())]);
        assert!(script.contains("heft-ok 0") && script.contains("heft-fail 2"));
        // Run it as the current user (no prompt) to check the shell side.
        let out = Command::new("/bin/sh").args(["-c", &script.replace("/bin/rm -f -- '/x'", "true")]).output().unwrap();
        let r = parse_admin_output(&String::from_utf8_lossy(&out.stdout).replace('\n', "\r"));
        assert_eq!(r.get(&0), Some(&Ok(())));
        assert_eq!(r.get(&2), Some(&Err("it failed".to_string())));
        let r = parse_admin_output("heft-ok 1\rheft-fail 3 rm: /x: Operation not permitted\r");
        assert_eq!(r.get(&3), Some(&Err("rm: /x: Operation not permitted".to_string())));
    }

    #[test]
    fn staged_copy_checks_its_hash() {
        assert_eq!(sha256(b"abc").unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        // The root command, run without root: chown fails, but the copy and
        // the hash check are exercised, and a mismatch leaves nothing behind.
        let d = temp("stage");
        let stage = Stage::new().unwrap();
        let dest = d.join("com.heftdev.test.plist");
        let cmd = stage.put(b"<plist/>", &dest).unwrap().replace("/usr/sbin/chown root:wheel", "/usr/bin/true");
        assert!(Command::new("/bin/sh").args(["-c", &cmd]).status().unwrap().success());
        assert_eq!(std::fs::read(&dest).unwrap(), b"<plist/>");
        assert!(!Command::new("/bin/sh").args(["-c", &cmd]).status().unwrap().success(), "never overwrites");
        let other = d.join("other.plist");
        let cmd = stage.put(b"good", &other).unwrap().replace("/usr/sbin/chown root:wheel", "/usr/bin/true");
        std::fs::write(stage.dir.join("1.part"), b"swapped").unwrap();
        assert!(!Command::new("/bin/sh").args(["-c", &cmd]).status().unwrap().success());
        assert!(!other.exists() && !d.join("other.plist.heft-new").exists());
        drop(stage);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A user launch agent (in a temporary folder, never loaded): back it up,
    /// read the backup, and put it back.
    #[test]
    fn dock_tiles() {
        use plist::{Dictionary, Value};
        let make = |guid: i64, url: &str, label: &str, book: Option<&[u8]>| {
            let mut file = Dictionary::new();
            file.insert("_CFURLString".into(), url.into());
            file.insert("_CFURLStringType".into(), Value::Integer(15.into()));
            let mut data = Dictionary::new();
            data.insert("file-data".into(), Value::Dictionary(file));
            data.insert("file-label".into(), label.into());
            if let Some(b) = book {
                data.insert("book".into(), Value::Data(b.to_vec()));
            }
            let mut t = Dictionary::new();
            t.insert("GUID".into(), Value::Integer(guid.into()));
            t.insert("tile-data".into(), Value::Dictionary(data));
            t.insert("tile-type".into(), "file-tile".into());
            Value::Dictionary(t)
        };
        let d = temp("dock");
        let here = d.join("Here.app");
        std::fs::create_dir_all(&here).unwrap();
        let url = |p: &Path| format!("file://{}/", p.to_string_lossy().replace(' ', "%20"));
        let apps = vec![
            make(1, &url(&here), "Here", None),
            make(2, &url(&d.join("Gone App.app")), "Gone App", None),
            // Moved: the path is stale, but the bookmark still finds it.
            make(3, &url(&d.join("Old Place.app")), "Moved", Some(b"moved")),
            make(4, &url(&d.join("Also Gone.app")), "Also Gone", Some(b"nowhere")),
        ];
        let others = vec![make(5, "file:///Volumes/Unplugged/Stuff/", "Stuff", None)];
        let mut prefs = Dictionary::new();
        prefs.insert("persistent-apps".into(), Value::Array(apps));
        prefs.insert("persistent-others".into(), Value::Array(others));
        let prefs = Value::Dictionary(prefs);
        let here_s = here.to_string_lossy().into_owned();
        let resolve = |b: &[u8]| (b == b"moved").then(|| here_s.clone());
        let found = dock_issues_in(&prefs, resolve);
        let labels: Vec<&str> = found.iter().map(|i| i.location.as_str()).collect();
        assert_eq!(labels, ["Dock › Gone App", "Dock › Also Gone"], "moved apps and unplugged disks aren't gone");
        assert_eq!(found[0].detail, d.join("Gone App.app").to_string_lossy());

        // Taking one out and putting it back where it was.
        let mut list = prefs.as_dictionary().unwrap().get("persistent-apps").unwrap().as_array().unwrap().clone();
        let Target::DockTile { guid, url: gone_url, .. } = &found[0].target else { panic!() };
        let taken = take_tiles(&mut list, &[(*guid, gone_url.clone())]);
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].0, 1);
        assert_eq!(list.len(), 3);
        assert!(find_tile(&list, None, gone_url).is_none());
        let saved = Saved::DockTile { key: "persistent-apps".into(), index: 1, tile: taken[0].1.clone(), label: "Gone App".into(), url: gone_url.clone() };
        let file = d.join("dock-backup.plist");
        write_backup(&file, std::slice::from_ref(&saved)).unwrap();
        assert_eq!(read_backup(&file).unwrap(), [saved]);
        assert_eq!(percent_decode("/Applications/My%20App.app"), "/Applications/My App.app");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn backup_round_trip() {
        let d = temp("backup");
        let plist = d.join("com.heftdev.test.agent.plist");
        std::fs::write(
            &plist,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>Label</key><string>com.heftdev.test.agent</string>
<key>ProgramArguments</key><array><string>/Applications/Heft No Such App.app/Contents/MacOS/x</string></array>
<key>RunAtLoad</key><true/></dict></plist>"#,
        )
        .unwrap();
        let job = super::super::parse_launch_job(&plist, Domain::UserAgent).unwrap();
        assert_eq!(path_state(&startup::job_target(&job).unwrap()), PathState::Missing);
        let item = LoginItem { name: "Gone".into(), path: "/Applications/Heft No Such App.app".into(), hidden: true };
        let link = d.join("tool");
        std::os::unix::fs::symlink("/Applications/Heft No Such App.app/x", &link).unwrap();
        let saved = vec![
            Target::Job(job).save().unwrap(),
            Target::LoginItem(item.clone(), true).save().unwrap(),
            Target::Link { path: link.clone(), target: "/Applications/Heft No Such App.app/x".into() }.save().unwrap(),
            Target::OpenWith("/Applications/Heft No Such App.app".into()).save().unwrap(),
            Saved::Receipt { id: "com.heftdev.test".into(), files: vec![("com.heftdev.test.plist".into(), b"<plist/>".to_vec())] },
        ];
        let file = d.join("broken-items-test.plist");
        write_backup(&file, &saved).unwrap();
        assert_eq!(read_backup(&file).unwrap(), saved);
        std::fs::write(d.join("unrelated.plist"), b"<plist><dict/></plist>").unwrap();
        let listed = backups_in(&d);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, file);

        // What can't come back says why.
        assert!(saved[1].blocker().unwrap().contains("isn't there"));
        assert!(saved[3].blocker().unwrap().starts_with("nothing to restore"));
        assert!(saved[0].blocker().is_some(), "not in a launch agents folder, so never written back");
        assert!(saved[2].blocker().is_some(), "not in a link folder");

        // The plist itself comes back byte for byte (written where it was).
        std::fs::remove_file(&plist).unwrap();
        if let Saved::Job { path, contents, .. } = &saved[0] {
            let s = Saved::Job { path: path.clone(), label: String::new(), domain: Domain::UserAgent, program: String::new(), contents: contents.clone() };
            s.user_step().unwrap();
            assert_eq!(&std::fs::read(&plist).unwrap(), contents);
            assert!(s.user_step().is_err(), "never overwrites");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Read-only: runs every check on this Mac.
    #[test]
    fn scans_without_changing_anything() {
        let issues = scan();
        for i in &issues {
            assert!(!i.detail.is_empty() && !i.location.is_empty());
            if i.category != Category::Receipts {
                assert_ne!(path_state(&i.detail), PathState::Exists, "{i:?}");
            }
        }
    }
}
