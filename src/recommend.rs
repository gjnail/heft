//! Cleanup suggestions: turns what a scan found into concrete things to do,
//! like "these installers in Downloads are two months old" or "you haven't
//! played this game in a year".
//!
//! Everything here comes from the scan itself, except Steam's small
//! `appmanifest_*.acf` files, which are read to learn when a game was last
//! played. Nothing is pre-selected unless it's clearly disposable, and
//! anything the risk rules call dangerous is left out.

use std::collections::HashMap;

use crate::colors::Category;
use crate::devjunk;
use crate::risk::{self, Level};
use crate::tree::{flags, NodeId, Tree, ROOT};

const DAY: i64 = 86_400;
const MB: u64 = 1 << 20;
const GB: u64 = 1 << 30;

#[derive(Clone, Debug)]
pub struct Game {
    pub appid: u32,
    pub name: String,
    /// The game's install folder in the scan, if it was found.
    pub folder: Option<NodeId>,
    pub bytes: u64,
    /// Unix time, or 0 if Steam has no record of it being played.
    pub last_played: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    /// Windows Disk Cleanup (for Windows.old and similar).
    #[cfg_attr(not(windows), allow(dead_code))]
    DiskCleanup,
    /// Heft's Cleaner page.
    Cleaner,
}

#[derive(Clone, Debug)]
pub enum Action {
    /// Review the items and move the chosen ones to the trash.
    Trash { preselect: bool },
    /// Uninstall through Steam, which asks for confirmation itself.
    Steam(Vec<Game>),
    /// Compress the chosen folders; they stay installed and working.
    Compress,
    /// Something Heft can't safely do itself; explain how.
    Explain { how: &'static str, tool: Option<Tool> },
}

#[derive(Clone, Debug)]
pub struct Suggestion {
    pub title: String,
    pub detail: &'static str,
    pub bytes: u64,
    pub items: Vec<NodeId>,
    pub action: Action,
}

/// All suggestions for `tree`, biggest first. Reads Steam manifests from
/// disk, so run it off the UI thread.
pub fn analyze(tree: &Tree, now: i64) -> Vec<Suggestion> {
    let mut out = Vec::new();
    let usable = |id: NodeId| {
        tree.node(id).flags & (flags::DELETED | flags::HARDLINK | flags::CLOUD | flags::MOUNT | flags::SEEN) == 0
            && risk::assess(tree, id).is_none_or(|r| r.level < Level::Danger)
    };

    // One pass to collect the folders and files the rules below look at.
    let mut downloads = Vec::new();
    let mut steamapps = Vec::new();
    let mut logs = Vec::new();
    let mut dumps = Vec::new();
    let mut old_big = Vec::new();
    let mut vdisks = Vec::new();
    let mut stack = vec![ROOT];
    while let Some(id) = stack.pop() {
        for &c in tree.children(id) {
            let n = tree.node(c);
            if n.is_dir() {
                let name = tree.name(c).to_lowercase();
                if name == "downloads" && is_home_folder(tree, c) {
                    downloads.push(c);
                }
                if name == "steamapps" {
                    steamapps.push(c);
                }
                stack.push(c);
                continue;
            }
            let ext = tree.ext_name(c);
            if matches!(ext, "log" | "etl") && n.size >= 100 * MB {
                logs.push(c);
            } else if matches!(ext, "dmp" | "mdmp" | "hdmp") && n.size >= MB {
                dumps.push(c);
            } else if matches!(ext, "vhdx" | "vhd" | "vdi" | "vmdk" | "qcow2") && n.size >= 10 * GB {
                vdisks.push(c);
            } else if n.size >= 500 * MB
                && n.mtime > 0
                && n.mtime < now - 730 * DAY
                && !matches!(tree.exts[n.ext as usize].category, Category::System | Category::Executable)
            {
                old_big.push(c);
            }
        }
    }

    // Installers and repeated downloads in Downloads.
    let mut installers = Vec::new();
    let mut repeats = Vec::new();
    for &d in &downloads {
        let mut by_name: HashMap<String, u64> = HashMap::new();
        for &c in tree.children(d) {
            if !tree.node(c).is_dir() {
                by_name.insert(tree.name(c).to_lowercase(), tree.node(c).size);
            }
        }
        for &c in tree.children(d) {
            let n = tree.node(c);
            if n.is_dir() || !usable(c) {
                continue;
            }
            let ext = tree.ext_name(c);
            let is_installer = matches!(
                ext,
                "exe" | "msi" | "msix" | "msixbundle" | "appx" | "appxbundle" | "dmg" | "pkg" | "deb" | "rpm" | "appimage"
            );
            if let Some(original) = repeat_of(tree.name(c))
                && by_name.get(&original.to_lowercase()) == Some(&n.size)
            {
                repeats.push(c);
            } else if is_installer && n.mtime < now - 30 * DAY {
                installers.push(c);
            }
        }
    }
    push_files(
        &mut out,
        tree,
        installers,
        "Old installers in Downloads",
        "Setup files for programs you've probably already installed. You can download them again if you ever need them.",
        true,
    );
    push_files(
        &mut out,
        tree,
        repeats,
        "Repeated downloads",
        "Files like \"report (1).pdf\" that are the same size as another file with the original name. Usually the same \
         thing downloaded twice.",
        true,
    );

    // Build folders in projects nobody has touched for six months.
    let stale: Vec<NodeId> = devjunk::find(tree, ROOT)
        .into_iter()
        .map(|j| j.id)
        .filter(|&j| usable(j) && project_last_touched(tree, j) < now - 182 * DAY)
        .collect();
    push_files(
        &mut out,
        tree,
        stale,
        "Build folders in inactive projects",
        "node_modules, build output and caches in projects you haven't changed in six months. The build tools recreate \
         them next time you work on the project.",
        true,
    );

    // Games.
    let games: Vec<Game> = steamapps
        .iter()
        .flat_map(|&s| steam_games(tree, s))
        .filter(|g| g.bytes >= GB && (g.last_played == 0 || g.last_played < now - 365 * DAY))
        .collect();
    if !games.is_empty() {
        let bytes = games.iter().map(|g| g.bytes).sum();
        let items = games.iter().filter_map(|g| g.folder).collect();
        out.push(Suggestion {
            title: format!("{} Steam game{} you haven't played in a year", games.len(), plural(games.len())),
            detail: "Uninstalling through Steam keeps your purchase, cloud saves and achievements. You can reinstall any \
                     time.",
            bytes,
            items,
            action: Action::Steam(games),
        });
    }

    let logs: Vec<NodeId> = logs.into_iter().filter(|&c| usable(c)).collect();
    push_files(
        &mut out,
        tree,
        logs,
        "Very large log files",
        "Logs are only needed when troubleshooting. Close the program that writes a log before removing it.",
        true,
    );
    let dumps: Vec<NodeId> = dumps.into_iter().filter(|&c| usable(c)).collect();
    push_files(
        &mut out,
        tree,
        dumps,
        "Crash dumps",
        "Snapshots saved when a program crashed. They're only useful if you plan to send them to a developer.",
        true,
    );
    let mut old_big: Vec<NodeId> = old_big.into_iter().filter(|&c| usable(c) && risk::assess(tree, c).is_none()).collect();
    old_big.sort_by_key(|&c| std::cmp::Reverse(tree.node(c).size));
    old_big.truncate(100);
    push_files(
        &mut out,
        tree,
        old_big,
        "Big files untouched for two years",
        "Large videos, disk images and archives that haven't changed in over two years. Worth a look: keep what matters, \
         move the rest to an external drive or remove it.",
        false,
    );

    // Things Heft points at but leaves to the right tool.
    for (name, title, detail, how, tool) in root_items() {
        if let Some(&c) = tree.children(ROOT).iter().find(|&&c| tree.name(c).eq_ignore_ascii_case(name))
            && tree.node(c).size >= 500 * MB
        {
            out.push(Suggestion {
                title: title.to_string(),
                detail,
                bytes: tree.node(c).size,
                items: vec![c],
                action: Action::Explain { how, tool },
            });
        }
    }
    if let Some(bin) = trash_folder(tree)
        && tree.node(bin).size >= 500 * MB
    {
        out.push(Suggestion {
            title: format!("Your {} is full of old files", crate::platform::TRASH),
            detail: "Deleted files keep using space until the bin is emptied.",
            bytes: tree.node(bin).size,
            items: vec![bin],
            action: Action::Explain { how: EMPTY_TRASH, tool: None },
        });
    }
    if !vdisks.is_empty() {
        let bytes = vdisks.iter().map(|&v| tree.node(v).size).sum();
        out.push(Suggestion {
            title: format!("{} large virtual disk{}", vdisks.len(), plural(vdisks.len())),
            detail: "WSL, Docker and virtual machine disks grow as you use them but don't shrink by themselves when you \
                     delete files inside.",
            bytes,
            items: vdisks,
            action: Action::Explain { how: SHRINK_VDISK, tool: cfg!(windows).then_some(Tool::Cleaner) },
        });
    }

    // Size isn't known until it's done, so this one counts as 0 bytes and
    // sorts last.
    let rarely = compress_candidates(tree, now);
    if !rarely.is_empty() && crate::compress::supported(std::path::Path::new(&tree.root_path)) {
        out.push(Suggestion {
            title: format!(
                "{} program{} and game{} you haven't updated in three months",
                rarely.len(),
                plural(rarely.len()),
                plural(rarely.len())
            ),
            detail: "Compressing keeps them installed and working in less space. Windows unpacks the files as they're \
                     read. How much you save depends on the program.",
            bytes: 0,
            items: rarely,
            action: Action::Compress,
        });
    }

    out.sort_by_key(|s| std::cmp::Reverse(s.bytes));
    out
}

/// Installed programs and games that haven't changed in three months, are
/// mostly compressible files, and aren't compressed already.
fn compress_candidates(tree: &Tree, now: i64) -> Vec<NodeId> {
    let mut parents = Vec::new();
    let mut stack = vec![(ROOT, 0)];
    while let Some((id, depth)) = stack.pop() {
        for &c in tree.children(id) {
            if !tree.node(c).is_dir() {
                continue;
            }
            let name = tree.name(c).to_lowercase();
            let parent = tree.name(id).to_lowercase();
            if matches!(name.as_str(), "program files" | "program files (x86)" | "epic games")
                || (name == "common" && parent == "steamapps")
            {
                parents.push(c);
            } else if depth < 5 {
                stack.push((c, depth + 1));
            }
        }
    }
    let mut out: Vec<NodeId> = parents
        .iter()
        .flat_map(|&p| tree.children(p).iter().copied())
        .filter(|&c| {
            let n = tree.node(c);
            n.is_dir()
                && n.alloc >= GB
                && n.mtime > 0
                && n.mtime < now - 90 * DAY
                && n.alloc.saturating_mul(10) >= n.size.saturating_mul(9)
                && n.flags & (flags::DELETED | flags::MOUNT | flags::SEEN | flags::LINK) == 0
                && risk::assess(tree, c).is_none_or(|r| r.level < Level::Danger)
        })
        .filter(|&c| {
            let worth: u64 = tree
                .files_under(c)
                .into_iter()
                .filter(|&f| crate::compress::worth_trying(tree.ext_name(f), tree.node(f).size))
                .map(|f| tree.node(f).alloc)
                .sum();
            worth * 2 >= tree.node(c).alloc
        })
        .collect();
    out.sort_by_key(|&c| std::cmp::Reverse(tree.node(c).alloc));
    out.truncate(20);
    out
}

/// Temporary files and caches the Cleaner's default rules would remove,
/// rolled into one suggestion that sends you to the Cleaner page. This runs
/// the cleaner's read-only analysis, which takes a few seconds.
pub fn cleaner_summary() -> Option<Suggestion> {
    let rules = crate::clean::rules::all();
    let chosen: Vec<usize> = (0..rules.len()).filter(|&i| rules[i].default_on && rules[i].warning.is_none()).collect();
    if chosen.is_empty() {
        return None;
    }
    let (tx, rx) = crossbeam_channel::unbounded();
    let progress = crate::clean::Progress::default();
    crate::clean::analyze_all(chosen, tx, &progress);
    let bytes: u64 = rx.try_iter().filter(|f| f.blocked_by.is_none() && !f.needs_admin).map(|f| f.bytes).sum();
    (bytes >= 100 * MB).then(|| Suggestion {
        title: "Temporary files and caches".to_string(),
        detail: "Leftovers from apps, browsers and the system that are safe to clear. They come back as you use the \
                 computer, so this is worth doing now and then rather than once.",
        bytes,
        items: Vec::new(),
        action: Action::Explain {
            how: "The Cleaner page lists them app by app, lets you untick anything you want to keep, and removes the rest.",
            tool: Some(Tool::Cleaner),
        },
    })
}

fn push_files(out: &mut Vec<Suggestion>, tree: &Tree, mut items: Vec<NodeId>, title: &str, detail: &'static str, preselect: bool) {
    if items.is_empty() {
        return;
    }
    items.sort_by_key(|&c| std::cmp::Reverse(tree.node(c).size));
    let bytes: u64 = items.iter().map(|&c| tree.node(c).size).sum();
    if bytes < MB {
        return;
    }
    out.push(Suggestion { title: title.to_string(), detail, bytes, items, action: Action::Trash { preselect } });
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// A user's own Downloads folder (`…/Users/ana/Downloads`,
/// `/home/ana/Downloads`), or one directly in the scanned folder.
fn is_home_folder(tree: &Tree, id: NodeId) -> bool {
    let parent = tree.node(id).parent;
    if parent == ROOT {
        return true;
    }
    let grand = tree.node(parent).parent;
    let g = if grand == ROOT { tree.root_path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or("") } else { tree.name(grand) };
    g.eq_ignore_ascii_case("users") || g.eq_ignore_ascii_case("home")
}

/// `report (1).pdf` / `report(2).pdf` -> `report.pdf`.
fn repeat_of(name: &str) -> Option<String> {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let open = stem.rfind('(')?;
    let digits = stem[open + 1..].strip_suffix(')')?;
    if digits.is_empty() || digits.len() > 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let base = stem[..open].trim_end();
    (!base.is_empty()).then(|| format!("{base}{ext}"))
}

/// Newest change anywhere in the project around a build folder, ignoring the
/// build folders themselves.
fn project_last_touched(tree: &Tree, junk: NodeId) -> i64 {
    let project = tree.node(junk).parent;
    tree.children(project)
        .iter()
        .filter(|&&c| c != junk && devjunk::classify(tree, c).is_none())
        .map(|&c| tree.node(c).mtime)
        .max()
        .unwrap_or(tree.node(project).mtime)
}

fn steam_games(tree: &Tree, steamapps: NodeId) -> Vec<Game> {
    let common = tree.children(steamapps).iter().copied().find(|&c| tree.name(c).eq_ignore_ascii_case("common"));
    let mut games = Vec::new();
    for &c in tree.children(steamapps) {
        let name = tree.name(c).to_lowercase();
        if !(name.starts_with("appmanifest_") && name.ends_with(".acf")) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(tree.path(c)) else { continue };
        let Some(m) = parse_manifest(&text) else { continue };
        let folder = common.and_then(|cm| {
            tree.children(cm).iter().copied().find(|&g| tree.name(g).eq_ignore_ascii_case(&m.installdir))
        });
        let bytes = folder.map(|f| tree.node(f).size).unwrap_or(m.size_on_disk);
        games.push(Game { appid: m.appid, name: m.name, folder, bytes, last_played: m.last_played });
    }
    games
}

struct Manifest {
    appid: u32,
    name: String,
    installdir: String,
    size_on_disk: u64,
    last_played: i64,
}

/// Pull the few fields we need out of Steam's KeyValues text format.
fn parse_manifest(text: &str) -> Option<Manifest> {
    let mut fields: HashMap<String, String> = HashMap::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('"').collect();
        // `"key"  "value"` splits into ["", key, ws, value, ""].
        if parts.len() >= 5 && !fields.contains_key(&parts[1].to_lowercase()) {
            fields.insert(parts[1].to_lowercase(), parts[3].to_string());
        }
    }
    Some(Manifest {
        appid: fields.get("appid")?.parse().ok()?,
        name: fields.get("name")?.clone(),
        installdir: fields.get("installdir")?.clone(),
        size_on_disk: fields.get("sizeondisk").and_then(|s| s.parse().ok()).unwrap_or(0),
        last_played: fields.get("lastplayed").and_then(|s| s.parse().ok()).unwrap_or(0),
    })
}

#[cfg(windows)]
const EMPTY_TRASH: &str = "Right-click the Recycle Bin on the desktop and choose Empty Recycle Bin, or use the Recycle \
                           Bin rule on the Cleaner page.";
#[cfg(target_os = "macos")]
const EMPTY_TRASH: &str = "Right-click the Trash in the Dock and choose Empty Trash.";
#[cfg(all(unix, not(target_os = "macos")))]
const EMPTY_TRASH: &str = "Open the Trash in your file manager and choose Empty.";

const SHRINK_VDISK: &str = "For WSL, run `wsl --shutdown`, then compact the disk (on Windows, Heft's Cleaner page can \
                            do this). For virtual machines, use the VM software's compact or shrink option.";

/// (name at the top of the scan, title, detail, how, tool)
#[cfg(windows)]
fn root_items() -> Vec<(&'static str, &'static str, &'static str, &'static str, Option<Tool>)> {
    vec![
        (
            "Windows.old",
            "Previous Windows installation",
            "Left behind by a Windows upgrade so you can go back. After a few weeks you won't need it.",
            "Open Disk Cleanup, click Clean up system files, tick Previous Windows installation(s) and press OK.",
            Some(Tool::DiskCleanup),
        ),
        (
            "hiberfil.sys",
            "Hibernation file",
            "Windows reserves this much space for hibernation and Fast Startup.",
            "If you never hibernate, open Command Prompt as administrator and run `powercfg /hibernate off`. Windows \
             removes the file. Fast Startup is turned off too, so the PC may boot a little slower.",
            None,
        ),
    ]
}

#[cfg(not(windows))]
fn root_items() -> Vec<(&'static str, &'static str, &'static str, &'static str, Option<Tool>)> {
    Vec::new()
}

/// The current user's trash folder, if it's in the scan.
fn trash_folder(tree: &Tree) -> Option<NodeId> {
    if cfg!(windows) {
        return tree.children(ROOT).iter().copied().find(|&c| tree.name(c).eq_ignore_ascii_case("$Recycle.Bin"));
    }
    let home = crate::platform::home_dir()?;
    let sep = crate::platform::SEP;
    let path = if cfg!(target_os = "macos") { format!("{home}{sep}.Trash") } else { format!("{home}{sep}.local{sep}share{sep}Trash") };
    tree.find(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::tests::root;
    use crate::tree::{ScanInfo, ScanMode, TreeBuilder};

    const NOW: i64 = 2_000_000_000;

    #[test]
    fn repeats() {
        assert_eq!(repeat_of("report (1).pdf").as_deref(), Some("report.pdf"));
        assert_eq!(repeat_of("setup(12).exe").as_deref(), Some("setup.exe"));
        assert_eq!(repeat_of("notes (draft).txt"), None);
        assert_eq!(repeat_of("(1).txt"), None);
        assert_eq!(repeat_of("plain.txt"), None);
    }

    #[test]
    fn manifest() {
        let acf = "\"AppState\"\n{\n\t\"appid\"\t\t\"620\"\n\t\"name\"\t\t\"Portal 2\"\n\t\"installdir\"\t\t\"Portal 2\"\n\t\
                   \"SizeOnDisk\"\t\t\"12345\"\n\t\"LastPlayed\"\t\t\"1600000000\"\n}\n";
        let m = parse_manifest(acf).unwrap();
        assert_eq!((m.appid, m.name.as_str(), m.size_on_disk, m.last_played), (620, "Portal 2", 12345, 1_600_000_000));
    }

    #[test]
    fn finds_installers_repeats_logs_and_stale_builds() {
        let mut b = TreeBuilder::new(root());
        let users = b.add(ROOT, if cfg!(windows) { "Users" } else { "home" }, flags::DIR, 0, 0, 0);
        let ana = b.add(users, "ana", flags::DIR, 0, 0, 0);
        let dl = b.add(ana, "Downloads", flags::DIR, 0, 0, 0);
        let old = NOW - 90 * DAY;
        b.add(dl, "setup.exe", 0, 80 * MB, 0, old);
        b.add(dl, "fresh-setup.exe", 0, 80 * MB, 0, NOW - DAY);
        b.add(dl, "report.pdf", 0, 3 * MB, 0, old);
        b.add(dl, "report (1).pdf", 0, 3 * MB, 0, old);
        let proj = b.add(ana, "old-site", flags::DIR, 0, 0, 0);
        b.add(proj, "package.json", 0, 100, 0, NOW - 400 * DAY);
        let nm = b.add(proj, "node_modules", flags::DIR, 0, 0, 0);
        b.add(nm, "big.js", 0, 50 * MB, 0, NOW - 10 * DAY);
        let logs = b.add(ana, "logs", flags::DIR, 0, 0, 0);
        b.add(logs, "trace.log", 0, 300 * MB, 0, NOW);
        let info = ScanInfo { mode: ScanMode::Walk, duration_ms: 0, finished_at: 0, unreadable_dirs: 0, note: None, phases: Vec::new() };
        let t = b.finish(root().into(), info);

        let s = analyze(&t, NOW);
        let find = |title: &str| s.iter().find(|x| x.title == title).unwrap_or_else(|| panic!("no {title}: {s:?}"));
        let names = |x: &Suggestion| x.items.iter().map(|&i| t.name(i).to_string()).collect::<Vec<_>>();
        assert_eq!(names(find("Old installers in Downloads")), ["setup.exe"]);
        assert_eq!(names(find("Repeated downloads")), ["report (1).pdf"]);
        assert_eq!(names(find("Build folders in inactive projects")), ["node_modules"]);
        assert_eq!(names(find("Very large log files")), ["trace.log"]);
        // Biggest first.
        assert!(s.windows(2).all(|w| w[0].bytes >= w[1].bytes));
    }

    #[test]
    fn compression_candidates() {
        let mut b = TreeBuilder::new(root());
        let pf = b.add(ROOT, "Program Files", flags::DIR, 0, 0, 0);
        let add_app = |b: &mut TreeBuilder, name: &str, ext: &str, alloc: u64, mtime: i64| {
            let app = b.add(pf, name, flags::DIR, 0, 0, 0);
            b.add(app, &format!("data.{ext}"), 0, 2 * GB, alloc, mtime);
        };
        add_app(&mut b, "Old Tool", "dll", 2 * GB, NOW - 200 * DAY);
        add_app(&mut b, "Updated Last Week", "dll", 2 * GB, NOW - 7 * DAY);
        add_app(&mut b, "Movie Maker Samples", "mp4", 2 * GB, NOW - 200 * DAY);
        add_app(&mut b, "Already Compressed", "dll", GB + GB / 10, NOW - 200 * DAY);
        let games = b.add(ROOT, "Games", flags::DIR, 0, 0, 0);
        let steamapps = b.add(games, "steamapps", flags::DIR, 0, 0, 0);
        let common = b.add(steamapps, "common", flags::DIR, 0, 0, 0);
        let game = b.add(common, "Some Game", flags::DIR, 0, 0, 0);
        b.add(game, "assets.pak", 0, 3 * GB, 3 * GB, NOW - 100 * DAY);
        let info = ScanInfo { mode: ScanMode::Walk, duration_ms: 0, finished_at: 0, unreadable_dirs: 0, note: None, phases: Vec::new() };
        let t = b.finish(root().into(), info);
        let names: Vec<&str> = compress_candidates(&t, NOW).into_iter().map(|c| t.name(c)).collect();
        assert_eq!(names, ["Some Game", "Old Tool"]);
    }
}
