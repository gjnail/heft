//! macOS and Linux side of the cleaner: known folders, process names, and
//! cleanups that belong to the system's own tools (package managers,
//! journald, snap, flatpak, Homebrew, Xcode's simulator manager).
//!
//! Heft never deletes system-owned files itself. For those it runs the tool
//! that owns them, estimating the saving first; commands that need root go
//! through `pkexec`, which shows the desktop's own password prompt.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::parse;

/// Name of the operating system's own group of rules.
#[cfg(target_os = "macos")]
pub const SYSTEM: &str = "macOS";
#[cfg(not(target_os = "macos"))]
pub const SYSTEM: &str = "Linux";

/// What a system tool would do: an estimate, and the commands that do it.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub bytes: u64,
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
}

#[derive(Clone, Debug, Default)]
pub struct Pending {
    plans: Vec<(Planner, Plan)>,
}

impl Pending {
    pub fn bytes(&self) -> u64 {
        self.plans.iter().map(|p| p.1.bytes).sum()
    }

    pub fn items(&self) -> u64 {
        self.plans.iter().map(|p| p.1.items).sum()
    }

    pub fn bare_action(&self) -> bool {
        false
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

/// Rules marked `admin` can still run: their commands go through `pkexec`.
pub fn can_elevate_tools() -> bool {
    cfg!(target_os = "linux") && find_exe("pkexec").is_some()
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
        _ => env(name),
    }?;
    Some(v.trim_end_matches('/').to_string())
}

/// Folders a rule may never resolve to, or to a parent of.
pub fn important_folders() -> Vec<String> {
    let mut v: Vec<String> = [
        "/usr", "/etc", "/var", "/var/cache", "/var/log", "/var/lib", "/opt", "/home", "/Users", "/System", "/Library",
        "/Applications", "/private", "/private/var",
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
    for k in ["CACHE", "CONFIG", "DATA", "CARGO_HOME", "GRADLE_USER_HOME"] {
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

pub fn analyze_special(s: &Special, p: &mut Pending) {
    let Special::Tool(planner) = s;
    if let Some(plan) = planner() {
        p.plans.push((*planner, plan));
    }
}

pub fn clean_special(p: &Pending) -> SpecialResult {
    let mut out = SpecialResult::default();
    for (planner, plan) in &p.plans {
        if plan.commands.is_empty() {
            continue;
        }
        match run(plan) {
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
    out
}

/// Run a plan's commands, all under one password prompt when they need root.
fn run(plan: &Plan) -> Result<(), String> {
    let root = plan.root && !crate::platform::is_elevated();
    if root {
        let script = plan
            .commands
            .iter()
            .map(|c| c.iter().map(|a| parse::shell_quote(a)).collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join("; ");
        let pkexec = find_exe("pkexec").ok_or("needs root, and pkexec isn't installed")?;
        let status = Command::new(pkexec).args(["/bin/sh", "-c", &script]).status().map_err(|e| e.to_string())?;
        // 126/127: the password prompt was dismissed or failed.
        return match status.code() {
            Some(0) => Ok(()),
            Some(126) | Some(127) => Err("cancelled at the password prompt".into()),
            _ => Err(format!("{script} failed ({status})")),
        };
    }
    for c in &plan.commands {
        let exe = find_exe(&c[0]).ok_or_else(|| format!("{} isn't installed", c[0]))?;
        let out = Command::new(exe).args(&c[1..]).output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            let msg = String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or_default().trim().to_string();
            return Err(format!("{} failed: {msg}", c.join(" ")));
        }
    }
    Ok(())
}

/// A program on PATH, or in the usual places a GUI app's PATH leaves out.
pub fn find_exe(name: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    let extra = ["/usr/bin", "/bin", "/usr/sbin", "/usr/local/bin", "/opt/homebrew/bin", "/home/linuxbrew/.linuxbrew/bin", "/snap/bin"];
    path.split(':').chain(extra).map(|d| Path::new(d).join(name)).find(|p| p.is_file())
}

fn output(exe: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new(exe).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Total size of files under `dir` that match `keep`, and how many.
fn files_under(dir: &Path, keep: &dyn Fn(&Path, &std::fs::Metadata) -> bool) -> (u64, u64) {
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

fn plan(bytes: u64, items: u64, commands: Vec<Vec<&str>>, root: bool) -> Option<Plan> {
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
