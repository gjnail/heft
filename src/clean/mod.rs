//! Junk cleaner: finds what each rule would remove ("analyze"), then deletes
//! exactly those files ("clean").
//!
//! Safety rules, in order of importance:
//! - Only locations named in the rule catalog are touched, and only after
//!   their `%VAR%` placeholders resolve to a real, deep-enough folder that
//!   isn't one of the user's or the system's top-level folders.
//! - Symlinks and junctions are never followed or deleted.
//! - Files that are in use are skipped, never forced. Rules whose program is
//!   running are skipped entirely.
//! - Cleaning deletes the files found by the analysis, not whatever happens
//!   to be in the folder later.
//! - System-owned files on macOS and Linux are left to the tool that owns
//!   them (apt, journalctl, snap, …); Heft only runs it. Where there is no
//!   such tool (macOS's system logs), root deletes exactly the files the
//!   analysis listed, and never a folder.
//!
//! Everything OS-specific lives in `windows.rs` / `unix.rs` (and `mac.rs`).

#[cfg(target_os = "macos")]
mod mac;
#[cfg(any(unix, test))]
mod parse;
pub mod rules;
#[cfg(unix)]
mod rules_unix;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
pub mod vdisk;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as os;
#[cfg(windows)]
use windows as os;

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

use rayon::prelude::*;

pub use os::{Special, SYSTEM};
#[cfg(target_os = "macos")]
pub use mac::{follow_move, has_full_disk_access, local_snapshots, THIN_SNAPSHOTS};
#[cfg(any(windows, target_os = "macos"))]
pub use os::{schedule_enabled, set_schedule};
use rules::{Rule, Target};

/// What one rule would clean.
#[derive(Clone, Debug, Default)]
pub struct Found {
    pub rule: usize,
    pub files: Vec<(PathBuf, u64)>,
    /// Folders that go once they're empty, deepest first.
    pub dirs: Vec<PathBuf>,
    pub bytes: u64,
    /// Files, plus Recycle Bin items, registry values, packages…
    pub items: u64,
    /// A program that must be closed first (exe name as listed in the rule).
    pub blocked_by: Option<&'static str>,
    /// The rule needs administrator rights and we don't have them.
    pub needs_admin: bool,
    /// What the rule's OS-specific targets would do.
    special: os::Pending,
}

impl Found {
    pub fn rule(&self) -> &'static Rule {
        &rules::all()[self.rule]
    }

    /// True if cleaning would do something.
    pub fn is_actionable(&self) -> bool {
        self.blocked_by.is_none() && !self.needs_admin && (self.items > 0 || self.special_only())
    }

    /// Clipboard / DNS: nothing to count, but still something to do.
    pub fn special_only(&self) -> bool {
        self.files.is_empty() && self.special.bare_action()
    }
}

/// Outcome of cleaning one rule.
#[derive(Clone, Debug, Default)]
pub struct Cleaned {
    pub rule: usize,
    pub freed: u64,
    pub removed: u64,
    /// Files that were in use or access was denied.
    pub skipped: u64,
    pub note: Option<String>,
}

#[derive(Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub freed: AtomicU64,
    pub cancel: AtomicBool,
    pub current: Mutex<String>,
}

/// Facts about the machine that analysis depends on, gathered once.
pub struct Env {
    pub running: HashSet<String>,
    pub elevated: bool,
    /// Admin-only rules can ask for a password themselves (Linux `pkexec`,
    /// macOS's administrator prompt).
    pub can_elevate: bool,
    /// A scheduled run with nobody watching: rules that would restart an app
    /// you can see (Finder) wait for a run you start yourself.
    pub unattended: bool,
    pub now: SystemTime,
}

/// Admin-only rules ask for the password when cleaning, rather than needing
/// Heft itself to run as administrator.
pub fn asks_password() -> bool {
    os::can_elevate_tools()
}

/// The first program in `close` that is running. Names may contain `*`,
/// because some programs carry their version in their name ("Adobe
/// Premiere Pro 2025").
fn running_one(close: &[&'static str], running: &HashSet<String>) -> Option<&'static str> {
    close.iter().copied().find(|exe| {
        if exe.contains('*') { running.iter().any(|r| wildcard(exe, r)) } else { running.contains(*exe) }
    })
}

impl Env {
    pub fn current() -> Env {
        Env {
            running: os::running_processes(),
            elevated: crate::platform::is_elevated(),
            can_elevate: os::can_elevate_tools(),
            unattended: false,
            now: SystemTime::now(),
        }
    }
}

// ----------------------------------------------------------------------
// Resolving locations

/// Replace `%VAR%` placeholders. `None` if any of them is unknown or the
/// result isn't a safe location.
pub fn resolve(template: &str) -> Option<PathBuf> {
    expand(template).filter(|p| safe_location(p))
}

/// Replace `%VAR%` placeholders, without the safety check. Only for a
/// folder that is listed, never cleaned itself (`EachDir`'s root).
fn expand(template: &str) -> Option<PathBuf> {
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after.find('%')?;
        out.push_str(&os::var(&after[..end])?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(PathBuf::from(out)).filter(|p| p.is_absolute())
}

/// Defence in depth against a bad rule or odd environment: the location must
/// be absolute, at least two folders deep, and not one of the user's or the
/// system's important folders (or a parent of one).
pub fn safe_location(p: &Path) -> bool {
    let mut comps = p.components();
    if cfg!(windows) && !matches!(comps.next(), Some(Component::Prefix(_))) {
        return false;
    }
    if !matches!(comps.next(), Some(Component::RootDir)) {
        return false;
    }
    let rest: Vec<Component> = comps.collect();
    if rest.len() < 2 || rest.iter().any(|c| !matches!(c, Component::Normal(_))) {
        return false;
    }
    // Compared without case everywhere: stricter than needed on Linux.
    let sep = crate::platform::SEP;
    let norm = |s: &str| s.trim_end_matches(sep).to_lowercase();
    let s = norm(&p.to_string_lossy());
    !os::important_folders().iter().map(|f| norm(f)).any(|f| f == s || f.starts_with(&format!("{s}{sep}")))
}

/// Case-insensitive `*` wildcard match.
pub fn wildcard(pattern: &str, name: &str) -> bool {
    let (p, n) = (pattern.to_lowercase(), name.to_lowercase());
    let parts: Vec<&str> = p.split('*').collect();
    if parts.len() == 1 {
        return p == n;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !n.starts_with(first) || !n[first.len()..].ends_with(last) || n.len() < first.len() + last.len() {
        return false;
    }
    let mut pos = first.len();
    let end = n.len() - last.len();
    for mid in &parts[1..parts.len() - 1] {
        match n[pos..end].find(mid) {
            Some(i) => pos += i + mid.len(),
            None => return false,
        }
    }
    true
}

/// Subfolders of `root`, never following links.
fn subdirs(root: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(root) else { return Vec::new() };
    rd.flatten()
        .filter(|e| e.metadata().is_ok_and(|m| m.is_dir() && !os::is_link(&m)))
        .map(|e| e.path())
        .collect()
}

/// Chromium profile folders in a user-data folder. Opera keeps its profile
/// in the user-data folder itself, so that's included too.
fn chromium_profiles(user_data: &Path) -> Vec<PathBuf> {
    let mut out = vec![user_data.to_path_buf()];
    for d in subdirs(user_data) {
        let name = d.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if name == "Default" || name.starts_with("Profile ") || name == "Guest Profile" || name == "System Profile" {
            out.push(d);
        }
    }
    out
}

// ----------------------------------------------------------------------
// Analysis

/// Newest of created and modified: a file copied in keeps its old modified
/// time, but gets a fresh creation time.
fn touched(md: &std::fs::Metadata) -> Option<SystemTime> {
    [md.modified().ok(), md.created().ok()].into_iter().flatten().max()
}

/// What can go from one folder.
#[derive(Default)]
struct Scan {
    files: Vec<(PathBuf, u64)>,
    dirs: Vec<PathBuf>,
    /// Something in the folder was touched since the cutoff.
    active: bool,
    /// The folder holds something Heft won't delete (a link, an unreadable
    /// entry), so the folder itself has to stay.
    pinned: bool,
}

/// Scan a folder in parallel.
///
/// With an age cutoff, folders are judged as a whole: a folder goes only if
/// nothing inside it is recent. Inside a folder that is still in use, only
/// its completely stale subfolders go. Loose files are left alone, because
/// a program that is using the folder may still need its older files. Loose
/// files directly in the target folder (`root`) are judged one by one.
fn scan_dir(dir: &Path, own: Option<SystemTime>, cutoff: Option<SystemTime>, root: bool, cancel: &AtomicBool) -> Scan {
    let fresh = |t: Option<SystemTime>| cutoff.is_some_and(|c| t.is_some_and(|t| t > c));
    let mut out = Scan { active: !root && fresh(own), ..Default::default() };
    if cancel.load(Ordering::Relaxed) {
        out.pinned = true;
        return out;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        out.pinned = true;
        return out;
    };
    let mut subdirs = Vec::new();
    let mut files = Vec::new();
    for e in rd.flatten() {
        // On Windows this comes from the directory listing itself and
        // describes the entry, not a link's target.
        let Ok(md) = e.metadata() else {
            out.pinned = true;
            continue;
        };
        if os::is_link(&md) || !(md.is_dir() || md.is_file()) {
            // Links are never followed or deleted, and neither are sockets
            // and pipes a running program may be listening on.
            out.pinned = true;
        } else if md.is_dir() {
            subdirs.push((e.path(), touched(&md)));
        } else {
            let recent = fresh(touched(&md));
            out.active |= recent;
            files.push((e.path(), md.len(), recent));
        }
    }
    let kids: Vec<(PathBuf, Scan)> = subdirs
        .into_par_iter()
        .map(|(p, t)| {
            let s = scan_dir(&p, t, cutoff, false, cancel);
            (p, s)
        })
        .collect();
    for (p, k) in kids {
        out.active |= k.active;
        out.pinned |= k.pinned;
        out.files.extend(k.files);
        out.dirs.extend(k.dirs);
        if !k.active && !k.pinned {
            out.dirs.push(p);
        }
    }
    let loose_ok = root || !out.active;
    out.files.extend(files.into_iter().filter(|f| loose_ok && !f.2).map(|(p, size, _)| (p, size)));
    out
}

struct Collector {
    cutoff: Option<SystemTime>,
    files: Vec<(PathBuf, u64)>,
    dirs: Vec<PathBuf>,
}

impl Collector {
    fn new(cutoff: Option<SystemTime>) -> Self {
        Collector { cutoff, files: Vec::new(), dirs: Vec::new() }
    }

    /// A file, or a folder's contents. The folder itself is only queued for
    /// removal when `remove_self` is set.
    fn add(&mut self, path: &Path, remove_self: bool, cancel: &AtomicBool) {
        let Ok(md) = std::fs::symlink_metadata(path) else { return };
        if os::is_link(&md) {
            return;
        }
        if md.is_dir() {
            let s = scan_dir(path, touched(&md), self.cutoff, !remove_self, cancel);
            self.files.extend(s.files);
            self.dirs.extend(s.dirs);
            if remove_self && !s.active && !s.pinned {
                self.dirs.push(path.to_path_buf());
            }
        } else if md.is_file() && self.cutoff.is_none_or(|c| touched(&md).is_some_and(|t| t <= c)) {
            self.files.push((path.to_path_buf(), md.len()));
        }
    }
}

/// Work out what `rule` would clean.
pub fn analyze(rule_index: usize, env: &Env, cancel: &AtomicBool) -> Found {
    let rule = &rules::all()[rule_index];
    let mut found = Found { rule: rule_index, ..Default::default() };
    found.needs_admin = rule.admin && !env.elevated && !env.can_elevate;
    found.blocked_by = running_one(rule.close, &env.running);

    let mut c = Collector::new(None);
    for t in rule.targets {
        c.cutoff = None;
        match t {
            Target::Path(p) => {
                if let Some(p) = resolve(p) {
                    c.add(&p, false, cancel);
                }
            }
            Target::Older(p, hours) => {
                if let Some(p) = resolve(p) {
                    c.cutoff = env.now.checked_sub(std::time::Duration::from_secs(*hours as u64 * 3600));
                    c.add(&p, false, cancel);
                }
            }
            Target::Glob(dir, pattern) => {
                if let Some(dir) = resolve(dir)
                    && let Ok(rd) = std::fs::read_dir(&dir)
                {
                    for e in rd.flatten() {
                        if wildcard(pattern, &e.file_name().to_string_lossy()) {
                            c.add(&e.path(), true, cancel);
                        }
                    }
                }
            }
            Target::Chromium(user_data, sub) => {
                if let Some(ud) = resolve(user_data) {
                    for profile in chromium_profiles(&ud) {
                        c.add(&profile.join(sub), false, cancel);
                    }
                }
            }
            Target::EachDir(root, sub) => {
                // The root may be a protected folder (your cache folder); what's
                // cleaned is inside its subfolders, and each of those is checked.
                if let Some(root) = expand(root) {
                    for d in subdirs(&root) {
                        let p = d.join(sub);
                        if safe_location(&p) {
                            c.add(&p, false, cancel);
                        }
                    }
                }
            }
            Target::Special(s) => {
                if !cancel.load(Ordering::Relaxed) {
                    os::analyze_special(s, &mut found.special);
                }
            }
        }
    }

    // A file can be reached twice (e.g. Opera's profile is its user-data folder).
    c.files.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    c.files.dedup_by(|a, b| a.0 == b.0);
    c.dirs.sort_unstable();
    c.dirs.dedup();
    c.dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));

    if env.unattended && found.blocked_by.is_none() {
        found.blocked_by = found.special.restarts_app();
    }
    found.bytes = c.files.iter().map(|f| f.1).sum::<u64>() + found.special.bytes();
    found.items = c.files.len() as u64 + found.special.items();
    found.files = c.files;
    found.dirs = c.dirs;
    found
}

/// Analyze several rules in parallel, sending each result as it's ready.
pub fn analyze_all(rules: Vec<usize>, tx: crossbeam_channel::Sender<Found>, p: &Progress) {
    analyze_with(rules, &Env::current(), tx, p);
}

/// `analyze_all` with facts the caller adjusted (`heft --clean` run by a
/// schedule can't ask for a password).
pub fn analyze_with(rules: Vec<usize>, env: &Env, tx: crossbeam_channel::Sender<Found>, p: &Progress) {
    p.total.store(rules.len() as u64, Ordering::Relaxed);
    p.done.store(0, Ordering::Relaxed);
    rules.into_par_iter().for_each(|r| {
        if p.cancel.load(Ordering::Relaxed) {
            return;
        }
        let found = analyze(r, env, &p.cancel);
        p.done.fetch_add(1, Ordering::Relaxed);
        let _ = tx.send(found);
    });
}

// ----------------------------------------------------------------------
// Cleaning

/// What's in use when cleaning starts.
struct Busy {
    running: HashSet<String>,
    /// Open files as (device, inode). Windows refuses to delete those by
    /// itself, so this is only filled on macOS and Linux.
    open: HashSet<(u64, u64)>,
}

/// Delete what each of `jobs` lists. Re-checks running programs and open
/// files first. Whatever needs the administrator password is gathered from
/// every rule and done at the end, under one password prompt.
pub fn clean_all(jobs: &[Found], p: &Progress) -> Vec<Cleaned> {
    if jobs.is_empty() {
        return Vec::new();
    }
    let busy = Busy { running: os::running_processes(), open: os::open_files() };
    let mut root = os::Elevated::default();
    let mut out = Vec::with_capacity(jobs.len());
    for f in jobs {
        if p.cancel.load(Ordering::Relaxed) {
            break;
        }
        let c = clean_rule(f, out.len(), &busy, p, &mut root);
        out.push(c);
    }
    if !root.is_empty() && !p.cancel.load(Ordering::Relaxed) {
        *p.current.lock().unwrap() = "Waiting for the administrator password…".into();
        root.run(&mut out);
    }
    out
}

/// Clean one rule. `result` is its place in the results, for the work it
/// leaves to `root`.
fn clean_rule(found: &Found, result: usize, busy: &Busy, p: &Progress, root: &mut os::Elevated) -> Cleaned {
    let rule = found.rule();
    let mut out = Cleaned { rule: found.rule, ..Default::default() };
    if let Some(exe) = running_one(rule.close, &busy.running) {
        out.note = Some(format!("skipped: {exe} is running"));
        return out;
    }
    if found.needs_admin {
        out.note = Some("skipped: needs administrator".into());
        return out;
    }
    *p.current.lock().unwrap() = format!("{}: {}", rule.app, rule.name);

    let removed = AtomicU64::new(0);
    let freed = AtomicU64::new(0);
    let skipped = AtomicU64::new(0);
    // Files in system folders only root may delete (admin rules only).
    let denied = Mutex::new(Vec::new());
    found.files.par_iter().for_each(|(path, size)| {
        if p.cancel.load(Ordering::Relaxed) {
            return;
        }
        if !busy.open.is_empty() && os::file_id(path).is_some_and(|id| busy.open.contains(&id)) {
            skipped.fetch_add(1, Ordering::Relaxed);
        } else {
            match os::remove_file(path) {
                Ok(()) => {
                    removed.fetch_add(1, Ordering::Relaxed);
                    freed.fetch_add(*size, Ordering::Relaxed);
                    p.freed.fetch_add(*size, Ordering::Relaxed);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) if rule.admin && e.kind() == std::io::ErrorKind::PermissionDenied && os::root_may_remove(path) => {
                    denied.lock().unwrap().push((path.clone(), *size));
                }
                Err(_) => {
                    skipped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        p.done.fetch_add(1, Ordering::Relaxed);
    });
    // Deepest first; folders that still hold skipped files stay. Folders in
    // system locations always stay: a service may need its log folder.
    if !rule.admin {
        for d in &found.dirs {
            let _ = std::fs::remove_dir(d);
        }
    }
    out.removed = removed.into_inner();
    out.freed = freed.into_inner();
    out.skipped = skipped.into_inner();
    let denied = denied.into_inner().unwrap();
    if !denied.is_empty() {
        root.add_files(result, rule.name, denied);
    }

    if !p.cancel.load(Ordering::Relaxed) {
        let sp = os::clean_special(&found.special, out.removed > 0, result, rule.name, root);
        out.freed += sp.freed;
        out.removed += sp.removed;
        out.skipped += sp.skipped;
        if !sp.notes.is_empty() {
            out.note = Some(sp.notes.join("; "));
        }
    }
    out
}

// ----------------------------------------------------------------------
// Saved selection (shared by the GUI and `heft --clean`)

fn selection_file() -> PathBuf {
    // HEFT_DATA_DIR keeps test runs away from the real selection.
    match std::env::var_os("HEFT_DATA_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("cleaner.txt"),
        None => crate::platform::data_dir().join("cleaner.txt"),
    }
}

/// Rule indices that are ticked: the defaults, adjusted by the saved file.
pub fn load_selection() -> HashSet<usize> {
    let all = rules::all();
    let mut on: HashSet<usize> = (0..all.len()).filter(|&i| all[i].default_on).collect();
    if let Ok(text) = std::fs::read_to_string(selection_file()) {
        for line in text.lines() {
            let line = line.trim();
            let (add, id) = match line.split_at_checked(1) {
                Some(("+", id)) => (true, id),
                Some(("-", id)) => (false, id),
                _ => continue,
            };
            if let Some(i) = rules::by_id(id) {
                if add {
                    on.insert(i);
                } else {
                    on.remove(&i);
                }
            }
        }
    }
    on
}

/// Save the differences from the defaults, so new default rules still apply.
pub fn save_selection(on: &HashSet<usize>) -> std::io::Result<()> {
    let mut text = String::from("# Heft cleaner selection: +id ticks a rule, -id unticks one.\n");
    for (i, r) in rules::all().iter().enumerate() {
        match (r.default_on, on.contains(&i)) {
            (false, true) => text.push_str(&format!("+{}\n", r.id)),
            (true, false) => text.push_str(&format!("-{}\n", r.id)),
            _ => {}
        }
    }
    let f = selection_file();
    if let Some(dir) = f.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(f, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("heft-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn wildcards() {
        assert!(wildcard("thumbcache_*.db", "thumbcache_1024.db"));
        assert!(wildcard("THUMBCACHE_*.DB", "thumbcache_idx.db"));
        assert!(!wildcard("thumbcache_*.db", "iconcache_1024.db"));
        assert!(wildcard("webcache*", "webcache_4147"));
        assert!(wildcard("*.lnk", "a.lnk"));
        assert!(!wildcard("*.lnk", "a.lnk.txt"));
        assert!(wildcard("a*b*c", "a-b-c"));
        assert!(!wildcard("ab*ba", "aba"));
    }

    #[cfg(windows)]
    #[test]
    fn resolves_and_refuses() {
        assert!(resolve(r"%LOCALAPPDATA%\Google\Chrome\User Data").is_some());
        assert!(resolve(r"%HEFT_NO_SUCH_VAR%\x").is_none());
        // Too shallow or an important folder.
        assert!(resolve(r"%LOCALAPPDATA%").is_none());
        assert!(resolve(r"%SystemRoot%").is_none());
        assert!(resolve(r"%SystemRoot%\System32").is_none());
        assert!(resolve(r"%USERPROFILE%\Documents").is_none());
        assert!(!safe_location(Path::new(r"C:\Users")));
        assert!(!safe_location(Path::new(r"C:\")));
        assert!(!safe_location(Path::new(r"relative\path\here")));
        assert!(!safe_location(Path::new(r"C:\a\..\Windows")));
        assert!(resolve(r"%SystemRoot%\Temp").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn resolves_and_refuses() {
        assert!(resolve("%CACHE%/pip").is_some());
        assert!(resolve("%HOME%/.npm/_cacache").is_some());
        assert!(resolve("%HEFT_NO_SUCH_VAR%/x").is_none());
        // Too shallow or an important folder.
        assert!(resolve("%CACHE%").is_none());
        assert!(resolve("%HOME%/Documents").is_none());
        assert!(resolve("%HOME%/.config").is_none());
        assert!(!safe_location(Path::new("/var/cache")));
        assert!(!safe_location(Path::new("/usr")));
        assert!(!safe_location(Path::new("relative/path/here")));
        assert!(!safe_location(Path::new("/home/x/../../etc")));
        assert!(safe_location(Path::new("/var/cache/apt/archives")));
        if cfg!(target_os = "macos") {
            // Your temp folder's contents may go; your cache folder as a whole may not.
            assert!(resolve("%TEMP%").is_some());
            assert!(resolve("%DARWIN_CACHE%").is_none());
            assert!(resolve("%DARWIN_CACHE%/com.apple.metal").is_some());
            assert!(resolve("%LIBRARY%").is_none());
            assert!(resolve("%LIBRARY%/Logs").is_some());
            assert!(!safe_location(Path::new("/private/var/folders")));
        }
    }

    /// Every location in the catalog resolves, to somewhere safe: catches
    /// misspelled `%VAR%` names and rules that would point at a top folder.
    #[cfg(unix)]
    #[test]
    fn every_location_resolves() {
        for r in rules::all() {
            for t in r.targets {
                let ok = match t {
                    Target::Path(p) | Target::Older(p, _) | Target::Glob(p, _) | Target::Chromium(p, _) => resolve(p).is_some(),
                    Target::EachDir(p, sub) => expand(p).is_some_and(|root| safe_location(&root.join("x").join(sub))),
                    _ => continue,
                };
                assert!(ok, "{}: {t:?} doesn't resolve to a safe folder", r.id);
            }
        }
        // EachDir's root may be protected, but nothing directly in it is cleaned.
        if cfg!(target_os = "macos") {
            assert!(!safe_location(&expand("%DARWIN_CACHE%").unwrap()));
            assert!(safe_location(&expand("%DARWIN_CACHE%/com.app/com.apple.metal").unwrap()));
        }
    }

    /// A scheduled run never restarts Finder under you. (Analysis only reads
    /// Finder's preferences.)
    #[cfg(target_os = "macos")]
    #[test]
    fn unattended_runs_leave_finder_alone() {
        let cancel = AtomicBool::new(false);
        let finder = rules::by_id("priv.finder").unwrap();
        let mut env = Env::current();
        assert_eq!(analyze(finder, &env, &cancel).blocked_by, None);
        env.unattended = true;
        assert_eq!(analyze(finder, &env, &cancel).blocked_by, Some("finder"));
        // Restarting the background agent behind Recent items is fine.
        assert_eq!(analyze(rules::by_id("priv.recent").unwrap(), &env, &cancel).blocked_by, None);
    }

    #[test]
    fn close_names_with_wildcards() {
        let running: HashSet<String> = ["adobe premiere pro 2026".to_string(), "finder".to_string()].into();
        assert_eq!(running_one(&["after effects", "adobe premiere pro*"], &running), Some("adobe premiere pro*"));
        assert_eq!(running_one(&["finder"], &running), Some("finder"));
        assert_eq!(running_one(&["adobe media encoder*", "safari"], &running), None);
    }

    #[cfg(unix)]
    #[test]
    fn sockets_are_never_listed() {
        let d = temp_dir("socket");
        std::fs::write(d.join("old.tmp"), b"x").unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(d.join("app.sock")).unwrap();
        let cancel = AtomicBool::new(false);
        let mut c = Collector::new(None);
        c.add(&d, false, &cancel);
        let names: Vec<_> = c.files.iter().map(|f| f.0.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["old.tmp"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn open_files_are_skipped() {
        let d = temp_dir("inuse");
        let (held, free) = (d.join("held.tmp"), d.join("free.tmp"));
        std::fs::write(&free, b"1").unwrap();
        let f = std::fs::File::create(&held).unwrap();
        let files = vec![(held.clone(), 0), (free.clone(), 1)];
        let found = Found { rule: 0, files, bytes: 1, items: 2, ..Default::default() };
        let r = clean_all(&[found], &Progress::default()).remove(0);
        assert_eq!((r.removed, r.skipped), (1, 1));
        assert!(held.exists() && !free.exists());
        drop(f);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn system_rules_keep_their_folders() {
        // Any rule that needs administrator rights, with nothing to close first.
        let admin = rules::all().iter().position(|r| r.admin && r.close.is_empty()).unwrap();
        let d = temp_dir("admin");
        std::fs::create_dir_all(d.join("service")).unwrap();
        std::fs::write(d.join("service/old.log"), b"12").unwrap();
        let found = Found {
            rule: admin,
            files: vec![(d.join("service/old.log"), 2)],
            dirs: vec![d.join("service")],
            bytes: 2,
            items: 1,
            ..Default::default()
        };
        let r = clean_all(&[found], &Progress::default()).remove(0);
        assert_eq!(r.removed, 1);
        assert!(d.join("service").is_dir(), "the folder stays");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn every_rule_is_well_formed() {
        let all = rules::all();
        let mut ids = HashSet::new();
        for r in all {
            assert!(ids.insert(r.id), "duplicate rule id {}", r.id);
            assert!(!r.targets.is_empty(), "{} has no targets", r.id);
            for exe in r.close {
                assert_eq!(*exe, exe.to_lowercase(), "{}: process names must be lower case", r.id);
            }
            for t in r.targets {
                let path = match t {
                    Target::Path(p) | Target::Older(p, _) | Target::Glob(p, _) | Target::Chromium(p, _) | Target::EachDir(p, _) => p,
                    _ => continue,
                };
                assert!(path.starts_with('%'), "{}: {path} must start from a known folder", r.id);
            }
        }
    }

    #[test]
    fn analysis_and_cleaning() {
        let d = temp_dir("clean");
        std::fs::create_dir_all(d.join("sub/deeper")).unwrap();
        std::fs::write(d.join("a.tmp"), vec![0u8; 1000]).unwrap();
        std::fs::write(d.join("sub/deeper/b.tmp"), vec![0u8; 24]).unwrap();
        let ro = d.join("sub/readonly.tmp");
        std::fs::write(&ro, b"x").unwrap();
        let mut perm = std::fs::metadata(&ro).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&ro, perm).unwrap();

        let cancel = AtomicBool::new(false);
        let mut c = Collector::new(None);
        c.add(&d, false, &cancel);
        assert_eq!(c.files.len(), 3);
        assert_eq!(c.files.iter().map(|f| f.1).sum::<u64>(), 1025);
        assert!(!c.dirs.contains(&d), "the folder itself is kept");

        // Nothing is old enough with a cutoff in the past.
        let mut old = Collector::new(SystemTime::now().checked_sub(std::time::Duration::from_secs(3600)));
        old.add(&d, false, &cancel);
        assert!(old.files.is_empty());

        let mut dirs = c.dirs.clone();
        dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        let found = Found { rule: 0, files: c.files, dirs, bytes: 1025, items: 3, ..Default::default() };
        let p = Progress::default();
        let r = clean_all(&[found], &p).remove(0);
        assert_eq!(r.removed, 3);
        assert_eq!(r.freed, 1025);
        assert!(d.exists());
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0, "empty subfolders are removed too");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stale_folders_go_whole_active_ones_stay() {
        let d = temp_dir("units");
        for sub in ["stale", "active/old-sub", "active"] {
            std::fs::create_dir_all(d.join(sub)).unwrap();
        }
        std::fs::write(d.join("stale/a"), b"1").unwrap();
        std::fs::write(d.join("active/old-sub/b"), b"22").unwrap();
        std::fs::write(d.join("active/old-loose"), b"333").unwrap();
        std::fs::write(d.join("active/new"), b"4444").unwrap();
        std::fs::write(d.join("loose-old"), b"55555").unwrap();
        std::fs::write(d.join("loose-new"), b"666666").unwrap();
        // Everything written so far is "old"; then touch the new files after the cutoff.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let cutoff = SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(d.join("active/new"), b"4444").unwrap();
        std::fs::write(d.join("loose-new"), b"666666").unwrap();

        let cancel = AtomicBool::new(false);
        let mut c = Collector::new(Some(cutoff));
        c.add(&d, false, &cancel);
        let rel = |p: &PathBuf| p.strip_prefix(&d).unwrap().to_string_lossy().replace('\\', "/");
        let mut files: Vec<String> = c.files.iter().map(|f| rel(&f.0)).collect();
        files.sort();
        // The stale folder goes whole; the active folder keeps its loose
        // files but loses its stale subfolder; loose root files are judged
        // one by one.
        assert_eq!(files, ["active/old-sub/b", "loose-old", "stale/a"]);
        let mut dirs: Vec<String> = c.dirs.iter().map(rel).collect();
        dirs.sort();
        assert_eq!(dirs, ["active/old-sub", "stale"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn never_follows_symlinks() {
        let d = temp_dir("symlink");
        let outside = temp_dir("symlink-target");
        std::fs::write(outside.join("precious.txt"), b"keep me").unwrap();
        std::fs::create_dir_all(d.join("cache")).unwrap();
        std::os::unix::fs::symlink(&outside, d.join("cache").join("link")).unwrap();
        std::os::unix::fs::symlink(outside.join("precious.txt"), d.join("cache").join("file-link")).unwrap();
        let cancel = AtomicBool::new(false);
        let mut c = Collector::new(None);
        c.add(&d.join("cache"), false, &cancel);
        assert!(c.files.is_empty(), "{:?}", c.files);
        assert!(outside.join("precious.txt").exists());
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(windows)]
    #[test]
    fn never_follows_junctions() {
        let d = temp_dir("junction");
        let outside = temp_dir("junction-target");
        std::fs::write(outside.join("precious.txt"), b"keep me").unwrap();
        std::fs::create_dir_all(d.join("cache")).unwrap();
        let link = d.join("cache").join("link");
        let status = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .unwrap();
        assert!(status.status.success(), "mklink /J failed");
        let cancel = AtomicBool::new(false);
        let mut c = Collector::new(None);
        c.add(&d.join("cache"), false, &cancel);
        assert!(c.files.is_empty());
        assert!(!c.dirs.iter().any(|x| x == &link));
        let _ = std::fs::remove_dir(&link);
        assert!(outside.join("precious.txt").exists());
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn finds_chromium_profiles() {
        let d = temp_dir("chromium");
        for p in ["Default", "Profile 3", "Guest Profile", "Crashpad", "ShaderCache"] {
            std::fs::create_dir_all(d.join(p)).unwrap();
        }
        let mut names: Vec<String> =
            chromium_profiles(&d).iter().skip(1).map(|p| p.file_name().unwrap().to_string_lossy().into()).collect();
        names.sort();
        assert_eq!(names, ["Default", "Guest Profile", "Profile 3"]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
