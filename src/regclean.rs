//! Registry issues: entries that point at programs, files or folders that no
//! longer exist.
//!
//! Registry "cleaning" doesn't make Windows faster, and aggressive cleaners
//! break things. So Heft only looks at a few kinds of entries whose meaning is
//! unambiguous, only reports an entry when the file or folder it names is
//! provably gone (never for network paths, missing drives or bare file names),
//! and saves a `.reg` backup before changing anything. COM registrations and
//! file associations are deliberately left alone.

use std::path::PathBuf;

use crate::reg::{self, Hive, Key, RegExport};
use crate::winsys::{self, PathState};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum Category {
    Uninstall,
    Startup,
    AppPaths,
    SharedDlls,
    InstallerFolders,
    Compatibility,
    MuiCache,
}

impl Category {
    pub const ALL: [Category; 7] = [
        Category::Uninstall,
        Category::Startup,
        Category::AppPaths,
        Category::SharedDlls,
        Category::InstallerFolders,
        Category::Compatibility,
        Category::MuiCache,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Uninstall => "Uninstall entries for removed programs",
            Category::Startup => "Startup entries for missing programs",
            Category::AppPaths => "App Paths for missing programs",
            Category::SharedDlls => "Shared DLL counts for missing files",
            Category::InstallerFolders => "Installer folder references",
            Category::Compatibility => "Compatibility settings for missing programs",
            Category::MuiCache => "Cached names of missing programs",
        }
    }

    /// Ticked after a scan. Only kinds with a visible effect (Apps &
    /// Features, startup, Win+R, uninstall bookkeeping); the rest is inert
    /// clutter that's listed but left for the user to choose.
    pub fn default_on(self) -> bool {
        matches!(self, Category::Uninstall | Category::Startup | Category::AppPaths | Category::SharedDlls)
    }

    pub fn about(self) -> &'static str {
        match self {
            Category::Uninstall => "Programs listed in Apps & Features whose uninstaller and install folder are both gone.",
            Category::Startup => "Run-at-startup entries whose program no longer exists; Windows tries and fails to start them.",
            Category::AppPaths => "Shortcuts that let Win+R find a program by name, for programs that are gone.",
            Category::SharedDlls => "Reference counts for shared files that have been deleted.",
            Category::InstallerFolders => "Folders Windows Installer tracks that no longer exist.",
            Category::Compatibility => "Program Compatibility Assistant records for programs that are gone.",
            Category::MuiCache => "Display names Explorer cached for programs that are gone. Harmless, but clutter.",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Issue {
    pub category: Category,
    pub hive: Hive,
    pub key: String,
    /// `None`: the whole key goes. `Some`: just this value.
    pub value: Option<String>,
    /// What it refers to (the missing file or folder), or a program name.
    pub detail: String,
}

impl Issue {
    pub fn location(&self) -> String {
        match &self.value {
            Some(v) => format!(r"{}\{}  ›  {}", self.hive.short(), self.key, if v.is_empty() { "(Default)" } else { v }),
            None => format!(r"{}\{}", self.hive.short(), self.key),
        }
    }

    pub fn needs_admin(&self) -> bool {
        self.hive.needs_admin()
    }
}

fn missing(path: &str) -> bool {
    winsys::path_state(path) == PathState::Missing
}

pub fn scan() -> Vec<Issue> {
    let mut out = Vec::new();
    uninstall(&mut out);
    startup(&mut out);
    app_paths(&mut out);
    value_names_are_paths(
        &mut out,
        Category::SharedDlls,
        &[
            (Hive::LocalMachine, r"SOFTWARE\Microsoft\Windows\CurrentVersion\SharedDLLs"),
            (Hive::LocalMachine, r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\SharedDLLs"),
        ],
        |name| Some(name.to_string()),
    );
    value_names_are_paths(
        &mut out,
        Category::InstallerFolders,
        &[(Hive::LocalMachine, r"SOFTWARE\Microsoft\Windows\CurrentVersion\Installer\Folders")],
        |name| Some(name.to_string()),
    );
    value_names_are_paths(
        &mut out,
        Category::Compatibility,
        &[
            (Hive::CurrentUser, r"Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Compatibility Assistant\Store"),
            (Hive::CurrentUser, r"Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers"),
            (Hive::LocalMachine, r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers"),
        ],
        |name| Some(name.to_string()),
    );
    value_names_are_paths(
        &mut out,
        Category::MuiCache,
        &[(Hive::CurrentUser, r"Software\Classes\Local Settings\Software\Microsoft\Windows\Shell\MuiCache")],
        mui_path,
    );
    out
}

/// MuiCache value names look like `C:\x\app.exe.FriendlyAppName`.
fn mui_path(name: &str) -> Option<String> {
    if name.starts_with('@') {
        return None;
    }
    let (path, _) = name.rsplit_once('.')?;
    path.to_ascii_lowercase().ends_with(".exe").then(|| path.to_string())
}

fn uninstall(out: &mut Vec<Issue>) {
    for (hive, root) in [
        (Hive::LocalMachine, r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"),
        (Hive::LocalMachine, r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"),
        (Hive::CurrentUser, r"Software\Microsoft\Windows\CurrentVersion\Uninstall"),
    ] {
        let Some(rk) = Key::open(hive, root) else { continue };
        for sub in rk.subkeys() {
            let path = format!(r"{root}\{sub}");
            let Some(k) = Key::open(hive, &path) else { continue };
            let (Some(name), Some(cmd)) = (k.get_string("DisplayName"), k.get_string("UninstallString")) else { continue };
            if cmd.to_ascii_lowercase().contains("msiexec") {
                continue; // Windows Installer owns these.
            }
            let Some(target) = winsys::command_target(&cmd) else { continue };
            let location_gone = k.get_string("InstallLocation").is_none_or(|l| missing(l.trim().trim_matches('"')));
            if missing(&target) && location_gone {
                out.push(Issue { category: Category::Uninstall, hive, key: path, value: None, detail: name });
            }
        }
    }
}

fn startup(out: &mut Vec<Issue>) {
    for (hive, path) in [
        (Hive::CurrentUser, r"Software\Microsoft\Windows\CurrentVersion\Run"),
        (Hive::CurrentUser, r"Software\Microsoft\Windows\CurrentVersion\RunOnce"),
        (Hive::LocalMachine, r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run"),
        (Hive::LocalMachine, r"SOFTWARE\Microsoft\Windows\CurrentVersion\RunOnce"),
        (Hive::LocalMachine, r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Run"),
    ] {
        let Some(k) = Key::open(hive, path) else { continue };
        for v in k.values() {
            let Some(target) = v.as_string().and_then(|c| winsys::command_target(&c)) else { continue };
            if missing(&target) {
                out.push(Issue { category: Category::Startup, hive, key: path.to_string(), value: Some(v.name), detail: target });
            }
        }
    }
}

fn app_paths(out: &mut Vec<Issue>) {
    for (hive, root) in [
        (Hive::LocalMachine, r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths"),
        (Hive::LocalMachine, r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\App Paths"),
        (Hive::CurrentUser, r"Software\Microsoft\Windows\CurrentVersion\App Paths"),
    ] {
        let Some(rk) = Key::open(hive, root) else { continue };
        for sub in rk.subkeys() {
            let path = format!(r"{root}\{sub}");
            let Some(target) = Key::open(hive, &path).and_then(|k| k.get_string("")) else { continue };
            let target = target.trim().trim_matches('"').to_string();
            if missing(&target) {
                out.push(Issue { category: Category::AppPaths, hive, key: path, value: None, detail: target });
            }
        }
    }
}

/// Keys whose value *names* are file paths.
fn value_names_are_paths(out: &mut Vec<Issue>, category: Category, keys: &[(Hive, &str)], to_path: impl Fn(&str) -> Option<String>) {
    for &(hive, path) in keys {
        let Some(k) = Key::open(hive, path) else { continue };
        for v in k.values() {
            let Some(target) = to_path(&v.name) else { continue };
            if missing(&target) {
                out.push(Issue { category, hive, key: path.to_string(), value: Some(v.name), detail: target });
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct FixReport {
    pub fixed: usize,
    pub failed: Vec<String>,
    pub backup: Option<PathBuf>,
}

/// Back up, then remove, the given issues. Nothing is removed if the backup
/// can't be written.
pub fn fix(issues: &[Issue]) -> Result<FixReport, String> {
    let mut backup = RegExport::default();
    for i in issues {
        match &i.value {
            Some(name) => {
                if let Some(v) = Key::open(i.hive, &i.key).and_then(|k| k.get(name)) {
                    backup.add_value(i.hive, &i.key, &v);
                }
            }
            None => backup.add_key(i.hive, &i.key),
        }
    }
    let mut report = FixReport::default();
    if backup.is_empty() {
        return Ok(report);
    }
    let file = reg::new_backup_path("registry");
    backup.save(&file).map_err(|e| format!("could not save a backup, nothing was changed: {e}"))?;
    report.backup = Some(file);
    for i in issues {
        let r = match &i.value {
            Some(name) => Key::open_writable(i.hive, &i.key).and_then(|k| k.delete_value(name)),
            None => reg::delete_tree(i.hive, &i.key),
        };
        match r {
            Ok(()) => report.fixed += 1,
            Err(e) => report.failed.push(format!("{}: {e}", i.location())),
        }
    }
    Ok(report)
}

#[derive(Clone, Debug)]
pub struct Backup {
    pub path: PathBuf,
    pub modified: i64,
    pub size: u64,
}

/// Saved backups, newest first.
pub fn backups() -> Vec<Backup> {
    let Ok(rd) = std::fs::read_dir(reg::backup_dir()) else { return Vec::new() };
    let mut v: Vec<Backup> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("reg")))
        .filter_map(|e| {
            let md = e.metadata().ok()?;
            let modified = md.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
            Some(Backup { path: e.path(), modified, size: md.len() })
        })
        .collect();
    v.sort_by_key(|b| std::cmp::Reverse(b.modified));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mui_names() {
        assert_eq!(mui_path(r"C:\A\b.exe.FriendlyAppName").as_deref(), Some(r"C:\A\b.exe"));
        assert_eq!(mui_path(r"C:\A\b.exe.ApplicationCompany").as_deref(), Some(r"C:\A\b.exe"));
        assert_eq!(mui_path(r"@%SystemRoot%\system32\shell32.dll,-22"), None);
        assert_eq!(mui_path("LangID"), None);
    }

    /// Round trip on a throwaway HKCU key: detect, fix with backup, restore.
    #[test]
    fn fix_backs_up_and_restores() {
        let base = format!(r"Software\HeftTest-{}", std::process::id());
        let run_like = format!(r"{base}\Store");
        let k = Key::create(Hive::CurrentUser, &run_like).unwrap();
        let gone = format!(r"{}\heft-missing-{}.exe", std::env::var("SystemRoot").unwrap(), std::process::id());
        k.set_raw(&gone, reg::REG_BINARY, &[1, 2, 3]).unwrap();
        let cmd = std::env::var("ComSpec").unwrap();
        k.set_raw(&cmd, reg::REG_BINARY, &[4]).unwrap();
        drop(k);

        let mut found = Vec::new();
        value_names_are_paths(&mut found, Category::Compatibility, &[(Hive::CurrentUser, &run_like)], |n| Some(n.to_string()));
        assert_eq!(found.len(), 1, "only the missing program is reported");
        assert_eq!(found[0].value.as_deref(), Some(gone.as_str()));

        let report = fix(&found).unwrap();
        assert_eq!(report.fixed, 1);
        let backup = report.backup.unwrap();
        let k = Key::open(Hive::CurrentUser, &run_like).unwrap();
        assert!(k.get(&gone).is_none());
        assert!(k.get(&cmd).is_some(), "entries for existing programs stay");

        reg::import(&backup).unwrap();
        let k = Key::open(Hive::CurrentUser, &run_like).unwrap();
        assert_eq!(k.get(&gone).unwrap().data, vec![1, 2, 3], "restored from the backup");

        reg::delete_tree(Hive::CurrentUser, &base).unwrap();
        assert!(Key::open(Hive::CurrentUser, &base).is_none());
        let _ = std::fs::remove_file(backup);
    }
}
