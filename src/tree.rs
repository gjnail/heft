//! Compact arena representation of a scanned directory tree.
//!
//! Invariant: a node's parent always has a smaller id than the node itself
//! (both scanners insert in parent-before-child order). This lets aggregation
//! run as a single reverse pass instead of a recursive traversal.

use std::collections::{BinaryHeap, HashMap};
use std::cmp::Reverse;

use crate::colors::{category_of, Category};
use crate::platform::{self, SEP};

pub type NodeId = u32;
pub const NO_NODE: NodeId = u32::MAX;

pub mod flags {
    pub const DIR: u16 = 1;
    pub const HIDDEN: u16 = 2;
    #[cfg_attr(not(windows), allow(dead_code))]
    pub const SYSTEM: u16 = 4;
    /// Symlink / junction that was not followed.
    pub const LINK: u16 = 8;
    /// Cloud placeholder (OneDrive, iCloud…). The data is not local; never read it.
    pub const CLOUD: u16 = 16;
    /// Directory could not be read (access denied etc.).
    pub const UNREADABLE: u16 = 32;
    /// Moved to the trash during this session.
    pub const DELETED: u16 = 64;
    /// Extra name of a hard-linked file whose bytes are counted at another path.
    pub const HARDLINK: u16 = 128;
    /// Mount point of a virtual file system (`/proc`, snapshots…), not scanned.
    pub const MOUNT: u16 = 256;
    /// Folder already scanned via another path (bind mount, macOS firmlink).
    pub const SEEN: u16 = 512;
}

#[derive(Clone, Copy, Debug)]
pub struct Node {
    name_off: u32,
    name_len: u16,
    pub ext: u16,
    pub flags: u16,
    pub parent: NodeId,
    child_start: u32,
    child_count: u32,
    /// Logical size in bytes (sum of descendants for directories).
    pub size: u64,
    /// Allocated size on disk.
    pub alloc: u64,
    /// Last-modified time (unix seconds). For directories: newest descendant.
    pub mtime: i64,
    /// Number of files in the subtree (1 for a file).
    pub files: u32,
}

impl Node {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & flags::DIR != 0
    }
}

#[derive(Clone, Debug)]
pub struct ExtStat {
    pub name: String,
    pub category: Category,
    pub size: u64,
    pub alloc: u64,
    pub count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanMode {
    /// Raw NTFS master file table read.
    Mft,
    /// Regular parallel directory enumeration.
    Walk,
}

impl ScanMode {
    pub fn label(self) -> &'static str {
        match self {
            ScanMode::Mft => "MFT",
            ScanMode::Walk => "standard",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScanInfo {
    pub mode: ScanMode,
    pub duration_ms: u64,
    pub finished_at: i64,
    pub unreadable_dirs: u64,
    pub note: Option<String>,
    /// Wall-clock time of each scan phase, for diagnostics.
    pub phases: Vec<(&'static str, u64)>,
}

#[derive(Clone)]
pub struct Tree {
    pub nodes: Vec<Node>,
    names: String,
    kids: Vec<NodeId>,
    pub exts: Vec<ExtStat>,
    /// Absolute path of the root node, e.g. `C:\` or `D:\Games`.
    pub root_path: String,
    pub info: ScanInfo,
    /// Bumped on every mutation so views know to refresh.
    pub version: u64,
}

pub const ROOT: NodeId = 0;

impl Tree {
    #[inline]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    #[inline]
    pub fn name(&self, id: NodeId) -> &str {
        let n = &self.nodes[id as usize];
        &self.names[n.name_off as usize..n.name_off as usize + n.name_len as usize]
    }

    #[inline]
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        let n = &self.nodes[id as usize];
        &self.kids[n.child_start as usize..(n.child_start + n.child_count) as usize]
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn ext_name(&self, id: NodeId) -> &str {
        &self.exts[self.nodes[id as usize].ext as usize].name
    }

    /// Chain from the root down to `id` (inclusive).
    pub fn ancestors(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = vec![id];
        let mut cur = id;
        while cur != ROOT {
            cur = self.nodes[cur as usize].parent;
            out.push(cur);
        }
        out.reverse();
        out
    }

    pub fn is_ancestor(&self, anc: NodeId, mut id: NodeId) -> bool {
        loop {
            if id == anc {
                return true;
            }
            if id == ROOT {
                return false;
            }
            id = self.nodes[id as usize].parent;
        }
    }

    pub fn path(&self, id: NodeId) -> String {
        if id == ROOT {
            return self.root_path.clone();
        }
        let chain = self.ancestors(id);
        let mut s = self.root_path.clone();
        for &n in &chain[1..] {
            if !s.ends_with(SEP) {
                s.push(SEP);
            }
            s.push_str(self.name(n));
        }
        s
    }

    /// Resolve an absolute path to a node. Names match exactly, or ignoring
    /// case on platforms whose file systems do (Windows, macOS).
    pub fn find(&self, path: &str) -> Option<NodeId> {
        let root = self.root_path.trim_end_matches(SEP);
        let path = path.trim_end_matches(SEP);
        if !platform::names_eq(path.get(..root.len())?, root) {
            return None;
        }
        let rest = &path[root.len()..];
        if !rest.is_empty() && !rest.starts_with(SEP) {
            return None; // `/data2` is not inside `/data`
        }
        let mut cur = ROOT;
        for comp in rest.split(SEP).filter(|c| !c.is_empty()) {
            let kids = self.children(cur);
            cur = kids
                .iter()
                .copied()
                .find(|&c| self.name(c) == comp)
                .or_else(|| kids.iter().copied().find(|&c| platform::names_eq(self.name(c), comp)))?;
        }
        Some(cur)
    }

    /// Largest files below `root`, biggest first, optionally filtered.
    pub fn largest_files(
        &self,
        root: NodeId,
        limit: usize,
        filter: impl Fn(NodeId, &Node) -> bool,
    ) -> Vec<NodeId> {
        let mut heap: BinaryHeap<Reverse<(u64, NodeId)>> = BinaryHeap::with_capacity(limit + 1);
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let n = self.node(id);
            if n.is_dir() {
                // Children are sorted by size: once a subtree can't beat the
                // current minimum, nothing after it can either.
                let floor = if heap.len() >= limit { heap.peek().map(|r| r.0.0).unwrap_or(0) } else { 0 };
                for &c in self.children(id) {
                    if heap.len() >= limit && self.node(c).size <= floor {
                        break;
                    }
                    stack.push(c);
                }
            } else if filter(id, n) {
                heap.push(Reverse((n.size, id)));
                if heap.len() > limit {
                    heap.pop();
                }
            }
        }
        let mut v: Vec<_> = heap.into_iter().map(|r| r.0).collect();
        v.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        v.into_iter().map(|(_, id)| id).collect()
    }

    /// Every file node below `root` (used by the duplicate finder).
    pub fn files_under(&self, root: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if self.node(id).is_dir() {
                stack.extend_from_slice(self.children(id));
            } else {
                out.push(id);
            }
        }
        out
    }

    /// Detach a node (after it was moved to the recycle bin) and fix up all
    /// ancestor totals and orderings.
    pub fn remove(&mut self, id: NodeId) {
        if id == ROOT || self.nodes[id as usize].flags & flags::DELETED != 0 {
            return;
        }
        // Extension statistics for every file in the subtree.
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            let node = self.nodes[n as usize];
            if node.is_dir() {
                stack.extend_from_slice(self.children(n));
            } else {
                let e = &mut self.exts[node.ext as usize];
                e.size = e.size.saturating_sub(node.size);
                e.alloc = e.alloc.saturating_sub(node.alloc);
                e.count = e.count.saturating_sub(1);
            }
        }

        let gone = self.nodes[id as usize];
        let parent = gone.parent;
        // Remove from the parent's child slice.
        let p = self.nodes[parent as usize];
        let start = p.child_start as usize;
        let end = start + p.child_count as usize;
        if let Some(pos) = self.kids[start..end].iter().position(|&c| c == id) {
            self.kids[start + pos..end].rotate_left(1);
            self.nodes[parent as usize].child_count -= 1;
        }
        self.nodes[id as usize].flags |= flags::DELETED;

        // Walk up, subtracting and re-sorting each level.
        let mut cur = parent;
        loop {
            let n = &mut self.nodes[cur as usize];
            n.size = n.size.saturating_sub(gone.size);
            n.alloc = n.alloc.saturating_sub(gone.alloc);
            n.files = n.files.saturating_sub(gone.files);
            self.sort_children(cur);
            if cur == ROOT {
                break;
            }
            cur = self.nodes[cur as usize].parent;
        }
        self.version += 1;
    }

    /// Change a file's size and allocation in place (after it was
    /// compressed, say) and fix up its ancestors.
    pub fn resize_file(&mut self, id: NodeId, size: u64, alloc: u64) {
        let node = self.nodes[id as usize];
        if node.is_dir() || node.flags & flags::DELETED != 0 {
            return;
        }
        let e = &mut self.exts[node.ext as usize];
        e.size = (e.size + size).saturating_sub(node.size);
        e.alloc = (e.alloc + alloc).saturating_sub(node.alloc);
        self.nodes[id as usize].size = size;
        self.nodes[id as usize].alloc = alloc;
        let mut cur = node.parent;
        loop {
            let n = &mut self.nodes[cur as usize];
            n.size = (n.size + size).saturating_sub(node.size);
            n.alloc = (n.alloc + alloc).saturating_sub(node.alloc);
            self.sort_children(cur);
            if cur == ROOT {
                break;
            }
            cur = self.nodes[cur as usize].parent;
        }
        self.version += 1;
    }

    /// A file's data now lives only in the cloud (its download was
    /// removed): it keeps its size but takes no space (see [`flags::CLOUD`]).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn mark_cloud(&mut self, id: NodeId) {
        let n = self.nodes[id as usize];
        if n.flags & flags::CLOUD == 0 {
            self.resize_file(id, n.size, 0);
            self.nodes[id as usize].flags |= flags::CLOUD;
        }
    }

    /// A file became another name for a file counted elsewhere, so it now
    /// takes no space of its own (see [`flags::HARDLINK`]).
    pub fn mark_hard_link(&mut self, id: NodeId) {
        if self.nodes[id as usize].flags & flags::HARDLINK == 0 {
            self.resize_file(id, 0, 0);
            self.nodes[id as usize].flags |= flags::HARDLINK;
        }
    }

    fn sort_children(&mut self, id: NodeId) {
        let n = self.nodes[id as usize];
        let range = n.child_start as usize..(n.child_start + n.child_count) as usize;
        let nodes = &self.nodes;
        self.kids[range].sort_by(|&a, &b| nodes[b as usize].size.cmp(&nodes[a as usize].size));
    }

    pub fn is_deleted(&self, id: NodeId) -> bool {
        self.nodes[id as usize].flags & flags::DELETED != 0
    }
}

/// Incrementally builds a [`Tree`]. Parents must be added before children.
pub struct TreeBuilder {
    nodes: Vec<Node>,
    names: String,
    ext_map: HashMap<String, u16>,
    ext_names: Vec<String>,
    scratch: String,
}

const MAX_EXTS: usize = 60_000;
const EXT_NONE: u16 = 0;
const EXT_OTHER: u16 = 1;

impl TreeBuilder {
    pub fn new(root_name: &str) -> Self {
        let mut b = TreeBuilder {
            nodes: Vec::with_capacity(1 << 16),
            names: String::with_capacity(1 << 20),
            ext_map: HashMap::new(),
            ext_names: vec![String::new(), "(other)".to_string()],
            scratch: String::new(),
        };
        b.push(NO_NODE, root_name, flags::DIR, 0, 0, 0, EXT_NONE);
        b
    }

    fn push(&mut self, parent: NodeId, name: &str, fl: u16, size: u64, alloc: u64, mtime: i64, ext: u16) -> NodeId {
        let id = self.nodes.len() as NodeId;
        // Names longer than u16::MAX bytes cannot exist on Windows, but be safe.
        let name = if name.len() > u16::MAX as usize { &name[..name.floor_char_boundary(u16::MAX as usize)] } else { name };
        self.nodes.push(Node {
            name_off: self.names.len() as u32,
            name_len: name.len() as u16,
            ext,
            flags: fl,
            parent,
            child_start: 0,
            child_count: 0,
            size,
            alloc,
            mtime,
            files: if fl & flags::DIR != 0 { 0 } else { 1 },
        });
        self.names.push_str(name);
        id
    }

    /// Add a child. `size`/`alloc` for a directory are its own overhead only
    /// (usually 0); descendants are summed in [`finish`](Self::finish).
    pub fn add(&mut self, parent: NodeId, name: &str, fl: u16, size: u64, alloc: u64, mtime: i64) -> NodeId {
        debug_assert!((parent as usize) < self.nodes.len());
        let ext = if fl & flags::DIR != 0 { EXT_NONE } else { self.intern_ext(name) };
        self.push(parent, name, fl, size, alloc, mtime, ext)
    }

    pub fn or_flags(&mut self, id: NodeId, fl: u16) {
        self.nodes[id as usize].flags |= fl;
    }

    fn intern_ext(&mut self, name: &str) -> u16 {
        let Some(dot) = name.rfind('.') else { return EXT_NONE };
        if dot == 0 || dot + 1 == name.len() {
            return EXT_NONE;
        }
        let ext = &name[dot + 1..];
        if ext.len() > 16 || ext.contains(' ') {
            return EXT_NONE;
        }
        self.scratch.clear();
        for c in ext.chars() {
            self.scratch.extend(c.to_lowercase());
        }
        if let Some(&id) = self.ext_map.get(self.scratch.as_str()) {
            return id;
        }
        if self.ext_names.len() >= MAX_EXTS {
            return EXT_OTHER;
        }
        let id = self.ext_names.len() as u16;
        self.ext_names.push(self.scratch.clone());
        self.ext_map.insert(self.scratch.clone(), id);
        id
    }

    pub fn finish(mut self, root_path: String, info: ScanInfo) -> Tree {
        let n = self.nodes.len();

        // Children lists in CSR form.
        let mut counts = vec![0u32; n];
        for node in &self.nodes[1..] {
            counts[node.parent as usize] += 1;
        }
        let mut start = 0u32;
        for (i, node) in self.nodes.iter_mut().enumerate() {
            node.child_start = start;
            start += counts[i];
        }
        let mut kids = vec![0 as NodeId; n.saturating_sub(1)];
        for i in 1..n {
            let p = self.nodes[i].parent as usize;
            let slot = (self.nodes[p].child_start + self.nodes[p].child_count) as usize;
            kids[slot] = i as NodeId;
            self.nodes[p].child_count += 1;
        }

        // Bottom-up aggregation (parents always precede children).
        for i in (1..n).rev() {
            let c = self.nodes[i];
            let p = &mut self.nodes[c.parent as usize];
            p.size += c.size;
            p.alloc += c.alloc;
            p.files = p.files.saturating_add(c.files);
            p.mtime = p.mtime.max(c.mtime);
        }

        // Biggest first everywhere.
        {
            let nodes = &self.nodes;
            for node in nodes.iter() {
                if node.child_count > 1 {
                    let r = node.child_start as usize..(node.child_start + node.child_count) as usize;
                    kids[r].sort_unstable_by(|&a, &b| nodes[b as usize].size.cmp(&nodes[a as usize].size));
                }
            }
        }

        let mut exts: Vec<ExtStat> = self
            .ext_names
            .into_iter()
            .map(|name| ExtStat { category: category_of(&name), name, size: 0, alloc: 0, count: 0 })
            .collect();
        for node in &self.nodes {
            if !node.is_dir() {
                let e = &mut exts[node.ext as usize];
                e.size += node.size;
                e.alloc += node.alloc;
                e.count += 1;
            }
        }

        Tree { nodes: self.nodes, names: self.names, kids, exts, root_path, info, version: 1 }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A root that is absolute on this platform (`C:\` or `/`).
    pub(crate) fn root() -> &'static str {
        if cfg!(windows) { "C:\\" } else { "/" }
    }

    /// `root()` joined with `parts`.
    pub(crate) fn p(parts: &[&str]) -> String {
        let mut s = root().to_string();
        s.push_str(&parts.join(&SEP.to_string()));
        s
    }

    fn info() -> ScanInfo {
        ScanInfo { mode: ScanMode::Walk, duration_ms: 0, finished_at: 0, unreadable_dirs: 0, note: None, phases: Vec::new() }
    }

    fn sample() -> Tree {
        let mut b = TreeBuilder::new(root());
        let a = b.add(ROOT, "a", flags::DIR, 0, 0, 0);
        b.add(a, "big.MP4", 0, 100, 104, 5);
        b.add(a, "small.txt", 0, 10, 12, 9);
        let c = b.add(ROOT, "c", flags::DIR, 0, 0, 0);
        b.add(c, "mid.mp4", 0, 50, 52, 1);
        b.finish(root().into(), info())
    }

    #[test]
    fn aggregates_and_sorts() {
        let t = sample();
        assert_eq!(t.node(ROOT).size, 160);
        assert_eq!(t.node(ROOT).files, 3);
        assert_eq!(t.node(ROOT).mtime, 9);
        let kids = t.children(ROOT);
        assert_eq!(t.name(kids[0]), "a");
        assert_eq!(t.path(t.children(kids[0])[0]), p(&["a", "big.MP4"]));
        let mp4 = t.exts.iter().find(|e| e.name == "mp4").unwrap();
        assert_eq!((mp4.size, mp4.count), (150, 2));
    }

    #[test]
    fn find_and_remove() {
        let mut t = sample();
        let id = t.find(&p(&["a", "big.MP4"])).unwrap();
        assert_eq!(t.node(id).size, 100);
        assert_eq!(t.find(&p(&["A", "BIG.mp4"])).is_some(), platform::CASE_INSENSITIVE);
        assert!(t.find(&p(&["ab"])).is_none());
        t.remove(id);
        assert_eq!(t.node(ROOT).size, 60);
        // "c" (50) is now bigger than "a" (10) and must come first.
        assert_eq!(t.name(t.children(ROOT)[0]), "c");
        let mp4 = t.exts.iter().find(|e| e.name == "mp4").unwrap();
        assert_eq!((mp4.size, mp4.count), (50, 1));
        assert!(t.find(&p(&["a", "big.MP4"])).is_none());
    }

    #[test]
    fn resize_and_hard_link() {
        let mut t = sample();
        let big = t.find(&p(&["a", "big.MP4"])).unwrap();
        t.resize_file(big, 100, 40);
        assert_eq!((t.node(ROOT).size, t.node(ROOT).alloc), (160, 104));
        let mid = t.find(&p(&["c", "mid.mp4"])).unwrap();
        t.mark_hard_link(mid);
        t.mark_hard_link(mid);
        assert_eq!((t.node(ROOT).size, t.node(ROOT).files), (110, 3));
        assert_ne!(t.node(mid).flags & flags::HARDLINK, 0);
        let mp4 = t.exts.iter().find(|e| e.name == "mp4").unwrap();
        assert_eq!((mp4.size, mp4.alloc, mp4.count), (100, 40, 2));
    }

    #[test]
    fn find_under_subfolder_root() {
        let base = p(&["data"]);
        let mut b = TreeBuilder::new(&base);
        let x = b.add(ROOT, "x", flags::DIR, 0, 0, 0);
        b.add(x, "f", 0, 1, 1, 0);
        let t = b.finish(base.clone(), info());
        assert!(t.find(&format!("{base}{SEP}x{SEP}f")).is_some());
        assert!(t.find(&format!("{base}2{SEP}x")).is_none(), "sibling with a longer name");
        assert_eq!(t.find(&base), Some(ROOT));
    }

    #[test]
    fn largest() {
        let t = sample();
        let v = t.largest_files(ROOT, 2, |_, _| true);
        assert_eq!(v.iter().map(|&i| t.node(i).size).collect::<Vec<_>>(), vec![100, 50]);
    }
}
