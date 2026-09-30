//! Directory listing with `getattrlistbulk` (macOS). One system call returns
//! the names and attributes of many entries at once, where `std::fs` needs a
//! `readdir` plus an `lstat` for every entry.
//!
//! Every entry comes back in the same fixed layout because of
//! `FSOPT_PACK_INVAL_ATTRS`: attributes that don't apply (file sizes for a
//! folder, say) are packed as zeros instead of left out. If the call fails
//! for a directory, that directory is listed through `std::fs` instead.

use std::ffi::{CString, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::Ordering;

use super::portable::StdLister;
use super::{Entry, Lister, Progress};
use crate::tree::flags;

const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const UF_HIDDEN: u32 = 0x0000_8000;
const SF_DATALESS: u32 = 0x4000_0000;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

/// Byte offsets of each attribute within an entry, in the order the kernel
/// packs them (common attributes by bit, then file attributes by bit).
mod at {
    pub const LENGTH: usize = 0;
    // attribute_set_t (20 bytes) at 4
    pub const ERROR: usize = 24;
    pub const NAME: usize = 28; // attrreference_t: offset from here, length
    pub const DEVID: usize = 36;
    pub const OBJTYPE: usize = 40;
    pub const MODTIME: usize = 44; // timespec: seconds, nanoseconds
    pub const FLAGS: usize = 60;
    pub const FILEID: usize = 64;
    pub const LINKCOUNT: usize = 72;
    pub const ALLOCSIZE: usize = 76;
    pub const DATALENGTH: usize = 84;
    pub const END: usize = 92;
}

pub(in crate::scan) struct BulkLister;

impl Lister for BulkLister {
    type Scratch = Vec<u8>;

    fn threads(&self) -> usize {
        StdLister.threads()
    }

    fn list(&self, path: &Path, buf: &mut Vec<u8>, progress: &Progress) -> Option<Vec<Entry>> {
        match list_bulk(path, buf) {
            Ok((entries, files, bytes)) => {
                progress.files.fetch_add(files, Ordering::Relaxed);
                progress.bytes.fetch_add(bytes, Ordering::Relaxed);
                Some(entries)
            }
            Err(_) => StdLister.list(path, &mut (), progress),
        }
    }
}

struct Fd(libc::c_int);

impl Drop for Fd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

fn list_bulk(path: &Path, buf: &mut Vec<u8>) -> io::Result<(Vec<Entry>, u64, u64)> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = Fd(fd);
    let mut al: libc::attrlist = unsafe { std::mem::zeroed() };
    al.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    al.commonattr = libc::ATTR_CMN_RETURNED_ATTRS
        | ATTR_CMN_ERROR
        | libc::ATTR_CMN_NAME
        | libc::ATTR_CMN_DEVID
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_MODTIME
        | libc::ATTR_CMN_FLAGS
        | libc::ATTR_CMN_FILEID;
    al.fileattr = libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE | libc::ATTR_FILE_DATALENGTH;
    if buf.len() < 256 * 1024 {
        buf.resize(256 * 1024, 0);
    }

    let mut out = Vec::new();
    let (mut files, mut bytes) = (0u64, 0u64);
    loop {
        let n = unsafe {
            libc::getattrlistbulk(
                fd.0,
                (&mut al as *mut libc::attrlist).cast(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::FSOPT_PACK_INVAL_ATTRS as u64,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            break;
        }
        let mut off = 0usize;
        for _ in 0..n {
            let rest = buf.get(off..).unwrap_or(&[]);
            let len = u32_at(rest, at::LENGTH) as usize;
            if len < at::END || len > rest.len() {
                return Err(io::Error::other("malformed getattrlistbulk entry"));
            }
            if let Some((e, is_file)) = parse(&rest[..len]) {
                if is_file {
                    files += 1;
                    bytes += e.size;
                }
                out.push(e);
            }
            off += len;
        }
    }
    Ok((out, files, bytes))
}

/// One packed entry, or `None` if the kernel couldn't read its attributes.
fn parse(e: &[u8]) -> Option<(Entry, bool)> {
    if u32_at(e, at::ERROR) != 0 {
        return None;
    }
    let name_off = at::NAME.checked_add_signed(i32::from_ne_bytes(e[at::NAME..at::NAME + 4].try_into().ok()?) as isize)?;
    let name_len = u32_at(e, at::NAME + 4) as usize;
    let raw = e.get(name_off..name_off + name_len)?;
    let raw = raw.split(|&b| b == 0).next().unwrap_or(raw);
    let os_name = OsStr::from_bytes(raw).to_os_string();
    let name = os_name.to_string_lossy().into_owned();

    let objtype = u32_at(e, at::OBJTYPE);
    let bsd_flags = u32_at(e, at::FLAGS);
    let is_link = objtype == VLNK;
    let is_dir = objtype == VDIR;
    let mut fl = 0u16;
    if is_dir {
        fl |= flags::DIR;
    }
    if is_link {
        fl |= flags::LINK;
    }
    if bsd_flags & UF_HIDDEN != 0 || name.starts_with('.') {
        fl |= flags::HIDDEN;
    }
    let is_file = !is_dir && !is_link;
    let (size, alloc) = if is_file {
        let size = u64_at(e, at::DATALENGTH);
        let alloc = if bsd_flags & SF_DATALESS != 0 {
            fl |= flags::CLOUD;
            0
        } else {
            u64_at(e, at::ALLOCSIZE)
        };
        (size, alloc)
    } else {
        (0, 0)
    };
    // Same identity as `StdLister`: (st_dev, st_ino), for folders and for
    // files with more than one name.
    let dev = i32::from_ne_bytes(e[at::DEVID..at::DEVID + 4].try_into().ok()?) as u64;
    let links = u32_at(e, at::LINKCOUNT);
    let file_id = if is_dir || links > 1 { ((dev as u128) << 64) | u64_at(e, at::FILEID) as u128 } else { 0 };
    let entry = Entry {
        os_name: is_dir.then_some(os_name),
        name,
        flags: fl,
        size,
        alloc,
        mtime: u64_at(e, at::MODTIME) as i64,
        file_id,
    };
    Some((entry, is_file))
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map(|s| u32::from_ne_bytes(s.try_into().unwrap())).unwrap_or(0)
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8).map(|s| u64::from_ne_bytes(s.try_into().unwrap())).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing<L: Lister>(l: &L, dir: &str) -> Vec<(String, u16, u64, u64, i64, u128)> {
        let mut v: Vec<_> = l
            .list(Path::new(dir), &mut L::Scratch::default(), &Progress::default())
            .unwrap_or_default()
            .into_iter()
            .map(|e| (e.name, e.flags, e.size, e.alloc, e.mtime, e.file_id))
            .collect();
        v.sort();
        v
    }

    /// Same answers as the plain `std::fs` lister, on folders that have a bit
    /// of everything: compressed system binaries, apps, links.
    #[test]
    fn matches_std_lister() {
        for dir in ["/usr/bin", "/usr/lib", "/System/Library/CoreServices", "/Applications", "/private/etc"] {
            if !Path::new(dir).is_dir() {
                continue;
            }
            // The bulk call itself has to work, not just the fallback.
            if let Err(e) = list_bulk(Path::new(dir), &mut Vec::new()) {
                panic!("{dir}: getattrlistbulk failed: {e}");
            }
            let (bulk, std) = (listing(&BulkLister, dir), listing(&StdLister, dir));
            assert!(!bulk.is_empty(), "{dir}: nothing listed");
            assert_eq!(bulk.len(), std.len(), "{dir}: entry count");
            for (a, b) in bulk.iter().zip(&std) {
                assert_eq!(a, b, "{dir}: bulk vs std");
            }
        }
    }
}
