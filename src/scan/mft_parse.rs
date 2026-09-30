//! Parsing NTFS master file table records. Pure functions over byte slices,
//! with no I/O and no dependencies outside `std`, so they build on every
//! platform and can be fuzzed (see `fuzz/`). The Windows-only reader in
//! `mft.rs` feeds them records straight off the volume.

#![cfg_attr(not(windows), allow(dead_code))]

pub(crate) const AT_STANDARD_INFORMATION: u32 = 0x10;
pub(crate) const AT_ATTRIBUTE_LIST: u32 = 0x20;
pub(crate) const AT_FILE_NAME: u32 = 0x30;
pub(crate) const AT_DATA: u32 = 0x80;
pub(crate) const AT_INDEX_ALLOCATION: u32 = 0xA0;
pub(crate) const AT_END: u32 = 0xFFFF_FFFF;

pub(crate) const NS_DOS: u8 = 2;

// Rec::fl
pub(crate) const R_INUSE: u8 = 1;
pub(crate) const R_DIR: u8 = 2;
pub(crate) const R_NAMED: u8 = 4;
pub(crate) const R_HAS_SIZE: u8 = 8;
pub(crate) const R_HAS_SI: u8 = 16;
// Rec::attrs (packed subset of FILE_ATTRIBUTE_*)
pub(crate) const A_HIDDEN: u8 = 1;
pub(crate) const A_SYSTEM: u8 = 2;
pub(crate) const A_CLOUD: u8 = 4;

/// Per-record scratch data, indexed by MFT record number.
#[derive(Clone, Copy, Default)]
pub(crate) struct Rec {
    pub(crate) parent: u32,
    pub(crate) parent_seq: u16,
    pub(crate) seq: u16,
    pub(crate) name: NameRef,
    pub(crate) ns: u8,
    pub(crate) fl: u8,
    pub(crate) attrs: u8,
    pub(crate) mtime: u32,
    pub(crate) size: u64,
    pub(crate) alloc: u64,
}

/// A name stored in one of the per-chunk string arenas.
#[derive(Clone, Copy, Default)]
pub(crate) struct NameRef {
    pub(crate) chunk: u32,
    pub(crate) off: u32,
    pub(crate) len: u16,
}

/// An additional hard link to a file (a second long `$FILE_NAME`).
#[derive(Clone, Copy)]
pub(crate) struct Link {
    pub(crate) target: u32,
    pub(crate) parent: u32,
    pub(crate) parent_seq: u16,
    pub(crate) name: NameRef,
}

#[derive(Default)]
pub(crate) struct ChunkOut {
    pub(crate) names: String,
    /// Attributes found in extension records, keyed by their base record.
    pub(crate) partials: Vec<(usize, Rec)>,
    pub(crate) links: Vec<Link>,
}

pub(crate) type Runs = Vec<(i64, u64)>; // (lcn or -1 for sparse, length in clusters)

/// Parse one raw record. Base records are written into `slot`; attributes of
/// extension records are queued for their base record. Returns true for an
/// in-use base record.
pub(crate) fn parse_record(buf: &mut [u8], recno: usize, slot: &mut Rec, chunk: u32, out: &mut ChunkOut) -> bool {
    if buf.get(0..4) != Some(b"FILE".as_slice()) || !fixup(buf) {
        return false;
    }
    parse_fixed_record(buf, recno, slot, chunk, out)
}

/// [`parse_record`] for a record whose update sequence is already undone.
pub(crate) fn parse_fixed_record(buf: &[u8], recno: usize, slot: &mut Rec, chunk: u32, out: &mut ChunkOut) -> bool {
    if buf.get(0..4) != Some(b"FILE".as_slice()) {
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
pub(crate) fn absorb(buf: &[u8], target: usize, r: &mut Rec, chunk: u32, out: &mut ChunkOut) {
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
                        r.alloc = r.alloc.saturating_add(nonres_alloc(a));
                    } else {
                        r.size = le32(a, 16).unwrap_or(0) as u64;
                    }
                    r.fl |= R_HAS_SIZE;
                } else if nonres && target >= 16 {
                    // Alternate streams (incl. WofCompressedData) take real space.
                    r.alloc = r.alloc.saturating_add(nonres_alloc(a));
                }
            }
            AT_INDEX_ALLOCATION if nonres && le64(a, 16) == Some(0) => {
                r.alloc = r.alloc.saturating_add(nonres_alloc(a));
            }
            _ => {}
        }
    }
}

/// Fold an extension record's attributes into its base record. Returns a
/// hard link if the extension carried a second long name.
pub(crate) fn merge(dst: &mut Rec, src: &Rec, target: usize) -> Option<Link> {
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
    dst.alloc = dst.alloc.saturating_add(src.alloc);
    extra
}

pub(crate) fn nonres_alloc(a: &[u8]) -> u64 {
    let aflags = le16(a, 12).unwrap_or(0);
    if aflags & 0x8001 != 0 {
        // compressed or sparse: the real allocation lives in compressed_size
        le64(a, 64).unwrap_or(0)
    } else {
        le64(a, 40).unwrap_or(0)
    }
}

pub(crate) struct Names<'a> {
    pub(crate) arenas: &'a [String],
}

impl Names<'_> {
    pub(crate) fn get(&self, n: NameRef) -> &str {
        self.arenas
            .get(n.chunk as usize)
            .and_then(|a| a.get(n.off as usize..n.off as usize + n.len as usize))
            .unwrap_or("")
    }
}

/// Like [`fixup`], for records NTFS hands out itself
/// (`FSCTL_GET_NTFS_FILE_RECORD`): if the sector tails don't carry the
/// update sequence number, the record has already been fixed up.
pub(crate) fn fixup_lenient(rec: &mut [u8]) -> bool {
    let (Some(off), Some(cnt)) = (le16(rec, 4), le16(rec, 6)) else { return false };
    let (off, cnt) = (off as usize, cnt as usize);
    if cnt == 0 || off + cnt * 2 > rec.len() {
        return false;
    }
    let usn = [rec[off], rec[off + 1]];
    let tails_match = (1..cnt).take_while(|i| i * 512 <= rec.len()).all(|i| rec[i * 512 - 2..i * 512] == usn);
    if tails_match { fixup(rec) } else { true }
}

/// Other records named in a record's resident `$ATTRIBUTE_LIST`: where a
/// heavily fragmented or many-named file keeps the attributes that didn't
/// fit.
pub(crate) fn attribute_list_records(rec: &[u8], base: u64) -> Vec<u64> {
    let mut out = Vec::new();
    for a in attributes(rec) {
        if le32(a, 0) != Some(AT_ATTRIBUTE_LIST) || a[8] != 0 {
            continue;
        }
        let Some(list) = resident_value(a) else { continue };
        let mut pos = 0;
        while pos + 24 <= list.len() {
            let len = le16(list, pos + 4).unwrap_or(0) as usize;
            if len < 24 {
                break;
            }
            let recno = le64(list, pos + 16).unwrap_or(0) & 0xFFFF_FFFF_FFFF;
            if recno != base && !out.contains(&recno) {
                out.push(recno);
            }
            pos += len;
        }
    }
    out
}

/// File record numbers from the records in an `FSCTL_READ_USN_JOURNAL`
/// result (after its leading 8-byte "next USN").
pub(crate) fn usn_record_numbers(buf: &[u8]) -> Vec<u64> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 16 <= buf.len() {
        let len = le32(buf, pos).unwrap_or(0) as usize;
        if len < 16 || pos + len > buf.len() {
            break;
        }
        // Versions 2 and 3 both start the file reference at offset 8; in
        // version 3 it's 128 bits, and NTFS keeps the record number in the
        // low 48.
        if matches!(le16(buf, pos + 4), Some(2) | Some(3))
            && let Some(frn) = le64(buf, pos + 8)
        {
            out.push(frn & 0xFFFF_FFFF_FFFF);
        }
        pos += len;
    }
    out
}

/// Windows FILETIME (100 ns ticks since 1601) to unix seconds.
pub(crate) fn filetime_to_unix(ft: u64) -> i64 {
    (ft / 10_000_000) as i64 - 11_644_473_600
}

/// Throw arbitrary bytes at the parser the way the MFT reader would: as a
/// record (padded or cut to 1 KB), as a run list, and as a second record
/// merged into the first. It must never panic. Used by the fuzz target and
/// the randomized test below.
#[allow(dead_code)] // used by tests and the fuzz target
pub fn fuzz_one(data: &[u8]) {
    let mut rec = vec![0u8; 1024];
    let n = data.len().min(1024);
    rec[..n].copy_from_slice(&data[..n]);
    let mut out = ChunkOut::default();
    let mut slot = Rec::default();
    parse_record(&mut rec, 7, &mut slot, 0, &mut out);
    let names = Names { arenas: std::slice::from_ref(&out.names) };
    let _ = names.get(slot.name);
    for (target, p) in out.partials.clone() {
        let _ = merge(&mut slot, &p, target);
    }
    for l in &out.links {
        let _ = names.get(l.name);
    }
    for a in attributes(&rec) {
        let _ = runs_of(a);
        let _ = resident_value(a);
        let _ = nonres_alloc(a);
    }
    let _ = decode_runs(data);
}

// ---------------------------------------------------------------------------
// Low-level record helpers

/// Undo NTFS "update sequence" protection: the last two bytes of every 512-byte
/// sector were replaced on disk with a check value; restore the originals.
pub(crate) fn fixup(rec: &mut [u8]) -> bool {
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

pub(crate) struct Attributes<'a> {
    rec: &'a [u8],
    off: usize,
    end: usize,
}

pub(crate) fn attributes(rec: &[u8]) -> Attributes<'_> {
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

pub(crate) fn resident_value(a: &[u8]) -> Option<&[u8]> {
    let len = le32(a, 16)? as usize;
    let off = le16(a, 20)? as usize;
    a.get(off..off + len)
}

pub(crate) fn runs_of(a: &[u8]) -> Runs {
    let off = le16(a, 32).unwrap_or(0) as usize;
    a.get(off..).map(decode_runs).unwrap_or_default()
}

pub(crate) fn decode_runs(b: &[u8]) -> Runs {
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
            // Corrupt runs can point anywhere; stop rather than wrap around.
            let Some(next) = lcn.checked_add(delta).filter(|&l| l >= 0) else { break };
            lcn = next;
            out.push((lcn, len));
            pos += os;
        }
    }
    out
}

#[inline]
pub(crate) fn le16(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
#[inline]
pub(crate) fn le32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
}
#[inline]
pub(crate) fn le64(b: &[u8], o: usize) -> Option<u64> {
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

    /// A cheap in-tree fuzz run: valid records with random bytes changed, and
    /// plain random buffers. Debug builds panic on arithmetic overflow, so
    /// this also catches size and offset math that could wrap.
    #[test]
    fn survives_mutated_and_random_records() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut nonres = vec![0u8; 80];
        nonres[0..4].copy_from_slice(&AT_DATA.to_le_bytes());
        nonres[4..8].copy_from_slice(&80u32.to_le_bytes());
        nonres[8] = 1;
        nonres[32..34].copy_from_slice(&64u16.to_le_bytes());
        nonres[48..56].copy_from_slice(&5000u64.to_le_bytes());
        nonres[64..70].copy_from_slice(&[0x21, 0x18, 0x34, 0x56, 0x11, 0x05]);
        let seeds = [
            record(0, false, &[file_name(5, "report.pdf", 1), resident(AT_DATA, &[1; 40])]),
            record(0, true, &[file_name(5, "folder", 3), resident(AT_STANDARD_INFORMATION, &[0; 48])]),
            record(77, false, &[nonres, file_name(9, "link.txt", 1)]),
        ];
        for i in 0..20_000usize {
            let mut data = seeds[i % seeds.len()].clone();
            for _ in 0..1 + next() % 12 {
                let pos = (next() % data.len() as u64) as usize;
                data[pos] = next() as u8;
            }
            if i % 7 == 0 {
                data.truncate((next() % 1024) as usize);
            }
            fuzz_one(&data);
        }
        for _ in 0..5_000 {
            let len = (next() % 1100) as usize;
            let data: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            fuzz_one(&data);
        }
    }

    #[test]
    fn usn_records() {
        let mut buf = Vec::new();
        for (ver, frn) in [(2u16, 0x0003_0000_0000_1234u64), (3, 0x0001_0000_0000_0042)] {
            let mut r = vec![0u8; 64];
            r[0..4].copy_from_slice(&64u32.to_le_bytes());
            r[4..6].copy_from_slice(&ver.to_le_bytes());
            r[8..16].copy_from_slice(&frn.to_le_bytes());
            buf.extend(r);
        }
        buf.extend([1, 2, 3]); // trailing garbage is ignored
        assert_eq!(usn_record_numbers(&buf), [0x1234, 0x42]);
        assert!(usn_record_numbers(&[0xFF; 20]).is_empty());
    }

    #[test]
    fn attribute_lists_and_lenient_fixup() {
        // An attribute list naming the base record (10) and an extension (99).
        let mut list = vec![0u8; 64];
        for (i, rec) in [10u64, 99].iter().enumerate() {
            let e = &mut list[i * 32..i * 32 + 32];
            e[0..4].copy_from_slice(&AT_DATA.to_le_bytes());
            e[4..6].copy_from_slice(&32u16.to_le_bytes());
            e[16..24].copy_from_slice(&(rec | (1u64 << 48)).to_le_bytes());
        }
        let mut rec = record(0, false, &[resident(AT_ATTRIBUTE_LIST, &list)]);
        assert!(fixup(&mut rec.clone()));
        let mut already_fixed = rec.clone();
        assert!(fixup(&mut already_fixed));
        // A second pass sees no sequence numbers in the tails and leaves it be.
        let before = already_fixed.clone();
        assert!(fixup_lenient(&mut already_fixed));
        assert_eq!(before, already_fixed);
        assert!(fixup_lenient(&mut rec));
        assert_eq!(attribute_list_records(&rec, 10), [99]);
    }
}
