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
//!
//! After a full scan the parsed records are kept, along with the position in
//! the NTFS change journal. A rescan then reads only the journal entries
//! written since, re-reads just the records they name (through NTFS, so no
//! volume flush), and rebuilds the tree: seconds become a fraction of one.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use rayon::prelude::*;

use super::mft_parse::*;
use super::{base_info, Progress};
use crate::tree::{flags, ScanMode, Tree, TreeBuilder, ROOT};

const SHARE_ALL: u32 = 1 | 2 | 4; // FILE_SHARE_READ | WRITE | DELETE
/// Direct reads: skip the cache manager (faster, and doesn't evict the
/// user's cached files with gigabytes of MFT). Needs sector-aligned I/O.
const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
const ALIGN: usize = 4096;
const ROOT_RECORD: usize = 5;
const CHUNK: u64 = 4 << 20;

#[derive(Clone, Copy)]
struct Geometry {
    cluster: u64,
    rec_size: u64,
}

pub fn scan(root: &str, progress: &Progress) -> Result<Tree, String> {
    scan_with_state(root, progress).map(|(t, _)| t)
}

/// A full MFT scan that also returns what [`refresh`] needs later.
pub fn scan_with_state(root: &str, progress: &Progress) -> Result<(Tree, MftState), String> {
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
    // Note the journal position before reading, so anything that changes
    // while we read is replayed by the next refresh.
    let journal = query_journal(&vol).ok().map(|j| (j.id, j.next));
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
    lap("link records", &mut phases);
    let mut info = base_info(ScanMode::Mft);
    let mut tree = b.finish(root.to_string(), info.clone());
    lap("aggregate", &mut phases);
    info.phases = phases;
    tree.info = info;
    let state = MftState { drive, root: root.to_string(), rec_size: rec_size as usize, recs, arenas, links, journal };
    Ok((tree, state))
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
// Incremental rescans through the NTFS change journal

/// Everything a full scan parsed, kept so a rescan can apply only what the
/// change journal says has changed.
pub struct MftState {
    drive: char,
    root: String,
    rec_size: usize,
    recs: Vec<Rec>,
    arenas: Vec<String>,
    links: Vec<Link>,
    /// (journal id, next USN) as of the last scan or refresh.
    journal: Option<(u64, i64)>,
}

/// More changes than this and a full scan is quicker than chasing them.
const MAX_CHANGES: usize = 250_000;

/// Bring `st` up to date from the change journal. `Ok(None)` means nothing
/// changed; `Err` means the journal can't be used (it was reset, or too much
/// changed) and a full scan is needed.
pub fn refresh(st: &mut MftState, progress: &Progress) -> Result<Option<Tree>, String> {
    let t0 = Instant::now();
    let (id, since) = st.journal.ok_or("this volume has no change journal")?;
    progress.set_phase("Reading the change journal");
    let vol = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        .open(format!(r"\\.\{}:", st.drive))
        .map_err(|e| format!("cannot open volume: {e}"))?;
    let now = query_journal(&vol)?;
    if now.id != id || since < now.first {
        return Err("the change journal was reset since the last scan".into());
    }
    let (changed, next) = read_journal(&vol, id, since, now.next)?;
    let journal_ms = t0.elapsed().as_millis() as u64;
    if changed.is_empty() {
        st.journal = Some((id, next));
        return Ok(None);
    }
    if changed.len() > MAX_CHANGES {
        return Err(format!("{} items changed", changed.len()));
    }

    progress.set_phase("Re-reading changed files");
    let t1 = Instant::now();
    let chunk = st.arenas.len() as u32;
    let mut out = ChunkOut::default();
    let mut todo: Vec<u64> = changed.into_iter().collect();
    todo.sort_unstable();
    let targets: std::collections::HashSet<u64> = todo.iter().copied().collect();
    st.links.retain(|l| !targets.contains(&(l.target as u64)));
    let mut i = 0;
    while i < todo.len() {
        let recno = todo[i] as usize;
        i += 1;
        if recno >= st.recs.len() {
            st.recs.resize(recno + 1, Rec::default());
        }
        st.recs[recno] = Rec::default();
        let Some(mut buf) = file_record(&vol, recno as u64, st.rec_size)? else { continue };
        if !fixup_lenient(&mut buf) {
            continue;
        }
        let mut slot = Rec::default();
        parse_fixed_record(&buf, recno, &mut slot, chunk, &mut out);
        st.recs[recno] = slot;
        // Attributes that overflowed into extension records.
        for ext in attribute_list_records(&buf, recno as u64) {
            if let Some(mut e) = file_record(&vol, ext, st.rec_size)?
                && fixup_lenient(&mut e)
            {
                let mut unused = Rec::default();
                parse_fixed_record(&e, ext as usize, &mut unused, chunk, &mut out);
            }
        }
    }
    let reread = todo.len();
    st.arenas.push(out.names);
    st.links.extend(out.links);
    for (target, p) in out.partials {
        if let Some(dst) = st.recs.get_mut(target)
            && let Some(extra) = merge(dst, &p, target)
        {
            st.links.push(extra);
        }
    }
    let reread_ms = t1.elapsed().as_millis() as u64;

    progress.set_phase("Building tree");
    let t2 = Instant::now();
    let names = Names { arenas: &st.arenas };
    let b = link_records(&st.root, &st.recs, &names, &st.links)?;
    let link_ms = t2.elapsed().as_millis() as u64;
    let t3 = Instant::now();
    let mut info = base_info(ScanMode::Mft);
    info.note = Some(format!("Updated {} changed item(s) from the NTFS change journal.", reread));
    let mut tree = b.finish(st.root.clone(), info);
    tree.info.phases = vec![
        ("read change journal", journal_ms),
        ("re-read changed records", reread_ms),
        ("link records", link_ms),
        ("aggregate", t3.elapsed().as_millis() as u64),
    ];
    st.journal = Some((id, next));
    Ok(Some(tree))
}

struct JournalInfo {
    id: u64,
    first: i64,
    next: i64,
}

fn ioctl(vol: &File, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize, std::io::Error> {
    use std::os::windows::io::AsRawHandle;
    let mut returned = 0u32;
    let ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            vol.as_raw_handle() as _,
            code,
            input.as_ptr().cast(),
            input.len() as u32,
            output.as_mut_ptr().cast(),
            output.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 { Err(std::io::Error::last_os_error()) } else { Ok(returned as usize) }
}

fn query_journal(vol: &File) -> Result<JournalInfo, String> {
    use windows_sys::Win32::System::Ioctl::{FSCTL_QUERY_USN_JOURNAL, USN_JOURNAL_DATA_V0};
    let mut out = [0u8; std::mem::size_of::<USN_JOURNAL_DATA_V0>()];
    ioctl(vol, FSCTL_QUERY_USN_JOURNAL, &[], &mut out).map_err(|e| format!("no change journal: {e}"))?;
    Ok(JournalInfo {
        id: le64(&out, 0).unwrap_or(0),
        first: le64(&out, 8).unwrap_or(0) as i64,
        next: le64(&out, 16).unwrap_or(0) as i64,
    })
}

/// Record numbers of everything the journal mentions between `start` and
/// `end`, and the USN to continue from next time.
fn read_journal(vol: &File, id: u64, start: i64, end: i64) -> Result<(std::collections::HashSet<u64>, i64), String> {
    use windows_sys::Win32::System::Ioctl::{FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0};
    let mut changed = std::collections::HashSet::new();
    let mut usn = start;
    let mut buf = vec![0u8; 1 << 16];
    while usn < end && changed.len() <= MAX_CHANGES {
        let req = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: usn,
            ReasonMask: u32::MAX,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: id,
        };
        let input = unsafe {
            std::slice::from_raw_parts((&req as *const READ_USN_JOURNAL_DATA_V0).cast::<u8>(), std::mem::size_of_val(&req))
        };
        let n = ioctl(vol, FSCTL_READ_USN_JOURNAL, input, &mut buf).map_err(|e| format!("reading the change journal: {e}"))?;
        if n < 8 {
            break;
        }
        let next = le64(&buf, 0).unwrap_or(0) as i64;
        changed.extend(usn_record_numbers(&buf[8..n]));
        if next <= usn {
            break;
        }
        usn = next;
    }
    Ok((changed, usn))
}

/// Record `recno` as NTFS has it now, or `None` if it isn't in use.
fn file_record(vol: &File, recno: u64, rec_size: usize) -> Result<Option<Vec<u8>>, String> {
    use windows_sys::Win32::System::Ioctl::FSCTL_GET_NTFS_FILE_RECORD;
    let mut out = vec![0u8; 12 + rec_size.max(4096)];
    let input = (recno as i64).to_le_bytes();
    ioctl(vol, FSCTL_GET_NTFS_FILE_RECORD, &input, &mut out).map_err(|e| format!("reading record {recno}: {e}"))?;
    // NTFS returns the nearest in-use record at or below the one asked for.
    if le64(&out, 0).unwrap_or(u64::MAX) & 0xFFFF_FFFF_FFFF != recno {
        return Ok(None);
    }
    let len = (le32(&out, 8).unwrap_or(0) as usize).min(out.len() - 12);
    Ok(Some(out[12..12 + len].to_vec()))
}
