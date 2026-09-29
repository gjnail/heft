//! Programs that start with Windows: Run keys, Startup folders, and
//! scheduled tasks that fire at logon or boot.
//!
//! Enabling and disabling uses the same `StartupApproved` flags as Task
//! Manager's Startup page, so the original entry is never touched and the
//! change shows up (and can be undone) there too.

use std::path::{Path, PathBuf};

use crate::reg::{self, Hive, Key, RegExport};
use crate::winsys::{self, PathState};

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_ONCE: &str = r"Software\Microsoft\Windows\CurrentVersion\RunOnce";
const RUN_32: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Source {
    RunUser,
    RunMachine,
    RunMachine32,
    RunOnceUser,
    RunOnceMachine,
    FolderUser,
    FolderCommon,
    Task,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::RunUser => "Registry (you)",
            Source::RunMachine => "Registry (all users)",
            Source::RunMachine32 => "Registry (all users, 32-bit)",
            Source::RunOnceUser => "Run once (you)",
            Source::RunOnceMachine => "Run once (all users)",
            Source::FolderUser => "Startup folder (you)",
            Source::FolderCommon => "Startup folder (all users)",
            Source::Task => "Scheduled task",
        }
    }

    /// Changing it needs administrator rights.
    pub fn needs_admin(self) -> bool {
        !matches!(self, Source::RunUser | Source::RunOnceUser | Source::FolderUser)
    }

    fn registry(self) -> Option<(Hive, &'static str)> {
        match self {
            Source::RunUser => Some((Hive::CurrentUser, RUN)),
            Source::RunMachine => Some((Hive::LocalMachine, RUN)),
            Source::RunMachine32 => Some((Hive::LocalMachine, RUN_32)),
            Source::RunOnceUser => Some((Hive::CurrentUser, RUN_ONCE)),
            Source::RunOnceMachine => Some((Hive::LocalMachine, RUN_ONCE)),
            _ => None,
        }
    }

    /// Where Task Manager keeps the enabled/disabled flag for this source.
    fn approval(self) -> Option<(Hive, String)> {
        match self {
            Source::RunUser => Some((Hive::CurrentUser, format!(r"{APPROVED}\Run"))),
            Source::RunMachine => Some((Hive::LocalMachine, format!(r"{APPROVED}\Run"))),
            Source::RunMachine32 => Some((Hive::LocalMachine, format!(r"{APPROVED}\Run32"))),
            Source::FolderUser => Some((Hive::CurrentUser, format!(r"{APPROVED}\StartupFolder"))),
            Source::FolderCommon => Some((Hive::LocalMachine, format!(r"{APPROVED}\StartupFolder"))),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    /// Value name, file name, or task path.
    pub name: String,
    /// Command line (Run keys, tasks) or shortcut path (Startup folders).
    pub command: String,
    pub source: Source,
    pub enabled: bool,
    /// The program that runs, if known.
    pub target: Option<String>,
    pub state: PathState,
    pub company: String,
    pub description: String,
}

impl Entry {
    pub fn can_toggle(&self) -> bool {
        self.source.approval().is_some() || self.source == Source::Task
    }

    /// A readable title: the program's own description when it has one,
    /// else the entry's name without a task folder's `\` or a `.lnk`.
    pub fn title(&self) -> &str {
        if !self.description.is_empty() && self.source != Source::Task {
            return &self.description;
        }
        let name = self.name.trim_start_matches('\\');
        match name.len().checked_sub(4) {
            Some(i) if name.is_char_boundary(i) && name[i..].eq_ignore_ascii_case(".lnk") => &name[..i],
            _ => name,
        }
    }
}

fn startup_folders() -> [(Source, Option<PathBuf>); 2] {
    [
        (Source::FolderUser, winsys::env_path("APPDATA").map(|p| p.join(r"Microsoft\Windows\Start Menu\Programs\Startup"))),
        (Source::FolderCommon, winsys::env_path("ProgramData").map(|p| p.join(r"Microsoft\Windows\Start Menu\Programs\StartUp"))),
    ]
}

/// Enabled unless Task Manager's flag says otherwise (odd first byte).
fn approved(source: Source, name: &str) -> bool {
    let Some((hive, path)) = source.approval() else { return true };
    let Some(v) = Key::open(hive, &path).and_then(|k| k.get(name)) else { return true };
    v.data.first().is_none_or(|b| b & 1 == 0)
}

fn describe(e: &mut Entry) {
    if let Some(t) = &e.target {
        e.state = winsys::path_state(t);
        if e.state == PathState::Exists
            && let Some(v) = winsys::version_info(Path::new(&winsys::expand_env(t)))
        {
            e.company = v.company;
            e.description = v.description;
        }
    }
}

/// Everything that starts automatically.
pub fn list(include_tasks: bool) -> Vec<Entry> {
    let mut out = Vec::new();
    for source in [Source::RunUser, Source::RunMachine, Source::RunMachine32, Source::RunOnceUser, Source::RunOnceMachine] {
        let (hive, path) = source.registry().unwrap();
        let Some(k) = Key::open(hive, path) else { continue };
        for v in k.values() {
            let Some(cmd) = v.as_string() else { continue };
            if v.name.is_empty() && cmd.is_empty() {
                continue;
            }
            let mut e = Entry {
                enabled: approved(source, &v.name),
                target: winsys::command_target(&cmd),
                name: v.name,
                command: cmd,
                source,
                state: PathState::Unknown,
                company: String::new(),
                description: String::new(),
            };
            describe(&mut e);
            out.push(e);
        }
    }
    for (source, dir) in startup_folders() {
        let Some(dir) = dir else { continue };
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for f in rd.flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            if name.eq_ignore_ascii_case("desktop.ini") || f.path().is_dir() {
                continue;
            }
            let path = f.path().to_string_lossy().into_owned();
            let is_exe = name.to_ascii_lowercase().ends_with(".exe");
            let mut e = Entry {
                enabled: approved(source, &name),
                name,
                target: is_exe.then(|| path.clone()),
                command: path,
                source,
                state: PathState::Exists,
                company: String::new(),
                description: String::new(),
            };
            describe(&mut e);
            out.push(e);
        }
    }
    if include_tasks {
        out.extend(logon_tasks());
    }
    out
}

pub fn set_enabled(e: &Entry, on: bool) -> Result<(), String> {
    if e.source == Source::Task {
        return schtasks(&["/Change", "/TN", &e.name, if on { "/ENABLE" } else { "/DISABLE" }]);
    }
    let (hive, path) = e.source.approval().ok_or("this entry can't be disabled, only removed")?;
    let mut data = [0u8; 12];
    if on {
        data[0] = 2;
    } else {
        data[0] = 3;
        // When it was disabled, as a FILETIME. Task Manager shows it.
        let ft = (crate::platform::now_unix() + 11_644_473_600) as u64 * 10_000_000;
        data[4..].copy_from_slice(&ft.to_le_bytes());
    }
    Key::create(hive, &path)?.set_raw(&e.name, reg::REG_BINARY, &data)
}

/// Remove an entry. Registry entries are backed up to a .reg file first;
/// Startup-folder shortcuts go to the Recycle Bin. Returns the backup path.
pub fn remove(e: &Entry) -> Result<Option<PathBuf>, String> {
    match e.source {
        Source::Task => schtasks(&["/Delete", "/F", "/TN", &e.name]).map(|_| None),
        Source::FolderUser | Source::FolderCommon => {
            trash::delete(&e.command).map_err(|err| err.to_string())?;
            clear_approval(e);
            Ok(None)
        }
        _ => {
            let (hive, path) = e.source.registry().unwrap();
            let k = Key::open_writable(hive, path)?;
            let v = k.get(&e.name).ok_or("the entry is already gone")?;
            let mut backup = RegExport::default();
            backup.add_value(hive, path, &v);
            if let Some((ah, ap)) = e.source.approval()
                && let Some(av) = Key::open(ah, &ap).and_then(|k| k.get(&e.name))
            {
                backup.add_value(ah, &ap, &av);
            }
            let file = reg::new_backup_path("startup");
            backup.save(&file).map_err(|err| format!("could not save a backup: {err}"))?;
            k.delete_value(&e.name)?;
            clear_approval(e);
            Ok(Some(file))
        }
    }
}

fn clear_approval(e: &Entry) {
    if let Some((hive, path)) = e.source.approval()
        && let Ok(k) = Key::open_writable(hive, &path)
    {
        let _ = k.delete_value(&e.name);
    }
}

fn schtasks(args: &[&str]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("schtasks.exe")
        .args(args)
        .creation_flags(winsys::CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("ERROR:").trim().to_string();
        Err(if msg.is_empty() { "schtasks failed".into() } else { msg })
    }
}

// ----------------------------------------------------------------------
// Scheduled tasks

/// Non-Microsoft scheduled tasks that run at logon or boot.
fn logon_tasks() -> Vec<Entry> {
    use std::os::windows::process::CommandExt;
    let Ok(out) = std::process::Command::new("schtasks.exe")
        .args(["/Query", "/XML"])
        .creation_flags(winsys::CREATE_NO_WINDOW)
        .output()
    else {
        return Vec::new();
    };
    parse_tasks(&String::from_utf8_lossy(&out.stdout))
        .into_iter()
        .map(|mut e| {
            describe(&mut e);
            e
        })
        .collect()
}

fn xml_unescape(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// Text of the first `<tag>…</tag>` in `xml`.
fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{name}>"))?;
    Some(&xml[start..start + end])
}

/// `schtasks /query /xml` prints every task's XML, each preceded by an
/// `<!-- \Path\Name -->` comment.
fn parse_tasks(text: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for chunk in text.split("<!-- ").skip(1) {
        let Some((name, xml)) = chunk.split_once(" -->") else { continue };
        let name = name.trim();
        if name.starts_with(r"\Microsoft\") {
            continue;
        }
        let Some(triggers) = tag(xml, "Triggers") else { continue };
        if !triggers.contains("<LogonTrigger") && !triggers.contains("<BootTrigger") {
            continue;
        }
        let Some(exec) = tag(xml, "Exec") else { continue };
        let Some(cmd) = tag(exec, "Command").map(|c| xml_unescape(c.trim())) else { continue };
        let args = tag(exec, "Arguments").map(|a| xml_unescape(a.trim())).unwrap_or_default();
        let enabled = tag(xml, "Settings").and_then(|s| tag(s, "Enabled")).is_none_or(|v| v.trim() != "false");
        let quoted = if cmd.contains(' ') && !cmd.starts_with('"') { format!("\"{cmd}\"") } else { cmd.clone() };
        out.push(Entry {
            name: name.to_string(),
            command: if args.is_empty() { quoted } else { format!("{quoted} {args}") },
            source: Source::Task,
            enabled,
            target: Some(winsys::expand_env(cmd.trim_matches('"'))),
            state: PathState::Unknown,
            company: String::new(),
            description: String::new(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_task_xml() {
        let xml = r#"
<!-- \GoogleUpdaterTaskUser -->
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2"><Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>
<Settings><Enabled>false</Enabled></Settings>
<Actions Context="Author"><Exec><Command>C:\Program Files\Google\updater.exe</Command><Arguments>--wake &amp; go</Arguments></Exec></Actions></Task>
<!-- \Microsoft\Windows\Defrag\ScheduledDefrag -->
<Task><Triggers><BootTrigger/></Triggers><Actions><Exec><Command>defrag.exe</Command></Exec></Actions></Task>
<!-- \Nightly -->
<Task><Triggers><CalendarTrigger/></Triggers><Actions><Exec><Command>x.exe</Command></Exec></Actions></Task>
"#;
        let tasks = parse_tasks(xml);
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert_eq!(t.name, r"\GoogleUpdaterTaskUser");
        assert!(!t.enabled);
        assert_eq!(t.command, r#""C:\Program Files\Google\updater.exe" --wake & go"#);
        assert_eq!(t.target.as_deref(), Some(r"C:\Program Files\Google\updater.exe"));
    }

    #[test]
    fn lists_without_crashing() {
        // Read-only: just make sure enumeration works on this machine.
        let entries = list(false);
        for e in &entries {
            assert!(!e.command.is_empty() || !e.name.is_empty());
        }
    }
}
