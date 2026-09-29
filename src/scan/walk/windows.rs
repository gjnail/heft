//! Windows directory listing: `GetFileInformationByHandleEx` fills a 64 KB
//! buffer with hundreds of entries per syscall, including allocation sizes,
//! reparse tags and file IDs. That is noticeably faster than
//! `FindFirstFile`/`std::fs::read_dir`. File IDs let us count hard-linked
//! files once, like the MFT scanner does.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileFullDirectoryInfo, FileIdExtdDirectoryInfo, GetFileInformationByHandleEx,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FULL_DIR_INFO, FILE_ID_EXTD_DIR_INFO, FILE_LIST_DIRECTORY, OPEN_EXISTING,
};

use super::{Entry, Lister, Progress};
use crate::platform;
use crate::tree::flags;

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x800;
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x40000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x400000;
const CLOUD_MASK: u32 = FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
/// Reparse tags with this bit (symlinks, junctions, mount points) point elsewhere.
const TAG_NAME_SURROGATE: u32 = 0x2000_0000;

pub(in crate::scan) struct WinLister {
    /// Cleared the first time a file system refuses to report file IDs.
    ids: AtomicBool,
}

impl Default for WinLister {
    fn default() -> Self {
        WinLister { ids: AtomicBool::new(true) }
    }
}

/// 64 KB listing buffer (u64 for alignment).
pub(in crate::scan) struct Buffer(Vec<u64>);

impl Default for Buffer {
    fn default() -> Self {
        Buffer(vec![0u64; 64 * 1024 / 8])
    }
}

impl Lister for WinLister {
    type Scratch = Buffer;

    fn threads(&self) -> usize {
        // Directory opens are latency-bound (filter drivers, disk), so oversubscribe.
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        (n * 3).clamp(8, 48)
    }

    fn list(&self, path: &Path, scratch: &mut Buffer, progress: &Progress) -> Option<Vec<Entry>> {
        list_dir(path, &mut scratch.0, progress, &self.ids)
    }
}

/// Current sizes of the files directly inside `dir` (lowercased name →
/// (size, allocated)). Used to correct MFT entries for live system files.
pub fn list_root_files(dir: &str) -> HashMap<String, (u64, u64)> {
    let mut buf = Buffer::default();
    let ids = AtomicBool::new(false);
    let progress = Progress::default();
    list_dir(Path::new(dir), &mut buf.0, &progress, &ids)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.flags & (flags::DIR | flags::LINK) == 0)
        .map(|e| (e.name.to_lowercase(), (e.size, e.alloc)))
        .collect()
}

/// `\\?\`-prefixed wide path so long paths work.
fn long_path(p: &Path) -> Vec<u16> {
    let w: Vec<u16> = p.as_os_str().encode_wide().collect();
    let bs = b'\\' as u16;
    let mut out: Vec<u16> = Vec::with_capacity(w.len() + 9);
    if w.starts_with(&[bs, bs, b'?' as u16, bs]) {
        out.extend_from_slice(&w);
    } else if w.starts_with(&[bs, bs]) {
        out.extend(r"\\?\UNC\".encode_utf16());
        out.extend_from_slice(&w[2..]);
    } else {
        out.extend(r"\\?\".encode_utf16());
        out.extend_from_slice(&w);
    }
    out.push(0);
    out
}

/// Field offsets for the two directory-info layouts we can ask for.
struct Layout {
    class: i32,
    reparse_tag: usize,
    file_id: Option<usize>,
    name: usize,
}

const WITH_IDS: Layout = Layout {
    class: FileIdExtdDirectoryInfo,
    reparse_tag: std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, ReparsePointTag),
    file_id: Some(std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileId)),
    name: std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName),
};

const WITHOUT_IDS: Layout = Layout {
    class: FileFullDirectoryInfo,
    // For reparse points, EaSize holds the reparse tag.
    reparse_tag: std::mem::offset_of!(FILE_FULL_DIR_INFO, EaSize),
    file_id: None,
    name: std::mem::offset_of!(FILE_FULL_DIR_INFO, FileName),
};

fn list_dir(path: &Path, buf: &mut [u64], progress: &Progress, ids: &AtomicBool) -> Option<Vec<Entry>> {
    let wpath = long_path(path);
    let h = unsafe {
        CreateFileW(
            wpath.as_ptr(),
            FILE_LIST_DIRECTORY,
            1 | 2 | 4,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut layout = if ids.load(Ordering::Relaxed) { &WITH_IDS } else { &WITHOUT_IDS };
    let mut out = Vec::new();
    let (mut files, mut bytes) = (0u64, 0u64);
    let byte_len = std::mem::size_of_val(buf);
    let mut first = true;
    loop {
        let ok = unsafe { GetFileInformationByHandleEx(h, layout.class, buf.as_mut_ptr().cast(), byte_len as u32) };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if first && err != ERROR_NO_MORE_FILES && layout.file_id.is_some() {
                // This file system can't report IDs (FAT, some network shares).
                ids.store(false, Ordering::Relaxed);
                layout = &WITHOUT_IDS;
                continue;
            }
            break; // end of listing (or a failure mid-way: keep what we have)
        }
        first = false;
        let b: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), byte_len) };
        let mut off = 0usize;
        loop {
            let rd32 = |o: usize| u32::from_le_bytes(b[off + o..off + o + 4].try_into().unwrap());
            let rd64 = |o: usize| u64::from_le_bytes(b[off + o..off + o + 8].try_into().unwrap());
            let next = rd32(0) as usize;
            let attrs = rd32(56);
            let name_bytes = rd32(60) as usize;
            let wname: Vec<u16> = b[off + layout.name..off + layout.name + name_bytes]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let dot = wname.len() <= 2 && wname.iter().all(|&c| c == b'.' as u16);
            if !dot {
                let reparse_tag = if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 { rd32(layout.reparse_tag) } else { 0 };
                let is_link = reparse_tag & TAG_NAME_SURROGATE != 0;
                let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0 && !is_link;
                // NTFS has no directory hard links and we never follow
                // junctions, so only files need identities.
                let file_id = match layout.file_id {
                    Some(o) if !is_dir => u128::from_le_bytes(b[off + o..off + o + 16].try_into().unwrap()),
                    _ => 0,
                };

                let mut fl = 0u16;
                if is_dir {
                    fl |= flags::DIR;
                }
                if is_link {
                    fl |= flags::LINK;
                }
                if attrs & FILE_ATTRIBUTE_HIDDEN != 0 {
                    fl |= flags::HIDDEN;
                }
                if attrs & FILE_ATTRIBUTE_SYSTEM != 0 {
                    fl |= flags::SYSTEM;
                }
                let name = String::from_utf16_lossy(&wname);
                let (size, alloc) = if is_dir || is_link {
                    (0, 0)
                } else {
                    let size = rd64(40);
                    let alloc = if attrs & CLOUD_MASK != 0 {
                        fl |= flags::CLOUD;
                        0
                    } else if attrs & (FILE_ATTRIBUTE_COMPRESSED | FILE_ATTRIBUTE_SPARSE_FILE) != 0 {
                        platform::compressed_size(&path.join(&name)).unwrap_or(size)
                    } else {
                        rd64(48)
                    };
                    files += 1;
                    bytes += size;
                    (size, alloc)
                };
                out.push(Entry {
                    os_name: is_dir.then(|| OsString::from_wide(&wname)),
                    name,
                    flags: fl,
                    size,
                    alloc,
                    mtime: platform::filetime_to_unix(rd64(24)),
                    file_id,
                });
            }
            if next == 0 {
                break;
            }
            off += next;
        }
    }
    unsafe { CloseHandle(h) };
    progress.files.fetch_add(files, Ordering::Relaxed);
    progress.bytes.fetch_add(bytes, Ordering::Relaxed);
    Some(out)
}
