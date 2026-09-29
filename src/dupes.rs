//! Duplicate file finder: size buckets → quick hash of head+tail → full hash.
//! Hard links (same physical file) are recognised and never reported.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rayon::prelude::*;
use xxhash_rust::xxh3::{xxh3_128, Xxh3};

use crate::platform;
use crate::tree::{flags, NodeId, Tree};

const PROBE: usize = 16 * 1024;

#[derive(Default)]
pub struct DupProgress {
    pub phase: Mutex<String>,
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub bytes: AtomicU64,
    pub cancel: AtomicBool,
}

#[derive(Clone, Debug)]
pub struct DupGroup {
    pub size: u64,
    pub files: Vec<NodeId>,
}

impl DupGroup {
    pub fn wasted(&self) -> u64 {
        self.size * (self.files.len() as u64).saturating_sub(1)
    }
}

pub fn find(tree: &Arc<Tree>, root: NodeId, min_size: u64, p: &DupProgress) -> Vec<DupGroup> {
    let set_phase = |s: &str| *p.phase.lock().unwrap() = s.to_string();
    set_phase("Grouping by size");

    let mut by_size: HashMap<u64, Vec<NodeId>> = HashMap::new();
    for id in tree.files_under(root) {
        let n = tree.node(id);
        // Never touch cloud placeholders: reading them would download them.
        if n.size >= min_size && n.flags & (flags::CLOUD | flags::LINK | flags::DELETED) == 0 {
            by_size.entry(n.size).or_default().push(id);
        }
    }
    let candidates: Vec<(u64, Vec<NodeId>)> = by_size.into_iter().filter(|(_, v)| v.len() > 1).collect();

    // Stage 1: head + tail probe (also resolves hard links).
    set_phase("Comparing file samples");
    p.total.store(candidates.iter().map(|c| c.1.len() as u64).sum(), Ordering::Relaxed);
    p.done.store(0, Ordering::Relaxed);
    let stage1: Vec<(u64, Vec<NodeId>)> = candidates
        .into_par_iter()
        .flat_map_iter(|(size, ids)| {
            let mut seen_identity = HashMap::new();
            let mut buckets: HashMap<u128, Vec<NodeId>> = HashMap::new();
            for id in ids {
                if p.cancel.load(Ordering::Relaxed) {
                    break;
                }
                p.done.fetch_add(1, Ordering::Relaxed);
                let path = tree.path(id);
                let Ok(mut f) = std::fs::File::open(&path) else { continue };
                if let Some(ident) = platform::file_identity(&f)
                    && seen_identity.insert(ident, id).is_some() {
                        continue; // hard link to a file already in this bucket
                    }
                let Some(h) = probe_hash(&mut f, size) else { continue };
                p.bytes.fetch_add(size.min(2 * PROBE as u64), Ordering::Relaxed);
                buckets.entry(h).or_default().push(id);
            }
            buckets.into_values().filter(|v| v.len() > 1).map(move |v| (size, v))
        })
        .collect();

    if p.cancel.load(Ordering::Relaxed) {
        return Vec::new();
    }

    // Stage 2: full content hash (small files were fully covered by the probe).
    set_phase("Hashing full contents");
    p.total.store(stage1.iter().filter(|g| g.0 > 2 * PROBE as u64).map(|g| g.1.len() as u64).sum(), Ordering::Relaxed);
    p.done.store(0, Ordering::Relaxed);
    let mut groups: Vec<DupGroup> = stage1
        .into_par_iter()
        .flat_map_iter(|(size, ids)| {
            if size <= 2 * PROBE as u64 {
                return vec![DupGroup { size, files: ids }].into_iter();
            }
            let mut buckets: HashMap<u128, Vec<NodeId>> = HashMap::new();
            for id in ids {
                if p.cancel.load(Ordering::Relaxed) {
                    break;
                }
                if let Some(h) = full_hash(&tree.path(id), p) {
                    buckets.entry(h).or_default().push(id);
                }
                p.done.fetch_add(1, Ordering::Relaxed);
            }
            buckets
                .into_values()
                .filter(|v| v.len() > 1)
                .map(|files| DupGroup { size, files })
                .collect::<Vec<_>>()
                .into_iter()
        })
        .collect();

    if p.cancel.load(Ordering::Relaxed) {
        return Vec::new();
    }
    for g in &mut groups {
        // Oldest first: that's usually the "original".
        g.files.sort_by_key(|&id| tree.node(id).mtime);
    }
    groups.sort_by(|a, b| b.wasted().cmp(&a.wasted()));
    set_phase("Done");
    groups
}

fn probe_hash(f: &mut std::fs::File, size: u64) -> Option<u128> {
    let mut buf = vec![0u8; PROBE * 2];
    if size <= (PROBE * 2) as u64 {
        buf.truncate(size as usize);
        f.read_exact(&mut buf).ok()?;
    } else {
        f.read_exact(&mut buf[..PROBE]).ok()?;
        f.seek(SeekFrom::End(-(PROBE as i64))).ok()?;
        f.read_exact(&mut buf[PROBE..]).ok()?;
    }
    Some(xxh3_128(&buf))
}

fn full_hash(path: &str, p: &DupProgress) -> Option<u128> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = Xxh3::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if p.cancel.load(Ordering::Relaxed) {
            return None;
        }
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        p.bytes.fetch_add(n as u64, Ordering::Relaxed);
    }
    Some(h.digest128())
}
