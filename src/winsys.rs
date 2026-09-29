//! Windows helpers for the cleaning and system tools: processes, version
//! resources, launching programs, the Recycle Bin, and making sense of the
//! command lines and paths stored in the registry.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::UI::Shell as shell;

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
pub const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Expand `%VAR%` references using the current environment. Unknown
/// variables are left as they are.
pub fn expand_env(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let w = wide(s);
    let mut buf = vec![0u16; 1024];
    loop {
        let n = unsafe {
            windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32)
        } as usize;
        if n == 0 {
            return s.to_string();
        }
        if n <= buf.len() {
            return from_wide(&buf);
        }
        buf.resize(n, 0);
    }
}

/// Lower-case executable names of every running process.
pub fn running_processes() -> HashSet<String> {
    let mut out = HashSet::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut e) != 0 {
            loop {
                out.insert(from_wide(&e.szExeFile).to_lowercase());
                if Process32NextW(snap, &mut e) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
}

#[derive(Clone, Debug, Default)]
pub struct VersionInfo {
    pub company: String,
    pub description: String,
}

/// Company and description from an executable's version resource.
pub fn version_info(path: &Path) -> Option<VersionInfo> {
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
    let w = wide(path.as_os_str());
    let size = unsafe { GetFileVersionInfoSizeW(w.as_ptr(), std::ptr::null_mut()) };
    if size == 0 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    if unsafe { GetFileVersionInfoW(w.as_ptr(), 0, size, data.as_mut_ptr().cast()) } == 0 {
        return None;
    }
    let query = |sub: &str| -> Option<(*const u8, u32)> {
        let q = wide(sub);
        let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let ok = unsafe { VerQueryValueW(data.as_ptr().cast(), q.as_ptr(), &mut ptr, &mut len) };
        (ok != 0 && !ptr.is_null() && len > 0).then_some((ptr as *const u8, len))
    };
    let mut langs = Vec::new();
    if let Some((p, len)) = query("\\VarFileInfo\\Translation") {
        let pairs = unsafe { std::slice::from_raw_parts(p as *const u16, (len / 2) as usize) };
        for [lang, codepage] in pairs.as_chunks::<2>().0 {
            langs.push(format!("{lang:04x}{codepage:04x}"));
        }
    }
    langs.extend(["040904b0".to_string(), "040904e4".to_string(), "000004b0".to_string()]);
    let string = |name: &str| -> String {
        for l in &langs {
            if let Some((p, len)) = query(&format!("\\StringFileInfo\\{l}\\{name}")) {
                let s = unsafe { std::slice::from_raw_parts(p as *const u16, len as usize) };
                let v = from_wide(s).trim().to_string();
                if !v.is_empty() {
                    return v;
                }
            }
        }
        String::new()
    };
    Some(VersionInfo { company: string("CompanyName"), description: string("FileDescription") })
}

/// A process started by [`launch`]; wait for it on a background thread.
pub struct Launched(HANDLE);

unsafe impl Send for Launched {}

impl Launched {
    /// Block until the process exits.
    pub fn wait(self) {
        unsafe {
            windows_sys::Win32::System::Threading::WaitForSingleObject(self.0, u32::MAX);
        }
    }
}

impl Drop for Launched {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Start a program through the shell, so installers that need elevation get
/// their UAC prompt. Returns a handle when Windows provides one.
pub fn launch(file: &str, params: &str, elevate: bool) -> Result<Option<Launched>, String> {
    let verb = wide(if elevate { "runas" } else { "open" });
    let f = wide(file);
    let p = wide(params);
    let mut info: shell::SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<shell::SHELLEXECUTEINFOW>() as u32;
    info.fMask = shell::SEE_MASK_NOCLOSEPROCESS | shell::SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = f.as_ptr();
    info.lpParameters = if params.is_empty() { std::ptr::null() } else { p.as_ptr() };
    info.nShow = windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    if unsafe { shell::ShellExecuteExW(&mut info) } == 0 {
        let e = std::io::Error::last_os_error();
        return Err(match e.raw_os_error() {
            Some(1223) => "cancelled".into(),
            Some(2) | Some(3) => format!("{file} was not found"),
            _ => e.to_string(),
        });
    }
    Ok((!info.hProcess.is_null()).then(|| Launched(info.hProcess)))
}

/// Open a document, folder, or URL (including `ms-settings:` pages).
pub fn shell_open(target: &str) {
    let _ = launch(target, "", false);
}

/// Run a command line in a new, visible console window that stays open
/// afterwards so the user can read the output.
pub fn run_in_console(title: &str, command: &str) -> std::io::Result<std::process::Child> {
    std::process::Command::new("cmd.exe")
        .raw_arg(format!("/c title {title} & {command} & echo. & pause"))
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
}

/// Split a command line into (program, arguments). Handles quoted paths,
/// and unquoted paths with spaces (`C:\Program Files\x\y.exe /S`).
pub fn split_command(cmd: &str) -> (String, String) {
    let cmd = cmd.trim();
    if let Some(rest) = cmd.strip_prefix('"') {
        return match rest.find('"') {
            Some(end) => (rest[..end].to_string(), rest[end + 1..].trim().to_string()),
            None => (rest.to_string(), String::new()),
        };
    }
    // Unquoted: the shortest prefix ending in an executable extension.
    let lower = cmd.to_ascii_lowercase();
    for ext in [".exe", ".com", ".bat", ".cmd", ".msc", ".lnk"] {
        let mut from = 0;
        while let Some(i) = lower[from..].find(ext) {
            let end = from + i + ext.len();
            if end == cmd.len() || cmd.as_bytes()[end] == b' ' {
                return (cmd[..end].to_string(), cmd[end..].trim().to_string());
            }
            from = end;
        }
    }
    match cmd.split_once(' ') {
        Some((a, b)) => (a.to_string(), b.trim().to_string()),
        None => (cmd.to_string(), String::new()),
    }
}

/// Programs that just host another file named in their arguments.
const HOSTS: [&str; 6] = ["rundll32.exe", "rundll32", "regsvr32.exe", "wscript.exe", "cscript.exe", "mshta.exe"];

/// The file a command line runs: the program, or for `rundll32 x.dll,Entry`
/// style hosts, the file they load. Environment variables are expanded.
pub fn command_target(cmd: &str) -> Option<String> {
    let (prog, args) = split_command(&expand_env(cmd));
    if prog.is_empty() {
        return None;
    }
    let name = prog.rsplit(['\\', '/']).next().unwrap_or(&prog).to_ascii_lowercase();
    if HOSTS.contains(&name.as_str()) {
        let (target, _) = split_command(&args);
        let target = target.split(',').next().unwrap_or("").trim().to_string();
        return (!target.is_empty() && !target.starts_with('/')).then_some(target);
    }
    Some(prog)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathState {
    Exists,
    Missing,
    /// Can't tell safely: relative name, network share, absent drive, …
    Unknown,
}

/// Whether a path from the registry points at something that exists. Only
/// says `Missing` when it can be sure.
pub fn path_state(raw: &str) -> PathState {
    let p = expand_env(raw.trim().trim_matches('"'));
    let b = p.as_bytes();
    if p.contains('%') || p.contains(['*', '?']) || p.starts_with("\\\\") || b.len() < 3 {
        return PathState::Unknown;
    }
    if !(b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\') {
        return PathState::Unknown;
    }
    if !fixed_drive_present(b[0]) {
        return PathState::Unknown;
    }
    match std::fs::metadata(&p) {
        Ok(_) => return PathState::Exists,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        // Access denied (WindowsApps, other users' folders) isn't "gone".
        Err(_) => return PathState::Unknown,
    }
    // 32-bit programs see SysWOW64 as System32 and may record either.
    let lower = p.to_ascii_lowercase();
    for (from, to) in [("\\system32\\", "\\syswow64\\"), ("\\syswow64\\", "\\system32\\"), ("\\sysnative\\", "\\system32\\")] {
        if let Some(i) = lower.find(from) {
            let alt = format!("{}{}{}", &p[..i], to, &p[i + from.len()..]);
            if Path::new(&alt).exists() {
                return PathState::Exists;
            }
        }
    }
    PathState::Missing
}

fn fixed_drive_present(letter: u8) -> bool {
    let root = wide(format!("{}:\\", letter as char));
    // DRIVE_FIXED = 3. Removable, network and optical drives come and go.
    unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(root.as_ptr()) == 3 }
}

/// (bytes, items) in the Recycle Bins of all drives.
pub fn recycle_bin_info() -> Option<(u64, u64)> {
    let mut info = shell::SHQUERYRBINFO { cbSize: std::mem::size_of::<shell::SHQUERYRBINFO>() as u32, ..Default::default() };
    let hr = unsafe { shell::SHQueryRecycleBinW(std::ptr::null(), &mut info) };
    (hr >= 0).then_some((info.i64Size.max(0) as u64, info.i64NumItems.max(0) as u64))
}

pub fn empty_recycle_bin() -> Result<(), String> {
    let flags = shell::SHERB_NOCONFIRMATION | shell::SHERB_NOPROGRESSUI | shell::SHERB_NOSOUND;
    let hr = unsafe { shell::SHEmptyRecycleBinW(std::ptr::null_mut(), std::ptr::null(), flags) };
    // E_UNEXPECTED is returned when the bin is already empty.
    if hr >= 0 || hr == 0x8000_FFFFu32 as i32 { Ok(()) } else { Err(format!("error 0x{:08x}", hr as u32)) }
}

pub fn clear_clipboard() -> Result<(), String> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard};
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("the clipboard is in use".into());
        }
        let ok = EmptyClipboard() != 0;
        CloseClipboard();
        if ok { Ok(()) } else { Err("could not empty the clipboard".into()) }
    }
}

pub fn flush_dns() -> Result<(), String> {
    let out = std::process::Command::new("ipconfig.exe")
        .arg("/flushdns")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() { Ok(()) } else { Err("ipconfig /flushdns failed".into()) }
}

/// Known folders, resolved from the environment.
pub fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_commands() {
        assert_eq!(split_command(r#""C:\Program Files\A\b.exe" /S"#), (r"C:\Program Files\A\b.exe".into(), "/S".into()));
        assert_eq!(split_command(r"C:\Program Files\A\b.exe /S /x"), (r"C:\Program Files\A\b.exe".into(), "/S /x".into()));
        assert_eq!(split_command(r"MsiExec.exe /X{1234}"), ("MsiExec.exe".into(), "/X{1234}".into()));
        assert_eq!(split_command(r"C:\a\my.executor\b.exe"), (r"C:\a\my.executor\b.exe".into(), String::new()));
        assert_eq!(split_command("foo bar"), ("foo".into(), "bar".into()));
    }

    #[test]
    fn finds_command_targets() {
        assert_eq!(command_target(r#"rundll32.exe "C:\x\y.dll",Start"#).as_deref(), Some(r"C:\x\y.dll"));
        assert_eq!(command_target(r"rundll32.exe C:\x\y.dll,Start").as_deref(), Some(r"C:\x\y.dll"));
        assert_eq!(command_target(r#""C:\A B\c.exe" --min"#).as_deref(), Some(r"C:\A B\c.exe"));
        let expanded = command_target(r"%SystemRoot%\system32\cmd.exe").unwrap();
        assert!(!expanded.contains('%'));
    }

    #[test]
    fn path_states() {
        let windir = std::env::var("SystemRoot").unwrap();
        assert_eq!(path_state(&format!(r"{windir}\System32\cmd.exe")), PathState::Exists);
        assert_eq!(path_state(r"%SystemRoot%\System32\cmd.exe"), PathState::Exists);
        assert_eq!(path_state(&format!(r"{windir}\heft-definitely-missing-file.exe")), PathState::Missing);
        assert_eq!(path_state("cmd.exe"), PathState::Unknown);
        assert_eq!(path_state(r"\\server\share\x.exe"), PathState::Unknown);
        assert_eq!(path_state(r"%HEFT_NO_SUCH_VAR%\x.exe"), PathState::Unknown);
    }

    #[test]
    fn lists_processes() {
        let p = running_processes();
        assert!(p.iter().any(|n| n.ends_with(".exe")));
    }
}
