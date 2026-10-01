//! What apps deleted long ago (or dragged to the Trash without Heft) left
//! in `~/Library`: data containers and the files named after the same app.
//!
//! This has to be strict, since there's no uninstall to go by. An app's
//! things are only listed when:
//!
//! - its bundle id is known to have been an app's: it has a data container
//!   (only sandboxed apps, extensions and services get one) or saved window
//!   state (only apps with windows have it), or Launch Services remembers an
//!   app with that id whose bundle is gone;
//! - no app on this Mac has that id, or is its extension, helper or parent
//!   (`com.foo.app.helper` belongs to `com.foo.app`): not the apps Launch
//!   Services knows anywhere (including on disks that aren't connected), not
//!   the extensions and helpers inside them, not anything running;
//! - it isn't Apple's (`com.apple.…`).
//!
//! Only items named exactly after the bundle id are listed, never by app
//! name, and group containers are left alone: several apps from one
//! developer share them, and nothing on disk says which. Nothing is ticked
//! by default.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use super::{id_hit, key_for, needs_admin, size_on_disk, Leftover, Libraries, Style};

/// The data one deleted app left.
#[derive(Clone, Debug)]
pub struct Orphan {
    /// The bundle id, as written on disk.
    pub id: String,
    /// The app's name, if Launch Services still remembers it.
    pub name: Option<String>,
    pub items: Vec<Leftover>,
}

impl Orphan {
    pub fn size(&self) -> u64 {
        self.items.iter().map(|l| l.size).sum()
    }

    /// Every item's size is known.
    pub fn measured(&self) -> bool {
        self.items.iter().all(|l| l.measured)
    }

    pub fn title(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }
}

/// Where to look, and how items there are named. Group containers are left
/// out on purpose (see the module comment).
const PLACES: [(&str, Style); 11] = [
    ("Containers", Style::Id),
    ("Application Support", Style::IdOrName),
    ("Caches", Style::IdOrName),
    ("Logs", Style::IdOrName),
    ("Application Scripts", Style::Id),
    ("Preferences", Style::Plist),
    ("Preferences/ByHost", Style::ByHost),
    ("Saved Application State", Style::SavedState),
    ("HTTPStorages", Style::Id),
    ("WebKit", Style::Id),
    ("Cookies", Style::Cookies),
];

/// Bundle ids of everything that counts as installed, lower-case.
#[derive(Default)]
pub struct Installed {
    ids: HashSet<String>,
}

impl Installed {
    pub fn new(ids: impl IntoIterator<Item = String>) -> Installed {
        Installed { ids: ids.into_iter().map(|i| i.to_lowercase()).collect() }
    }

    /// `id` is installed, or is an extension or helper of something
    /// installed, or has one installed.
    fn covers(&self, id: &str) -> bool {
        self.ids.contains(id) || self.ids.iter().any(|i| id_hit(id, i).is_some() || id_hit(i, id).is_some())
    }
}

/// A plausible bundle id: reverse DNS with at least three parts, like
/// `com.example.app`. Vendor folders (`com.example`) and names don't count.
fn looks_like_id(key: &str) -> bool {
    let parts: Vec<&str> = key.split('.').collect();
    parts.len() >= 3
        && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        && parts[0].chars().all(|c| c.is_ascii_alphabetic())
}

/// Leftovers of deleted apps in `~/Library`. `app_data`: Heft has Full Disk
/// Access, so data containers can be looked inside and measured. Asks Launch
/// Services and reads every app's bundle, so it takes a few seconds; call it
/// off the UI thread.
pub fn find(app_data: bool) -> Vec<Orphan> {
    let Some(libs) = Libraries::real() else { return Vec::new() };
    let registered = crate::mac::broken::registered_apps();
    let (installed, remembered) = installed_now(&registered);
    find_in(&libs, &installed, &remembered, app_data)
}

/// What's installed: every app Launch Services knows whose bundle is there
/// (or on a disk that isn't connected), with its extensions and the bundles
/// inside it, the apps in the Applications folders and macOS's own, and
/// whatever is running. Also the names of deleted apps Launch Services
/// still remembers, by lower-case bundle id.
fn installed_now(registered: &[crate::mac::broken::Registered]) -> (Installed, HashMap<String, String>) {
    let mut ids = Vec::new();
    let mut remembered = HashMap::new();
    let mut bundles = Vec::new();
    for r in registered {
        let gone = r.mounted && std::fs::symlink_metadata(&r.path).is_err();
        if gone {
            if let (Some(id), Some(name)) = (&r.id, &r.name) {
                remembered.insert(id.to_lowercase(), name.clone());
            }
            continue;
        }
        ids.extend(r.id.clone());
        ids.extend(r.plugins.iter().cloned());
        if r.mounted && !r.path.starts_with("/System/") {
            bundles.push(PathBuf::from(&r.path));
        }
    }
    for a in super::list() {
        ids.extend(a.id.clone());
        bundles.push(a.path);
    }
    ids.extend(super::system_apps().into_iter().filter_map(|i| i.id));
    let nested: Vec<String> = bundles.par_iter().flat_map_iter(|b| nested_ids(b)).collect();
    ids.extend(nested);
    ids.extend(running_ids());
    // Anything installed that happens to be remembered as gone somewhere else.
    let installed = Installed::new(ids);
    remembered.retain(|id, _| !installed.covers(id));
    (installed, remembered)
}

/// Bundle ids of the apps, extensions and services inside an app.
fn nested_ids(app: &Path) -> Vec<String> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let p = e.path();
            let ext = p.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
            if matches!(ext.as_str(), "app" | "appex" | "xpc" | "systemextension") {
                out.extend(crate::mac::bundle_info(&p).and_then(|b| b.id));
                walk(&p.join("Contents"), depth + 1, out);
            } else if depth < 6 && !matches!(ext.as_str(), "framework" | "lproj" | "bundle") {
                walk(&p, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(&app.join("Contents"), 0, &mut out);
    out
}

/// Bundle ids of the apps and extensions running now.
fn running_ids() -> Vec<String> {
    let mut apps: HashSet<PathBuf> = HashSet::new();
    for p in super::process_paths() {
        // The innermost bundle the program is in.
        if let Some(b) = p.ancestors().find(|a| {
            a.extension().is_some_and(|e| ["app", "appex", "xpc"].iter().any(|x| e.eq_ignore_ascii_case(x)))
        }) {
            apps.insert(b.to_path_buf());
        }
    }
    apps.iter().filter_map(|a| crate::mac::bundle_info(a).and_then(|b| b.id)).collect()
}

fn find_in(libs: &Libraries, installed: &Installed, remembered: &HashMap<String, String>, app_data: bool) -> Vec<Orphan> {
    // Every id-named item in the usual places, by lower-case id.
    let mut by_id: HashMap<String, (String, Vec<(PathBuf, Style)>)> = HashMap::new();
    for (place, style) in PLACES {
        let dir = libs.user.join(place);
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Some(key) = key_for(&name, style) else { continue };
            if !looks_like_id(&key) || key.starts_with("com.apple.") {
                continue;
            }
            // `<id>.binarycookies` in HTTPStorages sits beside the `<id>` folder.
            let id_as_written = name.split_at(key.len().min(name.len())).0.to_string();
            by_id.entry(key).or_insert_with(|| (id_as_written, Vec::new())).1.push((e.path(), style));
        }
    }

    let mut out = Vec::new();
    for (key, (id, paths)) in by_id {
        let was_app = is_container_or_state(libs, &paths) || remembered.contains_key(&key);
        if !was_app || installed.covers(&key) {
            continue;
        }
        let items = paths
            .into_iter()
            .map(|(path, _)| {
                let reason = if path.starts_with(libs.user.join("Containers")) {
                    "data container of an app that's no longer installed"
                } else {
                    "named after an app that's no longer installed"
                };
                let admin = needs_admin(&path);
                Leftover { path, size: 0, reason, confident: false, admin, measured: false }
            })
            .collect();
        out.push(Orphan { id, name: remembered.get(&key).cloned(), items });
    }

    out.par_iter_mut().flat_map(|o| o.items.par_iter_mut()).filter(|l| app_data || !libs.is_app_data(&l.path)).for_each(|l| {
        l.size = size_on_disk(&l.path);
        l.measured = true;
    });
    for o in &mut out {
        o.items.sort_by(|a, b| b.size.cmp(&a.size).then(a.path.cmp(&b.path)));
    }
    out.sort_by(|a, b| b.size().cmp(&a.size()).then(a.id.to_lowercase().cmp(&b.id.to_lowercase())));
    out
}

/// One of the paths is a data container or saved window state.
fn is_container_or_state(libs: &Libraries, paths: &[(PathBuf, Style)]) -> bool {
    paths.iter().any(|(p, _)| {
        p.parent() == Some(&libs.user.join("Containers")) || p.parent() == Some(&libs.user.join("Saved Application State"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("heft-orphans-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn ids_only() {
        assert!(looks_like_id("com.example.app"));
        assert!(looks_like_id("net.whatsapp.whatsapp"));
        assert!(looks_like_id("io.github.some-one.tool_2"));
        assert!(!looks_like_id("com.example"), "a vendor folder");
        assert!(!looks_like_id("Google"));
        assert!(!looks_like_id("some folder.with dots.x"));
        assert!(!looks_like_id("2.0.1"), "a version number");
        assert!(!looks_like_id("com..app"));
    }

    #[test]
    fn installed_covers_family() {
        let i = Installed::new(["com.foo.App".to_string(), "com.bar.app.helper".to_string()]);
        assert!(i.covers("com.foo.app"));
        assert!(i.covers("com.foo.app.shareextension"), "an extension of an installed app");
        assert!(i.covers("com.bar.app"), "the parent of an installed helper");
        assert!(!i.covers("com.foo.application"));
        assert!(!i.covers("com.foo.app2"));
    }

    /// A fake `~/Library` with data from installed apps, deleted apps and
    /// things that aren't apps at all.
    #[test]
    fn only_deleted_apps_data() {
        let base = temp("lib");
        let libs = Libraries { user: base.join("Library"), system: base.join("System Library") };
        let u = &libs.user;
        for d in [
            // Deleted sandboxed app: container, plus its caches.
            "Containers/com.gone.Editor",
            "Caches/com.gone.Editor",
            "HTTPStorages/com.gone.Editor",
            // Deleted app without a sandbox, known from its saved state.
            "Saved Application State/org.old.Viewer.savedState",
            "Application Support/org.old.Viewer",
            // Deleted app Launch Services remembers.
            "Application Support/com.remembered.tool",
            // Installed app and its extension.
            "Containers/com.here.App",
            "Containers/com.here.App.ShareExtension",
            "Caches/com.here.App",
            // Apple's.
            "Containers/com.apple.Notes",
            // Only a cache folder: could be a command-line tool, so left alone.
            "Caches/com.someone.clitool",
            // A vendor folder and a name.
            "Application Support/com.vendor",
            "Application Support/Editor",
            // Group containers are never listed.
            "Group Containers/group.com.gone.Editor",
        ] {
            std::fs::create_dir_all(u.join(d)).unwrap();
        }
        std::fs::create_dir_all(u.join("Preferences")).unwrap();
        for f in ["Preferences/com.gone.Editor.plist", "Preferences/com.here.App.plist", "HTTPStorages/com.gone.Editor.binarycookies"] {
            std::fs::write(u.join(f), "x").unwrap();
        }
        std::fs::write(u.join("Containers/com.gone.Editor/data"), vec![0u8; 5000]).unwrap();

        let installed = Installed::new(["com.here.App".to_string()]);
        let remembered = HashMap::from([("com.remembered.tool".to_string(), "Remembered Tool".to_string())]);
        let found = find_in(&libs, &installed, &remembered, true);
        let ids: HashSet<&str> = found.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, HashSet::from(["com.gone.Editor", "org.old.Viewer", "com.remembered.tool"]), "{found:#?}");

        let editor = found.iter().find(|o| o.id == "com.gone.Editor").unwrap();
        let paths: HashSet<PathBuf> = editor.items.iter().map(|l| l.path.clone()).collect();
        assert_eq!(
            paths,
            HashSet::from([
                u.join("Containers/com.gone.Editor"),
                u.join("Caches/com.gone.Editor"),
                u.join("HTTPStorages/com.gone.Editor"),
                u.join("HTTPStorages/com.gone.Editor.binarycookies"),
                u.join("Preferences/com.gone.Editor.plist"),
            ])
        );
        assert!(editor.items.iter().all(|l| !l.confident && l.measured), "nothing ticked by default");
        assert!(editor.size() >= 5000);
        assert_eq!(found.iter().find(|o| o.id == "com.remembered.tool").unwrap().title(), "Remembered Tool");

        // Without Full Disk Access, containers are listed but not measured.
        let blind = find_in(&libs, &installed, &remembered, false);
        let editor = blind.iter().find(|o| o.id == "com.gone.Editor").unwrap();
        for l in &editor.items {
            assert_eq!(l.measured, !l.path.starts_with(u.join("Containers")), "{}", l.path.display());
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Read-only: what this Mac's `~/Library` has. Run with `--ignored
    /// --nocapture` to see.
    #[test]
    #[ignore]
    fn this_macs_orphans() {
        for o in find(crate::mac::has_full_disk_access() == Some(true)) {
            eprintln!("{} ({}) {}", o.title(), o.id, o.size());
            for l in &o.items {
                eprintln!("    {} {}", if l.measured { l.size.to_string() } else { "?".into() }, l.path.display());
            }
        }
    }
}
