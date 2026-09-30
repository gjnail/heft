//! Freeing the space duplicates take without deleting any of them. Each extra
//! copy is replaced by a copy-on-write clone of the kept one where the file
//! system supports that (ReFS and Dev Drives, APFS, Btrfs, XFS), or by a hard
//! link elsewhere (NTFS, ext4).
//!
//! Both copies are compared byte for byte right before the swap, the new file
//! is made under a temporary name next to the duplicate, and only then renamed
//! over it. If anything goes wrong on the way the duplicate is left as it was.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    /// Still two separate files that happen to share storage until one of
    /// them changes.
    Clone,
    /// Two names for one file: a change made through either shows in both.
    HardLink,
}

#[derive(Default)]
pub struct Progress {
    pub bytes: AtomicU64,
    pub cancel: AtomicBool,
}

/// How files in `dir` can share storage, if at all.
pub fn method_for(dir: &Path) -> Option<Method> {
    sys::method_for(dir)
}

/// A cheap key for the volume `path` is on.
pub fn volume(path: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        crate::platform::volume_root(path).map(|r| r.to_lowercase())
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::symlink_metadata(path).ok().map(|m| m.dev().to_string())
    }
}

/// An identifier for the volume `path` is on and one for the file itself.
/// Files can only share storage with files on the same volume.
pub fn identity(path: &Path) -> Option<(u64, u64)> {
    crate::platform::file_identity(&File::open(path).ok()?)
}

/// Replace `dup` with a clone of, or a hard link to, `keep`.
pub fn share(keep: &Path, dup: &Path, method: Method, p: &Progress) -> Result<(), String> {
    let (km, dm) = (meta(keep)?, meta(dup)?);
    if !km.is_file() || !dm.is_file() {
        return Err("not a regular file".into());
    }
    if km.len() != dm.len() {
        return Err("changed since the search".into());
    }
    if dm.permissions().readonly() {
        return Err("read-only".into());
    }
    match (identity(keep), identity(dup)) {
        (Some(a), Some(b)) if a == b => return Err("already the same file".into()),
        (Some(a), Some(b)) if a.0 != b.0 => return Err("on a different drive than the kept copy".into()),
        _ => {}
    }
    if !same_contents(keep, dup, p)? {
        return Err("the contents differ now, so one of them changed since the search".into());
    }

    let tmp = temp_name(dup);
    match method {
        Method::HardLink => fs::hard_link(keep, &tmp),
        Method::Clone => sys::clone_file(keep, &tmp),
    }
    .map_err(|e| describe(&e))?;

    let finish = || -> Result<(), String> {
        if method == Method::Clone {
            // Paranoia: a clone that doesn't match would lose data.
            if !same_contents(keep, &tmp, p)? {
                return Err("the clone didn't match the original".into());
            }
            // The clone should look like the file it replaces.
            let _ = fs::set_permissions(&tmp, dm.permissions());
            if let Ok(t) = dm.modified() {
                let _ = File::options().write(true).open(&tmp).and_then(|f| f.set_modified(t));
            }
        }
        fs::rename(&tmp, dup).map_err(|e| describe(&e))
    };
    let r = finish();
    if r.is_err() {
        // Only the temporary name goes; for a hard link that leaves the kept
        // file alone.
        let _ = fs::remove_file(&tmp);
    }
    r
}

fn meta(path: &Path) -> Result<fs::Metadata, String> {
    fs::symlink_metadata(path).map_err(|e| describe(&e))
}

fn describe(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::PermissionDenied => "access denied, or it's open in another program".into(),
        io::ErrorKind::NotFound => "no longer there".into(),
        _ => e.to_string(),
    }
}

fn temp_name(dup: &Path) -> PathBuf {
    let name = dup.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    dup.with_file_name(format!(".{name}.heft-{}", std::process::id()))
}

/// Byte-for-byte comparison.
fn same_contents(a: &Path, b: &Path, p: &Progress) -> Result<bool, String> {
    let open = |x: &Path| File::open(x).map_err(|e| describe(&e));
    let (mut fa, mut fb) = (open(a)?, open(b)?);
    let mut ba = vec![0u8; 1 << 20];
    let mut bb = vec![0u8; 1 << 20];
    loop {
        if p.cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let n = read_full(&mut fa, &mut ba).map_err(|e| describe(&e))?;
        let m = read_full(&mut fb, &mut bb[..n.max(1)]).map_err(|e| describe(&e))?;
        if n != m || ba[..n] != bb[..n] {
            return Ok(false);
        }
        p.bytes.fetch_add(2 * n as u64, Ordering::Relaxed);
        if n == 0 {
            return Ok(true);
        }
    }
}

fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

#[cfg(windows)]
mod sys {
    use std::fs::File;
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem as wfs;
    use windows_sys::Win32::System::IO::DeviceIoControl;

    use super::Method;
    use crate::platform::wide;

    const FILE_SUPPORTS_HARD_LINKS: u32 = 0x0040_0000;
    const FILE_SUPPORTS_BLOCK_REFCOUNTING: u32 = 0x0800_0000;
    const FSCTL_DUPLICATE_EXTENTS_TO_FILE: u32 = 0x0009_8344;
    const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;
    const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;

    fn volume_root(path: &Path) -> Option<Vec<u16>> {
        crate::platform::volume_root(path).map(wide)
    }

    pub fn method_for(dir: &Path) -> Option<Method> {
        let root = volume_root(dir)?;
        let mut flags = 0u32;
        let ok = unsafe {
            wfs::GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut flags,
                std::ptr::null_mut(),
                0,
            )
        };
        if ok == 0 {
            None
        } else if flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0 {
            Some(Method::Clone)
        } else if flags & FILE_SUPPORTS_HARD_LINKS != 0 {
            Some(Method::HardLink)
        } else {
            None
        }
    }

    fn cluster_size(path: &Path) -> io::Result<u64> {
        let root = volume_root(path).ok_or_else(io::Error::last_os_error)?;
        let (mut spc, mut bps, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
        let ok = unsafe { wfs::GetDiskFreeSpaceW(root.as_ptr(), &mut spc, &mut bps, &mut free, &mut total) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((spc as u64 * bps as u64).max(512))
    }

    /// Mirrors `DUPLICATE_EXTENTS_DATA`.
    #[repr(C)]
    struct DuplicateExtents {
        file: *mut std::ffi::c_void,
        source_offset: i64,
        target_offset: i64,
        byte_count: i64,
    }

    fn ioctl(f: &File, code: u32, input: *const std::ffi::c_void, len: u32) -> io::Result<()> {
        let mut ret = 0u32;
        let ok = unsafe {
            DeviceIoControl(f.as_raw_handle() as _, code, input, len, std::ptr::null_mut(), 0, &mut ret, std::ptr::null_mut())
        };
        if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Block cloning on ReFS: make an empty file of the same size, then point
    /// its clusters at the source's.
    pub fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
        use std::os::windows::fs::MetadataExt;
        let s = File::open(src)?;
        let d = File::options().read(true).write(true).create_new(true).open(dst)?;
        let run = || -> io::Result<()> {
            let m = s.metadata()?;
            if m.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
                ioctl(&d, FSCTL_SET_SPARSE, std::ptr::null(), 0)?;
            }
            d.set_len(m.len())?;
            // Regions have to be whole clusters; the last one may run past
            // the end of the file.
            let cluster = cluster_size(dst)?;
            let total = m.len().div_ceil(cluster) * cluster;
            let mut off = 0u64;
            while off < total {
                let n = (total - off).min(1 << 30);
                let req = DuplicateExtents {
                    file: s.as_raw_handle() as _,
                    source_offset: off as i64,
                    target_offset: off as i64,
                    byte_count: n as i64,
                };
                ioctl(&d, FSCTL_DUPLICATE_EXTENTS_TO_FILE, (&req as *const DuplicateExtents).cast(), size_of::<DuplicateExtents>() as u32)?;
                off += n;
            }
            Ok(())
        };
        let r = run();
        drop(d);
        if r.is_err() {
            let _ = std::fs::remove_file(dst);
        }
        r
    }
}

#[cfg(target_os = "macos")]
mod sys {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::Method;

    fn cstr(p: &Path) -> io::Result<CString> {
        CString::new(p.as_os_str().as_bytes()).map_err(|_| io::ErrorKind::InvalidInput.into())
    }

    pub fn method_for(dir: &Path) -> Option<Method> {
        let c = cstr(dir).ok()?;
        let mut s: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        let name: Vec<u8> = s.f_fstypename.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
        match name.as_slice() {
            b"apfs" => Some(Method::Clone),
            b"hfs" => Some(Method::HardLink),
            _ => None,
        }
    }

    pub fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
        let (s, d) = (cstr(src)?, cstr(dst)?);
        if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod sys {
    use std::ffi::CString;
    use std::fs::File;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    use super::Method;

    /// `_IOW(0x94, 9, int)`
    const FICLONE: u64 = 0x4004_9409;

    pub fn method_for(dir: &Path) -> Option<Method> {
        let c = CString::new(dir.as_os_str().as_bytes()).ok()?;
        let mut s: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        match s.f_type as u64 {
            // btrfs, xfs, bcachefs
            0x9123_683E | 0x5846_5342 | 0xCA45_1A4E => Some(Method::Clone),
            // ext2/3/4, f2fs, zfs, tmpfs, jfs, reiserfs
            0xEF53 | 0xF2F5_2010 | 0x2FC1_2FC1 | 0x0102_1994 | 0x3153_464A | 0x5265_4973 => Some(Method::HardLink),
            // Network and FUSE file systems, FAT and anything else: leave alone.
            _ => None,
        }
    }

    pub fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
        let s = File::open(src)?;
        let d = File::options().write(true).create_new(true).open(dst)?;
        if unsafe { libc::ioctl(d.as_raw_fd(), FICLONE as _, s.as_raw_fd()) } != 0 {
            let e = io::Error::last_os_error();
            drop(d);
            let _ = std::fs::remove_file(dst);
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("heft-dedupe-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn hard_link_replaces_identical_copy() {
        let d = scratch("link");
        let (a, b) = (d.join("a.bin"), d.join("b.bin"));
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&a, &data).unwrap();
        fs::write(&b, &data).unwrap();
        let Some(_) = method_for(&d) else { return }; // temp on an unusual file system
        share(&a, &b, Method::HardLink, &Progress::default()).unwrap();
        assert_eq!(identity(&a), identity(&b));
        assert_eq!(fs::read(&b).unwrap(), data);
        // Nothing left over, and a second pass sees they're already one file.
        assert_eq!(fs::read_dir(&d).unwrap().count(), 2);
        assert!(share(&a, &b, Method::HardLink, &Progress::default()).is_err());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn leaves_files_alone_when_contents_differ() {
        let d = scratch("differ");
        let (a, b) = (d.join("a.bin"), d.join("b.bin"));
        fs::write(&a, vec![1u8; 70_000]).unwrap();
        let mut other = vec![1u8; 70_000];
        other[69_999] = 2;
        fs::write(&b, &other).unwrap();
        let err = share(&a, &b, Method::HardLink, &Progress::default()).unwrap_err();
        assert!(err.contains("differ"), "{err}");
        assert_eq!(fs::read(&b).unwrap(), other);
        assert_ne!(identity(&a), identity(&b));
        assert_eq!(fs::read_dir(&d).unwrap().count(), 2);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn clone_where_supported() {
        let d = scratch("clone");
        if method_for(&d) != Some(Method::Clone) {
            return; // needs APFS, ReFS, Btrfs or XFS
        }
        let (a, b) = (d.join("a.bin"), d.join("b.bin"));
        let data: Vec<u8> = (0..5_000_000u32).map(|i| (i * 7 % 256) as u8).collect();
        fs::write(&a, &data).unwrap();
        fs::write(&b, &data).unwrap();
        share(&a, &b, Method::Clone, &Progress::default()).unwrap();
        assert_ne!(identity(&a), identity(&b), "a clone is its own file");
        assert_eq!(fs::read(&b).unwrap(), data);
        // Changing one leaves the other as it was.
        fs::write(&a, b"changed").unwrap();
        assert_eq!(fs::read(&b).unwrap(), data);
        fs::remove_dir_all(&d).unwrap();
    }
}
