//! Parallel directory walker. Worker threads list directories; a single
//! builder thread (the caller) assigns node ids and hands out new work, so the
//! tree arena needs no locking.
//!
//! Listing is platform-specific (`windows.rs` batches entries with
//! `GetFileInformationByHandleEx`, `macos.rs` with `getattrlistbulk`;
//! `portable.rs` uses `std::fs` and is what Linux runs). The walk itself
//! (hard-link and loop detection, skipping virtual file systems) is shared.

#[cfg_attr(windows, allow(dead_code))]
mod portable;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod update;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
pub use update::{update, Changes};
#[cfg(windows)]
pub use windows::list_root_files;

use std::collections::HashSet;
use std::ffi::OsString;
use std::hash::{BuildHasherDefault, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use crossbeam_channel::{unbounded, Receiver, Sender};

use super::{base_info, Progress};
use crate::tree::{flags, NodeId, ScanMode, Tree, TreeBuilder, ROOT};

/// One directory entry as reported by a lister.
pub(super) struct Entry {
    pub name: String,
    /// Exact on-disk name for building child paths (only for directories).
    pub os_name: Option<OsString>,
    pub flags: u16,
    pub size: u64,
    pub alloc: u64,
    pub mtime: i64,
    /// Identity of the underlying file, or 0 if unknown / not needed. For
    /// files a repeat means a hard link; for folders, a loop or bind mount.
    pub file_id: u128,
}

/// Lists one directory. Implementations must be shareable across threads;
/// `Scratch` is per-thread working memory.
pub(super) trait Lister: Sync {
    type Scratch: Default;
    fn threads(&self) -> usize;
    fn list(&self, path: &Path, scratch: &mut Self::Scratch, progress: &Progress) -> Option<Vec<Entry>>;
}

struct Task {
    id: NodeId,
    path: PathBuf,
}

struct DirResult {
    id: NodeId,
    path: PathBuf,
    entries: Vec<Entry>,
    unreadable: bool,
}

/// File IDs are already well distributed; skip SipHash for the dedupe sets.
#[derive(Default)]
struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u128(&mut self, v: u128) {
        self.0 = ((v as u64) ^ ((v >> 64) as u64).rotate_left(29)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

type IdSet = HashSet<u128, BuildHasherDefault<IdHasher>>;

/// What a tree doesn't store but a quick rescan needs to come out exactly
/// like a full scan: the identity and own modification time of every
/// folder, and the identity of every file with more than one name. Both
/// lists are in node order. Only macOS rescans this way.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Default)]
pub struct Ids {
    pub dirs: Vec<(NodeId, u128, i64)>,
    pub links: Vec<(NodeId, u128)>,
}

pub fn scan(root: &str, progress: &Progress) -> Result<Tree, String> {
    scan_with(root, progress, &lister(), &skip_mounts(), None)
}

/// A scan that also returns the [`Ids`] quick rescans need.
#[cfg(target_os = "macos")]
pub fn scan_keeping_ids(root: &str, progress: &Progress) -> Result<(Tree, Ids), String> {
    let mut ids = Ids::default();
    let tree = scan_with(root, progress, &lister(), &skip_mounts(), Some(&mut ids))?;
    Ok((tree, ids))
}

#[cfg(windows)]
fn lister() -> windows::WinLister {
    windows::WinLister::default()
}

#[cfg(target_os = "macos")]
fn lister() -> macos::BulkLister {
    macos::BulkLister
}

#[cfg(not(any(windows, target_os = "macos")))]
fn lister() -> portable::StdLister {
    portable::StdLister
}

/// Mount points of virtual file systems (`/proc`, `/sys`, snapshots…) that a
/// scan of `/` must not wander into.
#[cfg(unix)]
fn skip_mounts() -> HashSet<PathBuf> {
    crate::platform::skip_mounts().into_iter().map(PathBuf::from).collect()
}

#[cfg(not(unix))]
fn skip_mounts() -> HashSet<PathBuf> {
    HashSet::new()
}

fn scan_with<L: Lister>(
    root: &str,
    progress: &Progress,
    lister: &L,
    skip: &HashSet<PathBuf>,
    mut ids: Option<&mut Ids>,
) -> Result<Tree, String> {
    let root_path = PathBuf::from(root);
    let md = std::fs::metadata(&root_path).map_err(|e| format!("cannot open {root}: {e}"))?;
    if !md.is_dir() {
        return Err(format!("{root} is not a folder"));
    }
    progress.set_phase("Reading folders");

    let mut b = TreeBuilder::new(root);
    let mut unreadable = 0u64;
    let mut seen_files = IdSet::default();
    let mut seen_dirs = IdSet::default();
    let t0 = std::time::Instant::now();

    std::thread::scope(|s| {
        let (task_tx, task_rx) = unbounded::<Task>();
        let (res_tx, res_rx) = unbounded::<DirResult>();
        for _ in 0..lister.threads() {
            let task_rx = task_rx.clone();
            let res_tx = res_tx.clone();
            s.spawn(move || worker(lister, task_rx, res_tx, progress));
        }
        drop(res_tx);
        drop(task_rx);

        task_tx.send(Task { id: ROOT, path: root_path.clone() }).unwrap();
        let mut pending = 1usize;
        while pending > 0 {
            let Ok(r) = res_rx.recv() else { break };
            pending -= 1;
            if r.unreadable {
                unreadable += 1;
                b.or_flags(r.id, flags::UNREADABLE);
            }
            for mut e in r.entries {
                let is_dir = e.flags & flags::DIR != 0;
                let mut descend = e.os_name.take().filter(|_| is_dir);
                if e.file_id != 0 {
                    if !is_dir && !seen_files.insert(e.file_id) {
                        // Counted under another name already: keep the entry
                        // visible, but don't count its bytes twice.
                        e.flags |= flags::HARDLINK;
                        e.size = 0;
                        e.alloc = 0;
                    } else if is_dir && !seen_dirs.insert(e.file_id) {
                        e.flags |= flags::SEEN;
                        descend = None;
                    }
                }
                let child = descend.map(|os| r.path.join(os));
                if let Some(path) = &child
                    && skip.contains(path)
                {
                    e.flags |= flags::MOUNT;
                }
                let id = b.add(r.id, &e.name, e.flags, e.size, e.alloc, e.mtime);
                if let Some(ids) = ids.as_deref_mut() {
                    if is_dir {
                        ids.dirs.push((id, e.file_id, e.mtime));
                    } else if e.file_id != 0 {
                        ids.links.push((id, e.file_id));
                    }
                }
                if let Some(path) = child
                    && e.flags & flags::MOUNT == 0
                    && !progress.cancelled()
                {
                    task_tx.send(Task { id, path }).unwrap();
                    pending += 1;
                }
            }
        }
        drop(task_tx);
    });

    if progress.cancelled() {
        return Err("cancelled".into());
    }
    progress.set_phase("Building tree");
    let walk_ms = t0.elapsed().as_millis() as u64;
    let mut info = base_info(ScanMode::Walk);
    info.unreadable_dirs = unreadable;
    let t1 = std::time::Instant::now();
    let mut tree = b.finish(root.to_string(), info);
    tree.info.phases = vec![("read folders", walk_ms), ("aggregate", t1.elapsed().as_millis() as u64)];
    Ok(tree)
}

fn worker<L: Lister>(lister: &L, tasks: Receiver<Task>, results: Sender<DirResult>, progress: &Progress) {
    let mut scratch = L::Scratch::default();
    while let Ok(task) = tasks.recv() {
        let (entries, unreadable) = if progress.cancelled() {
            (Vec::new(), false)
        } else {
            match lister.list(&task.path, &mut scratch, progress) {
                Some(e) => (e, false),
                None => (Vec::new(), true),
            }
        };
        progress.dirs.fetch_add(1, Ordering::Relaxed);
        if results.send(DirResult { id: task.id, path: task.path, entries, unreadable }).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::NodeId;

    /// A throwaway folder tree:
    /// ```text
    /// a/x.bin      1000 B
    /// a/b/y.txt      10 B
    /// c/z.dat      5000 B
    /// c/z-link.dat  hard link to c/z.dat
    /// .hidden         7 B
    /// ```
    fn fixture() -> PathBuf {
        // Tests run in parallel, sometimes within the same clock tick.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("heft-walk-{}-{nanos}-{n}", std::process::id()));
        std::fs::create_dir_all(dir.join("a").join("b")).unwrap();
        std::fs::create_dir_all(dir.join("c")).unwrap();
        std::fs::write(dir.join("a").join("x.bin"), vec![1u8; 1000]).unwrap();
        std::fs::write(dir.join("a").join("b").join("y.txt"), b"0123456789").unwrap();
        std::fs::write(dir.join("c").join("z.dat"), vec![2u8; 5000]).unwrap();
        std::fs::hard_link(dir.join("c").join("z.dat"), dir.join("c").join("z-link.dat")).unwrap();
        std::fs::write(dir.join(".hidden"), b"1234567").unwrap();
        dir
    }

    fn child(t: &Tree, parent: NodeId, name: &str) -> NodeId {
        *t.children(parent).iter().find(|&&c| t.name(c) == name).unwrap_or_else(|| panic!("no {name}"))
    }

    fn check(t: &Tree, dedupes_hard_links: bool) {
        let root = t.node(ROOT);
        assert_eq!(root.files, 5);
        let expect = if dedupes_hard_links { 6017 } else { 11017 };
        assert_eq!(root.size, expect);
        let a = child(t, ROOT, "a");
        assert_eq!(t.node(a).size, 1010);
        assert_eq!(t.node(child(t, a, "b")).files, 1);
        if dedupes_hard_links {
            let c = child(t, ROOT, "c");
            let links = t.children(c).iter().filter(|&&f| t.node(f).flags & flags::HARDLINK != 0).count();
            assert_eq!(links, 1, "exactly one of the two names carries the bytes");
        }
    }

    #[test]
    fn portable_lister_walks_a_tree() {
        let dir = fixture();
        let root = crate::scan::normalize_root(&dir.to_string_lossy());
        let t = scan_with(&root, &Progress::default(), &portable::StdLister, &HashSet::new(), None).unwrap();
        // Hard links are only identifiable through std on Unix (dev + inode).
        check(&t, cfg!(unix));
        if cfg!(unix) {
            let hidden = child(&t, ROOT, ".hidden");
            assert!(t.node(hidden).flags & flags::HIDDEN != 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(windows)]
    fn windows_lister_walks_a_tree() {
        let dir = fixture();
        let root = crate::scan::normalize_root(&dir.to_string_lossy());
        let t = scan_with(&root, &Progress::default(), &windows::WinLister::default(), &HashSet::new(), None).unwrap();
        check(&t, true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_lister_walks_a_tree() {
        let dir = fixture();
        let root = crate::scan::normalize_root(&dir.to_string_lossy());
        let t = scan_with(&root, &Progress::default(), &macos::BulkLister, &HashSet::new(), None).unwrap();
        check(&t, true);
        let hidden = child(&t, ROOT, ".hidden");
        assert!(t.node(hidden).flags & flags::HIDDEN != 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skipped_mounts_are_listed_but_not_entered() {
        let dir = fixture();
        let root = crate::scan::normalize_root(&dir.to_string_lossy());
        let skip = HashSet::from([PathBuf::from(&root).join("a")]);
        #[cfg(windows)]
        let lister = windows::WinLister::default();
        #[cfg(not(windows))]
        let lister = portable::StdLister;
        let t = scan_with(&root, &Progress::default(), &lister, &skip, None).unwrap();
        let a = child(&t, ROOT, "a");
        assert!(t.node(a).flags & flags::MOUNT != 0);
        assert!(t.children(a).is_empty());
        assert_eq!(t.node(ROOT).files, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
