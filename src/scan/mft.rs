//! Fast path: read the NTFS master file table straight off the volume.
//!
//! Instead of asking the OS for every directory listing, we read the MFT
//! (one ~1 KB record per file) in large chunks and rebuild the hierarchy from
//! the parent references in each `$FILE_NAME` attribute. This is how WizTree
//! gets its speed. Requires administrator rights.
//!
//! Chunks are read with positional I/O and parsed in parallel: every chunk
//! owns a disjoint range of record slots, so base records are written in
//! place without locking. The rare extension records (attributes that
//! overflowed into another record) are collected per chunk and merged after.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use rayon::prelude::*;

use super::{base_info, Progress};
use crate::platform::filetime_to_unix;
use crate::tree::{flags, ScanMode, Tree, TreeBuilder, ROOT};

const SHARE_ALL: u32 = 1 | 2 | 4; // FILE_SHARE_READ | WRITE | DELETE
/// Direct reads: skip the cache manager (faster, and doesn't evict the
/// user's cached files with gigabytes of MFT). Needs sector-aligned I/O.
const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
const ALIGN: usize = 4096;
const ROOT_RECORD: usize = 5;
const CHUNK: u64 = 4 << 20;

const AT_STANDARD_INFORMATION: u32 = 0x10;
const AT_ATTRIBUTE_LIST: u32 = 0x20;
const AT_FILE_NAME: u32 = 0x30;
const AT_DATA: u32 = 0x80;
const AT_INDEX_ALLOCATION: u32 = 0xA0;
const AT_END: u32 = 0xFFFF_FFFF;

const NS_DOS: u8 = 2;

// Rec::fl
const R_INUSE: u8 = 1;
const R_DIR: u8 = 2;
const R_NAMED: u8 = 4;
const R_HAS_SIZE: u8 = 8;
const R_HAS_SI: u8 = 16;
// Rec::attrs (packed subset of FILE_ATTRIBUTE_*)
const A_HIDDEN: u8 = 1;
const A_SYSTEM: u8 = 2;
const A_CLOUD: u8 = 4;

/// Per-record scratch data, indexed by MFT record number.
#[derive(Clone, Copy, Default)]
struct Rec {
    parent: u32,
    parent_seq: u16,
    seq: u16,
    name: NameRef,
    ns: u8,
    fl: u8,
    attrs: u8,
    mtime: u32,
    size: u64,
    alloc: u64,
}

/// A name stored in one of the per-chunk string arenas.
#[derive(Clone, Copy, Default)]
struct NameRef {
    chunk: u32,
    off: u32,
    len: u16,
}

/// An additional hard link to a file (a second long `$FILE_NAME`).
#[derive(Clone, Copy)]
struct Link {
    target: u32,
    parent: u32,
    parent_seq: u16,
    name: NameRef,
}

#[derive(Default)]
struct ChunkOut {
    names: String,
    /// Attributes found in extension records, keyed by their base record.
    partials: Vec<(usize, Rec)>,
    links: Vec<Link>,
}

#[derive(Clone, Copy)]
struct Geometry {
    cluster: u64,
    rec_size: u64,
}

type Runs = Vec<(i64, u64)>; // (lcn or -1 for sparse, length in clusters)

pub fn scan(root: &str, progress: &Progress) -> Result<Tree, String> {
    let drive = root.chars().next().filter(|c| c.is_ascii_alphabetic()).ok_or("not a drive path")?;
    let mut phases: Vec<(&'static str, u64)> = Vec::new();
    let mut clock = Instant::now();
    let mut lap = |name: &'static str, phases: &mut Vec<(&'static str, u64)>| {
        phases.push((name, clock.elapsed().as_millis() as u64));
        clock = Instant::now();
    };

    progress.set_phase("Opening volume");
    let open = |unbuffered: bool| {
        OpenOptions::new()
            .read(true)
            .share_mode(SHARE_ALL)
            .custom_flags(if unbuffered { FILE_FLAG_NO_BUFFERING } else { 0 })
            .open(format!(r"\\.\{drive}:"))
            .map_err(|e| format!("cannot open volume: {e}"))
    };
    let mut vol = open(true)?;
    lap("open volume", &mut phases);

    let boot = match read_at(&vol, 0, 4096) {
        Ok(b) => b,
        Err(_) => {
            // Unusual sector geometry: fall back to cached reads.
            vol = open(false)?;
            read_at(&vol, 0, 4096)?
        }
    };
    lap("read boot sector", &mut phases);
    if &boot[3..11] != b"NTFS    " {
        return Err("not an NTFS volume".into());
    }
    let bps = le16(&boot, 0x0B).unwrap_or(0) as u64;
    let spc_raw = boot[0x0D];
    let spc = if spc_raw > 0x80 { 1u64 << (256 - spc_raw as u32) } else { spc_raw as u64 };
    let cluster = bps * spc;
    let mft_lcn = le64(&boot, 0x30).unwrap_or(0);
    let cpr = boot[0x40] as i8;
    let rec_size = if cpr > 0 { cpr as u64 * cluster } else { 1u64 << (-(cpr as i32)) as u32 };
    if cluster == 0 || !cluster.is_power_of_two() || !(512..=65536).contains(&rec_size) {
        return Err("unexpected NTFS geometry".into());
    }
    let geo = Geometry { cluster, rec_size };

    // Record 0 describes the MFT itself.
    let first = read_at(&vol, mft_lcn * cluster, rec_size.max(cluster) as usize)?;
    let mut rec0 = first[..rec_size as usize].to_vec();
    if &rec0[0..4] != b"FILE" || !fixup(&mut rec0) {
        return Err("MFT record 0 is damaged".into());
    }
    lap("read MFT record 0", &mut phases);
    let (mft_size, runs) = mft_layout(&vol, &rec0, geo)?;
    let total = (mft_size / rec_size) as usize;
    lap("map MFT extents", &mut phases);

    progress.set_phase("Reading master file table");
    let mut recs = vec![Rec::default(); total];
    let per_chunk = (CHUNK.max(cluster) / rec_size) as usize;
    let read = AtomicU64::new(0);
    let outs: Vec<Result<ChunkOut, String>> = recs
        .par_chunks_mut(per_chunk)
        .enumerate()
        .map(|(ci, slots)| {
            let mut out = ChunkOut { names: String::with_capacity(slots.len() * 20), ..Default::default() };
            if progress.cancelled() {
                return Ok(out);
            }
            let first = ci * per_chunk;
            let len = slots.len() * rec_size as usize;
            let mut buf = AlignedBuf::new((len as u64).div_ceil(cluster) as usize * cluster as usize);
            read_stream(&vol, &runs, first as u64 * rec_size, &mut buf, cluster)?;
            let done = read.fetch_add(len as u64, Ordering::Relaxed) + len as u64;
            progress.set_fraction(Some(done as f32 / mft_size as f32));

            let mut in_use = 0;
            for (i, rec) in buf[..len].chunks_exact_mut(rec_size as usize).enumerate() {
                if parse_record(rec, first + i, &mut slots[i], ci as u32, &mut out) {
                    in_use += 1;
                }
            }
            progress.files.fetch_add(in_use, Ordering::Relaxed);
            Ok(out)
        })
        .collect();
    if progress.cancelled() {
        return Err("cancelled".into());
    }

    let mut arenas = Vec::with_capacity(outs.len());
    let mut links = Vec::new();
    for out in outs {
        let out = out?;
        arenas.push(out.names);
        links.extend(out.links);
        for (target, p) in out.partials {
            if let Some(dst) = recs.get_mut(target)
                && let Some(extra) = merge(dst, &p, target) {
                    links.push(extra);
                }
        }
    }
    lap("read + parse MFT", &mut phases);

    progress.set_phase("Building tree");
    progress.set_fraction(None);
    let names = Names { arenas: &arenas };
    let b = link_records(root, &recs, &names, &links)?;
    drop(recs);
    drop(arenas);
    lap("link records", &mut phases);
    let mut info = base_info(ScanMode::Mft);
    let mut tree = b.finish(root.to_string(), info.clone());
    lap("aggregate", &mut phases);
    info.phases = phases;
    tree.info = info;
    Ok(tree)
}

/// Heap buffer whose start is aligned for unbuffered I/O.
struct AlignedBuf {
    raw: Vec<u8>,
    off: usize,
    len: usize,
}

impl AlignedBuf {
    fn new(len: usize) -> Self {
        let raw = vec![0u8; len + ALIGN];
        let off = raw.as_ptr().align_offset(ALIGN);
        AlignedBuf { raw, off, len }
    }
}

impl std::ops::Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.raw[self.off..self.off + self.len]
    }
}

impl std::ops::DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.raw[self.off..self.off + self.len]
    }
}

fn read_at(vol: &File, offset: u64, len: usize) -> Result<AlignedBuf, String> {
    let mut buf = AlignedBuf::new(len);
    read_exact_at(vol, offset, &mut buf)?;
    Ok(buf)
}

fn read_exact_at(vol: &File, mut offset: u64, mut buf: &mut [u8]) -> Result<(), String> {
    while !buf.is_empty() {
        match vol.seek_read(buf, offset) {
            Ok(0) => return Err("unexpected end of volume".into()),
            Ok(n) => {
                offset += n as u64;
                buf = &mut buf[n..];
            }
            Err(e) => return Err(format!("read failed: {e}")),
        }
    }
    Ok(())
}

/// Read `buf.len()` bytes starting at byte `pos` of a non-resident stream,
/// following its run list. Sparse runs read as zeros.
fn read_stream(vol: &File, runs: &Runs, pos: u64, buf: &mut [u8], cluster: u64) -> Result<(), String> {
    let end = pos + buf.len() as u64;
    let mut run_start = 0u64;
    for &(lcn, clusters) in runs {
        let run_end = run_start + clusters * cluster;
        let (a, b) = (pos.max(run_start), end.min(run_end));
        if a < b {
            let dst = &mut buf[(a - pos) as usize..(b - pos) as usize];
            if lcn < 0 {
                dst.fill(0);
            } else {
                read_exact_at(vol, lcn as u64 * cluster + (a - run_start), dst)?;
            }
        }
        if run_end >= end {
            return Ok(());
        }
        run_start = run_end;
    }
    Err("offset outside MFT".into())
}

/// Size of the MFT in bytes and its full run list (following an attribute
/// list if the MFT is so fragmented that its runs spill into other records).
fn mft_layout(vol: &File, rec0: &[u8], geo: Geometry) -> Result<(u64, Runs), String> {
    let mut size = None;
    let mut runs = Runs::new();
    let mut attr_list = None;
    for a in attributes(rec0) {
        let (ty, nonres, name_len) = (le32(a, 0).unwrap_or(0), a[8] != 0, a[9]);
        if ty == AT_DATA && name_len == 0 && nonres {
            if le64(a, 16) == Some(0) {
                size = le64(a, 48);
            }
            runs.extend(runs_of(a));
        } else if ty == AT_ATTRIBUTE_LIST {
            attr_list = Some(if nonres {
                let list_runs = runs_of(a);
                let len = le64(a, 48).unwrap_or(0);
                let alloc = list_runs.iter().map(|r| r.1).sum::<u64>() * geo.cluster;
                let mut buf = AlignedBuf::new(alloc as usize);
                read_stream(vol, &list_runs, 0, &mut buf, geo.cluster)?;
                buf[..(len as usize).min(alloc as usize)].to_vec()
            } else {
                resident_value(a).unwrap_or_default().to_vec()
            });
        }
    }
    let size = size.ok_or("MFT has no data attribute")?;
    let needed = size.div_ceil(geo.cluster);
    let have = |runs: &Runs| runs.iter().map(|r| r.1).sum::<u64>();

    if have(&runs) < needed {
        let list = attr_list.ok_or("MFT run list incomplete")?;
        // (starting vcn, record number) of every extra $DATA extent.
        let mut extents = Vec::new();
        let mut pos = 0;
        while pos + 26 <= list.len() {
            let ty = le32(&list, pos).unwrap_or(0);
            let len = le16(&list, pos + 4).unwrap_or(0) as usize;
            if len == 0 {
                break;
            }
            let svcn = le64(&list, pos + 8).unwrap_or(0);
            let recno = le64(&list, pos + 16).unwrap_or(0) & 0xFFFF_FFFF_FFFF;
            if ty == AT_DATA && recno != 0 && svcn != 0 {
                extents.push((svcn, recno));
            }
            pos += len;
        }
        extents.sort_unstable();
        for (svcn, recno) in extents {
            // Extension records live early in the MFT, inside the runs we already know.
            let start = recno * geo.rec_size;
            let mut rec = AlignedBuf::new(geo.rec_size.max(geo.cluster) as usize);
            let aligned = start / geo.cluster * geo.cluster;
            read_stream(vol, &runs, aligned, &mut rec, geo.cluster)?;
            let off = (start - aligned) as usize;
            let mut rec = rec[off..off + geo.rec_size as usize].to_vec();
            if !fixup(&mut rec) {
                return Err("MFT extension record is damaged".into());
            }
            for a in attributes(&rec) {
                if le32(a, 0) == Some(AT_DATA) && a[9] == 0 && a[8] != 0 && le64(a, 16) == Some(svcn) {
                    runs.extend(runs_of(a));
                }
            }
        }
        if have(&runs) < needed {
            return Err("MFT run list incomplete".into());
        }
    }
    Ok((size, runs))
}

/// Parse one raw record. Base records are written into `slot`; attributes of
/// extension records are queued for their base record. Returns true for an
/// in-use base record.
fn parse_record(buf: &mut [u8], recno: usize, slot: &mut Rec, chunk: u32, out: &mut ChunkOut) -> bool {
    if &buf[0..4] != b"FILE" || !fixup(buf) {
        return false;
    }
    let hflags = le16(buf, 0x16).unwrap_or(0);
    if hflags & 1 == 0 {
        return false; // not in use
    }
    let base = (le64(buf, 0x20).unwrap_or(0) & 0xFFFF_FFFF_FFFF) as usize;
    if base == 0 {
        slot.fl |= R_INUSE;
        if hflags & 2 != 0 {
            slot.fl |= R_DIR;
        }
        slot.seq = le16(buf, 0x10).unwrap_or(0);
        absorb(buf, recno, slot, chunk, out);
        true
    } else {
        let mut p = Rec::default();
        absorb(buf, base, &mut p, chunk, out);
        out.partials.push((base, p));
        false
    }
}

/// Accumulate the attributes of one record into `r` (which describes file `target`).
fn absorb(buf: &[u8], target: usize, r: &mut Rec, chunk: u32, out: &mut ChunkOut) {
    for a in attributes(buf) {
        let ty = le32(a, 0).unwrap_or(0);
        let nonres = a[8] != 0;
        let name_len = a[9];
        match ty {
            AT_STANDARD_INFORMATION if !nonres => {
                if let Some(v) = resident_value(a).filter(|v| v.len() >= 36) {
                    r.mtime = filetime_to_unix(le64(v, 8).unwrap_or(0)).clamp(0, u32::MAX as i64) as u32;
                    let fa = le32(v, 32).unwrap_or(0);
                    r.attrs = 0;
                    if fa & 0x2 != 0 {
                        r.attrs |= A_HIDDEN;
                    }
                    if fa & 0x4 != 0 {
                        r.attrs |= A_SYSTEM;
                    }
                    if fa & (0x1000 | 0x40000 | 0x400000) != 0 {
                        r.attrs |= A_CLOUD;
                    }
                    r.fl |= R_HAS_SI;
                }
            }
            AT_FILE_NAME if !nonres => {
                let Some(v) = resident_value(a).filter(|v| v.len() >= 66) else { continue };
                let pref = le64(v, 0).unwrap_or(0);
                let nlen = v[64] as usize;
                let ns = v[65];
                let parent = pref & 0xFFFF_FFFF_FFFF;
                if parent > u32::MAX as u64 || v.len() < 66 + nlen * 2 {
                    continue;
                }
                let has_long = r.fl & R_NAMED != 0 && r.ns != NS_DOS;
                if ns == NS_DOS && r.fl & R_NAMED != 0 {
                    continue; // 8.3 alias of a name we already have
                }
                let off = out.names.len();
                let units = v[66..66 + nlen * 2].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]]));
                out.names.extend(char::decode_utf16(units).map(|c| c.unwrap_or('\u{FFFD}')));
                let name = NameRef { chunk, off: off as u32, len: (out.names.len() - off).min(u16::MAX as usize) as u16 };
                let parent_seq = (pref >> 48) as u16;
                if has_long {
                    // A second long name = another hard link to this file.
                    out.links.push(Link { target: target as u32, parent: parent as u32, parent_seq, name });
                } else {
                    r.name = name;
                    r.parent = parent as u32;
                    r.parent_seq = parent_seq;
                    r.ns = ns;
                    r.fl |= R_NAMED;
                }
            }
            AT_DATA => {
                let first_extent = !nonres || le64(a, 16) == Some(0);
                if !first_extent {
                    continue;
                }
                if name_len == 0 {
                    if nonres {
                        r.size = le64(a, 48).unwrap_or(0);
                        r.alloc += nonres_alloc(a);
                    } else {
                        r.size = le32(a, 16).unwrap_or(0) as u64;
                    }
                    r.fl |= R_HAS_SIZE;
                } else if nonres && target >= 16 {
                    // Alternate streams (incl. WofCompressedData) take real space.
                    r.alloc += nonres_alloc(a);
                }
            }
            AT_INDEX_ALLOCATION if nonres && le64(a, 16) == Some(0) => {
                r.alloc += nonres_alloc(a);
            }
            _ => {}
        }
    }
}

/// Fold an extension record's attributes into its base record. Returns a
/// hard link if the extension carried a second long name.
fn merge(dst: &mut Rec, src: &Rec, target: usize) -> Option<Link> {
    let mut extra = None;
    if src.fl & R_NAMED != 0 {
        let dst_long = dst.fl & R_NAMED != 0 && dst.ns != NS_DOS;
        if !dst_long {
            dst.name = src.name;
            dst.parent = src.parent;
            dst.parent_seq = src.parent_seq;
            dst.ns = src.ns;
            dst.fl |= R_NAMED;
        } else if src.ns != NS_DOS {
            extra = Some(Link { target: target as u32, parent: src.parent, parent_seq: src.parent_seq, name: src.name });
        }
    }
    if src.fl & R_HAS_SIZE != 0 {
        dst.size = src.size;
    }
    if src.fl & R_HAS_SI != 0 {
        dst.mtime = src.mtime;
        dst.attrs = src.attrs;
    }
    dst.alloc += src.alloc;
    extra
}

fn nonres_alloc(a: &[u8]) -> u64 {
    let aflags = le16(a, 12).unwrap_or(0);
    if aflags & 0x8001 != 0 {
        // compressed or sparse: the real allocation lives in compressed_size
        le64(a, 64).unwrap_or(0)
    } else {
        le64(a, 40).unwrap_or(0)
    }
}

struct Names<'a> {
    arenas: &'a [String],
}

impl Names<'_> {
    fn get(&self, n: NameRef) -> &str {
        self.arenas
            .get(n.chunk as usize)
            .and_then(|a| a.get(n.off as usize..n.off as usize + n.len as usize))
            .unwrap_or("")
    }
}

/// Children entries are record numbers, or link indices tagged with this bit.
const LINK_BIT: u32 = 1 << 31;

fn link_records(root: &str, recs: &[Rec], names: &Names, links: &[Link]) -> Result<TreeBuilder, String> {
    let n = recs.len();
    if n <= ROOT_RECORD || recs[ROOT_RECORD].fl & R_INUSE == 0 {
        return Err("root directory record missing".into());
    }
    let is_live_dir = |i: usize| recs[i].fl & (R_INUSE | R_DIR) == (R_INUSE | R_DIR);
    let parent_ok = |p: usize, seq: u16| p < n && is_live_dir(p) && (seq == 0 || recs[p].seq == seq);
    let valid = |i: usize| {
        let r = &recs[i];
        i != ROOT_RECORD
            && r.fl & (R_INUSE | R_NAMED) == (R_INUSE | R_NAMED)
            && r.parent as usize != i
            && parent_ok(r.parent as usize, r.parent_seq)
    };
    let link_ok = |l: &Link| {
        let t = l.target as usize;
        t < n && recs[t].fl & (R_INUSE | R_DIR) == R_INUSE && parent_ok(l.parent as usize, l.parent_seq)
    };

    // Children lists (CSR) over record numbers plus hard links.
    let mut start = vec![0u32; n + 1];
    for i in 0..n {
        if valid(i) {
            start[recs[i].parent as usize + 1] += 1;
        }
    }
    for l in links.iter().filter(|l| link_ok(l)) {
        start[l.parent as usize + 1] += 1;
    }
    for i in 0..n {
        start[i + 1] += start[i];
    }
    let mut fill = start.clone();
    let mut kids = vec![0u32; start[n] as usize];
    for i in 0..n {
        if valid(i) {
            let p = recs[i].parent as usize;
            kids[fill[p] as usize] = i as u32;
            fill[p] += 1;
        }
    }
    for (li, l) in links.iter().enumerate().filter(|(_, l)| link_ok(l)) {
        let p = l.parent as usize;
        kids[fill[p] as usize] = li as u32 | LINK_BIT;
        fill[p] += 1;
    }
    let children = |i: usize| &kids[start[i] as usize..start[i + 1] as usize];
    let name_of = |c: u32| {
        if c & LINK_BIT != 0 { names.get(links[(c & !LINK_BIT) as usize].name) } else { names.get(recs[c as usize].name) }
    };

    // Descend to the requested folder.
    let mut cur = ROOT_RECORD;
    for comp in root.get(3..).unwrap_or("").split('\\').filter(|c| !c.is_empty()) {
        let lower = comp.to_lowercase();
        cur = children(cur)
            .iter()
            .copied()
            .filter(|&c| c & LINK_BIT == 0)
            .find(|&c| name_of(c).to_lowercase() == lower)
            .ok_or_else(|| format!("folder not found in MFT: {comp}"))? as usize;
        if !is_live_dir(cur) {
            return Err(format!("{comp} is not a folder"));
        }
    }

    // The on-disk MFT can lag for files the kernel keeps open and resizes
    // (pagefile.sys, hiberfil.sys…). The directory listing has current sizes,
    // so use it for the scan root's own files.
    let live: HashMap<String, (u64, u64)> = super::walk::list_root_files(root);

    let mut b = TreeBuilder::new(root);
    let mut queue = VecDeque::from([(cur, ROOT, true)]);
    while let Some((rec, id, is_root)) = queue.pop_front() {
        for &c in children(rec) {
            let name = name_of(c);
            if c & LINK_BIT != 0 {
                let l = &links[(c & !LINK_BIT) as usize];
                let t = &recs[l.target as usize];
                b.add(id, name, flags::HARDLINK, 0, 0, t.mtime as i64);
                continue;
            }
            let r = &recs[c as usize];
            let is_dir = r.fl & R_DIR != 0;
            let mut fl = if is_dir { flags::DIR } else { 0 };
            if r.attrs & A_HIDDEN != 0 {
                fl |= flags::HIDDEN;
            }
            if r.attrs & A_SYSTEM != 0 || (c as usize) < 16 {
                fl |= flags::SYSTEM;
            }
            if r.attrs & A_CLOUD != 0 && !is_dir {
                fl |= flags::CLOUD;
            }
            let (mut size, mut alloc) = (if is_dir { 0 } else { r.size }, r.alloc);
            if is_root && !is_dir
                && let Some(&(s, a)) = live.get(&name.to_lowercase()) {
                    (size, alloc) = (s, a.max(alloc.min(s)));
                }
            let nid = b.add(id, name, fl, size, alloc, r.mtime as i64);
            if is_dir {
                queue.push_back((c as usize, nid, false));
            }
        }
    }
    Ok(b)
}

// ---------------------------------------------------------------------------
// Low-level record helpers

/// Undo NTFS "update sequence" protection: the last two bytes of every 512-byte
/// sector were replaced on disk with a check value; restore the originals.
fn fixup(rec: &mut [u8]) -> bool {
    let (Some(off), Some(cnt)) = (le16(rec, 4), le16(rec, 6)) else { return false };
    let (off, cnt) = (off as usize, cnt as usize);
    if cnt == 0 || off + cnt * 2 > rec.len() {
        return false;
    }
    let usn = [rec[off], rec[off + 1]];
    for i in 1..cnt {
        let end = i * 512;
        if end > rec.len() {
            break;
        }
        if rec[end - 2..end] != usn {
            return false; // torn write
        }
        rec[end - 2] = rec[off + 2 * i];
        rec[end - 1] = rec[off + 2 * i + 1];
    }
    true
}

struct Attributes<'a> {
    rec: &'a [u8],
    off: usize,
    end: usize,
}

fn attributes(rec: &[u8]) -> Attributes<'_> {
    let off = le16(rec, 0x14).unwrap_or(u16::MAX) as usize;
    let end = (le32(rec, 0x18).unwrap_or(0) as usize).min(rec.len());
    Attributes { rec, off, end }
}

impl<'a> Iterator for Attributes<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.off + 24 > self.end {
            return None;
        }
        let ty = le32(self.rec, self.off)?;
        if ty == AT_END {
            return None;
        }
        let len = le32(self.rec, self.off + 4)? as usize;
        if len < 24 || self.off + len > self.end {
            return None;
        }
        let a = &self.rec[self.off..self.off + len];
        self.off += len;
        Some(a)
    }
}

fn resident_value(a: &[u8]) -> Option<&[u8]> {
    let len = le32(a, 16)? as usize;
    let off = le16(a, 20)? as usize;
    a.get(off..off + len)
}

fn runs_of(a: &[u8]) -> Runs {
    let off = le16(a, 32).unwrap_or(0) as usize;
    a.get(off..).map(decode_runs).unwrap_or_default()
}

fn decode_runs(b: &[u8]) -> Runs {
    let mut out = Runs::new();
    let mut pos = 0;
    let mut lcn: i64 = 0;
    while pos < b.len() {
        let h = b[pos];
        if h == 0 {
            break;
        }
        pos += 1;
        let (ls, os) = ((h & 0x0F) as usize, (h >> 4) as usize);
        if ls == 0 || ls > 8 || os > 8 || pos + ls + os > b.len() {
            break;
        }
        let mut len = 0u64;
        for i in 0..ls {
            len |= (b[pos + i] as u64) << (8 * i);
        }
        pos += ls;
        if os == 0 {
            out.push((-1, len));
        } else {
            let mut delta: i64 = 0;
            for i in 0..os {
                delta |= (b[pos + i] as i64) << (8 * i);
            }
            let shift = 64 - 8 * os as u32;
            if shift < 64 {
                delta = (delta << shift) >> shift; // sign-extend
            }
            lcn += delta;
            out.push((lcn, len));
            pos += os;
        }
    }
    out
}

#[inline]
fn le16(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
#[inline]
fn le32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
}
#[inline]
fn le64(b: &[u8], o: usize) -> Option<u64> {
    b.get(o..o + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_decode_with_negative_offsets() {
        // len 0x18 @ lcn 0x5634; len 0x10 @ lcn -0x100 relative; sparse 4
        let data = [0x21, 0x18, 0x34, 0x56, 0x22, 0x10, 0x00, 0x00, 0xFF, 0x01, 0x04, 0x00];
        let r = decode_runs(&data);
        assert_eq!(r, vec![(0x5634, 0x18), (0x5634 - 0x100, 0x10), (-1, 4)]);
    }

    #[test]
    fn fixup_restores_sector_tails() {
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        rec[4..6].copy_from_slice(&0x30u16.to_le_bytes()); // usa offset
        rec[6..8].copy_from_slice(&3u16.to_le_bytes()); // 1 + 2 sectors
        rec[0x30..0x32].copy_from_slice(&[0xAB, 0xCD]);
        rec[0x32..0x34].copy_from_slice(&[1, 2]);
        rec[0x34..0x36].copy_from_slice(&[3, 4]);
        rec[510..512].copy_from_slice(&[0xAB, 0xCD]);
        rec[1022..1024].copy_from_slice(&[0xAB, 0xCD]);
        assert!(fixup(&mut rec));
        assert_eq!(&rec[510..512], &[1, 2]);
        assert_eq!(&rec[1022..1024], &[3, 4]);

        let mut torn = rec.clone();
        torn[510..512].copy_from_slice(&[0, 0]);
        assert!(!fixup(&mut torn));
    }

    /// Build a minimal in-use FILE record with the given attributes.
    fn record(base: u64, dir: bool, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; 1024];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        r[6..8].copy_from_slice(&3u16.to_le_bytes());
        r[0x10..0x12].copy_from_slice(&1u16.to_le_bytes()); // sequence
        r[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes()); // first attribute
        r[0x16..0x18].copy_from_slice(&(1u16 | if dir { 2 } else { 0 }).to_le_bytes());
        r[0x20..0x28].copy_from_slice(&base.to_le_bytes());
        let mut off = 0x38;
        for a in attrs {
            r[off..off + a.len()].copy_from_slice(a);
            off += a.len();
        }
        r[off..off + 4].copy_from_slice(&AT_END.to_le_bytes());
        r[0x18..0x1C].copy_from_slice(&((off + 8) as u32).to_le_bytes());
        // Sector tails carry the update sequence number.
        let (tail1, tail2) = ([r[510], r[511]], [r[1022], r[1023]]);
        r[0x30..0x32].copy_from_slice(&[7, 7]);
        r[0x32..0x34].copy_from_slice(&tail1);
        r[0x34..0x36].copy_from_slice(&tail2);
        r[510..512].copy_from_slice(&[7, 7]);
        r[1022..1024].copy_from_slice(&[7, 7]);
        r
    }

    fn resident(ty: u32, value: &[u8]) -> Vec<u8> {
        let len = (24 + value.len()).div_ceil(8) * 8;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&ty.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[20..22].copy_from_slice(&24u16.to_le_bytes());
        a[24..24 + value.len()].copy_from_slice(value);
        a
    }

    fn file_name(parent: u64, name: &str, ns: u8) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 66 + units.len() * 2];
        v[0..8].copy_from_slice(&(parent | (1u64 << 48)).to_le_bytes());
        v[64] = units.len() as u8;
        v[65] = ns;
        for (i, u) in units.iter().enumerate() {
            v[66 + i * 2..68 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        resident(AT_FILE_NAME, &v)
    }

    #[test]
    fn parses_names_sizes_and_hard_links() {
        let mut out = ChunkOut::default();
        let mut slot = Rec::default();
        let mut rec = record(
            0,
            false,
            &[
                file_name(5, "REPORT~1.PDF", NS_DOS),
                file_name(5, "report final.pdf", 1),
                file_name(40, "linked copy.pdf", 1),
                resident(AT_DATA, &[0u8; 100]),
            ],
        );
        assert!(parse_record(&mut rec, 77, &mut slot, 0, &mut out));
        let names = Names { arenas: std::slice::from_ref(&out.names) };
        assert_eq!(names.get(slot.name), "report final.pdf", "long name replaces the 8.3 alias");
        assert_eq!(slot.parent, 5);
        assert_eq!(slot.size, 100);
        assert_eq!(out.links.len(), 1);
        assert_eq!(names.get(out.links[0].name), "linked copy.pdf");
        assert_eq!(out.links[0].parent, 40);

        // An extension record adds its data size to the base record.
        let mut nonres = vec![0u8; 72];
        nonres[0..4].copy_from_slice(&AT_DATA.to_le_bytes());
        nonres[4..8].copy_from_slice(&72u32.to_le_bytes());
        nonres[8] = 1;
        nonres[32..34].copy_from_slice(&64u16.to_le_bytes());
        nonres[40..48].copy_from_slice(&8192u64.to_le_bytes());
        nonres[48..56].copy_from_slice(&5000u64.to_le_bytes());
        let mut ext = record(77, false, &[nonres]);
        let mut unused = Rec::default();
        assert!(!parse_record(&mut ext, 90, &mut unused, 0, &mut out));
        let (target, p) = out.partials.pop().unwrap();
        assert_eq!(target, 77);
        merge(&mut slot, &p, target);
        assert_eq!((slot.size, slot.alloc), (5000, 8192));
    }
}
