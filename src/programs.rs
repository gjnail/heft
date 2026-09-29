//! Installed programs: listing, measuring, uninstalling, and finding what an
//! uninstaller left behind.

use std::collections::{HashMap, HashSet};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::reg::{self, Hive, Key, RegExport};
use crate::winsys::{self, PathState};

const UNINSTALL_KEYS: [(Hive, &str); 3] = [
    (Hive::LocalMachine, r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"),
    (Hive::LocalMachine, r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"),
    (Hive::CurrentUser, r"Software\Microsoft\Windows\CurrentVersion\Uninstall"),
];

#[derive(Clone, Debug)]
pub struct Program {
    pub name: String,
    pub publisher: String,
    pub version: String,
    /// yyyy-mm-dd, when the installer recorded it.
    pub installed: String,
    /// What the installer claims, in bytes.
    pub estimated: u64,
    /// Measured size of the install folder, filled in later.
    pub measured: Option<u64>,
    pub location: Option<String>,
    /// `location` came from the installer's own `InstallLocation`, rather
    /// than being inferred from the uninstaller or icon path.
    pub location_recorded: bool,
    pub uninstall: String,
    pub hive: Hive,
    /// Full registry path of the program's uninstall key.
    pub key: String,
}

impl Program {
    pub fn size(&self) -> u64 {
        self.measured.filter(|&m| m > 0).unwrap_or(self.estimated)
    }

    pub fn is_msi(&self) -> bool {
        self.uninstall.to_ascii_lowercase().contains("msiexec")
    }

    /// The uninstaller exists (or can't be checked, like MSI).
    pub fn uninstaller_state(&self) -> PathState {
        if self.is_msi() {
            return PathState::Unknown;
        }
        match winsys::command_target(&self.uninstall) {
            Some(t) => winsys::path_state(&t),
            None => PathState::Unknown,
        }
    }
}

fn fmt_install_date(raw: &str) -> String {
    let d: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if d.len() == 8 { format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..]) } else { String::new() }
}

/// Folders that are too general to be one program's install folder.
fn is_generic_folder(p: &str) -> bool {
    let p = p.trim_end_matches('\\').to_lowercase();
    let env = |k: &str| std::env::var(k).unwrap_or_default().trim_end_matches('\\').to_lowercase();
    let generic = [
        env("ProgramFiles"),
        env("ProgramFiles(x86)"),
        env("ProgramData"),
        env("APPDATA"),
        env("LOCALAPPDATA"),
        format!(r"{}\programs", env("LOCALAPPDATA")),
        env("USERPROFILE"),
        env("SystemRoot"),
        format!(r"{}\system32", env("SystemRoot")),
        format!(r"{}\syswow64", env("SystemRoot")),
    ];
    p.len() <= 3
        || generic.contains(&p)
        || p.starts_with(&format!(r"{}\", env("SystemRoot")))
        || p.contains(r"\package cache\")
        || p.contains(r"\installer\")
        || p.ends_with(r"\package cache")
}

/// The install folder, and whether the installer recorded it: its
/// `InstallLocation`, else the folder of the icon or uninstaller when that
/// looks like the program's own folder.
fn install_folder(k: &Key) -> Option<(String, bool)> {
    let usable = |p: String| {
        let p = clean_path(&winsys::expand_env(p.trim().trim_matches('"')));
        (!p.is_empty() && !is_generic_folder(&p) && Path::new(&p).is_dir()).then_some(p)
    };
    if let Some(p) = k.get_string("InstallLocation").and_then(usable) {
        return Some((p, true));
    }
    // `steam.exe steam://uninstall/123`: the uninstaller belongs to a
    // launcher whose folder holds many programs.
    if k.get_string("UninstallString").is_some_and(|u| u.contains("://")) {
        return None;
    }
    for v in ["DisplayIcon", "UninstallString"] {
        let Some(cmd) = k.get_string(v) else { continue };
        // DisplayIcon is "path,index".
        let cmd = if v == "DisplayIcon" { cmd.split(',').next().unwrap_or_default() } else { cmd.as_str() };
        let Some(target) = winsys::command_target(cmd) else { continue };
        if let Some(parent) = Path::new(&target).parent().map(|p| p.to_string_lossy().into_owned())
            && let Some(p) = usable(parent)
        {
            return Some((p, false));
        }
    }
    None
}

/// Installed programs as Apps & Features shows them.
pub fn list() -> Vec<Program> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (hive, root) in UNINSTALL_KEYS {
        let Some(rk) = Key::open(hive, root) else { continue };
        for sub in rk.subkeys() {
            let path = format!(r"{root}\{sub}");
            let Some(k) = Key::open(hive, &path) else { continue };
            let Some(name) = k.get_string("DisplayName") else { continue };
            let Some(uninstall) = k.get_string("UninstallString") else { continue };
            // Updates, components and hidden entries aren't programs.
            if k.get_dword("SystemComponent") == Some(1)
                || k.get_string("ParentKeyName").is_some()
                || k.get_string("ReleaseType").is_some_and(|t| t.contains("Update") || t.contains("Hotfix"))
            {
                continue;
            }
            let version = k.get_string("DisplayVersion").unwrap_or_default();
            if !seen.insert((name.to_lowercase(), version.clone())) {
                continue;
            }
            let folder = install_folder(&k);
            out.push(Program {
                publisher: k.get_string("Publisher").unwrap_or_default(),
                installed: k.get_string("InstallDate").map(|d| fmt_install_date(&d)).unwrap_or_default(),
                estimated: k.get_dword("EstimatedSize").unwrap_or(0) as u64 * 1024,
                measured: None,
                location_recorded: folder.as_ref().is_some_and(|f| f.1),
                location: folder.map(|f| f.0),
                name,
                version,
                uninstall,
                hive,
                key: path,
            });
        }
    }
    // An inferred folder that several programs point at is a shared
    // launcher or runtime folder, not any one program's.
    let mut uses: HashMap<String, usize> = HashMap::new();
    for p in &out {
        if let Some(l) = &p.location {
            *uses.entry(l.to_lowercase()).or_default() += 1;
        }
    }
    for p in &mut out {
        if !p.location_recorded && p.location.as_ref().is_some_and(|l| uses[&l.to_lowercase()] > 1) {
            p.location = None;
        }
    }
    out.sort_by_key(|p| p.name.to_lowercase());
    out
}

/// Installers also write `E:/Games/X` or doubled backslashes; use one form
/// so paths compare.
fn clean_path(p: &str) -> String {
    let p = p.replace('/', "\\");
    // Keep a UNC path's leading `\\`.
    let (head, rest) = p.split_at(p.len().min(2));
    let mut out = head.to_string();
    for c in rest.chars() {
        if !(c == '\\' && out.ends_with('\\')) {
            out.push(c);
        }
    }
    out.trim_end_matches('\\').to_string()
}

fn norm(p: &str) -> String {
    p.trim_end_matches('\\').to_lowercase()
}

/// `inner` is `outer` or somewhere below it.
fn within(inner: &str, outer: &str) -> bool {
    inner == outer || inner.starts_with(&format!("{outer}\\"))
}

/// Install folders of other programs that sit inside `p`'s folder (Steam
/// libraries, bundled apps), so its size doesn't include theirs.
pub fn nested_locations(p: &Program, all: &[Program]) -> Vec<String> {
    let Some(own) = p.location.as_deref().map(norm) else { return Vec::new() };
    all.iter()
        .filter(|o| o.key != p.key)
        .filter_map(|o| o.location.as_deref().map(norm))
        .filter(|l| *l != own && within(l, &own))
        .collect()
}

/// Size of a folder without the listed subfolders (lower-case paths).
pub fn folder_size_excluding(dir: &Path, skip: &[String]) -> u64 {
    if skip.is_empty() {
        return folder_size(dir);
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let entries: Vec<_> = rd.flatten().collect();
    entries
        .par_iter()
        .map(|e| {
            let Ok(md) = e.metadata() else { return 0 };
            let path = e.path();
            let key = norm(&path.to_string_lossy());
            if md.file_attributes() & 0x400 != 0 || skip.contains(&key) {
                0
            } else if md.is_dir() {
                let deeper: Vec<String> = skip.iter().filter(|s| within(s, &key)).cloned().collect();
                folder_size_excluding(&path, &deeper)
            } else {
                md.len()
            }
        })
        .sum()
}

/// Total size of a folder, in parallel, never following links.
pub fn folder_size(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let entries: Vec<_> = rd.flatten().collect();
    entries
        .par_iter()
        .map(|e| {
            let Ok(md) = e.metadata() else { return 0 };
            if md.file_attributes() & 0x400 != 0 {
                0
            } else if md.is_dir() {
                folder_size(&e.path())
            } else {
                md.len()
            }
        })
        .sum()
}

/// Program + arguments to run its uninstaller. MSI entries registered with
/// `/I` (repair) are switched to `/X` (remove).
pub fn uninstall_command(p: &Program) -> (String, String) {
    let (file, args) = winsys::split_command(&winsys::expand_env(&p.uninstall));
    if p.is_msi() {
        let fixed = args.replacen("/I{", "/X{", 1).replacen("/i{", "/X{", 1);
        return (file, fixed);
    }
    (file, args)
}

/// Start the uninstaller and wait for it to exit (on the calling thread).
pub fn run_uninstaller(p: &Program) -> Result<(), String> {
    let (file, args) = uninstall_command(p);
    match winsys::launch(&file, &args, false)? {
        Some(proc) => {
            proc.wait();
            Ok(())
        }
        None => Ok(()),
    }
}

/// Whether the program's uninstall entry is still registered.
pub fn still_installed(p: &Program) -> bool {
    Key::open(p.hive, &p.key).is_some()
}

/// Remove an entry whose uninstaller is gone (after backing it up).
pub fn remove_entry(p: &Program) -> Result<PathBuf, String> {
    let mut backup = RegExport::default();
    backup.add_key(p.hive, &p.key);
    if backup.is_empty() {
        return Err("the entry is already gone".into());
    }
    let file = reg::new_backup_path("uninstall-entry");
    backup.save(&file).map_err(|e| format!("could not save a backup: {e}"))?;
    reg::delete_tree(p.hive, &p.key)?;
    Ok(file)
}

// ----------------------------------------------------------------------
// Leftovers

#[derive(Clone, Debug)]
pub struct Leftover {
    pub path: PathBuf,
    pub size: u64,
    /// Why Heft thinks it belongs to the program.
    pub reason: &'static str,
    /// Ticked by default: only for the program's own recorded install folder.
    pub confident: bool,
}

/// Names too general to match a folder on (they'd hit shared folders).
const TOO_GENERAL: [&str; 16] = [
    "microsoft", "windows", "common files", "programs", "temp", "google", "adobe", "intel", "nvidia", "amd",
    "packages", "microsoft corporation", "installer", "app", "update", "system",
];

/// "Foo 2.1.3 (x64)" → "Foo".
pub fn base_name(name: &str) -> String {
    let mut s = name.trim().to_string();
    for suffix in ["(x64)", "(x86)", "(64-bit)", "(32-bit)", "x64", "x86", "64-bit", "32-bit"] {
        if s.to_lowercase().ends_with(suffix) {
            s.truncate(s.len() - suffix.len());
            s = s.trim().to_string();
        }
    }
    while let Some((head, last)) = s.rsplit_once(' ') {
        let is_version = last.trim_start_matches(['v', 'V']).chars().all(|c| c.is_ascii_digit() || c == '.') && last.chars().any(|c| c.is_ascii_digit());
        if !is_version {
            break;
        }
        s = head.trim_end_matches([' ', '-']).to_string();
    }
    s
}

/// Folders an uninstaller may have left: the install folder, and folders
/// named exactly after the program (or publisher\program) in the usual
/// data locations.
///
/// `installed` is the list of programs still installed. A folder is never
/// suggested if it holds, or sits inside, one of their install folders or
/// uninstallers. A program's folder can be shared (a launcher's library, a
/// suite), and deleting it would break what's left.
pub fn find_leftovers(p: &Program, installed: &[Program]) -> Vec<Leftover> {
    let mut folders: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for o in installed.iter().filter(|o| o.key != p.key) {
        if let Some(l) = &o.location {
            folders.push(norm(l));
        }
        if let Some(t) = winsys::command_target(&o.uninstall) {
            files.push(norm(&winsys::expand_env(&t)));
        }
    }
    let in_use = |c: &str| {
        folders.iter().any(|f| within(f, c) || within(c, f)) || files.iter().any(|f| within(f, c))
    };
    let mut out: Vec<Leftover> = Vec::new();
    let mut push = |path: PathBuf, reason: &'static str, confident: bool| {
        let s = path.to_string_lossy().to_string();
        if path.is_dir()
            && !is_generic_folder(&s)
            && !crate::platform::is_protected_path(&s)
            && !in_use(&norm(&s))
            && !out.iter().any(|l| l.path == path)
        {
            out.push(Leftover { path, size: 0, reason, confident });
        }
    };
    if let Some(loc) = &p.location {
        // Only a folder the installer itself recorded is ticked by default.
        push(PathBuf::from(loc), "install folder", p.location_recorded);
    }
    let names: Vec<String> = [p.name.clone(), base_name(&p.name)]
        .into_iter()
        .filter(|n| n.len() >= 4 && !TOO_GENERAL.contains(&n.to_lowercase().as_str()))
        .collect();
    let publisher = p.publisher.trim().trim_end_matches('.').to_string();
    let roots = ["APPDATA", "LOCALAPPDATA", "ProgramData", "ProgramFiles", "ProgramFiles(x86)"];
    for root in roots.iter().filter_map(|r| winsys::env_path(r)) {
        for n in &names {
            push(root.join(n), "named after the program", false);
            if !publisher.is_empty() {
                push(root.join(&publisher).join(n), "named after the program", false);
            }
        }
    }
    out.par_iter_mut().for_each(|l| l.size = folder_size(&l.path));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_versions() {
        assert_eq!(base_name("7-Zip 24.08 (x64)"), "7-Zip");
        assert_eq!(base_name("Git version 2.45.1"), "Git version");
        assert_eq!(base_name("Mozilla Firefox (x64 en-US)"), "Mozilla Firefox (x64 en-US)");
        assert_eq!(base_name("Python 3.12.4 (64-bit)"), "Python");
        assert_eq!(base_name("Notepad++"), "Notepad++");
        assert_eq!(base_name("Foo v1.2"), "Foo");
    }

    #[test]
    fn msi_repair_becomes_remove() {
        let mut p = Program {
            name: "X".into(),
            publisher: String::new(),
            version: String::new(),
            installed: String::new(),
            estimated: 0,
            measured: None,
            location: None,
            location_recorded: false,
            uninstall: "MsiExec.exe /I{1234-5678}".into(),
            hive: Hive::LocalMachine,
            key: String::new(),
        };
        assert_eq!(uninstall_command(&p), ("MsiExec.exe".into(), "/X{1234-5678}".into()));
        p.uninstall = r#""C:\Program Files\X\unins000.exe" /LOG"#.into();
        assert_eq!(uninstall_command(&p), (r"C:\Program Files\X\unins000.exe".into(), "/LOG".into()));
    }

    fn program(name: &str, location: Option<&str>, uninstall: &str) -> Program {
        Program {
            name: name.into(),
            publisher: String::new(),
            version: String::new(),
            installed: String::new(),
            estimated: 0,
            measured: None,
            location: location.map(str::to_string),
            location_recorded: true,
            uninstall: uninstall.into(),
            hive: Hive::CurrentUser,
            key: format!(r"Software\x\{name}"),
        }
    }

    #[test]
    fn leftovers_never_include_other_programs() {
        let base = std::env::temp_dir().join(format!("heft-leftovers-{}", std::process::id()));
        let launcher = base.join("Launcher");
        let game = launcher.join("games").join("Game");
        let solo = base.join("Solo App");
        for d in [&game, &solo] {
            std::fs::create_dir_all(d).unwrap();
        }
        let s = |p: &PathBuf| p.to_string_lossy().into_owned();
        let launcher_exe = format!(r#""{}\launcher.exe""#, s(&launcher));

        // A launcher-managed program whose "install folder" is the launcher's.
        let game_entry = program("Game", Some(&s(&launcher)), &format!("{launcher_exe} launcher://uninstall/1"));
        let launcher_entry = program("Launcher", None, &launcher_exe);
        let found = find_leftovers(&game_entry, std::slice::from_ref(&launcher_entry));
        assert!(found.is_empty(), "the launcher's folder holds its uninstaller: {found:?}");

        // Nested install folders are protected too.
        let outer = program("Outer", Some(&s(&launcher)), "x.exe");
        let inner = program("Inner", Some(&s(&game)), "y.exe");
        assert!(find_leftovers(&outer, std::slice::from_ref(&inner)).is_empty());
        assert!(find_leftovers(&inner, &[outer]).iter().all(|l| l.path != launcher));

        // A program with its own folder gets it suggested, ticked.
        let solo_entry = program("Solo App", Some(&s(&solo)), "z.exe");
        let found = find_leftovers(&solo_entry, &[launcher_entry, inner]);
        assert!(found.iter().any(|l| l.path == solo && l.confident));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn size_excludes_nested_programs() {
        let base = std::env::temp_dir().join(format!("heft-nested-{}", std::process::id()));
        std::fs::create_dir_all(base.join("lib/other")).unwrap();
        std::fs::write(base.join("own.bin"), vec![0u8; 100]).unwrap();
        std::fs::write(base.join("lib/other/game.bin"), vec![0u8; 5000]).unwrap();
        let skip = vec![norm(&base.join("lib").join("other").to_string_lossy())];
        assert_eq!(folder_size(&base), 5100);
        assert_eq!(folder_size_excluding(&base, &skip), 100);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn cleans_paths() {
        assert_eq!(clean_path("E:/Riot Games/League of Legends/"), r"E:\Riot Games\League of Legends");
        assert_eq!(clean_path(r"C:\Program Files (x86)\\World of Warcraft"), r"C:\Program Files (x86)\World of Warcraft");
        assert_eq!(clean_path(r"\\server\share\x"), r"\\server\share\x");
    }

    #[test]
    fn generic_folders() {
        let pf = std::env::var("ProgramFiles").unwrap();
        assert!(is_generic_folder(&pf));
        assert!(is_generic_folder(r"C:\"));
        assert!(is_generic_folder(r"C:\ProgramData\Package Cache\{abc}"));
        assert!(!is_generic_folder(&format!(r"{pf}\7-Zip")));
    }

    #[test]
    fn lists_programs() {
        // Read-only enumeration of this machine.
        let all = list();
        assert!(!all.is_empty());
        assert!(all.iter().all(|p| !p.name.is_empty() && !p.uninstall.is_empty()));
    }
}
