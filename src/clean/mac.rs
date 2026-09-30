//! macOS-only parts of the cleaner: your temporary and cache folders under
//! `/var/folders`, open files, the clipboard, Finder's preferences, the DNS
//! cache, Quick Look's thumbnail cache, Time Machine's local snapshots, and
//! the weekly launch agent.

use std::collections::HashSet;
use std::ffi::{c_int, c_void, CStr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use super::unix::{files_under, Plan};

// ----------------------------------------------------------------------
// Folders

/// A per-user folder from `confstr` (`_CS_DARWIN_USER_TEMP_DIR`, …), with
/// `/var` resolved to `/private/var` so it matches what open files report.
fn user_dir(name: c_int) -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 1024];
    let n = unsafe { libc::confstr(name, buf.as_mut_ptr(), buf.len()) };
    if n == 0 || n > buf.len() {
        return None;
    }
    let raw = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned();
    let dir = std::fs::canonicalize(&raw).map(|p| p.to_string_lossy().into_owned()).unwrap_or(raw);
    let dir = dir.trim_end_matches('/').to_string();
    dir.starts_with('/').then_some(dir)
}

/// Your temporary folder (`$TMPDIR`).
pub fn temp_dir() -> Option<String> {
    static DIR: OnceLock<Option<String>> = OnceLock::new();
    DIR.get_or_init(|| user_dir(libc::_CS_DARWIN_USER_TEMP_DIR)).clone()
}

/// Your cache folder under `/var/folders`, where macOS keeps Metal shader
/// and Quick Look caches.
pub fn cache_dir() -> Option<String> {
    static DIR: OnceLock<Option<String>> = OnceLock::new();
    DIR.get_or_init(|| user_dir(libc::_CS_DARWIN_USER_CACHE_DIR)).clone()
}

pub use crate::mac::has_full_disk_access;

// ----------------------------------------------------------------------
// Open files

/// `struct proc_fileinfo` from <sys/proc_info.h>.
#[repr(C)]
struct ProcFileInfo {
    openflags: u32,
    status: u32,
    offset: i64,
    kind: i32,
    guardflags: u32,
}

/// `struct vnode_fdinfo`: what `PROC_PIDFDVNODEINFO` returns.
#[repr(C)]
struct VnodeFdInfo {
    pfi: ProcFileInfo,
    pvi: libc::vnode_info,
}

const PROC_PIDFDVNODEINFO: c_int = 1;

/// Files the processes Heft may inspect (yours) have open, as (device,
/// inode).
pub fn open_files() -> HashSet<(u64, u64)> {
    let mut out = HashSet::new();
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return out;
    }
    let mut pids: Vec<c_int> = vec![0; n as usize + 64];
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * size_of::<c_int>()) as c_int) };
    let fd_size = size_of::<libc::proc_fdinfo>();
    for &pid in &pids[..n.clamp(0, pids.len() as c_int) as usize] {
        let bytes = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if bytes <= 0 {
            continue;
        }
        let mut fds: Vec<libc::proc_fdinfo> =
            (0..bytes as usize / fd_size + 16).map(|_| libc::proc_fdinfo { proc_fd: 0, proc_fdtype: 0 }).collect();
        let bytes = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), (fds.len() * fd_size) as c_int)
        };
        let count = (bytes.max(0) as usize / fd_size).min(fds.len());
        for fd in &fds[..count] {
            if fd.proc_fdtype != libc::PROX_FDTYPE_VNODE as u32 {
                continue;
            }
            let mut info: VnodeFdInfo = unsafe { std::mem::zeroed() };
            let got = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd.proc_fd,
                    PROC_PIDFDVNODEINFO,
                    (&mut info as *mut VnodeFdInfo).cast::<c_void>(),
                    size_of::<VnodeFdInfo>() as c_int,
                )
            };
            if got as usize == size_of::<VnodeFdInfo>() {
                out.insert((info.pvi.vi_stat.vst_dev as u64, info.pvi.vi_stat.vst_ino));
            }
        }
    }
    out
}

// ----------------------------------------------------------------------
// Clipboard, preferences, processes

/// Empty the clipboard. Nothing is read from it, so macOS doesn't ask about
/// pasting.
pub fn clear_clipboard() -> Result<(), String> {
    let class = pasteboard_class()?;
    objc2::rc::autoreleasepool(|_| unsafe {
        let board: *mut objc2::runtime::AnyObject = objc2::msg_send![class, generalPasteboard];
        clear_pasteboard(board)
    })
}

fn pasteboard_class() -> Result<&'static objc2::runtime::AnyClass, String> {
    objc2::runtime::AnyClass::get(c"NSPasteboard").ok_or_else(|| "the clipboard isn't available".to_string())
}

/// `board` must be an `NSPasteboard` or null.
unsafe fn clear_pasteboard(board: *mut objc2::runtime::AnyObject) -> Result<(), String> {
    if board.is_null() {
        return Err("the clipboard isn't available".into());
    }
    let _: isize = unsafe { objc2::msg_send![board, clearContents] };
    Ok(())
}

/// How many entries each of `keys` holds in a preference domain. Read
/// through `defaults`, which asks the preferences daemon, so values not yet
/// written to disk count too.
pub fn pref_entries(domain: &str, keys: &[&'static str]) -> Vec<(&'static str, u64)> {
    crate::mac::output("/usr/bin/defaults", &["export", domain, "-"])
        .map(|xml| parse_pref_entries(xml.as_bytes(), keys))
        .unwrap_or_default()
}

fn parse_pref_entries(xml: &[u8], keys: &[&'static str]) -> Vec<(&'static str, u64)> {
    let Ok(value) = plist::Value::from_reader_xml(xml) else { return Vec::new() };
    let Some(dict) = value.as_dictionary() else { return Vec::new() };
    keys.iter()
        .filter_map(|&k| {
            let n = match dict.get(k)? {
                plist::Value::Array(a) => a.len() as u64,
                plist::Value::String(s) => u64::from(!s.is_empty()),
                _ => 1,
            };
            (n > 0).then_some((k, n))
        })
        .collect()
}

pub fn delete_pref(domain: &str, key: &str) -> Result<(), String> {
    let out = Command::new("/usr/bin/defaults").args(["delete", domain, key]).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("couldn't remove {key}: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Quit one of your own processes so it rereads what changed; launchd (or
/// the Dock, for Finder) starts it again.
pub fn restart(process: &str) {
    let _ = Command::new("/usr/bin/killall").arg(process).output();
}

// ----------------------------------------------------------------------
// Planners, referenced from the rule catalog.

/// Flushing the DNS cache: nothing to count, and it needs root.
pub fn dns() -> Option<Plan> {
    let cmd = |c: &[&str]| c.iter().map(|s| s.to_string()).collect();
    Some(Plan {
        bytes: 0,
        items: 0,
        commands: vec![cmd(&["dscacheutil", "-flushcache"]), cmd(&["killall", "-HUP", "mDNSResponder"])],
        root: true,
    })
}

/// Quick Look's thumbnail cache, which `qlmanage -r cache` resets. macOS
/// protects the folder, so without Full Disk Access its size is unknown;
/// the reset works either way.
pub fn quicklook() -> Option<Plan> {
    let base = PathBuf::from(cache_dir()?);
    // Without Full Disk Access even looking at the folder is refused, which
    // still tells that it's there.
    let dir = ["com.apple.quicklook.ThumbnailsAgent/com.apple.QuickLook.thumbnailcache", "com.apple.QuickLook.thumbnailcache"]
        .iter()
        .map(|d| base.join(d))
        .find(|d| match std::fs::symlink_metadata(d) {
            Ok(m) => m.is_dir(),
            Err(e) => e.kind() == std::io::ErrorKind::PermissionDenied,
        })?;
    let commands = vec![vec!["qlmanage".to_string(), "-r".to_string(), "cache".to_string()]];
    if std::fs::read_dir(&dir).is_err() {
        return Some(Plan { bytes: 0, items: 0, commands, root: false });
    }
    let (bytes, items) = files_under(&dir, &|_, _| true);
    (items > 0).then_some(Plan { bytes, items, commands, root: false })
}

// ----------------------------------------------------------------------
// Time Machine local snapshots

/// How many local Time Machine snapshots the startup disk has.
pub fn local_snapshots() -> Option<usize> {
    crate::mac::output("/usr/bin/tmutil", &["listlocalsnapshots", "/"]).map(|o| super::parse::tm_snapshots(&o))
}

/// Asks Time Machine to thin its local snapshots now, as it does by itself
/// when the disk runs low. Backups on the backup disk aren't touched.
pub const THIN_SNAPSHOTS: &str = "tmutil thinlocalsnapshots / 999999999999 4";

// ----------------------------------------------------------------------
// Weekly cleaning (a launch agent)

const AGENT_LABEL: &str = "io.github.gjnail.heft.weekly-clean";

fn agent_path() -> Option<PathBuf> {
    crate::mac::Domain::UserAgent.folder().map(|d| d.join(format!("{AGENT_LABEL}.plist")))
}

pub fn schedule_enabled() -> bool {
    agent_path().is_some_and(|p| p.exists())
}

/// Add or remove a launch agent that runs `heft --clean` with the saved
/// selection every Sunday at noon, like the Windows scheduled task. It runs
/// as you with nobody watching, so rules that need the administrator
/// password or would restart Finder are skipped.
pub fn set_schedule(on: bool) -> Result<(), String> {
    let path = agent_path().ok_or("no home folder")?;
    let job = format!("gui/{}/{AGENT_LABEL}", crate::mac::uid());
    // Unload whatever is there first; it's fine if nothing was.
    let _ = Command::new("/bin/launchctl").args(["bootout", &job]).output();
    if !on {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        };
    }
    let exe = std::env::current_exe().and_then(std::fs::canonicalize).map_err(|e| e.to_string())?;
    if exe.to_string_lossy().contains("/AppTranslocation/") {
        return Err("macOS is running Heft from a temporary copy. Move Heft to the Applications folder, open it from there, and try again.".into());
    }
    let bundle = crate::mac::enclosing_app(&exe).and_then(|a| crate::mac::bundle_info(&a)).and_then(|b| b.id);
    write_agent(&path, &exe, bundle.as_deref())?;
    let target = format!("gui/{}", crate::mac::uid());
    let out = Command::new("/bin/launchctl").args(["bootstrap", &target]).arg(&path).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&path);
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!(
            "macOS didn't start the launch agent ({msg}). Check that Heft is allowed in System Settings › General › Login Items"
        ));
    }
    Ok(())
}

/// After Heft.app was moved, set weekly cleaning up again at its new place.
pub fn follow_move() {
    let Some(job) = agent_path().and_then(|p| crate::mac::parse_launch_job(&p, crate::mac::Domain::UserAgent)) else {
        return;
    };
    if job.label == AGENT_LABEL && crate::mac::moved_heft(&job).is_some() {
        let _ = set_schedule(true);
    }
}

/// Write the launch agent's plist.
fn write_agent(path: &Path, exe: &Path, bundle_id: Option<&str>) -> Result<(), String> {
    use plist::{Dictionary, Value};
    let mut when = Dictionary::new();
    // Sunday at 12:00, like the Windows task. launchd runs a time missed
    // while the Mac slept as soon as it wakes.
    when.insert("Weekday".into(), Value::Integer(0.into()));
    when.insert("Hour".into(), Value::Integer(12.into()));
    when.insert("Minute".into(), Value::Integer(0.into()));
    let mut d = Dictionary::new();
    d.insert("Label".into(), Value::String(AGENT_LABEL.into()));
    d.insert(
        "ProgramArguments".into(),
        Value::Array(vec![Value::String(exe.to_string_lossy().into_owned()), Value::String("--clean".into())]),
    );
    d.insert("StartCalendarInterval".into(), Value::Dictionary(when));
    d.insert("ProcessType".into(), Value::String("Background".into()));
    if let Some(id) = bundle_id {
        // Shows the job under Heft's name in System Settings › Login Items.
        d.insert("AssociatedBundleIdentifiers".into(), Value::Array(vec![Value::String(id.into())]));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    Value::Dictionary(d).to_file_xml(path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_sizes_match_the_kernel() {
        // PROC_PIDFDVNODEINFO_SIZE in <sys/proc_info.h>.
        assert_eq!(size_of::<VnodeFdInfo>(), 176);
    }

    #[test]
    fn user_folders() {
        let t = temp_dir().unwrap();
        assert!(t.starts_with("/private/var/folders/"), "{t}");
        assert!(t.ends_with("/T"), "{t}");
        assert!(cache_dir().unwrap().ends_with("/C"));
    }

    #[test]
    fn finder_prefs() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>FXRecentFolders</key><array><dict><key>name</key><string>Downloads</string></dict><dict><key>name</key><string>src</string></dict></array>
<key>GoToField</key><string>/Users/x/Documents</string>
<key>GoToFieldHistory</key><array/>
<key>ShowPathbar</key><true/>
</dict></plist>"#;
        let keys: &[&'static str] = &["FXRecentFolders", "GoToField", "GoToFieldHistory"];
        assert_eq!(parse_pref_entries(xml, keys), [("FXRecentFolders", 2), ("GoToField", 1)]);
        assert!(parse_pref_entries(b"not a plist", keys).is_empty());
    }

    /// The real preference commands, on a plist in a temporary folder
    /// (`defaults` takes a path in place of a domain).
    #[test]
    fn preferences_round_trip() {
        let dir = std::env::temp_dir().join(format!("heft-prefs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let domain = dir.join("local.heft.test").to_string_lossy().into_owned();
        let write = |args: &[&str]| {
            let ok = Command::new("/usr/bin/defaults").arg("write").arg(&domain).args(args).status().unwrap().success();
            assert!(ok);
        };
        write(&["FXRecentFolders", "-array", "a", "b"]);
        write(&["GoToField", "-string", "/Users/x"]);
        let keys: &[&'static str] = &["FXRecentFolders", "GoToField", "GoToFieldHistory"];
        assert_eq!(pref_entries(&domain, keys), [("FXRecentFolders", 2), ("GoToField", 1)]);
        delete_pref(&domain, "GoToField").unwrap();
        assert_eq!(pref_entries(&domain, keys), [("FXRecentFolders", 2)]);
        assert!(delete_pref(&domain, "GoToField").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Clears a private pasteboard of its own, never the clipboard.
    #[test]
    fn clears_a_pasteboard() {
        let class = pasteboard_class().unwrap();
        objc2::rc::autoreleasepool(|_| unsafe {
            let board: *mut objc2::runtime::AnyObject = objc2::msg_send![class, pasteboardWithUniqueName];
            clear_pasteboard(board).unwrap();
            let _: () = objc2::msg_send![board, releaseGlobally];
        });
        assert!(unsafe { clear_pasteboard(std::ptr::null_mut()) }.is_err());
    }

    #[test]
    fn launch_agent_plist() {
        let dir = std::env::temp_dir().join(format!("heft-agent-{}", std::process::id()));
        let path = dir.join("LaunchAgents").join(format!("{AGENT_LABEL}.plist"));
        let exe = Path::new("/Applications/Heft.app/Contents/MacOS/heft");
        write_agent(&path, exe, Some("local.heft.Heft")).unwrap();
        let job = crate::mac::parse_launch_job(&path, crate::mac::Domain::UserAgent).unwrap();
        assert_eq!(job.label, AGENT_LABEL);
        assert_eq!(job.args, ["/Applications/Heft.app/Contents/MacOS/heft", "--clean"]);
        assert!(job.scheduled && !job.run_at_load && !job.keep_alive);
        assert_eq!(job.bundles, ["local.heft.Heft"]);
        let v = crate::mac::read_plist(&path).unwrap();
        let when = v.as_dictionary().unwrap().get("StartCalendarInterval").unwrap().as_dictionary().unwrap();
        let get = |k: &str| when.get(k).and_then(|v| v.as_signed_integer());
        assert_eq!((get("Weekday"), get("Hour"), get("Minute")), (Some(0), Some(12), Some(0)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn planners_without_side_effects() {
        let dns = dns().unwrap();
        assert!(dns.root && dns.items == 0 && dns.commands.len() == 2);
        // Only looks; the reset itself isn't run here.
        if let Some(q) = quicklook() {
            assert_eq!(q.commands, [["qlmanage", "-r", "cache"]]);
        }
    }
}
