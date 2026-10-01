//! Quick rescans: bring a finished walk up to date once we know which
//! folders changed (macOS learns that from FSEvents), without walking
//! everything again.
//!
//! Changed folders are listed again with the lister a full scan uses. New
//! folders, and folders whose whole contents may have been swapped (created,
//! renamed, or events lost), are walked. Everything else is copied from the
//! previous tree. Hard links, folders reached twice and skipped mounts follow
//! the full scan's rules, so the result is the tree a full scan would build.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crossbeam_channel::unbounded;

use super::{worker, DirResult, Entry, IdSet, Ids, Lister, Task};
use crate::scan::{base_info, Progress};
use crate::tree::{flags, NodeId, ScanMode, Tree, TreeBuilder, NO_NODE, ROOT};

/// Which folders of the previous tree changed.
#[derive(Debug, Default)]
pub struct Changes {
    /// Folders whose own entries have to be listed again.
    pub dirty: HashSet<NodeId>,
    /// Folders whose contents may have been replaced wholesale: walked again
    /// from scratch.
    pub deep: HashSet<NodeId>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.dirty.is_empty() && self.deep.is_empty()
    }
}

/// A folder's entries as listed now. For folders of the previous tree,
/// `same[i]` is the old folder that entry `i` continues (same name, same
/// identity, not walked again), or `NO_NODE`.
struct Listing {
    entries: Vec<Entry>,
    same: Vec<NodeId>,
    unreadable: bool,
}

/// The previous tree with its side tables.
struct Old<'a> {
    tree: &'a Tree,
    ids: &'a Ids,
    /// Bit per node: is it in `ids.links`?
    is_link: Vec<u64>,
}

impl<'a> Old<'a> {
    fn new(tree: &'a Tree, ids: &'a Ids) -> Self {
        let mut is_link = vec![0u64; tree.len().div_ceil(64)];
        for &(n, _) in &ids.links {
            if let Some(w) = is_link.get_mut(n as usize / 64) {
                *w |= 1 << (n % 64);
            }
        }
        Old { tree, ids, is_link }
    }

    /// Identity and own modification time of a folder.
    fn dir(&self, id: NodeId) -> Option<(u128, i64)> {
        let i = self.ids.dirs.binary_search_by_key(&id, |d| d.0).ok()?;
        Some((self.ids.dirs[i].1, self.ids.dirs[i].2))
    }

    fn link(&self, id: NodeId) -> Option<u128> {
        if self.is_link[id as usize / 64] & (1 << (id % 64)) == 0 {
            return None;
        }
        let i = self.ids.links.binary_search_by_key(&id, |l| l.0).ok()?;
        Some(self.ids.links[i].1)
    }

    fn parent(&self, id: NodeId) -> NodeId {
        if id == ROOT { ROOT } else { self.tree.node(id).parent }
    }

    /// Folders that are descended into (not a second path, not a skipped mount).
    fn descended(&self, id: NodeId) -> bool {
        let n = self.tree.node(id);
        n.is_dir() && n.flags & (flags::SEEN | flags::MOUNT) == 0
    }

    /// Is `id`, or a folder above it, in `a` or `b`?
    fn under(&self, mut id: NodeId, a: &HashSet<NodeId>, b: &HashSet<NodeId>) -> bool {
        loop {
            if a.contains(&id) || b.contains(&id) {
                return true;
            }
            if id == ROOT {
                return false;
            }
            id = self.tree.node(id).parent;
        }
    }
}

/// Build the updated tree. Returns it, its [`Ids`], and how many folders
/// were read from disk.
pub fn update(old: &Tree, ids: &Ids, changes: &Changes, progress: &Progress) -> Result<(Tree, Ids, usize), String> {
    update_with(old, ids, changes, progress, &super::lister(), &super::skip_mounts())
}

fn update_with<L: Lister>(
    tree: &Tree,
    ids: &Ids,
    changes: &Changes,
    progress: &Progress,
    lister: &L,
    skip: &HashSet<PathBuf>,
) -> Result<(Tree, Ids, usize), String> {
    let old = Old::new(tree, ids);
    let deep: HashSet<NodeId> =
        changes.deep.iter().copied().filter(|&d| (d as usize) < tree.len() && old.descended(d)).collect();
    if deep.contains(&ROOT) {
        return Err("the whole folder has to be read again".into());
    }
    // A folder walked again is reached through a fresh listing of its parent.
    let none = HashSet::new();
    let mut dirty: Vec<NodeId> = changes
        .dirty
        .iter()
        .copied()
        .chain(deep.iter().map(|&d| old.parent(d)))
        .filter(|&d| (d as usize) < tree.len() && old.descended(d) && !old.under(d, &deep, &none))
        .collect();
    dirty.sort_unstable();
    dirty.dedup();

    progress.set_phase("Reading changed folders");
    let t0 = Instant::now();
    let mut pass = Reread::new(&old, &deep, skip);
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

        for &d in &dirty {
            pass.relist(d);
        }
        let mut pending = 0usize;
        loop {
            for t in pass.outbox.drain(..) {
                task_tx.send(t).unwrap();
                pending += 1;
            }
            if pending == 0 {
                // Every name of a hard-linked file has to be listed afresh
                // once any of them is, or the bytes could land twice.
                for p in pass.link_folders() {
                    pass.relist(p);
                }
                if pass.outbox.is_empty() {
                    break;
                }
                continue;
            }
            let Ok(r) = res_rx.recv() else { break };
            pending -= 1;
            pass.take(r);
        }
        drop(task_tx);
    });
    pass.settle_new_folders();
    let reread = pass.relisted.len() + pass.walked.len();
    let reread_ms = t0.elapsed().as_millis() as u64;

    progress.set_phase("Building tree");
    let t1 = Instant::now();
    let mut b = TreeBuilder::new(&tree.root_path);
    let mut out = Ids::default();
    let mut seen_files = IdSet::default();
    let mut unreadable = 0u64;
    let mut queue: VecDeque<(NodeId, Source)> = VecDeque::from([(ROOT, Source::Old(ROOT))]);
    while let Some((id, src)) = queue.pop_front() {
        let (listing, base) = match &src {
            Source::Old(o) => match pass.relisted.get(o) {
                Some(l) => (l, PathBuf::from(tree.path(*o))),
                None => {
                    // Unchanged: copy it over.
                    let o = *o;
                    if tree.node(o).flags & flags::UNREADABLE != 0 {
                        b.or_flags(id, flags::UNREADABLE);
                        unreadable += 1;
                    }
                    for &c in tree.children(o) {
                        let n = tree.node(c);
                        if n.is_dir() {
                            let (fid, mtime) = old.dir(c).unwrap_or((0, n.mtime));
                            let fl = n.flags & !flags::UNREADABLE;
                            let nid = b.add(id, tree.name(c), fl, 0, 0, mtime);
                            out.dirs.push((nid, fid, mtime));
                            if fl & (flags::SEEN | flags::MOUNT) == 0 {
                                queue.push_back((nid, Source::Old(c)));
                            }
                        } else {
                            let nid = b.add(id, tree.name(c), n.flags, n.size, n.alloc, n.mtime);
                            if let Some(fid) = old.link(c) {
                                out.links.push((nid, fid));
                                if n.flags & flags::HARDLINK == 0 {
                                    seen_files.insert(fid);
                                }
                            }
                        }
                    }
                    continue;
                }
            },
            Source::New(p) => match pass.walked.get(p) {
                Some(l) => (l, p.clone()),
                None => {
                    b.or_flags(id, flags::UNREADABLE);
                    unreadable += 1;
                    continue;
                }
            },
        };
        if listing.unreadable {
            b.or_flags(id, flags::UNREADABLE);
            unreadable += 1;
            continue;
        }
        for (i, e) in listing.entries.iter().enumerate() {
            if e.flags & flags::DIR == 0 {
                let (mut fl, mut size, mut alloc) = (e.flags, e.size, e.alloc);
                if e.file_id != 0 && !seen_files.insert(e.file_id) {
                    fl |= flags::HARDLINK;
                    size = 0;
                    alloc = 0;
                }
                let nid = b.add(id, &e.name, fl, size, alloc, e.mtime);
                if e.file_id != 0 {
                    out.links.push((nid, e.file_id));
                }
                continue;
            }
            let same = listing.same.get(i).copied().unwrap_or(NO_NODE);
            let mut fl = e.flags;
            let next = if same != NO_NODE {
                fl |= tree.node(same).flags & (flags::SEEN | flags::MOUNT);
                (fl & (flags::SEEN | flags::MOUNT) == 0).then_some(Source::Old(same))
            } else {
                let path = child_path(&base, e);
                if pass.walked.contains_key(&path) {
                    Some(Source::New(path))
                } else {
                    fl |= if pass.seen_new.contains(&path) || !skip.contains(&path) { flags::SEEN } else { flags::MOUNT };
                    None
                }
            };
            let nid = b.add(id, &e.name, fl, e.size, e.alloc, e.mtime);
            out.dirs.push((nid, e.file_id, e.mtime));
            if let Some(src) = next {
                queue.push_back((nid, src));
            }
        }
    }
    let copy_ms = t1.elapsed().as_millis() as u64;
    let t2 = Instant::now();
    let mut info = base_info(ScanMode::Walk);
    info.unreadable_dirs = unreadable;
    let mut new = b.finish(tree.root_path.clone(), info);
    new.info.phases =
        vec![("re-read changed folders", reread_ms), ("copy the rest", copy_ms), ("aggregate", t2.elapsed().as_millis() as u64)];
    Ok((new, out, reread))
}

enum Source {
    /// A folder of the previous tree: copied, or listed again if it changed.
    Old(NodeId),
    /// A folder that was walked afresh.
    New(PathBuf),
}

fn child_path(base: &Path, e: &Entry) -> PathBuf {
    match &e.os_name {
        Some(os) => base.join(os),
        None => base.join(&e.name),
    }
}

/// The listing pass: which folders were read again and what they hold now.
struct Reread<'a, 'b> {
    old: &'b Old<'a>,
    deep: &'b HashSet<NodeId>,
    skip: &'b HashSet<PathBuf>,
    relisted: HashMap<NodeId, Listing>,
    walked: HashMap<PathBuf, Listing>,
    /// Old folders asked for (listed or queued).
    requested: HashSet<NodeId>,
    /// Old folders a fresh listing no longer has (or has as another folder).
    removed: HashSet<NodeId>,
    /// Identities of the new folders walked, to walk each only once.
    walked_ids: IdSet,
    /// New folders that are a second path to a folder counted elsewhere.
    seen_new: HashSet<PathBuf>,
    new_ids: HashMap<u128, Vec<PathBuf>>,
    /// Hard-linked files of the previous tree, by identity.
    groups: HashMap<u128, Vec<NodeId>>,
    outbox: Vec<Task>,
}

impl<'a, 'b> Reread<'a, 'b> {
    fn new(old: &'b Old<'a>, deep: &'b HashSet<NodeId>, skip: &'b HashSet<PathBuf>) -> Self {
        let mut groups: HashMap<u128, Vec<NodeId>> = HashMap::new();
        for &(n, id) in &old.ids.links {
            groups.entry(id).or_default().push(n);
        }
        Reread {
            old,
            deep,
            skip,
            relisted: HashMap::new(),
            walked: HashMap::new(),
            requested: HashSet::new(),
            removed: HashSet::new(),
            walked_ids: IdSet::default(),
            seen_new: HashSet::new(),
            new_ids: HashMap::new(),
            groups,
            outbox: Vec::new(),
        }
    }

    fn relist(&mut self, id: NodeId) {
        if self.requested.insert(id) {
            self.outbox.push(Task { id, path: PathBuf::from(self.old.tree.path(id)) });
        }
    }

    fn take(&mut self, r: DirResult) {
        let n = r.entries.len();
        let mut listing = Listing { entries: r.entries, same: vec![NO_NODE; n], unreadable: r.unreadable };
        if r.id == NO_NODE {
            for e in &listing.entries {
                if e.flags & flags::DIR != 0 {
                    self.new_folder(child_path(&r.path, e), e.file_id);
                }
            }
            self.walked.insert(r.path, listing);
            return;
        }

        let tree = self.old.tree;
        let mut before: HashMap<&str, NodeId> =
            tree.children(r.id).iter().filter(|&&c| tree.node(c).is_dir()).map(|&c| (tree.name(c), c)).collect();
        for (i, e) in listing.entries.iter().enumerate() {
            if e.flags & flags::DIR == 0 {
                continue;
            }
            let same = before
                .get(e.name.as_str())
                .copied()
                .filter(|c| !self.deep.contains(c) && self.old.dir(*c).map(|d| d.0) == Some(e.file_id));
            match same {
                Some(c) => {
                    before.remove(e.name.as_str());
                    listing.same[i] = c;
                    // It may have become readable (its permissions or its
                    // parent's changed).
                    if self.old.descended(c) && tree.node(c).flags & flags::UNREADABLE != 0 {
                        self.relist(c);
                    }
                }
                None => self.new_folder(child_path(&r.path, e), e.file_id),
            }
        }
        self.removed.extend(before.into_values());
        self.relisted.insert(r.id, listing);
    }

    /// A folder the previous tree doesn't have here: walk it, unless it's a
    /// second path to one already walked or a skipped mount.
    fn new_folder(&mut self, path: PathBuf, id: u128) {
        if id != 0 && !self.walked_ids.insert(id) {
            self.seen_new.insert(path);
            return;
        }
        if self.skip.contains(&path) {
            return;
        }
        self.new_ids.entry(id).or_default().push(path.clone());
        self.outbox.push(Task { id: NO_NODE, path });
    }

    /// Folders holding other names of hard-linked files that changed, or
    /// that share a folder with something that changed.
    fn link_folders(&self) -> Vec<NodeId> {
        let mut touched: HashSet<u128> = HashSet::new();
        for &(n, id) in &self.old.ids.links {
            let p = self.old.parent(n);
            if self.relisted.contains_key(&p) || self.old.under(p, self.deep, &self.removed) {
                touched.insert(id);
            }
        }
        for l in self.relisted.values().chain(self.walked.values()) {
            for e in &l.entries {
                if e.flags & flags::DIR == 0 && e.file_id != 0 && self.groups.contains_key(&e.file_id) {
                    touched.insert(e.file_id);
                }
            }
        }
        let mut out = Vec::new();
        for id in touched {
            for &m in &self.groups[&id] {
                let p = self.old.parent(m);
                if !self.requested.contains(&p) && !self.old.under(p, self.deep, &self.removed) {
                    out.push(p);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// A new folder with the identity of a folder the previous tree still
    /// has is another path to it (a firmlink, say): count it once.
    fn settle_new_folders(&mut self) {
        if self.new_ids.is_empty() {
            return;
        }
        for &(n, id, _) in &self.old.ids.dirs {
            if let Some(paths) = self.new_ids.get(&id)
                && self.old.descended(n)
                && !self.old.under(n, self.deep, &self.removed)
            {
                for p in paths {
                    self.walked.remove(p);
                    self.seen_new.insert(p.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{portable::StdLister, scan_with};
    use super::*;
    use crate::scan::fsevents::differences;

    fn fixture() -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("heft-update-{}-{nanos}-{n}", std::process::id()));
        for d in ["a/b/c/d", "e", "gone/sub", "moved", "linked", "swap"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join("a/x.bin"), vec![1u8; 1000]).unwrap();
        std::fs::write(dir.join("a/b/c/d/deep.txt"), b"deep").unwrap();
        std::fs::write(dir.join("e/grow.dat"), vec![2u8; 100]).unwrap();
        std::fs::write(dir.join("gone/sub/f"), vec![3u8; 3000]).unwrap();
        std::fs::write(dir.join("moved/m"), vec![4u8; 4000]).unwrap();
        std::fs::write(dir.join("linked/one"), vec![5u8; 5000]).unwrap();
        std::fs::hard_link(dir.join("linked/one"), dir.join("e/two")).unwrap();
        std::fs::write(dir.join("swap/old"), vec![6u8; 600]).unwrap();
        dir
    }

    fn scan(root: &str, lister: &StdLister) -> (Tree, Ids) {
        let mut ids = Ids::default();
        let t = scan_with(root, &Progress::default(), lister, &HashSet::new(), Some(&mut ids)).unwrap();
        (t, ids)
    }

    /// Folders marked by hand as FSEvents would report them: the result is
    /// what a full scan finds.
    #[test]
    fn matches_a_full_scan() {
        let dir = fixture();
        let root = crate::scan::normalize_root(&dir.to_string_lossy());
        let lister = StdLister;
        let (before, ids) = scan(&root, &lister);

        std::fs::write(dir.join("a/b/c/d/deep.txt"), vec![7u8; 7000]).unwrap();
        std::fs::write(dir.join("e/grow.dat"), vec![2u8; 20_000]).unwrap();
        std::fs::remove_dir_all(dir.join("gone")).unwrap();
        std::fs::rename(dir.join("moved"), dir.join("e/renamed")).unwrap();
        std::fs::create_dir_all(dir.join("new/inner")).unwrap();
        std::fs::write(dir.join("new/inner/n"), vec![8u8; 800]).unwrap();
        // The hard link's first name goes away: the other one now counts.
        std::fs::remove_file(dir.join("linked/one")).unwrap();
        // Same name, different folder.
        std::fs::remove_dir_all(dir.join("swap")).unwrap();
        std::fs::create_dir_all(dir.join("swap")).unwrap();
        std::fs::write(dir.join("swap/new"), vec![9u8; 900]).unwrap();

        let at = |p: &str| before.find(&format!("{root}/{p}")).unwrap();
        let mut changes = Changes::default();
        changes.dirty.extend([ROOT, at("a/b/c/d"), at("a/b/c"), at("e"), at("linked"), at("swap")]);
        let (after, _, reread) = update_with(&before, &ids, &changes, &Progress::default(), &lister, &HashSet::new()).unwrap();
        let full = scan(&root, &lister).0;
        assert_eq!(differences(&after, &full, 50), Vec::<String>::new());
        assert!(reread < 15, "{reread} folders read");
        let two = after.find(&format!("{root}/e/two")).unwrap();
        assert_eq!(after.node(two).size, 5000);
        assert!(after.find(&format!("{root}/gone")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deep_folders_are_walked_again() {
        let dir = fixture();
        let root = crate::scan::normalize_root(&dir.to_string_lossy());
        let lister = StdLister;
        let (before, ids) = scan(&root, &lister);
        std::fs::write(dir.join("a/b/c/d/more"), vec![1u8; 123]).unwrap();
        let mut changes = Changes::default();
        changes.deep.insert(before.find(&format!("{root}/a/b")).unwrap());
        let (after, _, _) = update_with(&before, &ids, &changes, &Progress::default(), &lister, &HashSet::new()).unwrap();
        assert_eq!(differences(&after, &scan(&root, &lister).0, 50), Vec::<String>::new());

        changes.deep = HashSet::from([ROOT]);
        assert!(update_with(&before, &ids, &changes, &Progress::default(), &lister, &HashSet::new()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_changed_copies_everything() {
        let dir = fixture();
        let root = crate::scan::normalize_root(&dir.to_string_lossy());
        let lister = StdLister;
        let (before, ids) = scan(&root, &lister);
        let (after, after_ids, reread) =
            update_with(&before, &ids, &Changes::default(), &Progress::default(), &lister, &HashSet::new()).unwrap();
        assert_eq!(reread, 0);
        assert_eq!(differences(&after, &before, 50), Vec::<String>::new());
        assert_eq!(after_ids.dirs.len(), ids.dirs.len());
        assert_eq!(after_ids.links.len(), ids.links.len());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
