//! OS integration. Everything the rest of Heft needs from the operating system
//! goes through here; `windows.rs` and `unix.rs` implement the same functions.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

use std::borrow::Cow;
use std::path::PathBuf;

pub const SEP: char = std::path::MAIN_SEPARATOR;

/// Whether the usual file systems here ignore case in names (NTFS, APFS).
pub const CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

/// Only Windows can relaunch Heft elevated (UAC), and only there does
/// elevation unlock the fast MFT scanner.
pub const CAN_ELEVATE: bool = cfg!(windows);

#[cfg(windows)]
pub const TRASH: &str = "Recycle Bin";
#[cfg(not(windows))]
pub const TRASH: &str = "Trash";

#[cfg(windows)]
pub const FILE_MANAGER: &str = "Explorer";
#[cfg(target_os = "macos")]
pub const FILE_MANAGER: &str = "Finder";
#[cfg(not(any(windows, target_os = "macos")))]
pub const FILE_MANAGER: &str = "file manager";

#[derive(Clone, Debug)]
pub struct DriveInfo {
    /// `C:\`, `/`, `/Volumes/Backup`, or a folder such as the home folder.
    pub root: String,
    pub label: String,
    pub fs: String,
    pub kind: &'static str,
    pub total: u64,
    pub free: u64,
}

impl DriveInfo {
    pub fn is_ntfs(&self) -> bool {
        self.fs.eq_ignore_ascii_case("NTFS")
    }
}

/// The home folder first, then every mounted drive worth scanning.
pub fn list_locations() -> Vec<DriveInfo> {
    let mut out = Vec::new();
    if let Some(home) = home_dir() {
        let (total, free) = free_space(&home).unwrap_or((0, 0));
        out.push(DriveInfo { root: home, label: "Home folder".into(), fs: String::new(), kind: "Folder", total, free });
    }
    out.extend(list_drives());
    out
}

pub fn home_dir() -> Option<String> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(var).ok().filter(|h| !h.is_empty())
}

/// Where Heft keeps its own data (scan history).
pub fn data_dir() -> PathBuf {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let dir = if cfg!(windows) {
        env("LOCALAPPDATA").map(|p| p.join("Heft"))
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|p| p.join("Library").join("Application Support").join("Heft"))
    } else {
        env("XDG_DATA_HOME")
            .or_else(|| env("HOME").map(|h| h.join(".local").join("share")))
            .map(|p| p.join("heft"))
    };
    dir.unwrap_or_else(|| std::env::temp_dir().join("heft"))
}

/// Key for comparing file names the way this platform's file systems do.
pub fn name_key(name: &str) -> Cow<'_, str> {
    if CASE_INSENSITIVE { Cow::Owned(name.to_lowercase()) } else { Cow::Borrowed(name) }
}

pub fn names_eq(a: &str, b: &str) -> bool {
    a == b || (CASE_INSENSITIVE && a.to_lowercase() == b.to_lowercase())
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Calendar time in the local time zone.
pub struct LocalTime {
    pub year: u32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

pub fn fmt_date(unix: i64) -> String {
    match local_time(unix) {
        Some(t) => format!("{:04}-{:02}-{:02}", t.year, t.month, t.day),
        None => "-".into(),
    }
}

pub fn fmt_datetime(unix: i64) -> String {
    match local_time(unix) {
        Some(t) => format!("{:04}-{:02}-{:02} {:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute),
        None => "-".into(),
    }
}
