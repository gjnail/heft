//! macOS and Linux implementation (POSIX + a little of each OS).

use std::ffi::CString;

use super::{DriveInfo, LocalTime};

/// A mounted file system.
#[derive(Clone, Debug)]
pub struct Mount {
    pub point: String,
    pub fs: String,
    pub device: String,
    /// macOS: hidden from Finder (system volumes, snapshots).
    pub hidden: bool,
    /// macOS: an APFS snapshot (e.g. Time Machine), a second view of data
    /// that is already counted elsewhere.
    pub snapshot: bool,
}

#[cfg(target_os = "linux")]
pub fn mounts() -> Vec<Mount> {
    let Ok(text) = std::fs::read_to_string("/proc/self/mounts") else { return Vec::new() };
    text.lines()
        .filter_map(|line| {
            let mut f = line.split(' ');
            let (device, point, fs) = (f.next()?, f.next()?, f.next()?);
            Some(Mount {
                point: unescape_mount(point),
                fs: fs.to_string(),
                device: unescape_mount(device),
                hidden: false,
                snapshot: false,
            })
        })
        .collect()
}

/// `/proc/mounts` escapes whitespace and backslashes as `\ooo` octal.
#[cfg(target_os = "linux")]
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(target_os = "macos")]
pub fn mounts() -> Vec<Mount> {
    let mut buf: *mut libc::statfs = std::ptr::null_mut();
    let n = unsafe { libc::getmntinfo(&mut buf, libc::MNT_NOWAIT) };
    if n <= 0 || buf.is_null() {
        return Vec::new();
    }
    let entries = unsafe { std::slice::from_raw_parts(buf, n as usize) };
    let text = |chars: &[libc::c_char]| {
        let bytes: Vec<u8> = chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    entries
        .iter()
        .map(|s| Mount {
            point: text(&s.f_mntonname),
            fs: text(&s.f_fstypename),
            device: text(&s.f_mntfromname),
            hidden: s.f_flags & libc::MNT_DONTBROWSE as u32 != 0,
            snapshot: s.f_flags & libc::MNT_SNAPSHOT as u32 != 0,
        })
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn mounts() -> Vec<Mount> {
    Vec::new()
}

/// Kernel interfaces and RAM disks rather than storage, plus mounts that show
/// data already stored elsewhere (snap/squashfs images, container overlays).
/// Scanning `/` skips these.
pub fn is_virtual_fs(fs: &str) -> bool {
    matches!(
        fs,
        "proc" | "sysfs" | "devtmpfs" | "devpts" | "devfs" | "tmpfs" | "ramfs" | "cgroup" | "cgroup2" | "securityfs"
            | "debugfs" | "tracefs" | "configfs" | "fusectl" | "pstore" | "bpf" | "mqueue" | "hugetlbfs" | "autofs"
            | "binfmt_misc" | "efivarfs" | "rpc_pipefs" | "nsfs" | "squashfs" | "overlay" | "fuse.snapfuse"
            | "fuse.gvfsd-fuse" | "fuse.portal" | "fdesc" | "nullfs"
    )
}

fn is_network_fs(fs: &str) -> bool {
    matches!(fs, "nfs" | "nfs4" | "cifs" | "smb3" | "smbfs" | "afpfs" | "webdav" | "fuse.sshfs" | "9p")
}

/// Mount points a scan should not descend into.
pub fn skip_mounts() -> Vec<String> {
    mounts().into_iter().filter(|m| is_virtual_fs(&m.fs) || m.snapshot).map(|m| m.point).collect()
}

pub fn list_drives() -> Vec<DriveInfo> {
    let mut out: Vec<DriveInfo> = Vec::new();
    let mut seen_devices = std::collections::HashSet::new();
    for m in mounts() {
        // Since macOS 11 the startup disk itself is mounted from a snapshot
        // of the sealed system volume.
        if is_virtual_fs(&m.fs) || (m.snapshot && m.point != "/") {
            continue;
        }
        let p = m.point.as_str();
        let network = is_network_fs(&m.fs);
        let wanted = if cfg!(target_os = "macos") {
            p == "/" || (p.starts_with("/Volumes/") && !m.hidden)
        } else {
            let system = ["/boot", "/efi", "/snap", "/var/", "/run/", "/sys", "/proc", "/dev", "/tmp"];
            (p == "/" || !system.iter().any(|s| p.starts_with(s)) || p.starts_with("/run/media/"))
                && (m.device.starts_with('/') || network)
        };
        if !wanted || !seen_devices.insert(m.device.clone()) {
            continue;
        }
        let Some((total, free)) = free_space(p) else { continue };
        if total == 0 {
            continue;
        }
        let label = if p == "/" {
            "System".to_string()
        } else {
            p.rsplit('/').find(|s| !s.is_empty()).unwrap_or(p).to_string()
        };
        let kind = if network {
            "Network"
        } else if p.starts_with("/media/") || p.starts_with("/run/media/") || (cfg!(target_os = "macos") && p != "/") {
            "Removable"
        } else {
            "Local disk"
        };
        let purgeable = if network { 0 } else { super::purgeable_space(p, free) };
        out.push(DriveInfo { root: m.point.clone(), label, fs: m.fs.clone(), kind, total, free, purgeable });
    }
    out.sort_by(|a, b| (a.root != "/").cmp(&(b.root != "/")).then(a.root.cmp(&b.root)));
    out
}

/// Finder's "available": free space plus what macOS can purge for
/// something important, like an app installing, less the `free` space
/// statfs reported. macOS works this out itself, which can take a moment
/// the first time.
#[cfg(target_os = "macos")]
pub fn purgeable_space(path: &str, free: u64) -> u64 {
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSString, NSURL};
    objc2::rc::autoreleasepool(|_| {
        let url = NSURL::fileURLWithPath(&NSString::from_str(path));
        let key = NSString::from_str("NSURLVolumeAvailableCapacityForImportantUsageKey");
        let mut value: *mut AnyObject = std::ptr::null_mut();
        let ok: bool = unsafe {
            objc2::msg_send![&url, getResourceValue: &mut value, forKey: &*key, error: std::ptr::null_mut::<*mut AnyObject>()]
        };
        if !ok || value.is_null() {
            return 0;
        }
        // An NSNumber.
        let available: i64 = unsafe { objc2::msg_send![value, longLongValue] };
        (available.max(0) as u64).saturating_sub(free)
    })
}

/// (total bytes, bytes available to the caller) of the file system holding `path`.
#[cfg(target_os = "macos")]
pub fn free_space(path: &str) -> Option<(u64, u64)> {
    // statfs, not statvfs: macOS statvfs has 32-bit block counts.
    let c = CString::new(path).ok()?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let bs = s.f_bsize as u64;
    Some((s.f_blocks * bs, s.f_bavail * bs))
}

#[cfg(not(target_os = "macos"))]
pub fn free_space(path: &str) -> Option<(u64, u64)> {
    let c = CString::new(path).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let unit = if s.f_frsize > 0 { s.f_frsize as u64 } else { s.f_bsize as u64 };
    Some((s.f_blocks as u64 * unit, s.f_bavail as u64 * unit))
}

/// (device, inode), which identifies hard links to the same file.
pub fn file_identity(file: &std::fs::File) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata().ok()?;
    Some((m.dev(), m.ino()))
}

pub fn is_elevated() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Elevation isn't offered on macOS/Linux (running GUI apps as root is
/// discouraged, and there's no MFT fast path to unlock).
pub fn relaunch_elevated(_args: &str) -> bool {
    false
}

/// Open the file manager with `path` selected.
#[cfg(target_os = "macos")]
pub fn reveal(path: &str) {
    let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
}

/// Open the file manager with `path` selected, via the freedesktop
/// FileManager1 D-Bus interface (Nautilus, Dolphin, Nemo, Thunar…), falling
/// back to opening the containing folder.
#[cfg(not(target_os = "macos"))]
pub fn reveal(path: &str) {
    let path = path.to_string();
    std::thread::spawn(move || {
        let shown = std::process::Command::new("dbus-send")
            .args([
                "--session",
                "--print-reply",
                "--dest=org.freedesktop.FileManager1",
                "--type=method_call",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
            ])
            .arg(format!("array:string:file://{}", percent_encode(&path)))
            .arg("string:")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !shown {
            let parent = std::path::Path::new(&path).parent().map(|p| p.to_path_buf()).unwrap_or_else(|| "/".into());
            let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
        }
    });
}

#[cfg(not(target_os = "macos"))]
fn percent_encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Move items to the Trash through Finder, so Finder's Put Back works for
/// them too (a setting; off unless chosen).
#[cfg(target_os = "macos")]
static THROUGH_FINDER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Why Finder last couldn't move something, for the Settings page.
#[cfg(target_os = "macos")]
static FINDER_REFUSED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[cfg(target_os = "macos")]
pub fn set_trash_through_finder(on: bool) {
    THROUGH_FINDER.store(on, std::sync::atomic::Ordering::Relaxed);
    *FINDER_REFUSED.lock().unwrap() = None;
}

#[cfg(target_os = "macos")]
pub fn trash_through_finder() -> bool {
    THROUGH_FINDER.load(std::sync::atomic::Ordering::Relaxed)
}

/// Why Finder didn't move the last item it was asked to, if it didn't.
#[cfg(target_os = "macos")]
pub fn finder_refused() -> Option<String> {
    FINDER_REFUSED.lock().unwrap().clone()
}

/// Move `path` to the Trash and return where it ended up. Normally with the
/// system's file manager API, which needs no permission. With the setting
/// on, Finder does it instead, so Finder's Put Back works as well as Heft's
/// own restore; macOS asks once to let Heft control Finder. Only items on
/// the home folder's disk that Heft could move itself go through Finder:
/// elsewhere Finder might offer to delete right away, or ask for a password
/// in its own window. If Finder won't, the usual way is used.
#[cfg(target_os = "macos")]
pub fn move_to_trash(path: &std::path::Path) -> Result<Option<std::path::PathBuf>, String> {
    if trash_through_finder() && finder_can_take(path) {
        match crate::mac::osascript(&finder_trash_script(path)) {
            Ok(landed) => {
                let landed = landed.trim().trim_end_matches('/');
                return Ok((!landed.is_empty()).then(|| std::path::PathBuf::from(landed)));
            }
            Err(e) if std::fs::symlink_metadata(path).is_err() => return Err(e),
            Err(e) => *FINDER_REFUSED.lock().unwrap() = Some(e),
        }
    }
    trash_with_file_manager(path)
}

/// On the home folder's disk, in a folder Heft can write to.
#[cfg(target_os = "macos")]
fn finder_can_take(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Some(home), Some(parent)) = (super::home_dir(), path.parent()) else { return false };
    let dev = |p: &std::path::Path| std::fs::symlink_metadata(p).ok().map(|m| m.dev());
    let writable = CString::new(parent.as_os_str().as_encoded_bytes())
        .is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0);
    writable && dev(path).is_some() && dev(path) == dev(std::path::Path::new(&home))
}

/// Finder moves the item to the Trash and says where it put it.
#[cfg(target_os = "macos")]
fn finder_trash_script(path: &std::path::Path) -> String {
    format!(
        "tell application \"Finder\"\nset t to delete (POSIX file {} as alias)\nreturn POSIX path of (t as alias)\nend tell",
        crate::mac::applescript_quote(&path.to_string_lossy())
    )
}

/// The system's file manager API: no Finder scripting, so no permission prompt.
#[cfg(target_os = "macos")]
fn trash_with_file_manager(path: &std::path::Path) -> Result<Option<std::path::PathBuf>, String> {
    use objc2_foundation::{NSFileManager, NSString, NSURL};
    objc2::rc::autoreleasepool(|_| {
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        let mut landed = None;
        NSFileManager::defaultManager()
            .trashItemAtURL_resultingItemURL_error(&url, Some(&mut landed))
            .map_err(|e| e.localizedDescription().to_string())?;
        Ok(landed.and_then(|u| u.path()).map(|p| std::path::PathBuf::from(p.to_string())))
    })
}

pub fn open_path(path: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener).arg(path).spawn();
}

pub fn attach_parent_console() {}

pub fn local_time(unix: i64) -> Option<LocalTime> {
    if unix <= 0 {
        return None;
    }
    let t = unix as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return None;
    }
    Some(LocalTime {
        year: (tm.tm_year + 1900) as u32,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
    })
}

/// The trash works on network mounts where the server allows it; failures
/// are reported per item.
pub fn is_network_path(_path: &str) -> bool {
    false
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn finder_script() {
        let s = super::finder_trash_script(std::path::Path::new("/Users/x/My \"odd\" file.txt"));
        assert_eq!(
            s,
            "tell application \"Finder\"\nset t to delete (POSIX file \"/Users/x/My \\\"odd\\\" file.txt\" as alias)\n\
             return POSIX path of (t as alias)\nend tell"
        );
    }

    /// Only items on the home folder's disk that Heft could move itself.
    #[test]
    fn what_finder_takes() {
        let home = super::super::home_dir().unwrap();
        let dir = std::path::Path::new(&home).join(format!(".heft-finder-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("f");
        std::fs::write(&f, "x").unwrap();
        assert!(super::finder_can_take(&f));
        assert!(!super::finder_can_take(&dir.join("missing")));
        assert!(!super::finder_can_take(std::path::Path::new("/System/Library/CoreServices/Finder.app")), "not writable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Read-only: the startup disk's free and purgeable space.
    #[test]
    fn purgeable_space_on_the_startup_disk() {
        let (total, free) = super::free_space("/").unwrap();
        let started = std::time::Instant::now();
        let purgeable = super::purgeable_space("/", free);
        eprintln!("free {free}, purgeable {purgeable}, took {:?}", started.elapsed());
        assert!(free + purgeable <= total);
    }
}
