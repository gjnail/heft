//! Minimal registry access: open, enumerate, read, write, delete, plus
//! exporting keys and values in `.reg` format so every change Heft makes to
//! the registry can be undone with `reg import`.

use std::fmt::Write as _;

use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry as r;

use crate::winsys::{from_wide, wide};

pub const REG_SZ: u32 = 1;
pub const REG_EXPAND_SZ: u32 = 2;
pub const REG_BINARY: u32 = 3;
pub const REG_DWORD: u32 = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Hive {
    CurrentUser,
    LocalMachine,
}

impl Hive {
    fn handle(self) -> r::HKEY {
        match self {
            Hive::CurrentUser => r::HKEY_CURRENT_USER,
            Hive::LocalMachine => r::HKEY_LOCAL_MACHINE,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Hive::CurrentUser => "HKEY_CURRENT_USER",
            Hive::LocalMachine => "HKEY_LOCAL_MACHINE",
        }
    }
    pub fn short(self) -> &'static str {
        match self {
            Hive::CurrentUser => "HKCU",
            Hive::LocalMachine => "HKLM",
        }
    }
    /// Writing here needs administrator rights.
    pub fn needs_admin(self) -> bool {
        self == Hive::LocalMachine
    }
}

/// A raw registry value.
#[derive(Clone, Debug)]
pub struct Value {
    pub name: String,
    pub kind: u32,
    pub data: Vec<u8>,
}

impl Value {
    /// The value as a string, for the string types.
    pub fn as_string(&self) -> Option<String> {
        match self.kind {
            REG_SZ | REG_EXPAND_SZ => Some(utf16_bytes_to_string(&self.data)),
            _ => None,
        }
    }
}

fn utf16_bytes_to_string(data: &[u8]) -> String {
    let words: Vec<u16> = data.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
    from_wide(&words)
}

pub struct Key(r::HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { r::RegCloseKey(self.0) };
    }
}

// Registry handles may be used from any thread.
unsafe impl Send for Key {}

impl Key {
    pub fn open(hive: Hive, path: &str) -> Option<Key> {
        Self::open_with(hive, path, r::KEY_READ)
    }

    pub fn open_writable(hive: Hive, path: &str) -> Result<Key, String> {
        Self::open_with_err(hive, path, r::KEY_READ | r::KEY_SET_VALUE)
    }

    fn open_with(hive: Hive, path: &str, access: u32) -> Option<Key> {
        Self::open_with_err(hive, path, access).ok()
    }

    fn open_with_err(hive: Hive, path: &str, access: u32) -> Result<Key, String> {
        let w = wide(path);
        let mut h: r::HKEY = std::ptr::null_mut();
        let rc = unsafe { r::RegOpenKeyExW(hive.handle(), w.as_ptr(), 0, access | r::KEY_WOW64_64KEY, &mut h) };
        if rc == ERROR_SUCCESS { Ok(Key(h)) } else { Err(win_error(rc)) }
    }

    /// Open, creating the key if it doesn't exist yet.
    pub fn create(hive: Hive, path: &str) -> Result<Key, String> {
        let w = wide(path);
        let mut h: r::HKEY = std::ptr::null_mut();
        let rc = unsafe {
            r::RegCreateKeyExW(
                hive.handle(),
                w.as_ptr(),
                0,
                std::ptr::null(),
                0,
                r::KEY_READ | r::KEY_SET_VALUE | r::KEY_WOW64_64KEY,
                std::ptr::null(),
                &mut h,
                std::ptr::null_mut(),
            )
        };
        if rc == ERROR_SUCCESS { Ok(Key(h)) } else { Err(win_error(rc)) }
    }

    pub fn subkeys(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut buf = [0u16; 512];
        for i in 0.. {
            let mut len = buf.len() as u32;
            let rc = unsafe {
                r::RegEnumKeyExW(
                    self.0,
                    i,
                    buf.as_mut_ptr(),
                    &mut len,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if rc == ERROR_NO_MORE_ITEMS {
                break;
            }
            if rc == ERROR_SUCCESS {
                out.push(String::from_utf16_lossy(&buf[..len as usize]));
            } else if rc != ERROR_MORE_DATA {
                break;
            }
        }
        out
    }

    pub fn values(&self) -> Vec<Value> {
        let mut out = Vec::new();
        let (mut max_name, mut max_data) = (0u32, 0u32);
        unsafe {
            r::RegQueryInfoKeyW(
                self.0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut max_name,
                &mut max_data,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }
        let mut name = vec![0u16; max_name as usize + 2];
        let mut data = vec![0u8; max_data as usize + 2];
        for i in 0.. {
            let mut name_len = name.len() as u32;
            let mut data_len = data.len() as u32;
            let mut kind = 0u32;
            let rc = unsafe {
                r::RegEnumValueW(
                    self.0,
                    i,
                    name.as_mut_ptr(),
                    &mut name_len,
                    std::ptr::null(),
                    &mut kind,
                    data.as_mut_ptr(),
                    &mut data_len,
                )
            };
            if rc == ERROR_NO_MORE_ITEMS {
                break;
            }
            if rc == ERROR_MORE_DATA {
                // A value grew since we asked; make room and retry this index.
                name.resize(name.len() * 2 + 256, 0);
                data.resize(data.len() * 2 + 4096, 0);
                if data.len() > 64 << 20 {
                    break;
                }
                continue;
            }
            if rc != ERROR_SUCCESS {
                break;
            }
            out.push(Value {
                name: String::from_utf16_lossy(&name[..name_len as usize]),
                kind,
                data: data[..data_len as usize].to_vec(),
            });
        }
        out
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        let w = wide(name);
        let mut kind = 0u32;
        let mut len = 0u32;
        let rc = unsafe { r::RegQueryValueExW(self.0, w.as_ptr(), std::ptr::null(), &mut kind, std::ptr::null_mut(), &mut len) };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let mut data = vec![0u8; len as usize + 2];
        let mut len2 = data.len() as u32;
        let rc = unsafe { r::RegQueryValueExW(self.0, w.as_ptr(), std::ptr::null(), &mut kind, data.as_mut_ptr(), &mut len2) };
        if rc != ERROR_SUCCESS {
            return None;
        }
        data.truncate(len2 as usize);
        Some(Value { name: name.to_string(), kind, data })
    }

    pub fn get_string(&self, name: &str) -> Option<String> {
        self.get(name).and_then(|v| v.as_string()).filter(|s| !s.is_empty())
    }

    pub fn get_dword(&self, name: &str) -> Option<u32> {
        let v = self.get(name)?;
        (v.kind == REG_DWORD && v.data.len() >= 4).then(|| u32::from_le_bytes([v.data[0], v.data[1], v.data[2], v.data[3]]))
    }

    pub fn set_raw(&self, name: &str, kind: u32, data: &[u8]) -> Result<(), String> {
        let w = wide(name);
        let rc = unsafe { r::RegSetValueExW(self.0, w.as_ptr(), 0, kind, data.as_ptr(), data.len() as u32) };
        if rc == ERROR_SUCCESS { Ok(()) } else { Err(win_error(rc)) }
    }

    pub fn delete_value(&self, name: &str) -> Result<(), String> {
        let w = wide(name);
        let rc = unsafe { r::RegDeleteValueW(self.0, w.as_ptr()) };
        if rc == ERROR_SUCCESS { Ok(()) } else { Err(win_error(rc)) }
    }
}

/// Delete a key and everything below it.
pub fn delete_tree(hive: Hive, path: &str) -> Result<(), String> {
    let (parent, leaf) = path.rsplit_once('\\').ok_or("refusing to delete a top-level key")?;
    let pk = Key::open_with_err(hive, parent, r::KEY_READ | r::KEY_SET_VALUE | 0x0001_0000 /* DELETE */)?;
    let lw = wide(leaf);
    let rc = unsafe { r::RegDeleteTreeW(pk.0, lw.as_ptr()) };
    if rc != ERROR_SUCCESS {
        return Err(win_error(rc));
    }
    // RegDeleteTreeW leaves the (now empty) key itself behind.
    let rc = unsafe { r::RegDeleteKeyExW(pk.0, lw.as_ptr(), r::KEY_WOW64_64KEY, 0) };
    if rc == ERROR_SUCCESS || rc == windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(win_error(rc))
    }
}

pub fn win_error(code: u32) -> String {
    match code {
        5 => "access denied".into(),
        2 => "not found".into(),
        _ => std::io::Error::from_raw_os_error(code as i32).to_string(),
    }
}

// ----------------------------------------------------------------------
// .reg export

/// Collects keys and values in `.reg` format. Written as UTF-16 with a BOM,
/// the encoding `reg import` and regedit expect.
#[derive(Default)]
pub struct RegExport {
    text: String,
    pub entries: usize,
}

impl RegExport {
    /// Add one value (with its key header).
    pub fn add_value(&mut self, hive: Hive, path: &str, v: &Value) {
        let _ = write!(self.text, "\r\n[{}\\{}]\r\n", hive.name(), path);
        self.text.push_str(&format_value(v));
        self.entries += 1;
    }

    /// Add a key with all its values and subkeys.
    pub fn add_key(&mut self, hive: Hive, path: &str) {
        let Some(k) = Key::open(hive, path) else { return };
        let _ = write!(self.text, "\r\n[{}\\{}]\r\n", hive.name(), path);
        for v in k.values() {
            self.text.push_str(&format_value(&v));
        }
        self.entries += 1;
        for sub in k.subkeys() {
            self.add_key(hive, &format!("{path}\\{sub}"));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries == 0
    }

    pub fn contents(&self) -> String {
        format!("Windows Registry Editor Version 5.00\r\n{}\r\n", self.text)
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut bytes = vec![0xFF, 0xFE];
        for u in self.contents().encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(path, bytes)
    }
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn hex_list(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 3);
    for (i, b) in data.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub fn format_value(v: &Value) -> String {
    let name = if v.name.is_empty() { "@".to_string() } else { quote(&v.name) };
    let body = match v.kind {
        // Plain strings only when they round-trip exactly (no embedded newlines).
        REG_SZ if !utf16_bytes_to_string(&v.data).contains(['\r', '\n']) => quote(&utf16_bytes_to_string(&v.data)),
        REG_DWORD if v.data.len() == 4 => {
            format!("dword:{:08x}", u32::from_le_bytes([v.data[0], v.data[1], v.data[2], v.data[3]]))
        }
        REG_BINARY => format!("hex:{}", hex_list(&v.data)),
        k => format!("hex({k:x}):{}", hex_list(&v.data)),
    };
    format!("{name}={body}\r\n")
}

/// Where registry backups live.
pub fn backup_dir() -> std::path::PathBuf {
    crate::platform::data_dir().join("backups")
}

/// A fresh, timestamped backup file path.
pub fn new_backup_path(prefix: &str) -> std::path::PathBuf {
    let t = crate::platform::local_time(crate::platform::now_unix());
    let stamp = t
        .map(|t| format!("{:04}{:02}{:02}-{:02}{:02}", t.year, t.month, t.day, t.hour, t.minute))
        .unwrap_or_else(|| crate::platform::now_unix().to_string());
    let mut p = backup_dir().join(format!("{prefix}-{stamp}.reg"));
    let mut n = 2;
    while p.exists() {
        p = backup_dir().join(format!("{prefix}-{stamp}-{n}.reg"));
        n += 1;
    }
    p
}

/// Merge a `.reg` file back into the registry.
pub fn import(path: &std::path::Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("reg.exe")
        .arg("import")
        .arg(path)
        .creation_flags(crate::winsys::CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if msg.is_empty() { "reg import failed (administrator rights may be needed)".into() } else { msg })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sz(name: &str, s: &str, kind: u32) -> Value {
        let data: Vec<u8> = s.encode_utf16().chain([0]).flat_map(|u| u.to_le_bytes()).collect();
        Value { name: name.into(), kind, data }
    }

    #[test]
    fn formats_values() {
        assert_eq!(format_value(&sz("Path", "C:\\A \"b\"", REG_SZ)), "\"Path\"=\"C:\\\\A \\\"b\\\"\"\r\n");
        assert_eq!(format_value(&sz("", "x", REG_SZ)), "@=\"x\"\r\n");
        let d = Value { name: "N".into(), kind: REG_DWORD, data: 0x1234u32.to_le_bytes().to_vec() };
        assert_eq!(format_value(&d), "\"N\"=dword:00001234\r\n");
        let b = Value { name: "B".into(), kind: REG_BINARY, data: vec![2, 0, 255] };
        assert_eq!(format_value(&b), "\"B\"=hex:02,00,ff\r\n");
        let e = sz("E", "%x%", REG_EXPAND_SZ);
        assert!(format_value(&e).starts_with("\"E\"=hex(2):25,00,78,00"));
    }

    #[test]
    fn reads_known_key() {
        let k = Key::open(Hive::LocalMachine, r"SOFTWARE\Microsoft\Windows NT\CurrentVersion").expect("open");
        assert!(k.get_string("ProductName").is_some());
        assert!(!k.values().is_empty());
    }
}
