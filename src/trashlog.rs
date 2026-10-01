//! A log of what Heft moved to the Recycle Bin / Trash, so it can be found and
//! put back later.
//!
//! The log is a small tab-separated text file in Heft's data folder. On
//! Windows and Linux restoring goes through the system trash. macOS has no
//! call for putting things back, so Heft records where each item landed in
//! the Trash and moves it back from there itself.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::platform;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removed {
    pub when: i64,
    pub size: u64,
    pub is_dir: bool,
    pub path: String,
    /// Where it landed in the Trash (macOS; older logs don't have it).
    pub trashed_at: Option<String>,
    /// Its file id there, so a later item that took the same name in the
    /// Trash isn't mistaken for it (0: unknown).
    pub trash_id: u64,
    /// Put back by Heft since.
    pub restored: bool,
}

impl Removed {
    /// What [`still_in_trash`] reports this entry by.
    pub fn key(&self) -> &str {
        self.trashed_at.as_deref().unwrap_or(&self.path)
    }
}

/// Where an item went in the Trash, and its file id there.
pub type Landed = Option<(String, u64)>;

/// Move `path` to the Recycle Bin or Trash, and say where it went when the
/// system tells (macOS).
pub fn move_to_trash(path: &str) -> Result<Landed, String> {
    let landed = platform::move_to_trash(Path::new(path))?;
    Ok(landed.as_deref().map(located))
}

/// An item's place in the Trash as the log records it: its path and file id.
pub fn located(at: &Path) -> (String, u64) {
    (at.to_string_lossy().into_owned(), file_id(at))
}

#[cfg(unix)]
fn file_id(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(p).map(|m| m.ino()).unwrap_or(0)
}

#[cfg(not(unix))]
fn file_id(_p: &Path) -> u64 {
    0
}

/// Keep the log from growing forever.
const KEEP: usize = 2000;

fn log_path() -> PathBuf {
    match std::env::var_os("HEFT_DATA_DIR").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d).join("removed.log"),
        None => platform::data_dir().join("removed.log"),
    }
}

// Lines in the log. The first two are what older versions wrote, with the
// path as the rest of the line; newer lines escape their paths so they can
// hold more than one:
//
//   trash     <when> <size> <d|f> <path>
//   restored  <when> 0 - <path>
//   trashed   <when> <size> <d|f> <path> <where in the trash> <file id>
//   put-back  <when> <path>

/// Remember `items` as just moved to the trash.
pub fn record(items: &[Removed]) {
    append(items.iter().map(|r| {
        format!(
            "trashed\t{}\t{}\t{}\t{}\t{}\t{}",
            r.when,
            r.size,
            if r.is_dir { "d" } else { "f" },
            escape(&r.path),
            escape(r.trashed_at.as_deref().unwrap_or("")),
            r.trash_id
        )
    }));
}

fn mark_restored(items: &[Removed]) {
    let now = platform::now_unix();
    append(items.iter().map(|r| format!("put-back\t{now}\t{}", escape(&r.path))));
}

/// Tabs, line breaks and `%` as `%09`-style escapes. Anything else, including
/// Windows' backslashes, stays as it is.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' | '\t' | '\n' | '\r' => out.push_str(&format!("%{:02X}", c as u32)),
            _ => out.push(c),
        }
    }
    out
}

fn unescape(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let hex = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b[i], hex) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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
    parse(&text)
}

fn parse(text: &str) -> Vec<Removed> {
    let mut out: Vec<Removed> = Vec::new();
    let mark = |out: &mut Vec<Removed>, path: &str| {
        if let Some(r) = out.iter_mut().rev().find(|r| !r.restored && platform::names_eq(&r.path, path)) {
            r.restored = true;
        }
    };
    for line in text.lines() {
        let (kind, rest) = line.split_once('\t').unwrap_or((line, ""));
        match kind {
            "trash" | "restored" => {
                let f: Vec<&str> = rest.splitn(4, '\t').collect();
                if f.len() != 4 {
                    continue;
                }
                if kind == "restored" {
                    mark(&mut out, f[3]);
                    continue;
                }
                out.push(Removed {
                    when: f[0].parse().unwrap_or(0),
                    size: f[1].parse().unwrap_or(0),
                    is_dir: f[2] == "d",
                    path: f[3].to_string(),
                    trashed_at: None,
                    trash_id: 0,
                    restored: false,
                });
            }
            "trashed" => {
                let f: Vec<&str> = rest.split('\t').collect();
                if f.len() < 6 {
                    continue;
                }
                out.push(Removed {
                    when: f[0].parse().unwrap_or(0),
                    size: f[1].parse().unwrap_or(0),
                    is_dir: f[2] == "d",
                    path: unescape(f[3]),
                    trashed_at: Some(unescape(f[4])).filter(|p| !p.is_empty()),
                    trash_id: f[5].parse().unwrap_or(0),
                    restored: false,
                });
            }
            "put-back" => {
                if let Some((_, path)) = rest.split_once('\t') {
                    mark(&mut out, &unescape(path));
                }
            }
            _ => {}
        }
    }
    out.reverse();
    out
}

/// Keys ([`Removed::key`]) of the items from `items` that are still sitting in
/// the system trash.
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
pub fn still_in_trash(items: &[Removed]) -> Result<std::collections::HashSet<String>, String> {
    let listed = trash::os_limited::list().map_err(|e| e.to_string())?;
    Ok(items
        .iter()
        .filter(|r| listed.iter().any(|t| same_item(t, &r.path)))
        .map(|r| r.key().to_string())
        .collect())
}

/// macOS doesn't let apps list the Trash, but an item Heft put there can be
/// looked up where it landed.
#[cfg(target_os = "macos")]
pub fn still_in_trash(items: &[Removed]) -> Result<std::collections::HashSet<String>, String> {
    Ok(items.iter().filter(|r| in_trash(r).is_some()).map(|r| r.key().to_string()).collect())
}

/// Where `r` is in the Trash, if it's still there and is the same item.
#[cfg(target_os = "macos")]
fn in_trash(r: &Removed) -> Option<&Path> {
    use std::os::unix::fs::MetadataExt;
    let at = Path::new(r.trashed_at.as_deref()?);
    let md = std::fs::symlink_metadata(at).ok()?;
    (md.is_dir() == r.is_dir && (r.trash_id == 0 || md.ino() == r.trash_id)).then_some(at)
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
        trash::Error::RestoreCollision { path, .. } => collision(&path),
        other => other.to_string(),
    })?;
    mark_restored(&done);
    Ok(done.len())
}

fn collision(path: &Path) -> String {
    format!("Something already exists at {}. Move it out of the way and try again.", path.display())
}

/// Put `items` back where they came from by moving them out of the Trash.
/// Nothing is ever replaced: if something already exists at the original
/// path, that item stays in the Trash.
#[cfg(target_os = "macos")]
pub fn restore(items: &[Removed]) -> Result<usize, String> {
    let mut done = Vec::new();
    let mut failed = Vec::new();
    for r in items {
        match put_back(r) {
            Ok(()) => done.push(r.clone()),
            Err(e) => failed.push(e),
        }
    }
    mark_restored(&done);
    match failed.first() {
        None => Ok(done.len()),
        Some(e) if done.is_empty() => Err(e.clone()),
        Some(e) => Err(format!("{e} The other {} were put back.", done.len())),
    }
}

#[cfg(target_os = "macos")]
fn put_back(r: &Removed) -> Result<(), String> {
    if r.trashed_at.is_none() {
        return Err("Heft didn't record where it went in the Trash. Open the Trash in Finder, right-click it and \
                    choose Put Back."
            .into());
    }
    let from = in_trash(r).ok_or_else(|| format!("Not in the {} any more. It may have been emptied.", platform::TRASH))?;
    let to = Path::new(&r.path);
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(collision(to));
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Couldn't recreate {}: {e}", parent.display()))?;
    }
    rename_new(from, to).map_err(|e| match e.raw_os_error() {
        Some(libc::EEXIST) => collision(to),
        Some(libc::EXDEV) => "It's on another drive now. Open the Trash in Finder, right-click it and choose Put Back.".into(),
        _ => e.to_string(),
    })
}

/// Rename without replacing anything that appeared at `to` in the meantime.
#[cfg(target_os = "macos")]
fn rename_new(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(std::io::Error::other);
    let (f, t) = (c(from)?, c(to)?);
    if unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, trashed_at: Option<&str>) -> Removed {
        Removed {
            when: 10,
            size: 5,
            is_dir: false,
            path: path.into(),
            trashed_at: trashed_at.map(String::from),
            trash_id: 7,
            restored: false,
        }
    }

    #[test]
    fn log_roundtrip() {
        let dir = std::env::temp_dir().join(format!("heft-trashlog-{}", std::process::id()));
        unsafe { std::env::set_var("HEFT_DATA_DIR", &dir) };
        let a = item("/x/a.txt", None);
        let b = Removed { when: 20, size: 7, is_dir: true, path: "/x/b dir".into(), ..item("", Some("/t/b dir 2")) };
        record(&[a.clone(), b.clone()]);
        mark_restored(std::slice::from_ref(&a));
        let got = read();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], b, "newest first");
        assert!(got[1].restored && got[1].path == "/x/a.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_old_logs() {
        let text = "trash\t10\t5\tf\t/x/a.txt\n\
                    trash\t11\t6\td\t/x/tab\there\n\
                    restored\t12\t0\t-\t/x/a.txt\n\
                    something new\t1\n";
        let got = parse(text);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].path, "/x/tab\there", "the path is the rest of the line");
        assert!(got[0].is_dir && !got[0].restored && got[0].trashed_at.is_none());
        assert!(got[1].restored && got[1].size == 5);
    }

    #[test]
    fn escapes_awkward_paths() {
        for p in ["/x/tab\there", "/x/line\nbreak", "/x/100%", "C:\\Users\\ana\\a b.txt", "/x/%09 literal"] {
            assert_eq!(unescape(&escape(p)), p);
            assert!(!escape(p).contains(['\t', '\n']));
        }
        let r = item("/x/tab\there", Some("/Users/ana/.Trash/tab\there 2"));
        let line = format!(
            "trashed\t{}\t{}\tf\t{}\t{}\t{}\nput-back\t30\t{}\n",
            r.when,
            r.size,
            escape(&r.path),
            escape(r.trashed_at.as_deref().unwrap()),
            r.trash_id,
            escape(&r.path)
        );
        let got = parse(&line);
        assert_eq!(got, [Removed { restored: true, ..r }]);
        assert_eq!(got[0].key(), "/Users/ana/.Trash/tab\there 2");
    }

    /// Uses the real Recycle Bin / Trash, so it only runs on request:
    /// `cargo test trash_and_restore -- --ignored`
    #[test]
    #[ignore]
    fn trash_and_restore_real_file() {
        let dir = std::env::temp_dir().join(format!("heft-restore-{}", std::process::id()));
        unsafe { std::env::set_var("HEFT_DATA_DIR", dir.join("data")) };
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("put-me-back.txt");
        std::fs::write(&file, b"hello").unwrap();
        let path = file.to_string_lossy().into_owned();
        let landed = move_to_trash(&path).unwrap();
        assert!(!file.exists());
        assert_eq!(landed.is_some(), cfg!(target_os = "macos"), "{landed:?}");
        let (trashed_at, trash_id) = landed.map_or((None, 0), |(p, id)| (Some(p), id));
        let item = Removed { when: platform::now_unix(), size: 5, is_dir: false, path, trashed_at, trash_id, restored: false };
        record(std::slice::from_ref(&item));
        assert!(still_in_trash(std::slice::from_ref(&item)).unwrap().contains(item.key()));
        // On macOS Heft moves it back itself, recreating the folder it was in.
        if cfg!(target_os = "macos") {
            std::fs::remove_dir(&sub).unwrap();
        }
        assert_eq!(restore(std::slice::from_ref(&item)), Ok(1));
        assert_eq!(std::fs::read(&file).unwrap(), b"hello");
        assert!(read()[0].restored);
        assert!(still_in_trash(std::slice::from_ref(&item)).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Something already at the original path is never replaced.
    #[test]
    #[cfg(target_os = "macos")]
    fn restore_refuses_to_replace() {
        let dir = std::env::temp_dir().join(format!("heft-putback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A stand-in for the item in the Trash.
        let trashed = dir.join("in-trash.txt");
        std::fs::write(&trashed, b"old").unwrap();
        let back = dir.join("back.txt");
        std::fs::write(&back, b"new").unwrap();
        let mut r = item(&back.to_string_lossy(), Some(&trashed.to_string_lossy()));
        r.trash_id = 0;
        let e = put_back(&r).unwrap_err();
        assert!(e.starts_with("Something already exists at"), "{e}");
        assert_eq!(std::fs::read(&back).unwrap(), b"new");
        assert!(trashed.exists());
        // A different item that took its name in the Trash isn't it.
        std::fs::remove_file(&back).unwrap();
        r.trash_id = 1;
        assert!(put_back(&r).unwrap_err().starts_with("Not in the Trash"));
        assert!(!back.exists());
        // The right one goes back.
        use std::os::unix::fs::MetadataExt;
        r.trash_id = std::fs::metadata(&trashed).unwrap().ino();
        put_back(&r).unwrap();
        assert_eq!(std::fs::read(&back).unwrap(), b"old");
        assert!(!trashed.exists());
        // Logged by an older version: no location, so no guessing.
        assert!(put_back(&item(&back.to_string_lossy(), None)).unwrap_err().contains("Put Back"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
