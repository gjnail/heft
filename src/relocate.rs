//! Moving a big folder to another drive and leaving a link behind, so
//! programs that use the old location keep working.
//!
//! The folder is copied, every copied file is read back and checked against
//! what was read from the original, and only then is the original renamed
//! out of the way and a link put in its place: a junction on Windows, a
//! symbolic link elsewhere. The original is handed back to the caller, which
//! sends it to the Recycle Bin. Until the link is in place the original is
//! never touched, and a failed or cancelled move removes the partial copy.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use xxhash_rust::xxh3::Xxh3;

#[derive(Default)]
pub struct Progress {
    pub phase: Mutex<&'static str>,
    pub bytes: AtomicU64,
    pub cancel: AtomicBool,
}

impl Progress {
    fn set_phase(&self, s: &'static str) {
        *self.phase.lock().unwrap() = s;
        self.bytes.store(0, Ordering::Relaxed);
    }
}

/// Move `src` into `dest_parent` and leave a link at `src`. Returns where
/// the original now is, renamed next to the link, for the caller to dispose
/// of.
pub fn relocate(src: &Path, dest_parent: &Path, p: &Progress) -> Result<PathBuf, String> {
    let name = src.file_name().ok_or("can't move the top of a drive")?;
    let meta = fs::symlink_metadata(src).map_err(|e| e.to_string())?;
    if !meta.is_dir() {
        return Err("only folders can be moved this way".into());
    }
    let dst = dest_parent.join(name);
    if fs::symlink_metadata(&dst).is_ok() {
        return Err(format!("{} already exists", dst.display()));
    }

    p.set_phase("Copying");
    let mut copied = Vec::new();
    let r = copy_dir(src, &dst, Path::new(""), &mut copied, p);
    let fail = |e: String| {
        let _ = fs::remove_dir_all(&dst);
        e
    };
    r.map_err(fail)?;

    p.set_phase("Checking the copy");
    for (rel, hash) in &copied {
        match hash_file(&dst.join(rel), p) {
            Ok(h) if h == *hash => {}
            Ok(_) => return Err(fail(format!("the copy of {} doesn't match the original", rel.display()))),
            Err(e) => return Err(fail(e)),
        }
    }

    p.set_phase("Swapping in the link");
    let old = src.with_file_name(format!("{}.heft-old-{}", name.to_string_lossy(), std::process::id()));
    fs::rename(src, &old).map_err(|e| fail(format!("couldn't move the original out of the way, so something may be using it ({e})")))?;
    if let Err(e) = make_link(src, &dst) {
        let _ = fs::rename(&old, src);
        return Err(fail(format!("couldn't create the link ({e})")));
    }
    Ok(old)
}

fn copy_dir(src: &Path, dst: &Path, rel: &Path, out: &mut Vec<(PathBuf, u128)>, p: &Progress) -> Result<(), String> {
    let err = |path: &Path, e: io::Error| format!("{}: {e}", path.display());
    fs::create_dir(dst).map_err(|e| err(dst, e))?;
    for entry in fs::read_dir(src).map_err(|e| err(src, e))? {
        if p.cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let entry = entry.map_err(|e| err(src, e))?;
        let (s, d, r) = (entry.path(), dst.join(entry.file_name()), rel.join(entry.file_name()));
        let m = fs::symlink_metadata(&s).map_err(|e| err(&s, e))?;
        if m.file_type().is_symlink() {
            return Err(format!("{} is a link, and Heft doesn't move folders that contain links", s.display()));
        }
        if sys::online_only(&m) {
            return Err(format!("{} is an online-only file; moving it would download it", s.display()));
        }
        if m.is_dir() {
            copy_dir(&s, &d, &r, out, p)?;
        } else {
            let h = copy_file(&s, &d, p).map_err(|e| err(&s, e))?;
            out.push((r, h));
        }
        sys::copy_attributes(&m, &d);
    }
    Ok(())
}

/// Copy one file, returning the hash of what was read.
fn copy_file(src: &Path, dst: &Path, p: &Progress) -> io::Result<u128> {
    let mut from = File::open(src)?;
    let mut to = File::options().write(true).create_new(true).open(dst)?;
    let mut h = Xxh3::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if p.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other("cancelled"));
        }
        let n = from.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        to.write_all(&buf[..n])?;
        p.bytes.fetch_add(n as u64, Ordering::Relaxed);
    }
    to.sync_all()?;
    if let Ok(t) = from.metadata().and_then(|m| m.modified()) {
        let _ = to.set_modified(t);
    }
    Ok(h.digest128())
}

fn hash_file(path: &Path, p: &Progress) -> Result<u128, String> {
    let mut f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut h = Xxh3::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if p.cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let n = f.read(&mut buf).map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            return Ok(h.digest128());
        }
        h.update(&buf[..n]);
        p.bytes.fetch_add(n as u64, Ordering::Relaxed);
    }
}

fn make_link(link: &Path, target: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        sys::junction(link, target)
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
}

#[cfg(windows)]
mod sys {
    use std::fs::{self, File};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    /// Offline, recall on open, recall on data access.
    const ONLINE_ONLY: u32 = 0x1000 | 0x4_0000 | 0x40_0000;
    /// Read-only, hidden, system, archive, not content indexed.
    const KEEP: u32 = 0x1 | 0x2 | 0x4 | 0x20 | 0x2000;

    pub fn online_only(m: &fs::Metadata) -> bool {
        m.file_attributes() & ONLINE_ONLY != 0
    }

    pub fn copy_attributes(m: &fs::Metadata, dst: &Path) {
        let w = crate::platform::wide(dst.as_os_str());
        unsafe {
            windows_sys::Win32::Storage::FileSystem::SetFileAttributesW(w.as_ptr(), m.file_attributes() & KEEP);
        }
    }

    /// A directory junction at `link` pointing to `target`. Unlike symbolic
    /// links, junctions don't need administrator rights.
    pub fn junction(link: &Path, target: &Path) -> io::Result<()> {
        fs::create_dir(link)?;
        let make = || -> io::Result<()> {
            let dir = File::options()
                .write(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
                .open(link)?;
            let print: Vec<u16> = target.as_os_str().encode_wide().collect();
            let subst: Vec<u16> = r"\??\".encode_utf16().chain(print.iter().copied()).collect();
            let names = (subst.len() + print.len() + 2) * 2;
            let mut buf: Vec<u8> = Vec::with_capacity(16 + names);
            buf.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
            buf.extend(((8 + names) as u16).to_le_bytes());
            buf.extend(0u16.to_le_bytes());
            buf.extend(0u16.to_le_bytes());
            buf.extend(((subst.len() * 2) as u16).to_le_bytes());
            buf.extend((((subst.len() + 1) * 2) as u16).to_le_bytes());
            buf.extend(((print.len() * 2) as u16).to_le_bytes());
            for c in subst.iter().chain(&[0]).chain(print.iter()).chain(&[0]) {
                buf.extend(c.to_le_bytes());
            }
            let mut ret = 0u32;
            let ok = unsafe {
                windows_sys::Win32::System::IO::DeviceIoControl(
                    dir.as_raw_handle() as _,
                    FSCTL_SET_REPARSE_POINT,
                    buf.as_ptr().cast(),
                    buf.len() as u32,
                    std::ptr::null_mut(),
                    0,
                    &mut ret,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
        };
        let r = make();
        if r.is_err() {
            let _ = fs::remove_dir(link);
        }
        r
    }
}

#[cfg(unix)]
mod sys {
    use std::fs;
    use std::path::Path;

    pub fn online_only(_m: &fs::Metadata) -> bool {
        false
    }

    pub fn copy_attributes(m: &fs::Metadata, dst: &Path) {
        let _ = fs::set_permissions(dst, m.permissions());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_and_links() {
        let base = std::env::temp_dir().join(format!("heft-relocate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (src, other) = (base.join("here").join("Big Folder"), base.join("there"));
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::write(src.join("a.txt"), "hello").unwrap();
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 253) as u8).collect();
        fs::write(src.join("sub").join("b.bin"), &big).unwrap();
        fs::create_dir(src.join("empty")).unwrap();

        // Something already in the way: nothing happens.
        fs::create_dir(other.join("Big Folder")).unwrap();
        assert!(relocate(&src, &other, &Progress::default()).unwrap_err().contains("already exists"));
        fs::remove_dir(other.join("Big Folder")).unwrap();

        let old = relocate(&src, &other, &Progress::default()).unwrap();
        assert!(fs::symlink_metadata(&src).unwrap().file_type().is_symlink());
        // Through the link and at the new place, the same files.
        assert_eq!(fs::read_to_string(src.join("a.txt")).unwrap(), "hello");
        assert_eq!(fs::read(other.join("Big Folder").join("sub").join("b.bin")).unwrap(), big);
        assert!(other.join("Big Folder").join("empty").is_dir());
        // The original is intact under its new name.
        assert_eq!(fs::read(old.join("sub").join("b.bin")).unwrap(), big);

        #[cfg(windows)]
        fs::remove_dir(&src).unwrap();
        #[cfg(unix)]
        fs::remove_file(&src).unwrap();
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn refuses_folders_with_links() {
        let base = std::env::temp_dir().join(format!("heft-relocate-links-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (src, other) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(src.join("real")).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::write(src.join("real").join("f"), "x").unwrap();
        if make_link(&src.join("link"), &src.join("real")).is_err() {
            return;
        }
        let err = relocate(&src, &other, &Progress::default()).unwrap_err();
        assert!(err.contains("link"), "{err}");
        assert!(!other.join("src").exists(), "partial copy removed");
        assert!(src.join("real").join("f").exists(), "original untouched");
        fs::remove_dir_all(&base).unwrap();
    }
}
