//! macOS helpers shared by the Mac-only tools (login items, apps, broken
//! items, the menu bar icon): property lists, launchd jobs, app bundles,
//! asking for an administrator password, and Heft's own backups.
//!
//! This is the macOS counterpart of `winsys.rs`. Shared code must not call
//! it directly; it's declared with `#[cfg(target_os = "macos")]` in `main.rs`.

pub mod appmenu;
pub mod apps;
pub mod broken;
pub mod icloud;
pub mod menubar;
pub mod notify;
pub mod startup;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

// ----------------------------------------------------------------------
// Property lists

/// A property list file, XML or binary.
pub fn read_plist(path: &Path) -> Option<plist::Value> {
    plist::Value::from_file(path).ok()
}

/// A string value from a dictionary.
pub fn plist_str(dict: &plist::Dictionary, key: &str) -> Option<String> {
    dict.get(key).and_then(|v| v.as_string()).map(str::to_string).filter(|s| !s.is_empty())
}

pub fn plist_bool(dict: &plist::Dictionary, key: &str) -> Option<bool> {
    dict.get(key).and_then(|v| v.as_boolean())
}

// ----------------------------------------------------------------------
// Quoting and running things

/// `s` quoted for `/bin/sh`.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `s` as an AppleScript string literal.
pub fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
}

/// Run an AppleScript and return what it printed. Scripts that talk to other
/// apps (System Events, Finder) make macOS ask the user for permission the
/// first time.
pub fn osascript(script: &str) -> Result<String, String> {
    let out = Command::new("/usr/bin/osascript").args(["-e", script]).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        Err(osascript_error(&String::from_utf8_lossy(&out.stderr)))
    }
}

/// A readable message from osascript's stderr, which looks like
/// `0:42: execution error: User canceled. (-128)`.
fn osascript_error(stderr: &str) -> String {
    let text = stderr.trim();
    if text.ends_with("(-128)") {
        return CANCELLED.into();
    }
    if text.ends_with("(-1743)") {
        return NOT_ALLOWED.into();
    }
    let msg = text.split_once("execution error: ").map_or(text, |(_, m)| m);
    // The error number (AppleScript's, or a shell command's exit status) comes last.
    let msg = match msg.rsplit_once(" (") {
        Some((m, code)) if code.trim_end_matches(')').parse::<i64>().is_ok() => m,
        _ => msg,
    };
    msg.trim().trim_end_matches('.').to_string()
}

/// What [`run_as_admin`] returns when the user closes the password prompt.
pub const CANCELLED: &str = "Cancelled";

/// What [`osascript`] returns when the user said no to (or hasn't allowed)
/// Heft controlling another app (AppleScript error -1743).
pub const NOT_ALLOWED: &str =
    "macOS didn't allow Heft to do this. Allow it in System Settings › Privacy & Security › Automation.";

/// Run a shell command as root, after macOS's own administrator password
/// prompt (the one installers use). `why` finishes the sentence "Heft wants
/// to make changes." in that prompt. Blocks until the command is done.
pub fn run_as_admin(command: &str, why: &str) -> Result<String, String> {
    let script = format!(
        "do shell script {} with prompt {} with administrator privileges",
        applescript_quote(command),
        applescript_quote(&format!("Heft wants to {why}."))
    );
    osascript(&script)
}

/// Run `program args` and return stdout if it succeeded.
pub fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The current user's id.
pub fn uid() -> u32 {
    unsafe { libc::getuid() }
}

/// Run a command in a new Terminal window, so the user can watch it and
/// answer its questions (Homebrew upgrades, uninstall scripts). The command
/// goes in a temporary `.command` file, which Terminal opens without Heft
/// having to script it.
pub fn run_in_terminal(title: &str, command: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join("heft-terminal");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let safe: String = title.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let file = dir.join(format!("{safe}-{}.command", crate::platform::now_unix()));
    let script = format!(
        "#!/bin/zsh -l\nprintf '\\e]0;%s\\a' {}\nclear\n{command}\necho\necho 'Done. You can close this window.'\nrm -f {}\n",
        sh_quote(title),
        sh_quote(&file.to_string_lossy())
    );
    std::fs::write(&file, script).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    let status = Command::new("/usr/bin/open").args(["-a", "Terminal"]).arg(&file).status().map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err("Terminal couldn't be opened".into()) }
}

/// Open a URL or a `x-apple.systempreferences:` pane.
pub fn open_url(url: &str) {
    let _ = Command::new("/usr/bin/open").arg(url).spawn();
}

/// System Settings › Privacy & Security › Full Disk Access.
pub const FULL_DISK_ACCESS_SETTINGS: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

/// Whether Heft may look inside protected folders (the Trash, Mail, Safari,
/// other apps' data). `None` if it can't tell. Reading a protected folder
/// without permission just fails; it doesn't ask.
pub fn has_full_disk_access() -> Option<bool> {
    let trash = PathBuf::from(std::env::var_os("HOME")?).join(".Trash");
    match std::fs::read_dir(trash) {
        Ok(_) => Some(true),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Some(false),
        Err(_) => None,
    }
}

// ----------------------------------------------------------------------
// launchd jobs (launch agents and daemons)

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Domain {
    /// `~/Library/LaunchAgents`: runs as you, when you sign in.
    UserAgent,
    /// `/Library/LaunchAgents`: runs as every user who signs in.
    Agent,
    /// `/Library/LaunchDaemons`: runs as root at startup.
    Daemon,
}

impl Domain {
    pub const ALL: [Domain; 3] = [Domain::UserAgent, Domain::Agent, Domain::Daemon];

    pub fn folder(self) -> Option<PathBuf> {
        match self {
            Domain::UserAgent => crate::platform::home_dir().map(|h| PathBuf::from(h).join("Library/LaunchAgents")),
            Domain::Agent => Some(PathBuf::from("/Library/LaunchAgents")),
            Domain::Daemon => Some(PathBuf::from("/Library/LaunchDaemons")),
        }
    }

    /// The `launchctl` domain its jobs run in.
    pub fn target(self) -> String {
        match self {
            Domain::UserAgent | Domain::Agent => format!("gui/{}", uid()),
            Domain::Daemon => "system".into(),
        }
    }
}

/// One launch agent or daemon, from its `.plist`.
#[derive(Clone, Debug)]
pub struct LaunchJob {
    pub plist: PathBuf,
    pub domain: Domain,
    pub label: String,
    /// `Program`, or the first of `ProgramArguments`.
    pub program: Option<String>,
    pub args: Vec<String>,
    /// Starts when it's loaded (at sign-in or startup).
    pub run_at_load: bool,
    /// Restarted whenever it quits.
    pub keep_alive: bool,
    /// Runs on a timer or calendar schedule.
    pub scheduled: bool,
    /// Started on demand (sockets, Mach services, watched paths).
    pub on_demand: bool,
    /// `Disabled` set in the plist itself.
    pub disabled_in_plist: bool,
    /// App bundles this job belongs to (`AssociatedBundleIdentifiers`).
    pub bundles: Vec<String>,
}

impl LaunchJob {
    /// The command line, for display.
    pub fn command(&self) -> String {
        if !self.args.is_empty() {
            let mut parts: Vec<String> = self.args.iter().map(|a| if a.contains(' ') { sh_quote(a) } else { a.clone() }).collect();
            if let Some(p) = &self.program
                && self.args.first() != Some(p)
            {
                parts.insert(0, p.clone());
            }
            parts.join(" ")
        } else {
            self.program.clone().unwrap_or_default()
        }
    }
}

/// Read one launchd plist. `None` if it isn't one.
pub fn parse_launch_job(path: &Path, domain: Domain) -> Option<LaunchJob> {
    let value = read_plist(path)?;
    let dict = value.as_dictionary()?;
    let label = plist_str(dict, "Label")?;
    let args: Vec<String> = dict
        .get("ProgramArguments")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_string().map(str::to_string)).collect())
        .unwrap_or_default();
    let program = plist_str(dict, "Program").or_else(|| args.first().cloned());
    let keep_alive = match dict.get("KeepAlive") {
        Some(plist::Value::Boolean(b)) => *b,
        Some(plist::Value::Dictionary(_)) => true,
        _ => false,
    };
    let has = |k: &str| dict.contains_key(k);
    let bundles = dict
        .get("AssociatedBundleIdentifiers")
        .map(|v| match v {
            plist::Value::String(s) => vec![s.clone()],
            plist::Value::Array(a) => a.iter().filter_map(|v| v.as_string().map(str::to_string)).collect(),
            _ => Vec::new(),
        })
        .unwrap_or_default();
    Some(LaunchJob {
        plist: path.to_path_buf(),
        domain,
        label,
        program,
        args,
        run_at_load: plist_bool(dict, "RunAtLoad").unwrap_or(false),
        keep_alive,
        scheduled: has("StartInterval") || has("StartCalendarInterval"),
        on_demand: has("Sockets") || has("MachServices") || has("WatchPaths") || has("QueueDirectories") || has("StartOnMount") || has("LaunchEvents"),
        disabled_in_plist: plist_bool(dict, "Disabled").unwrap_or(false),
        bundles,
    })
}

/// Every launch agent and daemon installed outside the system (`/System`
/// is Apple's and read-only).
pub fn launch_jobs() -> Vec<LaunchJob> {
    let mut out = Vec::new();
    for domain in Domain::ALL {
        let Some(dir) = domain.folder() else { continue };
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut paths: Vec<PathBuf> =
            rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "plist")).collect();
        paths.sort();
        out.extend(paths.iter().filter_map(|p| parse_launch_job(p, domain)));
    }
    out
}

/// launchd's own on/off switches for a domain (`gui/501`, `system`): label →
/// disabled. Labels that aren't listed follow the plist's `Disabled` key.
pub fn disabled_overrides(target: &str) -> HashMap<String, bool> {
    output("/bin/launchctl", &["print-disabled", target]).map(|t| parse_disabled(&t)).unwrap_or_default()
}

/// Parse `launchctl print-disabled`: lines like `"com.foo" => disabled`
/// (older systems print `=> true`).
pub fn parse_disabled(text: &str) -> HashMap<String, bool> {
    text.lines()
        .filter_map(|l| {
            let (name, state) = l.trim().split_once("=>")?;
            let name = name.trim().trim_matches('"');
            let disabled = match state.trim() {
                "disabled" | "true" => true,
                "enabled" | "false" => false,
                _ => return None,
            };
            (!name.is_empty()).then(|| (name.to_string(), disabled))
        })
        .collect()
}

/// Heft's own launch agents (Start at login, weekly cleaning) run Heft by
/// its path, so they stop working when Heft.app is moved. Where the agent
/// should point now: this copy of Heft, when the program the job runs is
/// gone, this copy isn't a temporary one macOS made, and, when the job
/// names the app it belongs to, this is that app.
pub fn moved_heft(job: &LaunchJob) -> Option<PathBuf> {
    let exe = std::env::current_exe().and_then(std::fs::canonicalize).ok()?;
    let ours = enclosing_app(&exe).and_then(|a| bundle_info(&a)).and_then(|b| b.id);
    follows_heft(job, &exe, ours.as_deref()).then_some(exe)
}

fn follows_heft(job: &LaunchJob, exe: &Path, bundle_id: Option<&str>) -> bool {
    let Some(program) = job.program.as_deref() else { return false };
    if Path::new(program).exists() || Path::new(program) == exe || exe.to_string_lossy().contains("/AppTranslocation/") {
        return false;
    }
    job.bundles.first().is_none_or(|id| Some(id.as_str()) == bundle_id)
}

/// Point Heft's launch agents at this copy of Heft if it was moved since
/// they were set up. Quietly: it's what the settings meant.
pub fn follow_move() {
    menubar::follow_move();
    crate::clean::follow_move();
}

// ----------------------------------------------------------------------
// App bundles

#[derive(Clone, Debug, Default)]
pub struct Bundle {
    /// Display name, falling back to the bundle's file name without `.app`.
    pub name: String,
    pub id: Option<String>,
    pub version: Option<String>,
    pub copyright: Option<String>,
}

/// Read `Contents/Info.plist` of an app (or any) bundle.
pub fn bundle_info(path: &Path) -> Option<Bundle> {
    let file_name = path.file_stem()?.to_string_lossy().into_owned();
    let info = read_plist(&path.join("Contents/Info.plist"));
    let dict = info.as_ref().and_then(|v| v.as_dictionary());
    let get = |k: &str| dict.and_then(|d| plist_str(d, k));
    let name = get("CFBundleDisplayName").or_else(|| get("CFBundleName")).unwrap_or_else(|| file_name.clone());
    Some(Bundle {
        // Some apps leave their display name as a build variable.
        name: if name.contains("$(") { file_name } else { name },
        id: get("CFBundleIdentifier"),
        version: get("CFBundleShortVersionString").or_else(|| get("CFBundleVersion")),
        copyright: get("NSHumanReadableCopyright"),
    })
}

/// The `.app` bundle a path is inside, if any (`/Applications/X.app/Contents/MacOS/x` → `/Applications/X.app`).
pub fn enclosing_app(path: &Path) -> Option<PathBuf> {
    path.ancestors().find(|a| a.extension().is_some_and(|e| e.eq_ignore_ascii_case("app"))).map(Path::to_path_buf)
}

// ----------------------------------------------------------------------
// Backups of anything Heft changes (plists, login items)

/// Where Heft keeps backups of launchd plists and other settings it changes.
pub fn backup_dir() -> PathBuf {
    data_dir().join("backups")
}

/// Heft's data folder, or `HEFT_DATA_DIR` (so tests don't touch your own,
/// as with the Removed list).
pub fn data_dir() -> PathBuf {
    std::env::var_os("HEFT_DATA_DIR").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(crate::platform::data_dir)
}

/// A fresh, dated path in the backup folder: `<prefix>-<date>-<n>.<ext>`.
pub fn new_backup_path(prefix: &str, ext: &str) -> PathBuf {
    let dir = backup_dir();
    let _ = std::fs::create_dir_all(&dir);
    let now = crate::platform::now_unix();
    let date = crate::platform::local_time(now)
        .map(|t| format!("{:04}{:02}{:02}-{:02}{:02}", t.year, t.month, t.day, t.hour, t.minute))
        .unwrap_or_else(|| now.to_string());
    let safe: String = prefix.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect();
    (0..)
        .map(|n| dir.join(if n == 0 { format!("{safe}-{date}.{ext}") } else { format!("{safe}-{date}-{n}.{ext}") }))
        .find(|p| !p.exists())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(applescript_quote(r#"say "hi" \ bye"#), r#""say \"hi\" \\ bye""#);
    }

    #[test]
    fn osascript_errors() {
        assert_eq!(osascript_error("0:10: execution error: User canceled. (-128)\n"), CANCELLED);
        assert_eq!(osascript_error("0:10: execution error: rm: /x: Permission denied (1)\n"), "rm: /x: Permission denied");
    }

    #[test]
    fn disabled_list() {
        let text = "\tdisabled services = {\n\t\t\"com.a\" => enabled\n\t\t\"com.b\" => disabled\n\t\t\"com.c\" => true\n\t}\n";
        let m = parse_disabled(text);
        assert_eq!(m.get("com.a"), Some(&false));
        assert_eq!(m.get("com.b"), Some(&true));
        assert_eq!(m.get("com.c"), Some(&true));
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn agents_follow_a_moved_heft() {
        let job = |program: &str, bundle: Option<&str>| LaunchJob {
            plist: PathBuf::from("/tmp/x.plist"),
            domain: Domain::UserAgent,
            label: "io.github.gjnail.heft".into(),
            program: Some(program.into()),
            args: vec![program.into(), "--tray".into()],
            run_at_load: true,
            keep_alive: false,
            scheduled: false,
            on_demand: false,
            disabled_in_plist: false,
            bundles: bundle.map(|b| vec![b.to_string()]).unwrap_or_default(),
        };
        let gone = "/Applications/Old place/Heft.app/Contents/MacOS/heft";
        let now = Path::new("/Applications/Heft.app/Contents/MacOS/heft");
        let id = Some("io.github.gjnail.heft");
        assert!(follows_heft(&job(gone, id), now, id));
        assert!(follows_heft(&job(gone, None), now, None), "a bare binary names no app");
        assert!(!follows_heft(&job("/bin/sh", id), now, id), "still there");
        assert!(!follows_heft(&job(gone, id), now, Some("local.heft.Heft")), "another build of Heft");
        assert!(!follows_heft(&job(gone, id), now, None), "not an app at all, like cargo run");
        let translocated = Path::new("/private/var/folders/x/AppTranslocation/1234/d/Heft.app/Contents/MacOS/heft");
        assert!(!follows_heft(&job(gone, id), translocated, id), "a temporary copy");
    }

    #[test]
    fn launch_job_plist() {
        let dir = std::env::temp_dir().join(format!("heft-launchd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("com.example.agent.plist");
        std::fs::write(
            &p,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.example.agent</string>
<key>ProgramArguments</key><array><string>/Applications/Example.app/Contents/MacOS/helper</string><string>--quiet</string></array>
<key>RunAtLoad</key><true/>
<key>StartInterval</key><integer>3600</integer>
</dict></plist>"#,
        )
        .unwrap();
        let j = parse_launch_job(&p, Domain::UserAgent).unwrap();
        assert_eq!(j.label, "com.example.agent");
        assert_eq!(j.program.as_deref(), Some("/Applications/Example.app/Contents/MacOS/helper"));
        assert!(j.run_at_load && j.scheduled && !j.keep_alive);
        assert_eq!(j.command(), "/Applications/Example.app/Contents/MacOS/helper --quiet");
        assert_eq!(enclosing_app(Path::new(j.program.as_ref().unwrap())), Some(PathBuf::from("/Applications/Example.app")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
