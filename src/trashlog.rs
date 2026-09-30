//! A log of what Heft moved to the Recycle Bin / Trash, so it can be found and
//! put back later.
//!
//! The log is a small tab-separated text file in Heft's data folder. Restoring
//! goes through the system trash, which works on Windows and Linux; macOS
//! doesn't let other apps put things back, so there Heft opens the Trash in
//! Finder instead.

use std::io::Write;
use std::path::PathBuf;

use crate::platform;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removed {
    pub when: i64,
    pub size: u64,
    pub is_dir: bool,
    pub path: String,
    /// Put back by Heft since.
    pub restored: bool,
}

/// Keep the log from growing forever.
const KEEP: usize = 2000;

fn log_path() -> PathBuf {
    match std::env::var_os("HEFT_DATA_DIR").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d).join("removed.log"),
        None => platform::data_dir().join("removed.log"),
    }
}

/// Remember `items` as just moved to the trash.
pub fn record(items: &[Removed]) {
    append(items.iter().map(|r| format!("trash\t{}\t{}\t{}\t{}", r.when, r.size, if r.is_dir { "d" } else { "f" }, r.path)));
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn mark_restored(items: &[Removed]) {
    let now = platform::now_unix();
    append(items.iter().map(|r| format!("restored\t{now}\t0\t-\t{}", r.path)));
}

fn append(lines: impl Iterator<Item = String>) {
    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text: String = lines.map(|l| l + "\n").collect();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(text.as_bytes());
    }
    trim(&path);
}

fn trim(path: &PathBuf) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() > KEEP * 2 {
        let kept = lines[lines.len() - KEEP..].join("\n") + "\n";
        let _ = std::fs::write(path, kept);
    }
}

/// Everything in the log, newest first.
pub fn read() -> Vec<Removed> {
    let Ok(text) = std::fs::read_to_string(log_path()) else { return Vec::new() };
    let mut out: Vec<Removed> = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.splitn(5, '\t').collect();
        if f.len() != 5 {
            continue;
        }
        match f[0] {
            "trash" => out.push(Removed {
                when: f[1].parse().unwrap_or(0),
                size: f[2].parse().unwrap_or(0),
                is_dir: f[3] == "d",
                path: f[4].to_string(),
                restored: false,
            }),
            "restored" => {
                if let Some(r) = out.iter_mut().rev().find(|r| !r.restored && platform::names_eq(&r.path, f[4])) {
                    r.restored = true;
                }
            }
            _ => {}
        }
    }
    out.reverse();
    out
}

/// Paths from `items` that are still sitting in the system trash.
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
pub fn still_in_trash(items: &[Removed]) -> Result<std::collections::HashSet<String>, String> {
    let listed = trash::os_limited::list().map_err(|e| e.to_string())?;
    Ok(items
        .iter()
        .filter(|r| listed.iter().any(|t| same_item(t, &r.path)))
        .map(|r| r.path.clone())
        .collect())
}

#[cfg(target_os = "macos")]
pub fn still_in_trash(_items: &[Removed]) -> Result<std::collections::HashSet<String>, String> {
    Err("not available on macOS".into())
}

/// Whether trash entry `t` is the item that was at `path`. Windows lists names
/// the way Explorer shows them, which can leave off the extension, so the
/// name may match with or without it.
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
fn same_item(t: &trash::TrashItem, path: &str) -> bool {
    let p = std::path::Path::new(path);
    let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else { return false };
    if !platform::names_eq(&t.original_parent.to_string_lossy(), &parent.to_string_lossy()) {
        return false;
    }
    let listed = t.name.to_string_lossy();
    platform::names_eq(&listed, &name.to_string_lossy())
        || p.file_stem().is_some_and(|stem| platform::names_eq(&listed, &stem.to_string_lossy()))
}

/// Put `items` back where they came from. For each path the most recently
/// trashed copy is restored.
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
pub fn restore(items: &[Removed]) -> Result<usize, String> {
    let listed = trash::os_limited::list().map_err(|e| e.to_string())?;
    let mut chosen = Vec::new();
    let mut done = Vec::new();
    for r in items {
        let best = listed
            .iter()
            .filter(|t| same_item(t, &r.path))
            .min_by_key(|t| (t.time_deleted - r.when).abs());
        if let Some(t) = best {
            let mut t = t.clone();
            // The trash crate restores under the listed name, which on Windows
            // can be missing the extension. Use the name we recorded instead.
            if let Some(name) = std::path::Path::new(&r.path).file_name() {
                t.name = name.to_os_string();
            }
            chosen.push(t);
            done.push(r.clone());
        }
    }
    if chosen.is_empty() {
        return Err(format!("Not in the {} any more. It may have been emptied.", platform::TRASH));
    }
    trash::os_limited::restore_all(chosen).map_err(|e| match e {
        trash::Error::RestoreCollision { path, .. } => {
            format!("Something already exists at {}. Move it out of the way and try again.", path.display())
        }
        other => other.to_string(),
    })?;
    mark_restored(&done);
    Ok(done.len())
}

#[cfg(target_os = "macos")]
pub fn restore(_items: &[Removed]) -> Result<usize, String> {
    Err("macOS only lets Finder put things back. Open the Trash, right-click the item and choose Put Back.".into())
}

pub const CAN_RESTORE: bool = !cfg!(target_os = "macos");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_roundtrip() {
        let dir = std::env::temp_dir().join(format!("heft-trashlog-{}", std::process::id()));
        unsafe { std::env::set_var("HEFT_DATA_DIR", &dir) };
        let a = Removed { when: 10, size: 5, is_dir: false, path: "/x/a.txt".into(), restored: false };
        let b = Removed { when: 20, size: 7, is_dir: true, path: "/x/b dir".into(), restored: false };
        record(&[a.clone(), b.clone()]);
        mark_restored(std::slice::from_ref(&a));
        let got = read();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], b, "newest first");
        assert!(got[1].restored && got[1].path == "/x/a.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Uses the real Recycle Bin / Trash, so it only runs on request:
    /// `cargo test trash_and_restore -- --ignored`
    #[test]
    #[ignore]
    #[cfg(any(windows, all(unix, not(target_os = "macos"))))]
    fn trash_and_restore_real_file() {
        let dir = std::env::temp_dir().join(format!("heft-restore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("put-me-back.txt");
        std::fs::write(&file, b"hello").unwrap();
        let path = file.to_string_lossy().into_owned();
        trash::delete(&file).unwrap();
        assert!(!file.exists());
        let item = Removed { when: platform::now_unix(), size: 5, is_dir: false, path, restored: false };
        assert!(still_in_trash(std::slice::from_ref(&item)).unwrap().contains(&item.path));
        assert_eq!(restore(std::slice::from_ref(&item)), Ok(1));
        assert_eq!(std::fs::read(&file).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
