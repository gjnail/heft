//! Directory listing through `std::fs`, used on macOS and Linux.
//!
//! On Unix, `DirEntry::metadata` is an `lstat` (symlinks aren't followed),
//! allocation comes from `st_blocks`, and `(st_dev, st_ino)` identifies hard
//! links and bind-mount/firmlink loops. On macOS the BSD file flags also
//! mark Finder-hidden items and iCloud "dataless" (online-only) files.
//! It compiles everywhere so the walk can be tested on any machine.

use std::fs::Metadata;
use std::path::Path;
use std::sync::atomic::Ordering;

use super::{Entry, Lister, Progress};
use crate::tree::flags;

pub(in crate::scan) struct StdLister;

impl Lister for StdLister {
    type Scratch = ();

    fn threads(&self) -> usize {
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        (n * 2).clamp(4, 32)
    }

    fn list(&self, path: &Path, _: &mut (), progress: &Progress) -> Option<Vec<Entry>> {
        let rd = std::fs::read_dir(path).ok()?;
        let mut out = Vec::new();
        let (mut files, mut bytes) = (0u64, 0u64);
        for e in rd {
            let Ok(e) = e else { continue };
            let Ok(md) = e.metadata() else { continue };
            let os_name = e.file_name();
            let name = os_name.to_string_lossy().into_owned();
            let ft = md.file_type();
            let is_link = ft.is_symlink();
            let is_dir = ft.is_dir() && !is_link;
            let m = details(&md);

            let mut fl = 0u16;
            if is_dir {
                fl |= flags::DIR;
            }
            if is_link {
                fl |= flags::LINK;
            }
            if m.hidden || (cfg!(unix) && name.starts_with('.')) {
                fl |= flags::HIDDEN;
            }
            let (size, alloc) = if is_dir || is_link {
                (0, 0)
            } else {
                let size = md.len();
                let alloc = if m.cloud {
                    fl |= flags::CLOUD;
                    0
                } else {
                    m.alloc.unwrap_or(size)
                };
                files += 1;
                bytes += size;
                (size, alloc)
            };
            // Folders always carry their identity (loop detection); files
            // only when they have more than one name, to keep the set small.
            let file_id = if is_dir || m.links > 1 { m.id } else { 0 };
            out.push(Entry {
                os_name: is_dir.then_some(os_name),
                name,
                flags: fl,
                size,
                alloc,
                mtime: m.mtime,
                file_id,
            });
        }
        progress.files.fetch_add(files, Ordering::Relaxed);
        progress.bytes.fetch_add(bytes, Ordering::Relaxed);
        Some(out)
    }
}

struct Details {
    alloc: Option<u64>,
    id: u128,
    links: u64,
    mtime: i64,
    hidden: bool,
    cloud: bool,
}

#[cfg(unix)]
fn details(md: &Metadata) -> Details {
    use std::os::unix::fs::MetadataExt;
    #[cfg(target_os = "macos")]
    let (hidden, cloud) = {
        use std::os::macos::fs::MetadataExt as _;
        const UF_HIDDEN: u32 = 0x0000_8000;
        const SF_DATALESS: u32 = 0x4000_0000;
        let f = md.st_flags();
        (f & UF_HIDDEN != 0, f & SF_DATALESS != 0)
    };
    #[cfg(not(target_os = "macos"))]
    let (hidden, cloud) = (false, false);
    Details {
        alloc: Some(md.blocks() * 512),
        id: ((md.dev() as u128) << 64) | md.ino() as u128,
        links: md.nlink(),
        mtime: md.mtime(),
        hidden,
        cloud,
    }
}

#[cfg(not(unix))]
fn details(md: &Metadata) -> Details {
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Details { alloc: None, id: 0, links: 1, mtime, hidden: false, cloud: false }
}
