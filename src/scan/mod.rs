//! Scanning: picks the fast MFT reader when possible (Windows, NTFS, admin),
//! otherwise walks the directory tree in parallel. Runs on a background thread.

#[cfg(windows)]
pub mod mft;
pub mod walk;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel::Receiver;

use crate::platform::{self, SEP};
use crate::tree::{ScanInfo, ScanMode, Tree};

#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub cancel: AtomicBool,
    /// Fraction done in 1/10000ths, or u32::MAX if unknown.
    pub fraction: AtomicU32,
    pub phase: Mutex<String>,
    pub mode: Mutex<Option<ScanMode>>,
}

impl Progress {
    pub fn set_phase(&self, s: impl Into<String>) {
        *self.phase.lock().unwrap() = s.into();
    }
    pub fn set_fraction(&self, f: Option<f32>) {
        let v = f.map(|f| (f.clamp(0.0, 1.0) * 10_000.0) as u32).unwrap_or(u32::MAX);
        self.fraction.store(v, Ordering::Relaxed);
    }
    pub fn fraction(&self) -> Option<f32> {
        let v = self.fraction.load(Ordering::Relaxed);
        (v != u32::MAX).then(|| v as f32 / 10_000.0)
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

pub enum ScanOutcome {
    Done(Tree),
    Cancelled,
    Failed(String),
}

pub struct ScanHandle {
    pub root: String,
    pub progress: Arc<Progress>,
    pub started: Instant,
    pub rx: Receiver<ScanOutcome>,
}

/// Normalise user input into an absolute path: quotes trimmed, `~`
/// expanded, relative paths resolved, trailing separators dropped (except on
/// a root such as `/` or `C:\`). On Windows also `c:` → `C:\`.
pub fn normalize_root(path: &str) -> String {
    let mut p = path.trim().trim_matches('"').to_string();
    if cfg!(windows) {
        p = p.replace('/', "\\");
        if p.len() == 2 && p.ends_with(':') {
            p.push('\\');
        }
        if p.len() >= 2 && p.as_bytes()[1] == b':' {
            p[..1].make_ascii_uppercase();
        }
    }
    if (p == "~" || p.starts_with("~/") || p.starts_with("~\\"))
        && let Some(home) = platform::home_dir()
    {
        p = format!("{home}{}", &p[1..]);
    }
    if !p.is_empty()
        && !std::path::Path::new(&p).is_absolute()
        && let Ok(abs) = std::path::absolute(&p)
    {
        p = abs.to_string_lossy().into_owned();
    }
    let keep = if cfg!(windows) { 3 } else { 1 };
    while p.len() > keep && p.ends_with(SEP) {
        p.pop();
    }
    p
}

/// Why the fast path can't be used for `root`, or `None` if it can.
#[cfg(windows)]
pub fn mft_unavailable_reason(root: &str) -> Option<String> {
    let bytes = root.as_bytes();
    if bytes.len() < 3 || bytes[1] != b':' || bytes[2] != b'\\' {
        return Some("not a local drive path".into());
    }
    let vol = &root[..3];
    match platform::volume_info(vol) {
        Some((_, fs)) if fs.eq_ignore_ascii_case("NTFS") => {}
        Some((_, fs)) => return Some(format!("{fs} volume (fast scan needs NTFS)")),
        None => return Some("volume information unavailable".into()),
    }
    if !platform::is_elevated() {
        return Some("needs administrator rights".into());
    }
    None
}

#[cfg(not(windows))]
pub fn mft_unavailable_reason(_root: &str) -> Option<String> {
    Some("the MFT fast scan is only available on Windows".into())
}

#[cfg(windows)]
pub fn mft_scan(root: &str, progress: &Progress) -> Result<Tree, String> {
    mft::scan(root, progress)
}

#[cfg(not(windows))]
pub fn mft_scan(_root: &str, _progress: &Progress) -> Result<Tree, String> {
    Err("not supported on this platform".into())
}

pub fn start(root: &str, allow_mft: bool, on_done: impl Fn() + Send + 'static) -> ScanHandle {
    let root = normalize_root(root);
    let progress = Arc::new(Progress::default());
    progress.set_fraction(None);
    let (tx, rx) = crossbeam_channel::bounded(1);
    let handle = ScanHandle { root: root.clone(), progress: progress.clone(), started: Instant::now(), rx };

    std::thread::Builder::new()
        .name("scan".into())
        .spawn(move || {
            let outcome = run(&root, allow_mft, &progress);
            let _ = tx.send(outcome);
            on_done();
        })
        .expect("spawn scan thread");
    handle
}

/// Synchronous scan (used by the CLI and the background thread).
pub fn run(root: &str, allow_mft: bool, progress: &Progress) -> ScanOutcome {
    let t0 = Instant::now();
    let mut note = None;
    let mut result = None;

    if allow_mft {
        match mft_unavailable_reason(root) {
            None => {
                *progress.mode.lock().unwrap() = Some(ScanMode::Mft);
                match mft_scan(root, progress) {
                    Ok(t) => result = Some(t),
                    Err(e) => note = Some(format!("Fast MFT scan failed ({e}); used a standard scan instead.")),
                }
            }
            // Only worth mentioning where the fast path exists at all.
            Some(reason) if cfg!(windows) => note = Some(format!("Standard scan: {reason}.")),
            Some(_) => {}
        }
    }
    if progress.cancelled() {
        return ScanOutcome::Cancelled;
    }
    let result = match result {
        Some(t) => Ok(t),
        None => {
            *progress.mode.lock().unwrap() = Some(ScanMode::Walk);
            progress.files.store(0, Ordering::Relaxed);
            progress.dirs.store(0, Ordering::Relaxed);
            progress.bytes.store(0, Ordering::Relaxed);
            progress.set_fraction(None);
            walk::scan(root, progress)
        }
    };
    if progress.cancelled() {
        return ScanOutcome::Cancelled;
    }
    match result {
        Ok(mut tree) => {
            tree.info.duration_ms = t0.elapsed().as_millis() as u64;
            tree.info.finished_at = platform::now_unix();
            if tree.info.note.is_none() {
                tree.info.note = note;
            }
            ScanOutcome::Done(tree)
        }
        Err(e) => ScanOutcome::Failed(e),
    }
}

pub(crate) fn base_info(mode: ScanMode) -> ScanInfo {
    ScanInfo { mode, duration_ms: 0, finished_at: 0, unreadable_dirs: 0, note: None, phases: Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn normalizes() {
        assert_eq!(normalize_root("c:"), "C:\\");
        assert_eq!(normalize_root("d:/games/"), "D:\\games");
        assert_eq!(normalize_root("\"C:\\Users\\\""), "C:\\Users");
        assert_eq!(normalize_root("C:\\"), "C:\\");
        assert_eq!(normalize_root("\\\\server\\share\\"), "\\\\server\\share");
    }

    #[test]
    #[cfg(unix)]
    fn normalizes() {
        assert_eq!(normalize_root("/"), "/");
        assert_eq!(normalize_root("/usr/local/"), "/usr/local");
        assert_eq!(normalize_root("\"/Volumes/My Disk\""), "/Volumes/My Disk");
        if let Some(home) = platform::home_dir() {
            assert_eq!(normalize_root("~/Music"), format!("{home}/Music"));
        }
    }

    #[test]
    fn relative_paths_become_absolute() {
        assert!(std::path::Path::new(&normalize_root(".")).is_absolute());
    }
}
