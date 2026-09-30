//! Transparent compression for folders you rarely change (Windows, NTFS).
//!
//! This is the compression `compact /EXE` uses: files stay where they are and
//! open as usual, and Windows unpacks them as they're read. A file that is
//! written to later is quietly stored uncompressed again, so it suits
//! programs, games and old projects rather than things you edit.

use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Level {
    /// XPRESS 8K: quick to read back.
    #[default]
    Fast,
    /// LZX: smaller, slower to read back.
    Small,
}

/// Whether files on the volume holding `path` can be compressed this way.
pub fn supported(path: &Path) -> bool {
    #[cfg(windows)]
    {
        crate::platform::volume_root(path)
            .and_then(|r| crate::platform::volume_info(&r))
            .is_some_and(|(_, fs)| fs.eq_ignore_ascii_case("NTFS"))
    }
    #[cfg(not(windows))]
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
    "jar", "apk", "pdf", "vhd", "vhdx", "vmdk", "qcow2", "vdi", "db", "sqlite", "ldb", "log", "pst", "ost",
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

#[cfg(not(windows))]
pub fn compress(_path: &Path, _level: Level) -> Result<Option<u64>, String> {
    Err("not supported on this system".into())
}

#[cfg(not(windows))]
pub fn decompress(_path: &Path) -> Result<u64, String> {
    Err("not supported on this system".into())
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
}
