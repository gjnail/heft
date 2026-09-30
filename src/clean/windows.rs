//! Windows side of the cleaner: known folders, links, the Recycle Bin,
//! clipboard, DNS cache, registry MRU lists and Task Scheduler.

use std::collections::HashSet;
use std::os::windows::fs::MetadataExt;
use std::os::windows::process::CommandExt;
use std::path::Path;

use crate::reg::{Hive, Key};
use crate::winsys;

const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Name of the operating system's own group of rules.
pub const SYSTEM: &str = "Windows";

/// Windows-only things a rule can clean besides files.
#[derive(Debug)]
pub enum Special {
    RecycleBin,
    Clipboard,
    DnsCache,
    /// Every value of an HKCU key (MRU lists).
    RegistryValues(&'static str),
}

/// What the analysis found for a rule's special targets.
#[derive(Clone, Debug, Default)]
pub struct Pending {
    recycle: Option<(u64, u64)>,
    reg_values: Vec<(String, String)>,
    /// Clipboard / DNS: nothing to count, but something to do.
    actions: Vec<&'static str>,
}

impl Pending {
    pub fn bytes(&self) -> u64 {
        self.recycle.map_or(0, |r| r.0)
    }

    pub fn items(&self) -> u64 {
        self.recycle.map_or(0, |r| r.1) + self.reg_values.len() as u64
    }

    /// Has work to do even though there's nothing to count.
    pub fn bare_action(&self) -> bool {
        !self.actions.is_empty()
    }
}

/// Outcome of the special part of cleaning a rule.
#[derive(Default)]
pub struct SpecialResult {
    pub freed: u64,
    pub removed: u64,
    pub skipped: u64,
    pub notes: Vec<String>,
}

pub fn running_processes() -> HashSet<String> {
    winsys::running_processes()
}

/// Rules marked `admin` need Heft itself to run elevated.
pub fn can_elevate_tools() -> bool {
    false
}

/// Value for a `%VAR%` placeholder. Besides the environment, Heft knows a few
/// locations that aren't environment variables.
pub fn var(name: &str) -> Option<String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let home = || env("USERPROFILE");
    let v = match name.to_ascii_uppercase().as_str() {
        "TEMP" | "TMP" => Some(std::env::temp_dir().to_string_lossy().into_owned()),
        "LOCALLOW" => home().map(|h| format!(r"{h}\AppData\LocalLow")),
        "CARGO_HOME" => env("CARGO_HOME").or_else(|| home().map(|h| format!(r"{h}\.cargo"))),
        "GRADLE_USER_HOME" => env("GRADLE_USER_HOME").or_else(|| home().map(|h| format!(r"{h}\.gradle"))),
        "STEAM" => Key::open(Hive::CurrentUser, r"Software\Valve\Steam")
            .and_then(|k| k.get_string("SteamPath"))
            .map(|p| p.replace('/', "\\")),
        _ => env(name),
    }?;
    Some(v.trim_end_matches('\\').to_string())
}

/// Folders a rule may never resolve to, or to a parent of.
pub fn important_folders() -> Vec<String> {
    let mut v: Vec<String> = [
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "ProgramData",
        "SystemRoot",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "OneDrive",
        "PUBLIC",
    ]
    .iter()
    .filter_map(|k| std::env::var(k).ok())
    .filter(|v| !v.is_empty())
    .collect();
    if let Ok(root) = std::env::var("SystemRoot") {
        v.push(format!(r"{root}\System32"));
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        for sub in ["Documents", "Desktop", "Downloads", "Pictures", "Music", "Videos", r"AppData\LocalLow"] {
            v.push(format!(r"{home}\{sub}"));
        }
    }
    v
}

/// Symlink, junction or other reparse point.
pub fn is_link(md: &std::fs::Metadata) -> bool {
    md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

pub fn remove_file(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            // Read-only files can't be deleted until the flag is cleared.
            let md = std::fs::symlink_metadata(path)?;
            if md.file_attributes() & FILE_ATTRIBUTE_READONLY == 0 {
                return Err(e);
            }
            let mut perm = md.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            std::fs::set_permissions(path, perm)?;
            std::fs::remove_file(path)
        }
        r => r,
    }
}

pub fn analyze_special(s: &Special, p: &mut Pending) {
    match s {
        Special::RecycleBin => p.recycle = Some(winsys::recycle_bin_info().unwrap_or((0, 0))),
        Special::Clipboard => p.actions.push("clipboard"),
        Special::DnsCache => p.actions.push("dns"),
        Special::RegistryValues(path) => {
            if let Some(k) = Key::open(Hive::CurrentUser, path) {
                for v in k.values() {
                    p.reg_values.push((path.to_string(), v.name));
                }
            }
        }
    }
}

pub fn clean_special(p: &Pending) -> SpecialResult {
    let mut out = SpecialResult::default();
    if let Some((bytes, items)) = p.recycle {
        match winsys::empty_recycle_bin() {
            Ok(()) => {
                out.freed += bytes;
                out.removed += items;
            }
            Err(e) => out.notes.push(e),
        }
    }
    for a in &p.actions {
        let r = match *a {
            "clipboard" => winsys::clear_clipboard(),
            _ => winsys::flush_dns(),
        };
        if let Err(e) = r {
            out.notes.push(e);
        }
    }
    for (path, name) in &p.reg_values {
        match Key::open_writable(Hive::CurrentUser, path).and_then(|k| k.delete_value(name)) {
            Ok(()) => out.removed += 1,
            Err(_) => out.skipped += 1,
        }
    }
    out
}

// ----------------------------------------------------------------------
// Scheduled cleaning (Task Scheduler)

const TASK_NAME: &str = r"Heft\Weekly clean";

pub fn schedule_enabled() -> bool {
    std::process::Command::new("schtasks.exe")
        .args(["/Query", "/TN", TASK_NAME])
        .creation_flags(winsys::CREATE_NO_WINDOW)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Create or remove a weekly task that runs `heft --clean` with the saved
/// selection. As administrator the task runs elevated so admin-only rules
/// are included.
pub fn set_schedule(on: bool) -> Result<(), String> {
    let mut cmd = std::process::Command::new("schtasks.exe");
    if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let tr = format!("\"{}\" --clean", exe.display());
        cmd.args(["/Create", "/F", "/TN", TASK_NAME, "/SC", "WEEKLY", "/D", "SUN", "/ST", "12:00", "/TR", &tr]);
        if crate::platform::is_elevated() {
            cmd.args(["/RL", "HIGHEST"]);
        }
    } else {
        cmd.args(["/Delete", "/F", "/TN", TASK_NAME]);
    }
    let out = cmd.creation_flags(winsys::CREATE_NO_WINDOW).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}
