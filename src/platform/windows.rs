//! Windows implementation (Win32).

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::Storage::FileSystem as fs;
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows_sys::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};

use super::{DriveInfo, LocalTime};

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;
const DRIVE_RAMDISK: u32 = 6;

pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

pub fn list_drives() -> Vec<DriveInfo> {
    let mask = unsafe { fs::GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let root = format!("{}:\\", (b'A' + i as u8) as char);
        let w = wide(&root);
        let kind = match unsafe { fs::GetDriveTypeW(w.as_ptr()) } {
            DRIVE_FIXED => "Local disk",
            DRIVE_REMOVABLE => "Removable",
            DRIVE_REMOTE => "Network",
            DRIVE_CDROM => "Optical",
            DRIVE_RAMDISK => "RAM disk",
            _ => continue,
        };
        let (label, fsname) = volume_info(&root).unwrap_or_default();
        let (total, free) = free_space(&root).unwrap_or((0, 0));
        if total == 0 && kind == "Optical" {
            continue; // empty drive
        }
        out.push(DriveInfo { root, label, fs: fsname, kind, total, free });
    }
    out
}

/// The root of the volume holding `path`: `C:\`, or a mounted folder.
pub fn volume_root(path: &Path) -> Option<String> {
    let w = wide(path.as_os_str());
    let mut root = [0u16; 1024];
    let ok = unsafe { fs::GetVolumePathNameW(w.as_ptr(), root.as_mut_ptr(), root.len() as u32) };
    (ok != 0).then(|| from_wide(&root))
}

/// (label, filesystem name) for a volume root like `C:\`.
pub fn volume_info(root: &str) -> Option<(String, String)> {
    let w = wide(root);
    let mut label = [0u16; 261];
    let mut fsname = [0u16; 261];
    let ok = unsafe {
        fs::GetVolumeInformationW(
            w.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fsname.as_mut_ptr(),
            fsname.len() as u32,
        )
    };
    (ok != 0).then(|| (from_wide(&label), from_wide(&fsname)))
}

/// (total bytes, bytes available to the caller) of the volume holding `path`.
pub fn free_space(path: &str) -> Option<(u64, u64)> {
    let w = wide(path);
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    let ok = unsafe { fs::GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0).then_some((total, avail))
}

/// Actual allocation for compressed / sparse files.
pub fn compressed_size(path: &Path) -> Option<u64> {
    let w = wide(path.as_os_str());
    let mut high = 0u32;
    let low = unsafe { fs::GetCompressedFileSizeW(w.as_ptr(), &mut high) };
    if low == u32::MAX && std::io::Error::last_os_error().raw_os_error().unwrap_or(0) != 0 {
        return None;
    }
    Some(((high as u64) << 32) | low as u64)
}

/// (volume, file index), which identifies hard links to the same file.
pub fn file_identity(file: &std::fs::File) -> Option<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    let mut info: fs::BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { fs::GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) };
    (ok != 0).then_some({
        (info.dwVolumeSerialNumber as u64, ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64)
    })
}

pub fn is_elevated() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

/// Start a new elevated copy of Heft (UAC prompt). Returns false if the
/// user declined or it failed.
pub fn relaunch_elevated(args: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let verb = wide("runas");
    let file = wide(exe.as_os_str());
    let params = wide(args);
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    r as isize > 32
}

/// Open the file manager with `path` selected.
pub fn reveal(path: &str) {
    let _ = std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{path}\"")).spawn();
}

pub fn open_path(path: &str) {
    let _ = std::process::Command::new("explorer.exe").raw_arg(format!("\"{path}\"")).spawn();
}

/// Release builds are GUI-subsystem apps; reattach so CLI output shows up.
pub fn attach_parent_console() {
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
}

pub fn filetime_to_unix(ft: u64) -> i64 {
    (ft / 10_000_000) as i64 - 11_644_473_600
}

pub fn local_time(unix: i64) -> Option<LocalTime> {
    if unix <= 0 {
        return None;
    }
    let ft64 = ((unix + 11_644_473_600) as u64) * 10_000_000;
    let ft = FILETIME { dwLowDateTime: ft64 as u32, dwHighDateTime: (ft64 >> 32) as u32 };
    let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let mut local: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc) == 0 {
            return None;
        }
        if SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0 {
            return None;
        }
    }
    Some(LocalTime {
        year: local.wYear as u32,
        month: local.wMonth as u32,
        day: local.wDay as u32,
        hour: local.wHour as u32,
        minute: local.wMinute as u32,
    })
}

/// Network shares have no Recycle Bin: deleting there would be permanent.
pub fn is_network_path(path: &str) -> bool {
    if path.starts_with("\\\\") {
        return true;
    }
    let root: String = path.chars().take(3).collect();
    let w = wide(&root);
    unsafe { fs::GetDriveTypeW(w.as_ptr()) == DRIVE_REMOTE }
}

/// Paths where deleting things is very likely to break Windows.
pub fn is_protected_path(path: &str) -> bool {
    let p = path.to_lowercase();
    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()).to_lowercase();
    let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into()).to_lowercase();
    let pf86 = std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| "C:\\Program Files (x86)".into()).to_lowercase();
    let pd = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".into()).to_lowercase();
    let starts = |base: &str| p == base || p.starts_with(&format!("{base}\\"));
    p.len() <= 3
        || starts(&windir)
        || p == pf
        || p == pf86
        || p == pd
        || p.ends_with("\\pagefile.sys")
        || p.ends_with("\\hiberfil.sys")
        || p.ends_with("\\swapfile.sys")
        || p.contains("\\system volume information")
        || p.contains("\\$recycle.bin")
        || p.split('\\').nth(1).is_some_and(|c| c.starts_with('$'))
}
