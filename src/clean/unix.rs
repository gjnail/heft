//! macOS and Linux side of the cleaner: known folders, process names, open
//! files, and cleanups that belong to the system's own tools (package
//! managers, journald, snap, flatpak, Homebrew, Docker, Xcode's simulator
//! manager). What only macOS has is in `mac.rs`.
//!
//! Heft never deletes system-owned files itself. For those it runs the tool
//! that owns them, estimating the saving first, or (macOS's system logs) has
//! root delete exactly the files the analysis listed. Everything that needs
//! root is gathered from all the rules being cleaned and run under one
//! password prompt: `pkexec` on Linux, macOS's administrator prompt on macOS.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{parse, Cleaned};

#[cfg(target_os = "macos")]
pub use super::mac::{schedule_enabled, set_schedule};

/// Name of the operating system's own group of rules.
#[cfg(target_os = "macos")]
pub const SYSTEM: &str = "macOS";
#[cfg(not(target_os = "macos"))]
pub const SYSTEM: &str = "Linux";

/// What a system tool would do: an estimate, and the commands that do it.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub bytes: u64,
    /// 0 for an action with nothing to count (flushing the DNS cache).
    pub items: u64,
    pub commands: Vec<Vec<String>>,
    /// The commands need root.
    pub root: bool,
}

pub type Planner = fn() -> Option<Plan>;

/// Unix-only things a rule can clean besides files.
#[derive(Debug)]
pub enum Special {
    /// Let a system tool do the cleaning.
    Tool(Planner),
    /// Empty the clipboard.
    #[cfg(target_os = "macos")]
    Clipboard,
    /// Keys of a preference domain, removed through `defaults` so the
    /// preferences daemon's cache agrees.
    #[cfg(target_os = "macos")]
    Prefs(&'static str, &'static [&'static str]),
    /// Once something is removed, quit this process of yours so it forgets
    /// it; macOS starts it again.
    #[cfg(target_os = "macos")]
    Restart(&'static str),
}

#[derive(Clone, Debug, Default)]
pub struct Pending {
    plans: Vec<(Planner, Plan)>,
    clipboard: bool,
    /// (domain, key, entries it holds).
    prefs: Vec<(&'static str, &'static str, u64)>,
    #[cfg(target_os = "macos")]
    restart: Vec<&'static str>,
}

impl Pending {
    pub fn bytes(&self) -> u64 {
        self.plans.iter().map(|p| p.1.bytes).sum()
    }

    pub fn items(&self) -> u64 {
        self.plans.iter().map(|p| p.1.items).sum::<u64>() + self.prefs.iter().map(|p| p.2).sum::<u64>()
    }

    /// Has work to do even though there's nothing to count.
    pub fn bare_action(&self) -> bool {
        self.clipboard || self.plans.iter().any(|p| p.1.items == 0 && !p.1.commands.is_empty())
    }

    /// An app you'd see restart (its windows close), as a lower-case process
    /// name. Background agents such as sharedfilelistd don't count.
    #[cfg(target_os = "macos")]
    pub fn restarts_app(&self) -> Option<&'static str> {
        self.restart.contains(&"Finder").then_some("finder")
    }

    #[cfg(not(target_os = "macos"))]
    pub fn restarts_app(&self) -> Option<&'static str> {
        None
    }
}

#[derive(Default)]
pub struct SpecialResult {
    pub freed: u64,
    pub removed: u64,
    pub skipped: u64,
    pub notes: Vec<String>,
}

/// Lower-case names of running processes.
#[cfg(target_os = "linux")]
pub fn running_processes() -> HashSet<String> {
    let Ok(rd) = std::fs::read_dir("/proc") else { return HashSet::new() };
    rd.flatten()
        .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|e| std::fs::read_to_string(e.path().join("comm")).ok())
        .map(|c| c.trim().to_lowercase())
        .collect()
}

#[cfg(not(target_os = "linux"))]
pub fn running_processes() -> HashSet<String> {
    let Ok(out) = Command::new("/bin/ps").args(["-A", "-c", "-o", "comm="]).output() else { return HashSet::new() };
    String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim().to_lowercase()).filter(|l| !l.is_empty()).collect()
}

/// Files your programs have open, as (device, inode). Unlike Windows, macOS
/// and Linux let an open file be deleted, so the cleaner has to look.
#[cfg(target_os = "macos")]
pub fn open_files() -> HashSet<(u64, u64)> {
    super::mac::open_files()
}

#[cfg(target_os = "linux")]
pub fn open_files() -> HashSet<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let mut out = HashSet::new();
    let Ok(procs) = std::fs::read_dir("/proc") else { return out };
    for p in procs.flatten().filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit())) {
        // Other users' processes can't be read, and their files aren't ours to delete anyway.
        let Ok(fds) = std::fs::read_dir(p.path().join("fd")) else { continue };
        for fd in fds.flatten() {
            if let Ok(m) = std::fs::metadata(fd.path())
                && m.is_file()
            {
                out.insert((m.dev(), m.ino()));
            }
        }
    }
    out
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn open_files() -> HashSet<(u64, u64)> {
    HashSet::new()
}

/// (device, inode) of a file, to compare with `open_files`.
pub fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(path).ok()?;
    // macOS's `dev_t` is 32 bits; the process list reports it unsigned.
    let dev = if cfg!(target_os = "macos") { m.dev() as u32 as u64 } else { m.dev() };
    Some((dev, m.ino()))
}

/// Rules marked `admin` can still run: their commands go through `pkexec`
/// on Linux and macOS's administrator password prompt on macOS.
pub fn can_elevate_tools() -> bool {
    cfg!(target_os = "macos") || (cfg!(target_os = "linux") && find_exe("pkexec").is_some())
}

fn home() -> Option<String> {
    std::env::var("HOME").ok().filter(|h| !h.trim().is_empty())
}

pub fn var(name: &str) -> Option<String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let v = match name {
        "HOME" => home(),
        "CACHE" if cfg!(target_os = "macos") => home().map(|h| format!("{h}/Library/Caches")),
        "CACHE" => env("XDG_CACHE_HOME").or_else(|| home().map(|h| format!("{h}/.cache"))),
        "CONFIG" if cfg!(target_os = "macos") => home().map(|h| format!("{h}/Library/Application Support")),
        "CONFIG" => env("XDG_CONFIG_HOME").or_else(|| home().map(|h| format!("{h}/.config"))),
        "DATA" => env("XDG_DATA_HOME").or_else(|| home().map(|h| format!("{h}/.local/share"))),
        "CARGO_HOME" => env("CARGO_HOME").or_else(|| home().map(|h| format!("{h}/.cargo"))),
        "GRADLE_USER_HOME" => env("GRADLE_USER_HOME").or_else(|| home().map(|h| format!("{h}/.gradle"))),
        // Your own temporary and cache folders under /var/folders, and the
        // system-wide Library.
        #[cfg(target_os = "macos")]
        "TEMP" => super::mac::temp_dir(),
        #[cfg(target_os = "macos")]
        "DARWIN_CACHE" => super::mac::cache_dir(),
        #[cfg(target_os = "macos")]
        "LIBRARY" => Some("/Library".into()),
        _ => env(name),
    }?;
    Some(v.trim_end_matches('/').to_string())
}

/// Folders a rule may never resolve to, or to a parent of.
pub fn important_folders() -> Vec<String> {
    let mut v: Vec<String> = [
        "/usr", "/etc", "/var", "/var/cache", "/var/log", "/var/lib", "/opt", "/home", "/Users", "/System", "/Library",
        "/Applications", "/private", "/private/var", "/var/folders", "/private/var/folders",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(h) = home() {
        for sub in [
            "", "/Documents", "/Desktop", "/Downloads", "/Pictures", "/Music", "/Movies", "/Videos", "/Public", "/Library",
            "/Library/Caches", "/Library/Application Support", "/Library/Developer", "/.cache", "/.config", "/.local",
            "/.local/share",
        ] {
            v.push(format!("{h}{sub}"));
        }
    }
    for k in ["CACHE", "CONFIG", "DATA", "CARGO_HOME", "GRADLE_USER_HOME", "DARWIN_CACHE"] {
        v.extend(var(k));
    }
    v
}

pub fn is_link(md: &std::fs::Metadata) -> bool {
    md.file_type().is_symlink()
}

pub fn remove_file(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

/// Root may delete this file for the user only if no folder above it is
/// writable by the user. Otherwise a program running as the user could swap
/// a folder on the way for a link, and root would delete something else.
/// The path also has to survive the trip through a shell script intact.
pub fn root_may_remove(path: &Path) -> bool {
    let printable = path.to_str().is_some_and(|s| !s.chars().any(char::is_control));
    printable && path.is_absolute() && path.ancestors().skip(1).all(|dir| !writable(dir))
}

fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return true };
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

pub fn analyze_special(s: &Special, p: &mut Pending) {
    match s {
        Special::Tool(planner) => {
            if let Some(plan) = planner() {
                p.plans.push((*planner, plan));
            }
        }
        #[cfg(target_os = "macos")]
        Special::Clipboard => p.clipboard = true,
        #[cfg(target_os = "macos")]
        Special::Prefs(domain, keys) => {
            for (key, n) in super::mac::pref_entries(domain, keys) {
                p.prefs.push((domain, key, n));
            }
        }
        #[cfg(target_os = "macos")]
        Special::Restart(process) => p.restart.push(process),
    }
}

/// Run what a rule found besides files. Commands that need root are left in
/// `root` for the one password prompt at the end. `changed`: the rule
/// already removed files.
pub fn clean_special(p: &Pending, changed: bool, result: usize, label: &str, root: &mut Elevated) -> SpecialResult {
    let mut out = SpecialResult::default();
    for (planner, plan) in &p.plans {
        if plan.commands.is_empty() {
            continue;
        }
        if plan.root && !crate::platform::is_elevated() {
            root.job(result, label).plans.push((*planner, plan.clone()));
            continue;
        }
        match run(&plan.commands) {
            Ok(()) => {
                // Measure what's left rather than trusting the estimate.
                let left = planner().unwrap_or_default();
                out.freed += plan.bytes.saturating_sub(left.bytes);
                out.removed += plan.items.saturating_sub(left.items);
            }
            Err(e) => {
                out.skipped += plan.items;
                out.notes.push(e);
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let mut changed = changed;
        if p.clipboard
            && let Err(e) = super::mac::clear_clipboard()
        {
            out.notes.push(e);
        }
        for (domain, key, n) in &p.prefs {
            match super::mac::delete_pref(domain, key) {
                Ok(()) => {
                    out.removed += n;
                    changed = true;
                }
                Err(e) => {
                    out.skipped += n;
                    out.notes.push(e);
                }
            }
        }
        if changed {
            for process in &p.restart {
                super::mac::restart(process);
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = changed;
    out
}

/// Run commands as the user, stopping at the first that fails.
fn run(commands: &[Vec<String>]) -> Result<(), String> {
    for c in commands {
        let exe = find_exe(&c[0]).ok_or_else(|| format!("{} isn't installed", c[0]))?;
        let out = Command::new(exe).args(&c[1..]).output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            let msg = String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or_default().trim().to_string();
            return Err(format!("{} failed: {msg}", c.join(" ")));
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------
// Everything that needs root, under one password prompt

/// Work that needs root, gathered from every rule being cleaned.
#[derive(Default)]
pub struct Elevated {
    jobs: Vec<RootJob>,
}

struct RootJob {
    /// Where the rule's outcome is in the results.
    result: usize,
    label: String,
    plans: Vec<(Planner, Plan)>,
    /// Files in system folders the user may not delete.
    files: Vec<(PathBuf, u64)>,
}

impl RootJob {
    /// The commands, with programs looked up now: root's PATH is minimal.
    fn commands(&self) -> Result<Vec<Vec<String>>, String> {
        let mut out = Vec::new();
        for (_, plan) in &self.plans {
            for c in &plan.commands {
                let exe = find_exe(&c[0]).ok_or_else(|| format!("{} isn't installed", c[0]))?;
                let mut c = c.clone();
                c[0] = exe.to_string_lossy().into_owned();
                out.push(c);
            }
        }
        // Exactly the files the analysis listed; `rm` never follows a link
        // it's given, and it's never asked to remove a folder.
        for chunk in self.files.chunks(200) {
            let mut c: Vec<String> = ["/bin/rm", "-f", "--"].iter().map(|s| s.to_string()).collect();
            c.extend(chunk.iter().map(|f| f.0.to_string_lossy().into_owned()));
            out.push(c);
        }
        Ok(out)
    }
}

impl Elevated {
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    fn job(&mut self, result: usize, label: &str) -> &mut RootJob {
        let i = match self.jobs.iter().position(|j| j.result == result) {
            Some(i) => i,
            None => {
                self.jobs.push(RootJob { result, label: label.to_string(), plans: Vec::new(), files: Vec::new() });
                self.jobs.len() - 1
            }
        };
        &mut self.jobs[i]
    }

    pub fn add_files(&mut self, result: usize, label: &str, files: Vec<(PathBuf, u64)>) {
        self.job(result, label).files.extend(files);
    }

    /// Run everything under one password prompt, then measure what's gone
    /// and add it to each rule's outcome.
    pub fn run(self, out: &mut [Cleaned]) {
        let commands: Vec<Result<Vec<Vec<String>>, String>> = self.jobs.iter().map(RootJob::commands).collect();
        let script = parse::root_script(&commands.iter().map(|c| c.clone().unwrap_or_default()).collect::<Vec<_>>());
        let names: Vec<&str> = self.jobs.iter().map(|j| j.label.as_str()).collect();
        let ran = if script.is_empty() { Ok(Default::default()) } else { run_as_root(&script, &names).map(|o| parse::root_statuses(&o)) };
        for (i, job) in self.jobs.iter().enumerate() {
            let c = &mut out[job.result];
            let planned: u64 = job.plans.iter().map(|p| p.1.items).sum();
            let failed = match (&ran, &commands[i]) {
                (Err(e), _) | (_, Err(e)) => Some(e.clone()),
                (Ok(statuses), Ok(_)) => {
                    for (path, size) in &job.files {
                        if std::fs::symlink_metadata(path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
                            c.removed += 1;
                            c.freed += size;
                        } else {
                            c.skipped += 1;
                        }
                    }
                    for (planner, plan) in &job.plans {
                        let left = planner().unwrap_or_default();
                        c.freed += plan.bytes.saturating_sub(left.bytes);
                        c.removed += plan.items.saturating_sub(left.items);
                    }
                    match statuses.get(&i) {
                        Some(0) => None,
                        Some(code) => Some(format!("finished with errors (status {code})")),
                        None => Some("didn't run".into()),
                    }
                }
            };
            if let Some(e) = failed {
                if ran.is_err() || commands[i].is_err() {
                    c.skipped += job.files.len() as u64 + planned;
                }
                c.note = Some(match c.note.take() {
                    Some(n) => format!("{n}; {e}"),
                    None => e,
                });
            }
        }
    }
}

/// Run a script as root and return what it printed.
fn run_as_root(script: &str, names: &[&str]) -> Result<String, String> {
    if crate::platform::is_elevated() {
        let out = Command::new("/bin/sh").args(["-c", script]).output().map_err(|e| e.to_string())?;
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    #[cfg(target_os = "macos")]
    {
        // Finishes "Heft wants to …" in macOS's prompt.
        let why = format!("clean {}", names.join(", "));
        crate::mac::run_as_admin(script, &why)
            .map_err(|e| if e == crate::mac::CANCELLED { "cancelled at the password prompt".to_string() } else { e })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = names;
        let pkexec = find_exe("pkexec").ok_or("needs root, and pkexec isn't installed")?;
        let out = Command::new(pkexec).args(["/bin/sh", "-c", script]).output().map_err(|e| e.to_string())?;
        // 126/127: the password prompt was dismissed or failed.
        match out.status.code() {
            Some(126) | Some(127) => Err("cancelled at the password prompt".into()),
            _ => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        }
    }
}

/// A program on PATH, or in the usual places a GUI app's PATH leaves out.
pub fn find_exe(name: &str) -> Option<PathBuf> {
    if name.starts_with('/') {
        return Some(PathBuf::from(name)).filter(|p| p.is_file());
    }
    let path = std::env::var("PATH").unwrap_or_default();
    let extra = ["/usr/bin", "/bin", "/usr/sbin", "/usr/local/bin", "/opt/homebrew/bin", "/home/linuxbrew/.linuxbrew/bin", "/snap/bin"];
    path.split(':').chain(extra).map(|d| Path::new(d).join(name)).find(|p| p.is_file())
}

fn output(exe: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new(exe).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Total size of files under `dir` that match `keep`, and how many.
pub(super) fn files_under(dir: &Path, keep: &dyn Fn(&Path, &std::fs::Metadata) -> bool) -> (u64, u64) {
    let Ok(rd) = std::fs::read_dir(dir) else { return (0, 0) };
    let mut total = (0, 0);
    for e in rd.flatten() {
        let Ok(md) = e.metadata() else { continue };
        if md.file_type().is_symlink() {
            continue;
        }
        let p = e.path();
        if md.is_dir() {
            let (b, n) = files_under(&p, keep);
            total = (total.0 + b, total.1 + n);
        } else if keep(&p, &md) {
            total = (total.0 + md.len(), total.1 + 1);
        }
    }
    total
}

pub(super) fn plan(bytes: u64, items: u64, commands: Vec<Vec<&str>>, root: bool) -> Option<Plan> {
    (items > 0).then(|| Plan {
        bytes,
        items,
        commands: commands.into_iter().map(|c| c.into_iter().map(str::to_string).collect()).collect(),
        root,
    })
}

// ----------------------------------------------------------------------
// Planners, referenced from the rule catalog.

/// Downloaded .deb packages.
#[cfg(target_os = "linux")]
pub fn apt() -> Option<Plan> {
    find_exe("apt-get")?;
    let (b, n) = files_under(Path::new("/var/cache/apt/archives"), &|p, _| p.extension().is_some_and(|x| x == "deb"));
    plan(b, n, vec![vec!["apt-get", "clean"]], true)
}

/// Downloaded .rpm packages.
#[cfg(target_os = "linux")]
pub fn dnf() -> Option<Plan> {
    find_exe("dnf")?;
    let rpm = |p: &Path, _: &std::fs::Metadata| p.extension().is_some_and(|x| x == "rpm");
    let (b1, n1) = files_under(Path::new("/var/cache/dnf"), &rpm);
    let (b2, n2) = files_under(Path::new("/var/cache/libdnf5"), &rpm);
    plan(b1 + b2, n1 + n2, vec![vec!["dnf", "clean", "packages"]], true)
}

/// Archived journal files older than two weeks.
#[cfg(target_os = "linux")]
pub fn journald() -> Option<Plan> {
    find_exe("journalctl")?;
    let cutoff = std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(14 * 86_400))?;
    let (b, n) = files_under(Path::new("/var/log/journal"), &|p, md| {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        parse::is_archived_journal(&name) && md.modified().is_ok_and(|t| t < cutoff)
    });
    plan(b, n, vec![vec!["journalctl", "--vacuum-time=2weeks"]], true)
}

/// Snap keeps the previous revisions of every snap; drop the disabled ones.
#[cfg(target_os = "linux")]
pub fn snap() -> Option<Plan> {
    let exe = find_exe("snap")?;
    let list = output(&exe, &["list", "--all"])?;
    let old = parse::snap_disabled(&list);
    let bytes = old
        .iter()
        .filter_map(|(name, rev)| std::fs::metadata(format!("/var/lib/snapd/snaps/{name}_{rev}.snap")).ok())
        .map(|m| m.len())
        .sum();
    let commands = old.iter().map(|(name, rev)| vec!["snap", "remove", name.as_str(), "--revision", rev.as_str()]).collect();
    plan(bytes, old.len() as u64, commands, true)
}

/// Runtimes no installed app needs. flatpak asks for a password itself when
/// the system installation is involved.
#[cfg(target_os = "linux")]
pub fn flatpak() -> Option<Plan> {
    let exe = find_exe("flatpak")?;
    let runtimes = output(&exe, &["list", "--runtime", "--columns=ref,size"])?;
    let apps = output(&exe, &["list", "--app", "--columns=runtime"])?;
    let unused = parse::flatpak_unused(&runtimes, &apps);
    let bytes = unused.iter().map(|u| u.1).sum();
    plan(bytes, unused.len() as u64, vec![vec!["flatpak", "uninstall", "--unused", "--noninteractive", "-y"]], false)
}

/// Homebrew's download cache and superseded versions of installed packages.
pub fn homebrew() -> Option<Plan> {
    let exe = find_exe("brew")?;
    let out = output(&exe, &["cleanup", "--prune=all", "--dry-run"])?;
    let (bytes, items) = parse::brew_dry_run(&out);
    plan(bytes, items, vec![vec!["brew", "cleanup", "--prune=all"]], false)
}

/// Docker's build cache and dangling images, only when the daemon is
/// running. Docker Desktop and Colima put the command-line tool in places a
/// GUI app's PATH leaves out.
pub fn docker() -> Option<Plan> {
    let exe = find_exe("docker").or_else(|| {
        let mut dirs = vec![PathBuf::from("/Applications/Docker.app/Contents/Resources/bin")];
        if let Some(h) = home() {
            dirs.push(PathBuf::from(&h).join(".docker/bin"));
            dirs.push(PathBuf::from(&h).join(".local/bin"));
        }
        dirs.into_iter().map(|d| d.join("docker")).find(|p| p.is_file())
    })?;
    output(&exe, &["info", "--format", "{{.ServerVersion}}"])?;
    let df = output(&exe, &["system", "df", "--format", "{{json .}}"])?;
    let (cache_bytes, cache_items) = parse::docker_build_cache(&df);
    let dangling = output(&exe, &["images", "--filter", "dangling=true", "--format", "{{.Size}}"]).unwrap_or_default();
    let (image_bytes, images) = parse::docker_image_sizes(&dangling);
    let docker = exe.to_string_lossy().into_owned();
    plan(
        cache_bytes + image_bytes,
        cache_items + images,
        vec![vec![docker.as_str(), "builder", "prune", "-f"], vec![docker.as_str(), "image", "prune", "-f"]],
        false,
    )
}

/// Simulators whose iOS/watchOS runtime is no longer installed.
#[cfg(target_os = "macos")]
pub fn simulators() -> Option<Plan> {
    // Without Xcode, `xcrun simctl` pops up an installer; don't trigger it.
    let xcode = Path::new("/Applications/Xcode.app").exists()
        || output(Path::new("/usr/bin/xcode-select"), &["-p"]).is_some_and(|p| p.contains("Xcode"));
    if !xcode {
        return None;
    }
    let json = output(Path::new("/usr/bin/xcrun"), &["simctl", "list", "devices", "unavailable", "--json"])?;
    let devices = PathBuf::from(home()?).join("Library/Developer/CoreSimulator/Devices");
    let udids = parse::simctl_udids(&json);
    let bytes = udids.iter().map(|u| files_under(&devices.join(u), &|_, _| true).0).sum();
    plan(bytes, udids.len() as u64, vec![vec!["xcrun", "simctl", "delete", "unavailable"]], false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_only_where_the_user_cannot_swap_folders() {
        // The temp folder is the user's own, so root must not touch files in it.
        let d = std::env::temp_dir().join(format!("heft-root-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert!(!root_may_remove(&d.join("file")));
        assert!(!root_may_remove(Path::new("relative/file")));
        // Where nobody but root may write to /, /usr and /usr/share. GitHub's
        // Ubuntu runners make /usr/share writable for everyone, and there
        // root must not remove anything from it.
        let locked = |dir: &Path| {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(dir).is_ok_and(|m| m.uid() == 0 && m.mode() & 0o022 == 0)
        };
        let path = Path::new("/usr/share/heft-no-such-file");
        if !crate::platform::is_elevated() && path.ancestors().skip(1).all(locked) {
            assert!(root_may_remove(path));
        }
        assert!(!root_may_remove(Path::new("/usr/share/heft\nno-such-file")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bare_actions() {
        let mut p = Pending::default();
        assert!(!p.bare_action());
        p.plans.push((|| None, Plan { commands: vec![vec!["true".into()]], root: true, ..Default::default() }));
        assert!(p.bare_action());
        assert_eq!(p.items(), 0);
    }

    #[test]
    fn open_files_include_our_own() {
        let d = std::env::temp_dir().join(format!("heft-open-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("held");
        let f = std::fs::File::create(&path).unwrap();
        let open = open_files();
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            assert!(open.contains(&file_id(&path).unwrap()));
        }
        drop(f);
        let _ = std::fs::remove_dir_all(&d);
    }
}
