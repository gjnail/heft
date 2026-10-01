//! Transparent compression for folders you rarely change: NTFS on Windows,
//! APFS and HFS+ on macOS.
//!
//! On Windows this is the compression `compact /EXE` uses; on macOS it's the
//! file system compression Apple uses for its own files. Either way files
//! stay where they are and open as usual, and the system unpacks them as
//! they're read. A file that is written to later is quietly stored
//! uncompressed again, so it suits programs, games and old projects rather
//! than things you edit.

use std::path::Path;

#[cfg(target_os = "macos")]
pub use mac::{compress, decompress, needs_admin, rewrite_as_admin, rewrite_listed, InUse};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Level {
    /// XPRESS 8K: quick to read back.
    #[default]
    Fast,
    /// LZX: smaller, slower to read back.
    #[cfg_attr(not(windows), allow(dead_code))]
    Small,
}

/// Whether the compression level can be chosen. macOS picks the algorithm
/// itself.
pub const HAS_LEVELS: bool = cfg!(windows);

/// Whether files on the volume holding `path` can be compressed this way.
pub fn supported(path: &Path) -> bool {
    #[cfg(windows)]
    {
        crate::platform::volume_root(path)
            .and_then(|r| crate::platform::volume_info(&r))
            .is_some_and(|(_, fs)| fs.eq_ignore_ascii_case("NTFS"))
    }
    #[cfg(target_os = "macos")]
    {
        mac::supported(path)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = path;
        false
    }
}

/// Formats that are compressed already, and files that change too often for
/// compression to stick. Not worth the time.
const SKIP: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "avif", "jxl", "mp3", "aac", "m4a", "ogg", "opus", "flac",
    "wma", "mp4", "m4v", "mkv", "webm", "avi", "mov", "wmv", "flv", "zip", "7z", "rar", "gz", "tgz", "bz2", "xz",
    "zst", "lz4", "lzma", "cab", "msi", "msu", "msix", "appx", "docx", "xlsx", "pptx", "odt", "ods", "odp", "epub",
    "jar", "apk", "pdf", "vhd", "vhdx", "vmdk", "qcow2", "vdi", "db", "sqlite", "ldb", "log", "pst", "ost", "dmg",
    "pkg", "xip", "ipsw",
];

/// Smaller than this and there's nothing to gain.
pub const MIN_SIZE: u64 = 8 * 1024;

pub fn worth_trying(ext: &str, size: u64) -> bool {
    size >= MIN_SIZE && !SKIP.iter().any(|s| s.eq_ignore_ascii_case(ext))
}

/// Compress one file. `Ok(Some(bytes))` is its new size on disk; `Ok(None)`
/// means Windows found it wouldn't shrink and left it alone.
#[cfg(windows)]
pub fn compress(path: &Path, level: Level) -> Result<Option<u64>, String> {
    #[repr(C)]
    struct Request {
        wof_version: u32,
        wof_provider: u32,
        file_version: u32,
        algorithm: u32,
        flags: u32,
    }
    const FSCTL_SET_EXTERNAL_BACKING: u32 = 0x0009_030C;
    const WOF_PROVIDER_FILE: u32 = 2;
    const ERROR_COMPRESSION_NOT_BENEFICIAL: i32 = 344;

    let f = open(path)?;
    let before = f.metadata().ok();
    let req = Request {
        wof_version: 1,
        wof_provider: WOF_PROVIDER_FILE,
        file_version: 1,
        algorithm: match level {
            Level::Fast => 2, // XPRESS8K
            Level::Small => 1, // LZX
        },
        flags: 0,
    };
    match ioctl(&f, FSCTL_SET_EXTERNAL_BACKING, (&req as *const Request).cast(), size_of::<Request>() as u32) {
        Err(e) if e.raw_os_error() == Some(ERROR_COMPRESSION_NOT_BENEFICIAL) => return Ok(None),
        Err(e) => return Err(describe(&e)),
        Ok(()) => {}
    }
    keep_times(&f, before);
    drop(f);
    Ok(Some(crate::platform::compressed_size(path).unwrap_or(0)))
}

/// Undo [`compress`]. Returns the file's size on disk afterwards.
#[cfg(windows)]
pub fn decompress(path: &Path) -> Result<u64, String> {
    const FSCTL_DELETE_EXTERNAL_BACKING: u32 = 0x0009_0314;
    const ERROR_OBJECT_NOT_EXTERNALLY_BACKED: i32 = 342;
    let f = open(path)?;
    let before = f.metadata().ok();
    match ioctl(&f, FSCTL_DELETE_EXTERNAL_BACKING, std::ptr::null(), 0) {
        Err(e) if e.raw_os_error() == Some(ERROR_OBJECT_NOT_EXTERNALLY_BACKED) => {}
        Err(e) => return Err(describe(&e)),
        Ok(()) => keep_times(&f, before),
    }
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    drop(f);
    Ok(crate::platform::compressed_size(path).unwrap_or(len))
}

#[cfg(windows)]
fn open(path: &Path) -> Result<std::fs::File, String> {
    std::fs::File::options().read(true).write(true).open(path).map_err(|e| describe(&e))
}

#[cfg(windows)]
fn describe(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(5) => "read-only or access denied".into(),
        Some(32) | Some(33) => "in use by another program".into(),
        Some(112) => "not enough free space".into(),
        _ => e.to_string(),
    }
}

/// Compression isn't a change to the contents; don't let it look like one.
#[cfg(windows)]
fn keep_times(f: &std::fs::File, before: Option<std::fs::Metadata>) {
    let Some(before) = before else { return };
    let after = f.metadata().ok();
    if let (Ok(m), Some(after)) = (before.modified(), after)
        && after.modified().ok() != Some(m)
    {
        let _ = f.set_modified(m);
    }
}

#[cfg(windows)]
fn ioctl(f: &std::fs::File, code: u32, input: *const std::ffi::c_void, len: u32) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    let mut ret = 0u32;
    let ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            f.as_raw_handle() as _,
            code,
            input,
            len,
            std::ptr::null_mut(),
            0,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn compress(_path: &Path, _level: Level) -> Result<Option<u64>, String> {
    Err("not supported on this system".into())
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn decompress(_path: &Path) -> Result<u64, String> {
    Err("not supported on this system".into())
}

/// macOS keeps no public call for compressing a file in place, so each file
/// is rewritten: `ditto --hfsCompression` (Apple's own copier, which keeps
/// permissions, ACLs, extended attributes and dates) makes a compressed copy
/// next to it, Heft checks the copy byte for byte and attribute by attribute
/// against the original, and only then renames it over the original. If
/// anything fails or changes on the way, the copy is removed and the
/// original is left as it was. Uncompressing works the same way.
///
/// Renaming gives the file a new identity, so files with more than one name
/// (hard links) are skipped, as are files that belong to someone else, locked
/// files, and anything a running program has open or is part of an app
/// that's running.
#[cfg(target_os = "macos")]
mod mac {
    use std::collections::HashSet;
    use std::ffi::{c_char, c_int, c_void, CStr, CString};
    use std::fs::Metadata;
    use std::io::Read;
    use std::os::macos::fs::MetadataExt as _;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::Level;

    const SF_DATALESS: u32 = 0x4000_0000;
    const SF_RESTRICTED: u32 = 0x0008_0000;
    const LOCKED: u32 = libc::UF_IMMUTABLE | libc::SF_IMMUTABLE | libc::UF_APPEND | libc::SF_APPEND;
    /// The startup disk's Data volume, where everything you can change under
    /// `/` lives (the system volume is sealed).
    const DATA: &str = "/System/Volumes/Data";
    /// Free space to leave when uncompressing.
    const HEADROOM: u64 = 1 << 30;

    pub fn supported(path: &Path) -> bool {
        let Some(s) = statfs(path) else { return false };
        let fs = c_text(&s.f_fstypename);
        if s.f_flags & libc::MNT_RDONLY as u32 == 0 {
            return fs == "apfs" || fs == "hfs";
        }
        fs == "apfs" && path == Path::new("/") && supported(Path::new(DATA))
    }

    fn statfs(path: &Path) -> Option<libc::statfs> {
        let c = cpath(path)?;
        let mut s: libc::statfs = unsafe { std::mem::zeroed() };
        (unsafe { libc::statfs(c.as_ptr(), &mut s) } == 0).then_some(s)
    }

    fn c_text(chars: &[c_char]) -> String {
        let bytes: Vec<u8> = chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn cpath(path: &Path) -> Option<CString> {
        CString::new(path.as_os_str().as_bytes()).ok()
    }

    /// Compress one file. `Ok(Some(bytes))` is its new size on disk; `Ok(None)`
    /// means macOS found it wouldn't shrink and it was left alone.
    pub fn compress(path: &Path, _level: Level) -> Result<Option<u64>, String> {
        rewrite(path, true)
    }

    /// Undo [`compress`]. Returns the file's size on disk afterwards.
    pub fn decompress(path: &Path) -> Result<u64, String> {
        rewrite(path, false).map(|alloc| alloc.unwrap_or(0))
    }

    fn rewrite(path: &Path, compress: bool) -> Result<Option<u64>, String> {
        let before = std::fs::symlink_metadata(path).map_err(|e| describe(&e))?;
        let compressed = before.file_type().is_file() && before.st_flags() & libc::UF_COMPRESSED != 0;
        if compressed == compress {
            return Ok(Some(before.blocks() * 512));
        }
        if let Some(why) = refuse(&before, unsafe { libc::getuid() }) {
            return Err(why.into());
        }
        let parent = path.parent().ok_or("not a file")?;
        if !writable(path) || !writable(parent) {
            return Err("read-only or access denied".into());
        }
        if !compress && crate::platform::free_space(&parent.to_string_lossy()).is_none_or(|(_, free)| free < before.size() + HEADROOM) {
            return Err("not enough free space".into());
        }
        if InUse::cached().blocks(path, &before) {
            return Err(IN_USE.into());
        }

        let tmp = temp_beside(path).ok_or("couldn't pick a temporary name")?;
        let folder = std::fs::symlink_metadata(parent).ok();
        let r = copy_and_swap(path, &tmp, &before, compress);
        if !matches!(r, Ok(Some(_))) {
            let _ = std::fs::remove_file(&tmp);
        }
        // The temporary copy and the swap count as changes to the folder;
        // they aren't.
        if let Some(f) = folder {
            keep_times(parent, &f);
        }
        match r {
            Err(e) if e == NOT_PERMITTED && app_bundle(path).is_some() => Err(APP_MANAGEMENT.into()),
            other => other,
        }
    }

    const IN_USE: &str = "in use by a program that's running";
    const NOT_PERMITTED: &str = "macOS didn't allow it";
    /// macOS stops apps from changing other developers' apps unless you allow it.
    const APP_MANAGEMENT: &str = "macOS didn't let Heft change this app. Allow Heft in System Settings \u{203a} Privacy & \
                                  Security \u{203a} App Management, then try again";

    /// Make the copy at `tmp`, check it and put it in place of `path`.
    fn copy_and_swap(path: &Path, tmp: &Path, before: &Metadata, compress: bool) -> Result<Option<u64>, String> {
        ditto(path, tmp, compress)?;
        // ditto sets the creation date to the modification date.
        set_birthtime(tmp, before.st_birthtime(), before.st_birthtime_nsec());
        let copy = std::fs::symlink_metadata(tmp).map_err(|e| describe(&e))?;
        let now_compressed = copy.st_flags() & libc::UF_COMPRESSED != 0;
        if compress && (!now_compressed || copy.blocks() >= before.blocks()) {
            return Ok(None);
        }
        if !compress && now_compressed {
            return Err("macOS kept it compressed".into());
        }
        if let Some(what) = differs(path, before, tmp, &copy) {
            return Err(format!("the copy's {what} didn't match, so it was left as it was"));
        }
        match same_bytes(path, tmp) {
            Ok(true) => {}
            Ok(false) => return Err("the copy didn't match the original, so it was left as it was".into()),
            Err(e) => return Err(describe(&e)),
        }

        // Last checks right before the swap: nothing has the file open, and
        // it hasn't changed since the copy was made.
        let now = std::fs::symlink_metadata(path).map_err(|e| describe(&e))?;
        if InUse::now().blocks(path, &now) {
            return Err(IN_USE.into());
        }
        if !unchanged(before, &now) {
            return Err("it changed while Heft was working on it".into());
        }
        std::fs::rename(tmp, path).map_err(|e| describe(&e))?;
        Ok(Some(copy.blocks() * 512))
    }

    /// Why Heft won't rewrite a file, if it won't.
    fn refuse(md: &Metadata, uid: u32) -> Option<&'static str> {
        let flags = md.st_flags();
        if !md.file_type().is_file() {
            Some("not a regular file")
        } else if md.nlink() > 1 {
            Some("it has more than one name (a hard link)")
        } else if flags & SF_DATALESS != 0 {
            Some("it's online-only")
        } else if flags & LOCKED != 0 {
            Some("it's locked")
        } else if flags & SF_RESTRICTED != 0 {
            Some("macOS protects it")
        } else if uid != 0 && md.uid() != uid {
            Some("it belongs to macOS or another user")
        } else {
            None
        }
    }

    fn writable(path: &Path) -> bool {
        cpath(path).is_some_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0)
    }

    /// A hidden, unused name in the same folder, so the swap is a rename.
    fn temp_beside(path: &Path) -> Option<PathBuf> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = path.parent()?;
        let pid = std::process::id();
        (0..100)
            .map(|_| dir.join(format!(".heft-compress-{pid}-{}", NEXT.fetch_add(1, Ordering::Relaxed))))
            .find(|p| std::fs::symlink_metadata(p).is_err())
    }

    fn ditto(src: &Path, dst: &Path, compress: bool) -> Result<(), String> {
        let mut cmd = std::process::Command::new("/usr/bin/ditto");
        if compress {
            cmd.arg("--hfsCompression");
        } else {
            cmd.args(["--nohfsCompression", "--nopreserveHFSCompression"]);
        }
        let out = cmd.arg(src).arg(dst).stdin(std::process::Stdio::null()).output().map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(ditto_error(&String::from_utf8_lossy(&out.stderr)))
        }
    }

    /// ditto prints `ditto: <path>: <reason>`; the path is Heft's temporary
    /// copy, so keep the reason.
    pub(super) fn ditto_error(stderr: &str) -> String {
        let line = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("ditto failed").trim();
        if line.contains("No space left") {
            return "not enough free space".into();
        }
        let reason = line.rsplit(": ").next().unwrap_or(line);
        if reason.contains("Permission denied") {
            "read-only or access denied".into()
        } else if reason.contains("Operation not permitted") {
            NOT_PERMITTED.into()
        } else {
            reason.to_string()
        }
    }

    fn describe(e: &std::io::Error) -> String {
        match e.raw_os_error() {
            Some(libc::EACCES) => "read-only or access denied".into(),
            Some(libc::EPERM) => NOT_PERMITTED.into(),
            Some(libc::EBUSY) | Some(libc::ETXTBSY) => IN_USE.into(),
            Some(libc::ENOSPC) => "not enough free space".into(),
            _ => e.to_string(),
        }
    }

    /// The attribute of the copy that doesn't match the original, if any.
    fn differs(orig: &Path, a: &Metadata, copy: &Path, b: &Metadata) -> Option<&'static str> {
        if a.size() != b.size() {
            Some("size")
        } else if a.mode() != b.mode() || a.uid() != b.uid() || a.gid() != b.gid() {
            Some("permissions")
        } else if (a.st_flags() ^ b.st_flags()) & !libc::UF_COMPRESSED != 0 {
            Some("flags")
        } else if (a.mtime(), a.mtime_nsec()) != (b.mtime(), b.mtime_nsec())
            || (a.st_birthtime(), a.st_birthtime_nsec()) != (b.st_birthtime(), b.st_birthtime_nsec())
        {
            Some("dates")
        } else if xattrs(orig).is_none_or(|x| xattrs(copy) != Some(x)) {
            Some("extended attributes")
        } else if acl(orig) != acl(copy) {
            Some("access list")
        } else {
            None
        }
    }

    /// Nothing about the file itself changed between two looks at it.
    fn unchanged(a: &Metadata, b: &Metadata) -> bool {
        (a.dev(), a.ino(), a.size(), a.nlink(), a.st_flags()) == (b.dev(), b.ino(), b.size(), b.nlink(), b.st_flags())
            && (a.mtime(), a.mtime_nsec(), a.ctime(), a.ctime_nsec()) == (b.mtime(), b.mtime_nsec(), b.ctime(), b.ctime_nsec())
    }

    /// Compare two files' contents. Both are opened read-only: opening a
    /// compressed file for writing makes macOS uncompress it.
    fn same_bytes(a: &Path, b: &Path) -> std::io::Result<bool> {
        let (mut fa, mut fb) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
        let (mut ba, mut bb) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
        loop {
            let (n, m) = (read_full(&mut fa, &mut ba)?, read_full(&mut fb, &mut bb)?);
            if ba[..n] != bb[..m] {
                return Ok(false);
            }
            if n == 0 {
                return Ok(true);
            }
        }
    }

    fn read_full(f: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut n = 0;
        while n < buf.len() {
            match f.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(n)
    }

    /// Extended attributes (name, value), sorted. `com.apple.provenance` is
    /// left out: macOS stamps it on every new file and it can't be copied.
    fn xattrs(path: &Path) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
        let c = cpath(path)?;
        let len = unsafe { libc::listxattr(c.as_ptr(), std::ptr::null_mut(), 0, libc::XATTR_NOFOLLOW) };
        let mut names = vec![0u8; usize::try_from(len).ok()?];
        let len = unsafe { libc::listxattr(c.as_ptr(), names.as_mut_ptr().cast(), names.len(), libc::XATTR_NOFOLLOW) };
        names.truncate(usize::try_from(len).ok()?);
        let mut out = Vec::new();
        for name in names.split(|&b| b == 0).filter(|n| !n.is_empty() && *n != b"com.apple.provenance") {
            let cname = CString::new(name).ok()?;
            let get = |buf: *mut c_void, size: usize| unsafe {
                libc::getxattr(c.as_ptr(), cname.as_ptr(), buf, size, 0, libc::XATTR_NOFOLLOW)
            };
            let mut value = vec![0u8; usize::try_from(get(std::ptr::null_mut(), 0)).ok()?];
            let len = get(value.as_mut_ptr().cast(), value.len());
            value.truncate(usize::try_from(len).ok()?);
            out.push((name.to_vec(), value));
        }
        out.sort();
        Some(out)
    }

    unsafe extern "C" {
        fn acl_get_link_np(path: *const c_char, kind: c_int) -> *mut c_void;
        fn acl_to_text(acl: *mut c_void, len: *mut isize) -> *mut c_char;
        fn acl_free(obj: *mut c_void) -> c_int;
    }
    const ACL_TYPE_EXTENDED: c_int = 0x100;

    /// The file's access control list as text, if it has one.
    fn acl(path: &Path) -> Option<String> {
        let c = cpath(path)?;
        unsafe {
            let acl = acl_get_link_np(c.as_ptr(), ACL_TYPE_EXTENDED);
            if acl.is_null() {
                return None;
            }
            let text = acl_to_text(acl, std::ptr::null_mut());
            let out = (!text.is_null()).then(|| CStr::from_ptr(text).to_string_lossy().into_owned());
            if !text.is_null() {
                acl_free(text.cast());
            }
            acl_free(acl);
            out
        }
    }

    fn set_birthtime(path: &Path, secs: i64, nsec: i64) {
        let Some(c) = cpath(path) else { return };
        let mut list: libc::attrlist = unsafe { std::mem::zeroed() };
        list.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
        list.commonattr = libc::ATTR_CMN_CRTIME;
        let mut ts = libc::timespec { tv_sec: secs, tv_nsec: nsec };
        unsafe {
            libc::setattrlist(
                c.as_ptr(),
                (&mut list as *mut libc::attrlist).cast(),
                (&mut ts as *mut libc::timespec).cast(),
                size_of::<libc::timespec>(),
                libc::FSOPT_NOFOLLOW,
            );
        }
    }

    /// Put a folder's access and modification dates back.
    fn keep_times(dir: &Path, was: &Metadata) {
        let Some(c) = cpath(dir) else { return };
        let times = [
            libc::timespec { tv_sec: was.atime(), tv_nsec: was.atime_nsec() },
            libc::timespec { tv_sec: was.mtime(), tv_nsec: was.mtime_nsec() },
        ];
        unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    }

    // ------------------------------------------------------------------
    // Apps installed for all users

    /// Whether Heft may ask root to rewrite `path`: a file inside an app in
    /// /Applications (installed from a package or the App Store, so owned by
    /// root). Nothing else is ever handed to root, and what macOS protects
    /// itself is refused as usual (see `refuse`).
    pub fn admin_may_rewrite(path: &Path) -> bool {
        let n = crate::mac::broken::normalize(path);
        n.starts_with("/Applications") && n == path && app_bundle(path).is_some()
    }

    /// A file only root can rewrite that root may be asked to: it belongs to
    /// someone else and is inside an app in /Applications.
    pub fn needs_admin(path: &Path) -> bool {
        admin_may_rewrite(path)
            && std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file() && m.uid() != unsafe { libc::getuid() })
    }

    /// Compress (or with `undo`, uncompress) files as root, after macOS's
    /// administrator password prompt: Heft runs itself as root to do exactly
    /// what it does for your own files, with the same checks, file by file.
    /// Returns each file's result, in order. `Err` if it didn't run at all.
    pub fn rewrite_as_admin(paths: &[String], undo: bool) -> Result<Vec<Result<Option<u64>, String>>, String> {
        use std::os::unix::fs::DirBuilderExt;
        let dir = std::env::temp_dir().join(format!("heft-compress-{}-{}", std::process::id(), crate::platform::now_unix()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|e| e.to_string())?;
        let (list, out) = (dir.join("files"), dir.join("results"));
        let result = (|| {
            // One path after another, each ended by a NUL: names may hold newlines.
            let mut bytes = Vec::new();
            for p in paths {
                bytes.extend_from_slice(p.as_bytes());
                bytes.push(0);
            }
            std::fs::write(&list, bytes).map_err(|e| e.to_string())?;
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let q = |p: &Path| crate::mac::sh_quote(&p.to_string_lossy());
            let command = format!("{} --compress-files {} {}{}", q(&exe), q(&list), q(&out), if undo { " --undo" } else { "" });
            let why = if undo { "uncompress apps installed for all users" } else { "compress apps installed for all users" };
            crate::mac::run_as_admin(&command, why)?;
            let text = std::fs::read_to_string(&out).map_err(|_| "it didn't finish".to_string())?;
            Ok(parse_results(&text, paths.len()))
        })();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    /// Lines of `ok <bytes> <index>`, `same <index>` or `err <index> <why>`.
    pub(super) fn parse_results(text: &str, n: usize) -> Vec<Result<Option<u64>, String>> {
        let mut out: Vec<Result<Option<u64>, String>> = (0..n).map(|_| Err("it didn't run".to_string())).collect();
        for line in text.lines() {
            let mut f = line.splitn(3, ' ');
            let (kind, a, b) = (f.next(), f.next(), f.next());
            let r = match (kind, a, b) {
                (Some("ok"), Some(bytes), Some(i)) => bytes.parse().ok().map(|b| (i, Ok(Some(b)))),
                (Some("same"), Some(i), None) => Some((i, Ok(None))),
                (Some("err"), Some(i), why) => Some((i, Err(why.unwrap_or("it failed").to_string()))),
                _ => None,
            };
            if let Some((i, r)) = r
                && let Some(slot) = i.parse::<usize>().ok().and_then(|i| out.get_mut(i))
            {
                *slot = r;
            }
        }
        out
    }

    /// The root side of [`rewrite_as_admin`] (`heft --compress-files`): read
    /// the list, rewrite each file that may be rewritten, and write the results.
    pub fn rewrite_listed(list: &Path, results: &Path, undo: bool) -> Result<(), String> {
        let bytes = std::fs::read(list).map_err(|e| e.to_string())?;
        let mut report = String::new();
        for (i, raw) in bytes.split(|&b| b == 0).filter(|p| !p.is_empty()).enumerate() {
            let path = Path::new(std::ffi::OsStr::from_bytes(raw));
            let r = if !admin_may_rewrite(path) {
                Err("Heft only does this for apps in /Applications".to_string())
            } else if undo {
                decompress(path).map(Some)
            } else {
                compress(path, Level::Fast)
            };
            report.push_str(&match r {
                Ok(Some(bytes)) => format!("ok {bytes} {i}\n"),
                Ok(None) => format!("same {i}\n"),
                Err(e) => format!("err {i} {}\n", e.replace(['\n', '\r'], " ")),
            });
        }
        std::fs::write(results, report).map_err(|e| e.to_string())
    }

    /// The outermost app bundle `path` is inside, if any. Folders named after
    /// a bundle id (`com.example.app`) don't count; bundles have `Contents`.
    fn app_bundle(path: &Path) -> Option<&Path> {
        path.ancestors()
            .filter(|a| a.extension().is_some_and(|e| e.eq_ignore_ascii_case("app")) && path.starts_with(a.join("Contents")))
            .last()
    }

    /// Files that running programs have open, and the programs themselves.
    /// Only programs Heft may look at are seen: yours, not other users' or
    /// the system's.
    pub struct InUse {
        open: HashSet<(u32, u64)>,
        programs: Vec<PathBuf>,
    }

    #[repr(C)]
    struct ProcFileInfo {
        openflags: u32,
        status: u32,
        offset: i64,
        kind: i32,
        guardflags: u32,
    }

    #[repr(C)]
    struct VnodeFdInfo {
        pfi: ProcFileInfo,
        pvi: libc::vnode_info,
    }

    const PROC_PIDFDVNODEINFO: c_int = 1;

    impl InUse {
        /// A fresh look at every process. Takes a few milliseconds.
        pub fn now() -> InUse {
            let me = std::process::id() as i32;
            let mut out = InUse { open: HashSet::new(), programs: Vec::new() };
            let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) }.max(0) as usize + 64;
            let mut pids = vec![0i32; count];
            let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (count * 4) as c_int) }.max(0) as usize;
            for &pid in pids.iter().take(n).filter(|&&p| p > 0 && p != me) {
                let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
                let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
                if len > 0 {
                    buf.truncate(len as usize);
                    out.programs.push(PathBuf::from(std::ffi::OsStr::from_bytes(&buf)));
                }
                let size = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
                if size <= 0 {
                    continue; // not ours to look at
                }
                let mut fds: Vec<libc::proc_fdinfo> =
                    Vec::with_capacity(size as usize / size_of::<libc::proc_fdinfo>() + 16);
                let size = unsafe {
                    libc::proc_pidinfo(
                        pid,
                        libc::PROC_PIDLISTFDS,
                        0,
                        fds.as_mut_ptr().cast(),
                        (fds.capacity() * size_of::<libc::proc_fdinfo>()) as c_int,
                    )
                };
                unsafe { fds.set_len(size.max(0) as usize / size_of::<libc::proc_fdinfo>()) };
                for fd in fds.iter().filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32) {
                    let mut info: VnodeFdInfo = unsafe { std::mem::zeroed() };
                    let want = size_of::<VnodeFdInfo>() as c_int;
                    let got = unsafe {
                        libc::proc_pidfdinfo(pid, fd.proc_fd, PROC_PIDFDVNODEINFO, (&mut info as *mut VnodeFdInfo).cast(), want)
                    };
                    if got == want {
                        out.open.insert((info.pvi.vi_stat.vst_dev, info.pvi.vi_stat.vst_ino));
                    }
                }
            }
            out
        }

        /// [`InUse::now`], reused for a second. Good enough to skip the
        /// files of a running app early; the last check before a file is
        /// replaced always takes a fresh look.
        fn cached() -> std::sync::Arc<InUse> {
            use std::sync::{Arc, Mutex};
            use std::time::{Duration, Instant};
            static LAST: Mutex<Option<(Instant, Arc<InUse>)>> = Mutex::new(None);
            let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
            match &*last {
                Some((at, snap)) if at.elapsed() < Duration::from_secs(1) => snap.clone(),
                _ => {
                    let snap = Arc::new(InUse::now());
                    *last = Some((Instant::now(), snap.clone()));
                    snap
                }
            }
        }

        /// A program has the file open, or it's part of an app that's running.
        fn blocks(&self, path: &Path, md: &Metadata) -> bool {
            self.open.contains(&(md.dev() as u32, md.ino())) || self.program_using(path).is_some()
        }

        /// The name of the running program that `path` is part of: the
        /// program itself, anything inside the same app bundle, or a folder
        /// that holds it.
        pub fn program_using(&self, path: &Path) -> Option<String> {
            self.programs.iter().find(|p| p.starts_with(path) || app_bundle(p).is_some_and(|app| path.starts_with(app))).map(|p| {
                let shown = app_bundle(p).unwrap_or(p);
                shown.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| shown.display().to_string())
            })
        }
    }

    #[cfg(test)]
    impl InUse {
        pub(super) fn with_programs(programs: &[&str]) -> InUse {
            InUse { open: HashSet::new(), programs: programs.iter().map(PathBuf::from).collect() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn compress_and_restore() {
        let d = std::env::temp_dir().join(format!("heft-compress-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        if !supported(&d) {
            return;
        }
        let f = d.join("text.txt");
        let data = "the quick brown fox jumps over the lazy dog\n".repeat(20_000);
        std::fs::write(&f, &data).unwrap();
        let mtime = std::fs::metadata(&f).unwrap().modified().unwrap();
        let after = compress(&f, Level::Fast).unwrap().expect("text compresses");
        assert!(after < data.len() as u64 / 4, "{after} bytes on disk");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), data);
        assert_eq!(std::fs::metadata(&f).unwrap().modified().unwrap(), mtime);
        let restored = decompress(&f).unwrap();
        assert!(restored >= data.len() as u64, "{restored}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), data);
        // Random bytes don't shrink, and Windows says so.
        let noise: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let g = d.join("noise.bin");
        std::fs::write(&g, &noise).unwrap();
        let _ = compress(&g, Level::Fast).unwrap();
        assert_eq!(std::fs::read(&g).unwrap(), noise);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn skips_media_and_tiny_files() {
        assert!(!worth_trying("JPG", 10 << 20));
        assert!(!worth_trying("exe", 100));
        assert!(worth_trying("dll", 1 << 20));
    }

    #[cfg(target_os = "macos")]
    mod mac_tests {
        use std::ffi::CString;
        use std::os::macos::fs::MetadataExt as _;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::MetadataExt;
        use std::path::{Path, PathBuf};

        use super::super::*;

        fn temp(name: &str) -> PathBuf {
            let d = std::env::temp_dir().join(format!("heft-compress-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        fn c(p: &Path) -> CString {
            CString::new(p.as_os_str().as_bytes()).unwrap()
        }

        fn compressed(p: &Path) -> bool {
            std::fs::symlink_metadata(p).unwrap().st_flags() & libc::UF_COMPRESSED != 0
        }

        fn times(p: &Path) -> (i64, i64, i64, i64) {
            let m = std::fs::symlink_metadata(p).unwrap();
            (m.mtime(), m.mtime_nsec(), m.st_birthtime(), m.st_birthtime_nsec())
        }

        fn leftovers(d: &Path) -> Vec<String> {
            std::fs::read_dir(d)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(".heft-compress-"))
                .collect()
        }

        #[test]
        fn volumes() {
            assert!(supported(&std::env::temp_dir()));
            assert!(supported(Path::new("/")), "the startup disk, through its Data volume");
            assert!(!supported(Path::new("/no/such/folder")));
        }

        #[test]
        fn compress_and_restore() {
            let d = temp("roundtrip");
            let f = d.join("text.txt");
            let data = "the quick brown fox jumps over the lazy dog\n".repeat(20_000);
            std::fs::write(&f, &data).unwrap();
            // An old modification date and a tag-like attribute that must survive.
            let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::new(1_500_000_000, 123_456_789);
            std::fs::File::options().write(true).open(&f).unwrap().set_modified(old).unwrap();
            let value = b"kept";
            let name = CString::new("com.example.heft-test").unwrap();
            assert_eq!(unsafe { libc::setxattr(c(&f).as_ptr(), name.as_ptr(), value.as_ptr().cast(), 4, 0, 0) }, 0);
            let acl = std::process::Command::new("/bin/chmod").args(["+a", "everyone allow read"]).arg(&f).status().unwrap();
            assert!(acl.success());
            let (before_times, before_mode) = (times(&f), std::fs::metadata(&f).unwrap().mode());
            let folder_mtime = std::fs::metadata(&d).unwrap().modified().unwrap();

            let after = compress(&f, Level::Fast).unwrap().expect("text compresses");
            assert!(after < data.len() as u64 / 4, "{after} bytes on disk");
            assert_eq!(after, std::fs::symlink_metadata(&f).unwrap().blocks() * 512);
            assert!(compressed(&f));
            assert_eq!(std::fs::read_to_string(&f).unwrap(), data);
            assert_eq!(times(&f), before_times);
            assert_eq!(std::fs::metadata(&f).unwrap().mode(), before_mode);
            let mut buf = [0u8; 16];
            let n = unsafe { libc::getxattr(c(&f).as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), 16, 0, 0) };
            assert_eq!(&buf[..n.max(0) as usize], value);
            let acl = std::process::Command::new("/bin/ls").arg("-le").arg(&f).output().unwrap();
            assert!(String::from_utf8_lossy(&acl.stdout).contains("everyone allow read"));
            assert_eq!(std::fs::metadata(&d).unwrap().modified().unwrap(), folder_mtime, "the folder's date is kept");
            // Compressing again changes nothing.
            assert_eq!(compress(&f, Level::Fast), Ok(Some(after)));

            let restored = decompress(&f).unwrap();
            assert!(restored >= data.len() as u64, "{restored}");
            assert!(!compressed(&f));
            assert_eq!(std::fs::read_to_string(&f).unwrap(), data);
            assert_eq!(times(&f), before_times);
            assert!(leftovers(&d).is_empty(), "{:?}", leftovers(&d));
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn leaves_noise_links_locked_and_open_files_alone() {
            let d = temp("skips");
            let text = "all work and no play makes jack a dull boy\n".repeat(5_000);

            // Random bytes don't shrink: left exactly as they were.
            let mut x = 0x2545_F491_4F6C_DD1Du64;
            let noise: Vec<u8> = (0..400_000)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    (x >> 32) as u8
                })
                .collect();
            let g = d.join("noise.bin");
            std::fs::write(&g, &noise).unwrap();
            let ino = std::fs::metadata(&g).unwrap().ino();
            assert_eq!(compress(&g, Level::Fast), Ok(None));
            assert_eq!(std::fs::read(&g).unwrap(), noise);
            assert_eq!(std::fs::metadata(&g).unwrap().ino(), ino, "not replaced");

            // A file with two names would lose one of them.
            let h = d.join("linked.txt");
            std::fs::write(&h, &text).unwrap();
            std::fs::hard_link(&h, d.join("other-name.txt")).unwrap();
            assert!(compress(&h, Level::Fast).unwrap_err().contains("hard link"));
            assert!(!compressed(&h));

            // Locked in Finder (uchg).
            let l = d.join("locked.txt");
            std::fs::write(&l, &text).unwrap();
            assert_eq!(unsafe { libc::chflags(c(&l).as_ptr(), libc::UF_IMMUTABLE) }, 0);
            assert!(compress(&l, Level::Fast).unwrap_err().contains("locked"));
            assert!(!compressed(&l));
            unsafe { libc::chflags(c(&l).as_ptr(), 0) };

            // Held open by another program.
            let o = d.join("open.txt");
            std::fs::write(&o, &text).unwrap();
            let mut child = std::process::Command::new("/bin/sleep")
                .arg("30")
                .stdin(std::fs::File::open(&o).unwrap())
                .spawn()
                .unwrap();
            let r = compress(&o, Level::Fast);
            let _ = child.kill();
            let _ = child.wait();
            assert!(r.as_ref().is_err_and(|e| e.contains("in use")), "{r:?}");
            assert!(!compressed(&o));
            assert_eq!(std::fs::read_to_string(&o).unwrap(), text);

            assert!(leftovers(&d).is_empty(), "{:?}", leftovers(&d));
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn running_apps() {
            let running = InUse::with_programs(&[
                "/Applications/Editor.app/Contents/MacOS/Editor",
                "/Applications/Editor.app/Contents/Frameworks/Helper.app/Contents/MacOS/Helper",
                "/usr/local/bin/tool",
                "/Users/ana/Library/Application Support/com.example.app/agent",
            ]);
            let using = |p: &str| running.program_using(Path::new(p));
            assert_eq!(using("/Applications/Editor.app").as_deref(), Some("Editor"));
            assert_eq!(using("/Applications/Editor.app/Contents/Resources/big.dat").as_deref(), Some("Editor"));
            assert_eq!(using("/Applications").as_deref(), Some("Editor"), "a folder that holds it");
            assert_eq!(using("/usr/local/bin/tool").as_deref(), Some("tool"));
            assert_eq!(using("/Applications/Other.app"), None);
            assert_eq!(using("/usr/local/bin/other"), None);
            // Named like a bundle id, but not an app bundle.
            assert_eq!(using("/Users/ana/Library/Application Support/com.example.app/data.bin"), None);
        }

        #[test]
        fn ditto_messages() {
            use super::super::mac::ditto_error;
            assert_eq!(ditto_error("ditto: /x/.heft-compress-1-0: No space left on device\n"), "not enough free space");
            assert_eq!(ditto_error("ditto: /x/.heft-compress-1-0: Permission denied\n"), "read-only or access denied");
            assert_eq!(ditto_error("ditto: /x/y: Operation not permitted\n"), "macOS didn't allow it");
            assert_eq!(ditto_error("ditto: Cannot get the real path for source '/x'\n"), "Cannot get the real path for source '/x'");
        }

        /// What may be handed to root, and the results coming back.
        #[test]
        fn apps_for_all_users() {
            use super::super::mac::{admin_may_rewrite, parse_results, rewrite_listed};
            assert!(admin_may_rewrite(Path::new("/Applications/Xcode.app/Contents/Frameworks/x.dylib")));
            assert!(admin_may_rewrite(Path::new("/Applications/Adobe/Some.app/Contents/MacOS/some")));
            assert!(!admin_may_rewrite(Path::new("/Applications/Xcode.app")), "the bundle itself isn't a file in it");
            assert!(!admin_may_rewrite(Path::new("/Applications/notes.txt")));
            assert!(!admin_may_rewrite(Path::new("/Library/Application Support/x.app/Contents/y")));
            assert!(!admin_may_rewrite(Path::new("/Applications/X.app/Contents/../../../etc/hosts")));
            assert!(!admin_may_rewrite(Path::new("/System/Applications/Mail.app/Contents/MacOS/Mail")));

            let r = parse_results("ok 4096 0\nsame 2\nerr 1 it's locked\nnonsense\n", 4);
            assert_eq!(r[0], Ok(Some(4096)));
            assert_eq!(r[1], Err("it's locked".to_string()));
            assert_eq!(r[2], Ok(None));
            assert_eq!(r[3], Err("it didn't run".to_string()));

            // The root side refuses anything else, whoever runs it.
            let d = temp("root-side");
            let f = d.join("big.txt");
            std::fs::write(&f, vec![b'a'; 100_000]).unwrap();
            let list = d.join("files");
            let mut bytes = f.to_string_lossy().into_owned().into_bytes();
            bytes.push(0);
            std::fs::write(&list, bytes).unwrap();
            let out = d.join("results");
            rewrite_listed(&list, &out, false).unwrap();
            let got = parse_results(&std::fs::read_to_string(&out).unwrap(), 1);
            assert!(matches!(&got[0], Err(e) if e.contains("/Applications")), "{got:?}");
            assert!(!compressed(&f));
            let _ = std::fs::remove_dir_all(&d);
        }
    }
}
