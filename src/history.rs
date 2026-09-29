//! Scan history: a compact snapshot of every folder (plus big files) is saved
//! after each scan so later scans can show what grew or shrank.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::platform::{self, SEP};
use crate::tree::{NodeId, Tree, ROOT};

const MAGIC: &[u8; 8] = b"HEFTSNP1";
/// Files at least this big are tracked individually.
pub const BIG_FILE: u64 = 16 << 20;
const KEEP: usize = 60;

#[derive(Clone, Debug)]
pub struct SnapMeta {
    pub file: PathBuf,
    pub root_path: String,
    pub taken_at: i64,
    pub total_size: u64,
    pub total_files: u64,
}

pub struct SnapEntry {
    pub parent: u32,
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

pub struct Snapshot {
    pub meta: SnapMeta,
    pub entries: Vec<SnapEntry>,
}

pub fn history_dir() -> PathBuf {
    match std::env::var_os("HEFT_HISTORY_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => platform::data_dir().join("history"),
    }
}

fn key_for(root: &str) -> String {
    let mut k: String = root
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    while k.ends_with('_') {
        k.pop();
    }
    k.truncate(120);
    if k.trim_matches('_').is_empty() {
        k = "root".into(); // `/`
    }
    k
}

fn dir_for(root: &str) -> PathBuf {
    history_dir().join(key_for(root))
}

pub fn save(tree: &Tree) -> Result<SnapMeta, String> {
    let dir = dir_for(&tree.root_path);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    // BFS so every entry's parent precedes it.
    let mut payload = Vec::with_capacity(1 << 20);
    let mut queue = std::collections::VecDeque::from([(ROOT, u32::MAX)]);
    let mut count = 0u32;
    while let Some((id, parent)) = queue.pop_front() {
        let n = tree.node(id);
        let idx = count;
        count += 1;
        let name = if id == ROOT { "" } else { tree.name(id) };
        payload.extend_from_slice(&parent.to_le_bytes());
        payload.extend_from_slice(&(name.len() as u16).to_le_bytes());
        payload.extend_from_slice(name.as_bytes());
        payload.extend_from_slice(&n.size.to_le_bytes());
        payload.extend_from_slice(&n.files.to_le_bytes());
        payload.push(n.is_dir() as u8);
        if n.is_dir() {
            for &c in tree.children(id) {
                let cn = tree.node(c);
                if cn.is_dir() || cn.size >= BIG_FILE {
                    queue.push_back((c, idx));
                }
            }
        }
    }

    let root = tree.node(ROOT);
    let meta = SnapMeta {
        file: dir.join(format!("{}.snap", tree.info.finished_at)),
        root_path: tree.root_path.clone(),
        taken_at: tree.info.finished_at,
        total_size: root.size,
        total_files: root.files as u64,
    };
    let mut header = Vec::new();
    write_str(&mut header, &meta.root_path);
    header.extend_from_slice(&meta.taken_at.to_le_bytes());
    header.extend_from_slice(&meta.total_size.to_le_bytes());
    header.extend_from_slice(&meta.total_files.to_le_bytes());
    header.extend_from_slice(&count.to_le_bytes());

    let compressed = lz4_flex::compress_prepend_size(&payload);
    let tmp = meta.file.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(MAGIC).map_err(|e| e.to_string())?;
        f.write_all(&(header.len() as u32).to_le_bytes()).map_err(|e| e.to_string())?;
        f.write_all(&header).map_err(|e| e.to_string())?;
        f.write_all(&compressed).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, &meta.file).map_err(|e| e.to_string())?;
    prune(&dir);
    Ok(meta)
}

fn prune(dir: &Path) {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "snap"))
        .collect();
    if files.len() > KEEP {
        files.sort();
        for f in &files[..files.len() - KEEP] {
            let _ = std::fs::remove_file(f);
        }
    }
}

/// Snapshots for `root`, newest first.
pub fn list(root: &str) -> Vec<SnapMeta> {
    let mut out: Vec<SnapMeta> = std::fs::read_dir(dir_for(root))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "snap"))
        .filter_map(|p| read_header(&p).ok().map(|(m, _)| m))
        .filter(|m| platform::names_eq(&m.root_path, root))
        .collect();
    out.sort_by(|a, b| b.taken_at.cmp(&a.taken_at));
    out
}

fn read_header(path: &Path) -> Result<(SnapMeta, u32), String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut magic = [0u8; 12];
    f.read_exact(&mut magic).map_err(|e| e.to_string())?;
    if &magic[..8] != MAGIC {
        return Err("not a snapshot".into());
    }
    let hlen = u32::from_le_bytes(magic[8..12].try_into().unwrap()) as usize;
    let mut h = vec![0u8; hlen];
    f.read_exact(&mut h).map_err(|e| e.to_string())?;
    let mut r = Reader { b: &h, pos: 0 };
    let root_path = r.string()?;
    let meta = SnapMeta {
        file: path.to_path_buf(),
        root_path,
        taken_at: r.u64()? as i64,
        total_size: r.u64()?,
        total_files: r.u64()?,
    };
    let count = r.u32()?;
    Ok((meta, count))
}

pub fn load(meta: &SnapMeta) -> Result<Snapshot, String> {
    let (meta, count) = read_header(&meta.file)?;
    let raw = std::fs::read(&meta.file).map_err(|e| e.to_string())?;
    let hlen = u32::from_le_bytes(raw[8..12].try_into().unwrap()) as usize;
    let payload = lz4_flex::decompress_size_prepended(&raw[12 + hlen..]).map_err(|e| e.to_string())?;
    let mut r = Reader { b: &payload, pos: 0 };
    let mut entries = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let parent = r.u32()?;
        let name = r.string()?;
        let size = r.u64()?;
        let _files = r.u32()?;
        let is_dir = r.u8()? != 0;
        entries.push(SnapEntry { parent, name, size, is_dir });
    }
    Ok(Snapshot { meta, entries })
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        let s = self.b.get(self.pos..self.pos + n).ok_or("truncated snapshot")?;
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String, String> {
        let len = u16::from_le_bytes(self.take(2)?.try_into().unwrap()) as usize;
        Ok(String::from_utf8_lossy(self.take(len)?).into_owned())
    }
}

// ---------------------------------------------------------------------------
// Diffing

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Old {
    /// Not tracked (small file), so no information.
    Unknown,
    /// Did not exist in the snapshot.
    New,
    Size(u64),
}

const UNKNOWN: u64 = u64::MAX;
const NEW: u64 = u64::MAX - 1;

pub struct Removed {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
}

pub struct Diff {
    pub against: SnapMeta,
    old: Vec<u64>,
    pub removed: Vec<Removed>,
}

impl Diff {
    pub fn old_size(&self, id: NodeId) -> Old {
        match self.old.get(id as usize).copied().unwrap_or(UNKNOWN) {
            UNKNOWN => Old::Unknown,
            NEW => Old::New,
            s => Old::Size(s),
        }
    }

    /// Change in bytes, if known. New items count as fully grown.
    pub fn delta(&self, tree: &Tree, id: NodeId) -> Option<i64> {
        match self.old_size(id) {
            Old::Unknown => None,
            Old::New => Some(tree.node(id).size as i64),
            Old::Size(s) => Some(tree.node(id).size as i64 - s as i64),
        }
    }
}

pub fn compute(tree: &Tree, snap: &Snapshot) -> Diff {
    let mut old = vec![UNKNOWN; tree.len()];
    let mut matched = vec![false; snap.entries.len()];
    let mut by_parent: HashMap<(u32, String), u32> = HashMap::with_capacity(snap.entries.len());
    for (i, e) in snap.entries.iter().enumerate().skip(1) {
        by_parent.insert((e.parent, platform::name_key(&e.name).into_owned()), i as u32);
    }

    if !snap.entries.is_empty() {
        old[ROOT as usize] = snap.entries[0].size;
        matched[0] = true;
        let mut stack = vec![(ROOT, 0u32)];
        while let Some((id, sidx)) = stack.pop() {
            for &c in tree.children(id) {
                let n = tree.node(c);
                if !n.is_dir() && n.size < BIG_FILE {
                    continue; // small files were never recorded
                }
                match by_parent.get(&(sidx, platform::name_key(tree.name(c)).into_owned())) {
                    Some(&si) => {
                        let e = &snap.entries[si as usize];
                        old[c as usize] = e.size;
                        matched[si as usize] = true;
                        if n.is_dir() && e.is_dir {
                            stack.push((c, si));
                        }
                    }
                    None => mark_new(tree, c, &mut old),
                }
            }
        }
    }

    // Things that disappeared: report only the topmost missing item.
    let mut removed = Vec::new();
    for (i, e) in snap.entries.iter().enumerate().skip(1) {
        if !matched[i] && matched[e.parent as usize] {
            let mut parts = vec![e.name.as_str()];
            let mut p = e.parent;
            while p != 0 && p != u32::MAX {
                parts.push(&snap.entries[p as usize].name);
                p = snap.entries[p as usize].parent;
            }
            let mut path = snap.meta.root_path.trim_end_matches(SEP).to_string();
            for part in parts.iter().rev() {
                path.push(SEP);
                path.push_str(part);
            }
            removed.push(Removed { path, size: e.size, is_dir: e.is_dir });
        }
    }
    removed.sort_by(|a, b| b.size.cmp(&a.size));

    Diff { against: snap.meta.clone(), old, removed }
}

fn mark_new(tree: &Tree, id: NodeId, old: &mut [u64]) {
    let mut stack = vec![id];
    while let Some(n) = stack.pop() {
        old[n as usize] = NEW;
        stack.extend_from_slice(tree.children(n));
    }
}

/// The places that explain most of the change: folders/files whose delta is
/// significant and not mostly accounted for by a single child.
pub fn hotspots(tree: &Tree, diff: &Diff, root: NodeId, limit: usize) -> Vec<(NodeId, i64)> {
    let total = diff.delta(tree, root).unwrap_or(0).unsigned_abs();
    let min = (total / 200).max(1 << 20) as i64;
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let Some(d) = diff.delta(tree, id) else { continue };
        if d.abs() < min {
            continue;
        }
        let mut dominated = false;
        if tree.node(id).is_dir() && diff.old_size(id) != Old::New {
            for &c in tree.children(id) {
                if let Some(cd) = diff.delta(tree, c)
                    && cd.abs() >= min {
                        stack.push(c);
                        if cd.signum() == d.signum() && cd.abs() * 10 >= d.abs() * 6 {
                            dominated = true;
                        }
                    }
            }
        }
        if !dominated && id != root {
            out.push((id, d));
        }
    }
    out.sort_by(|a, b| b.1.abs().cmp(&a.1.abs()));
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::tests::{p, root};
    use crate::tree::{flags, ScanInfo, ScanMode, TreeBuilder};

    fn tree(extra: bool, finished_at: i64) -> Tree {
        let mut b = TreeBuilder::new(root());
        let games = b.add(ROOT, "Games", flags::DIR, 0, 0, 0);
        b.add(games, "big.pak", 0, 100 << 20, 0, 0);
        let docs = b.add(ROOT, "Docs", flags::DIR, 0, 0, 0);
        b.add(docs, "a.txt", 0, 1000, 0, 0);
        if extra {
            let new = b.add(games, "NewGame", flags::DIR, 0, 0, 0);
            b.add(new, "data.bin", 0, 50 << 20, 0, 0);
        } else {
            b.add(ROOT, "Old", flags::DIR, 0, 0, 0);
        }
        let info = ScanInfo {
            mode: ScanMode::Walk,
            duration_ms: 0,
            finished_at,
            unreadable_dirs: 0,
            note: None,
            phases: Vec::new(),
        };
        b.finish(root().into(), info)
    }

    #[test]
    fn keys_are_never_empty() {
        assert_eq!(key_for("/"), "root");
        assert_eq!(key_for("C:\\"), "c");
        assert_eq!(key_for("/home/ana"), "_home_ana");
    }

    #[test]
    fn roundtrip_and_diff() {
        // Keep test snapshots away from the real history folder.
        let tmp = std::env::temp_dir().join(format!("heft-test-{}", std::process::id()));
        unsafe { std::env::set_var("HEFT_HISTORY_DIR", &tmp) };

        let before = tree(false, 1000);
        let meta = save(&before).unwrap();
        let listed = list(root());
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].total_size, before.node(ROOT).size);

        let snap = load(&meta).unwrap();
        let after = tree(true, 2000);
        let d = compute(&after, &snap);

        let games = after.find(&p(&["Games"])).unwrap();
        assert_eq!(d.delta(&after, games), Some(50 << 20));
        let newgame = after.find(&p(&["Games", "NewGame"])).unwrap();
        assert_eq!(d.old_size(newgame), Old::New);
        let small = after.find(&p(&["Docs", "a.txt"])).unwrap();
        assert_eq!(d.old_size(small), Old::Unknown);
        assert_eq!(d.removed.len(), 1);
        assert_eq!(d.removed[0].path, p(&["Old"]));

        let hs = hotspots(&after, &d, ROOT, 10);
        assert_eq!(hs.first().map(|h| h.0), Some(newgame));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
