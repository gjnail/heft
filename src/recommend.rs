//! Cleanup suggestions: turns what a scan found into concrete things to do,
//! like "these installers in Downloads are two months old" or "you haven't
//! played this game in a year".
//!
//! Everything here comes from the scan itself, except Steam's small
//! `appmanifest_*.acf` files, which are read to learn when a game was last
//! played, and on macOS the list of Time Machine's local snapshots. Nothing
//! is pre-selected unless it's clearly disposable, and anything the risk
//! rules call dangerous is left out.

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
    /// Remove the downloaded copies of the chosen iCloud Drive files; they
    /// stay in iCloud and download again when opened (macOS).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    RemoveDownload,
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
            } else if matches!(ext, "vhdx" | "vhd" | "vdi" | "vmdk" | "qcow2" | "hds") && n.size >= 10 * GB {
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
    #[cfg(target_os = "macos")]
    mac_items(tree, &mut out, &usable);
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
            detail: VDISK_DETAIL,
            bytes,
            items: vdisks,
            action: Action::Explain { how: SHRINK_VDISK, tool: cfg!(windows).then_some(Tool::Cleaner) },
        });
    }

    // Size isn't known until it's done, so this one counts as 0 bytes and
    // sorts last.
    let rarely = compress_candidates(tree, now, &steamapps, &can_compress);
    if !rarely.is_empty() && crate::compress::supported(std::path::Path::new(&tree.root_path)) {
        let program = if cfg!(target_os = "macos") { "app" } else { "program" };
        out.push(Suggestion {
            title: format!(
                "{} {program}{} and game{} you haven't updated in three months",
                rarely.len(),
                plural(rarely.len()),
                plural(rarely.len())
            ),
            detail: COMPRESS_DETAIL,
            bytes: 0,
            items: rarely,
            action: Action::Compress,
        });
    }

    out.sort_by_key(|s| std::cmp::Reverse(s.bytes));
    out
}

#[cfg(not(target_os = "macos"))]
const COMPRESS_DETAIL: &str = "Compressing keeps them installed and working in less space. Windows unpacks the files \
                               as they're read. How much you save depends on the program.";
#[cfg(target_os = "macos")]
const COMPRESS_DETAIL: &str = "Compressing keeps them installed and working in less space. macOS unpacks the files as \
                               they're read. How much you save depends on the app.";

/// Whether you can compress an installed program's folder. On macOS, App
/// Store apps and apps installed for every user belong to the system, and
/// only their owner can rewrite them.
#[cfg(target_os = "macos")]
fn can_compress(path: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(md) = std::fs::symlink_metadata(path) else { return false };
    let uid = unsafe { libc::getuid() };
    let writable = std::ffi::CString::new(path).is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0);
    md.is_dir()
        && (uid == 0 || md.uid() == uid)
        && writable
        && !std::path::Path::new(path).join("Contents/_MASReceipt").exists()
}

#[cfg(not(target_os = "macos"))]
fn can_compress(_path: &str) -> bool {
    true
}

/// Installed programs and games that haven't changed in three months, are
/// mostly compressible files, aren't compressed already, and that you can
/// change (`can_change`, given the folder's path).
fn compress_candidates(tree: &Tree, now: i64, steamapps: &[NodeId], can_change: &dyn Fn(&str) -> bool) -> Vec<NodeId> {
    let mut out: Vec<NodeId> = install_folders(tree, steamapps)
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
        .filter(|&c| can_change(&tree.path(c)))
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

/// Folders whose subfolders are installed programs and games.
#[cfg(not(target_os = "macos"))]
fn install_folders(tree: &Tree, _steamapps: &[NodeId]) -> Vec<NodeId> {
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
    parents
}

/// Folders whose subfolders are installed apps and games: Applications and
/// your own ~/Applications, and Steam's `common` folder (deep in
/// ~/Library/Application Support, or wherever a Steam library is).
#[cfg(target_os = "macos")]
fn install_folders(tree: &Tree, steamapps: &[NodeId]) -> Vec<NodeId> {
    let mut out: Vec<NodeId> = std::iter::once("/Applications".to_string())
        .chain(homes(tree).iter().map(|h| format!("{h}/Applications")))
        .filter_map(|p| tree.find(&p))
        .collect();
    out.extend(
        steamapps
            .iter()
            .filter_map(|&s| tree.children(s).iter().copied().find(|&c| tree.name(c).eq_ignore_ascii_case("common"))),
    );
    out
}

/// The home folders in the scan: every folder in `/Users`, or the one the
/// scanned folder is in.
#[cfg(target_os = "macos")]
fn homes(tree: &Tree) -> Vec<String> {
    if let Some(users) = tree.find("/Users") {
        return tree.children(users).iter().filter(|&&c| tree.node(c).is_dir()).map(|&c| tree.path(c)).collect();
    }
    let comps: Vec<&str> = tree.root_path.split('/').filter(|c| !c.is_empty()).collect();
    match comps.as_slice() {
        [users, name, ..] if users.eq_ignore_ascii_case("users") => vec![format!("/Users/{name}")],
        _ => Vec::new(),
    }
}

/// macOS: big things in known places that Heft either lists or explains:
/// old macOS installers, iPhone updates and backups, the hibernation image,
/// Docker Desktop's disk, and what Messages, Mail, Xcode and Photos keep.
#[cfg(target_os = "macos")]
fn mac_items(tree: &Tree, out: &mut Vec<Suggestion>, usable: &dyn Fn(NodeId) -> bool) {
    let installers: Vec<NodeId> = tree
        .find("/Applications")
        .map(|apps| {
            tree.children(apps)
                .iter()
                .copied()
                .filter(|&c| {
                    let name = tree.name(c).to_lowercase();
                    tree.node(c).is_dir()
                        && (name.starts_with("install macos") || name.starts_with("install os x"))
                        && name.ends_with(".app")
                        && tree.node(c).size >= GB
                        && usable(c)
                })
                .collect()
        })
        .unwrap_or_default();
    push_files(
        out,
        tree,
        installers,
        "Old macOS installers",
        "Full macOS installers left in Applications after an upgrade, or downloaded to make a startup disk. They \
         take several GB each, and you can download them again from Apple if you ever need one.",
        false,
    );

    let homes = homes(tree);
    let ipsw: Vec<NodeId> = homes
        .iter()
        .filter_map(|h| tree.find(&format!("{h}/Library/iTunes")))
        .flat_map(|d| tree.files_under(d))
        .filter(|&f| tree.ext_name(f).eq_ignore_ascii_case("ipsw") && usable(f))
        .collect();
    push_files(
        out,
        tree,
        ipsw,
        "iPhone and iPad software downloads",
        "Copies of iOS and iPadOS that Finder downloaded to update or restore a device. Finder downloads a fresh one \
         when it needs it.",
        true,
    );

    for h in &homes {
        if let Some(b) = tree.find(&format!("{h}/Library/Application Support/MobileSync/Backup"))
            && tree.node(b).size >= 500 * MB
        {
            out.push(Suggestion {
                title: "iPhone and iPad backups".to_string(),
                detail: "Finder keeps a full backup of each iPhone and iPad you back up to this Mac, including \
                         devices you no longer have.",
                bytes: tree.node(b).size,
                items: vec![b],
                action: Action::Explain {
                    how: "Remove old ones in Finder rather than here: connect an iPhone or iPad, select it in the \
                          sidebar and click Manage Backups. The list shows every device's backups by name and date; \
                          select the ones you don't need and click Delete Backup. The folders here are named by \
                          device ID, so it's easy to delete the wrong one by hand.",
                    tool: None,
                },
            });
        }
    }

    if let Some(s) = tree.find("/private/var/vm/sleepimage")
        && tree.node(s).size >= 500 * MB
    {
        out.push(Suggestion {
            title: "Hibernation image".to_string(),
            detail: "macOS writes what's in memory to this file when a laptop sleeps, so your work survives if the \
                     battery runs out.",
            bytes: tree.node(s).size,
            items: vec![s],
            action: Action::Explain {
                how: "Keep it on a laptop. On a Mac that's always plugged in, you can turn hibernation off by running \
                      `sudo pmset -a hibernatemode 0` in Terminal, then `sudo rm /private/var/vm/sleepimage` to \
                      remove the file. `sudo pmset -a hibernatemode 3` turns it back on.",
                tool: None,
            },
        });
    }

    // Docker.raw is sparse: its size is the limit, its space on disk is what it holds.
    let docker: Vec<NodeId> = homes
        .iter()
        .filter_map(|h| tree.find(&format!("{h}/Library/Containers/com.docker.docker")))
        .flat_map(|d| tree.files_under(d))
        .filter(|&f| tree.name(f).eq_ignore_ascii_case("Docker.raw") && tree.node(f).alloc >= 10 * GB)
        .collect();
    if !docker.is_empty() {
        out.push(Suggestion {
            title: "Docker Desktop's disk".to_string(),
            detail: "Docker Desktop keeps every image, container and volume in this one file. It grows as you pull \
                     and build, and only shrinks when you remove things in Docker.",
            bytes: docker.iter().map(|&d| tree.node(d).alloc).sum(),
            items: docker,
            action: Action::Explain {
                how: "Remove what you no longer need with `docker system prune -a` in Terminal (add `--volumes` to \
                      include unused volumes), or in Docker Desktop's Images, Containers and Volumes views. Docker \
                      Desktop then gives the space back to macOS, which can take a few minutes. Don't delete \
                      Docker.raw itself: everything in Docker goes with it.",
                tool: None,
            },
        });
    }

    // What an app keeps that the app itself should clean up: only explained.
    for h in &homes {
        explain_folder(
            out,
            tree,
            &format!("{h}/Library/Messages/Attachments"),
            500 * MB,
            "Messages attachments",
            "Every photo, video and file sent or received in Messages is kept on this Mac, including in conversations \
             you no longer read.",
            "Delete them in macOS rather than here, so your conversations don't end up with missing pieces: System \
             Settings › General › Storage, then the ⓘ next to Messages, lists the largest attachments to delete. Or set \
             Messages › Settings › General › Keep messages to a year or 30 days, which removes older messages with \
             their attachments. With Messages in iCloud on, deleting removes them from your other devices too.",
        );
        explain_folder(
            out,
            tree,
            &format!("{h}/Library/Developer/Xcode/Archives"),
            GB,
            "Xcode archives",
            "Builds you archived to upload to the App Store or share, each with the app and its debug symbols.",
            "Keep the archives of versions people still use: their debug symbols make crash reports readable. Delete \
             older ones in Xcode's Organizer (Window › Organizer › Archives), which shows each one's version and date.",
        );
        explain_folder(
            out,
            tree,
            &format!("{h}/Library/Containers/com.apple.mail/Data/Library/Mail Downloads"),
            200 * MB,
            "Mail downloads",
            "Copies of attachments you opened from Mail. The attachments themselves stay in the messages.",
            "Mail removes these by itself, as set in Mail › Settings › General › Remove unedited downloads: choose \
             When Mail Quits to keep this folder small. Attachments you edited are kept, so look in the folder for \
             anything you changed and want to keep.",
        );
        // Photos libraries, usually one in Pictures.
        let libraries: Vec<NodeId> = tree
            .find(&format!("{h}/Pictures"))
            .map(|p| {
                tree.children(p)
                    .iter()
                    .copied()
                    .filter(|&c| tree.node(c).is_dir() && tree.name(c).to_lowercase().ends_with(".photoslibrary"))
                    .filter(|&c| tree.node(c).alloc >= 5 * GB)
                    .collect()
            })
            .unwrap_or_default();
        if !libraries.is_empty() {
            out.push(Suggestion {
                title: "Photos library".to_string(),
                detail: "Photos keeps the full-size original of every photo and video in its library on this Mac.",
                bytes: libraries.iter().map(|&l| tree.node(l).alloc).sum(),
                items: libraries,
                action: Action::Explain {
                    how: "If you use iCloud Photos, turn on Photos › Settings › iCloud › Optimize Mac Storage: the \
                          originals stay in iCloud, this Mac keeps smaller versions and downloads an original when you \
                          open it, and macOS frees space that way as the disk fills up. Never delete files inside the \
                          library itself; that breaks it.",
                    tool: None,
                },
            });
        }
    }
}

/// A folder the app that owns it should clean up, explained if it's at least
/// `min` on disk.
#[cfg(target_os = "macos")]
fn explain_folder(out: &mut Vec<Suggestion>, tree: &Tree, path: &str, min: u64, title: &str, detail: &'static str, how: &'static str) {
    if let Some(f) = tree.find(path)
        && tree.node(f).alloc >= min
    {
        out.push(Suggestion {
            title: title.to_string(),
            detail,
            bytes: tree.node(f).alloc,
            items: vec![f],
            action: Action::Explain { how, tool: None },
        });
    }
}

/// Time Machine's local snapshots on the startup disk, which a scan can't
/// see. Runs `tmutil`, so call it off the UI thread, and only for a scan of
/// this Mac (not the demo disk).
pub fn local_snapshots(root: &str) -> Option<Suggestion> {
    #[cfg(target_os = "macos")]
    {
        let startup = root == "/" || root == "/System/Volumes/Data" || crate::platform::home_dir().as_deref() == Some(root);
        if !startup {
            return None;
        }
        let out = std::process::Command::new("/usr/bin/tmutil").args(["listlocalsnapshots", "/"]).output().ok()?;
        let n = count_snapshots(&String::from_utf8_lossy(&out.stdout));
        (n > 0).then(|| Suggestion {
            title: format!("{n} Time Machine snapshot{} on this disk", plural(n)),
            detail: "Time Machine keeps hourly snapshots on the Mac itself, so you can get files back even without the \
                     backup disk. The space they take doesn't show up in a scan.",
            bytes: 0,
            items: Vec::new(),
            action: Action::Explain {
                how: "macOS deletes them by itself after a day, or sooner when the disk runs low, so they rarely need \
                      attention. To remove them now, run `sudo tmutil deletelocalsnapshots /` in Terminal. Backups on \
                      your Time Machine disk aren't affected.",
                tool: None,
            },
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = root;
        None
    }
}

/// Big iCloud Drive files that are also downloaded to this Mac and haven't
/// changed in a month. Their downloads can be removed without deleting
/// anything. Asks iCloud about each candidate, so call it off the UI thread,
/// and only for a scan of this Mac.
#[cfg(target_os = "macos")]
pub fn icloud_downloads(tree: &Tree, now: i64) -> Option<Suggestion> {
    use crate::mac::icloud::{self, State};
    let home = crate::platform::home_dir()?;
    let mut candidates = Vec::new();
    let mut stack = vec![ROOT];
    while let Some(id) = stack.pop() {
        for &c in tree.children(id) {
            let n = tree.node(c);
            if n.flags & (flags::CLOUD | flags::LINK | flags::HARDLINK | flags::DELETED | flags::MOUNT | flags::SEEN) != 0 {
                continue;
            }
            if n.is_dir() {
                stack.push(c);
            } else if n.alloc >= 25 * MB && n.mtime > 0 && n.mtime < now - 30 * DAY {
                candidates.push(c);
            }
        }
    }
    // Asking iCloud takes a moment per file, so only the biggest few hundred.
    candidates.sort_by_key(|&c| std::cmp::Reverse(tree.node(c).alloc));
    candidates.truncate(400);
    let home = std::path::Path::new(&home);
    let items: Vec<NodeId> = candidates
        .into_iter()
        .filter(|&c| {
            let path = tree.path(c);
            let path = std::path::Path::new(&path);
            path.starts_with(home) && icloud::state(path) == State::Downloaded
        })
        .collect();
    let bytes: u64 = items.iter().map(|&c| tree.node(c).alloc).sum();
    (bytes >= 100 * MB).then(|| Suggestion {
        title: format!("{} big iCloud Drive file{} kept on this Mac", items.len(), plural(items.len())),
        detail: "These are also in iCloud. Removing the download frees the space here, as Finder's Remove Download does: \
                 each file stays in iCloud Drive and in its folder, and downloads again when you open it, which needs an \
                 internet connection. With Optimize Mac Storage on in iCloud Drive's settings, macOS does this by itself \
                 when space runs low.",
        bytes,
        items,
        action: Action::RemoveDownload,
    })
}

/// Count the Time Machine snapshots in `tmutil listlocalsnapshots` output.
#[cfg(target_os = "macos")]
fn count_snapshots(text: &str) -> usize {
    text.lines().filter(|l| l.trim().starts_with("com.apple.TimeMachine.")).count()
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

#[cfg(not(target_os = "macos"))]
const VDISK_DETAIL: &str = "WSL, Docker and virtual machine disks grow as you use them but don't shrink by themselves \
                            when you delete files inside.";
#[cfg(target_os = "macos")]
const VDISK_DETAIL: &str = "Virtual machine disks grow as you use them but don't shrink by themselves when you delete \
                            files inside.";

#[cfg(not(target_os = "macos"))]
const SHRINK_VDISK: &str = "For WSL, run `wsl --shutdown`, then compact the disk (on Windows, Heft's Cleaner page can \
                            do this). For virtual machines, use the VM software's compact or shrink option.";
#[cfg(target_os = "macos")]
const SHRINK_VDISK: &str = "Delete what you don't need inside the virtual machine first, then use the VM app's own \
                            option to reclaim or compact its disk: Parallels Desktop and VMware Fusion have one in the \
                            virtual machine's settings, and for VirtualBox it's `VBoxManage modifymedium --compact`.";

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

    /// Read-only: scans your iCloud Drive and lists what the suggestion
    /// would offer. Run with `--ignored --nocapture` to see.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn icloud_drive_suggestion() {
        let home = crate::platform::home_dir().unwrap();
        let drive = format!("{home}/Library/Mobile Documents/com~apple~CloudDocs");
        let p = crate::scan::Progress::default();
        let crate::scan::ScanOutcome::Done(tree, _) = crate::scan::run(&drive, false, &p) else { panic!("scan failed") };
        let s = icloud_downloads(&tree, crate::platform::now_unix());
        match &s {
            Some(s) => {
                eprintln!("{} ({} bytes)", s.title, s.bytes);
                for &id in &s.items {
                    eprintln!("  {}  {}", tree.node(id).alloc, tree.path(id));
                }
            }
            None => eprintln!("no suggestion"),
        }
    }
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
    #[cfg(not(target_os = "macos"))]
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
        let names: Vec<&str> = compress_candidates(&t, NOW, &[], &|_| true).into_iter().map(|c| t.name(c)).collect();
        assert_eq!(names, ["Some Game", "Old Tool"]);
    }

    #[cfg(target_os = "macos")]
    fn info() -> ScanInfo {
        ScanInfo { mode: ScanMode::Walk, duration_ms: 0, finished_at: 0, unreadable_dirs: 0, note: None, phases: Vec::new() }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn mac_compression_candidates() {
        let mut b = TreeBuilder::new("/");
        let apps = b.add(ROOT, "Applications", flags::DIR, 0, 0, 0);
        let add_app = |b: &mut TreeBuilder, parent: NodeId, name: &str, ext: &str, mtime: i64| {
            let app = b.add(parent, name, flags::DIR, 0, 0, 0);
            let contents = b.add(app, "Contents", flags::DIR, 0, 0, 0);
            b.add(contents, &format!("data.{ext}"), 0, 2 * GB, 2 * GB, mtime);
        };
        add_app(&mut b, apps, "Old Tool.app", "dylib", NOW - 200 * DAY);
        add_app(&mut b, apps, "Fresh.app", "dylib", NOW - 7 * DAY);
        add_app(&mut b, apps, "Store App.app", "dylib", NOW - 200 * DAY);
        add_app(&mut b, apps, "Movie Samples.app", "mov", NOW - 200 * DAY);
        let users = b.add(ROOT, "Users", flags::DIR, 0, 0, 0);
        let ana = b.add(users, "ana", flags::DIR, 0, 0, 0);
        let my_apps = b.add(ana, "Applications", flags::DIR, 0, 0, 0);
        add_app(&mut b, my_apps, "Mine.app", "dylib", NOW - 100 * DAY);
        let mut dir = ana;
        for name in ["Library", "Application Support", "Steam", "steamapps", "common"] {
            dir = b.add(dir, name, flags::DIR, 0, 0, 0);
        }
        add_app(&mut b, dir, "Some Game", "pak", NOW - 120 * DAY);
        // Nothing outside the known places, however old.
        let elsewhere = b.add(ana, "Projects", flags::DIR, 0, 0, 0);
        add_app(&mut b, elsewhere, "Old Project", "o", NOW - 400 * DAY);
        let t = b.finish("/".into(), info());

        let steamapps: Vec<NodeId> = t.find("/Users/ana/Library/Application Support/Steam/steamapps").into_iter().collect();
        // Belongs to root, as App Store apps do.
        let yours = |p: &str| !p.ends_with("Store App.app");
        let mut names: Vec<&str> = compress_candidates(&t, NOW, &steamapps, &yours).into_iter().map(|c| t.name(c)).collect();
        names.sort();
        assert_eq!(names, ["Mine.app", "Old Tool.app", "Some Game"]);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn mac_items_are_found_and_explained() {
        let mut b = TreeBuilder::new("/");
        let apps = b.add(ROOT, "Applications", flags::DIR, 0, 0, 0);
        let installer = b.add(apps, "Install macOS Sequoia.app", flags::DIR, 0, 0, 0);
        b.add(installer, "SharedSupport.dmg", 0, 14 * GB, 14 * GB, NOW - 90 * DAY);
        let stub = b.add(apps, "Install macOS Tahoe.app", flags::DIR, 0, 0, 0);
        b.add(stub, "stub", 0, 20 * MB, 20 * MB, NOW);
        let users = b.add(ROOT, "Users", flags::DIR, 0, 0, 0);
        let ana = b.add(users, "ana", flags::DIR, 0, 0, 0);
        let lib = b.add(ana, "Library", flags::DIR, 0, 0, 0);
        let itunes = b.add(lib, "iTunes", flags::DIR, 0, 0, 0);
        let updates = b.add(itunes, "iPhone Software Updates", flags::DIR, 0, 0, 0);
        b.add(updates, "iPhone17,1_26.0_Restore.ipsw", 0, 9 * GB, 9 * GB, NOW - 30 * DAY);
        let support = b.add(lib, "Application Support", flags::DIR, 0, 0, 0);
        let sync = b.add(support, "MobileSync", flags::DIR, 0, 0, 0);
        let backup = b.add(sync, "Backup", flags::DIR, 0, 0, 0);
        let device = b.add(backup, "00008110-001A2B3C4D5E", flags::DIR, 0, 0, 0);
        b.add(device, "Manifest.db", 0, 30 * GB, 30 * GB, NOW - 60 * DAY);
        let mut dir = lib;
        for name in ["Containers", "com.docker.docker", "Data", "vms", "0", "data"] {
            dir = b.add(dir, name, flags::DIR, 0, 0, 0);
        }
        b.add(dir, "Docker.raw", 0, 64 * GB, 22 * GB, NOW);
        let messages = b.add(lib, "Messages", flags::DIR, 0, 0, 0);
        let attachments = b.add(messages, "Attachments", flags::DIR, 0, 0, 0);
        b.add(attachments, "IMG_0001.heic", 0, 3 * GB, 3 * GB, NOW - 400 * DAY);
        let mut dir = lib;
        for name in ["Developer", "Xcode", "Archives", "2026-01-02"] {
            dir = b.add(dir, name, flags::DIR, 0, 0, 0);
        }
        b.add(dir, "App.xcarchive", 0, 2 * GB, 2 * GB, NOW - 200 * DAY);
        let mut dir = lib;
        for name in ["Containers", "com.apple.mail", "Data", "Library", "Mail Downloads"] {
            dir = b.add(dir, name, flags::DIR, 0, 0, 0);
        }
        b.add(dir, "small.pdf", 0, 10 * MB, 10 * MB, NOW);
        let pictures = b.add(ana, "Pictures", flags::DIR, 0, 0, 0);
        let photos = b.add(pictures, "Photos Library.photoslibrary", flags::DIR, 0, 0, 0);
        b.add(photos, "originals.db", 0, 40 * GB, 40 * GB, NOW);
        let private = b.add(ROOT, "private", flags::DIR, 0, 0, 0);
        let var = b.add(private, "var", flags::DIR, 0, 0, 0);
        let vm = b.add(var, "vm", flags::DIR, 0, 0, 0);
        b.add(vm, "sleepimage", 0, 2 * GB, 2 * GB, NOW);
        let t = b.finish("/".into(), info());

        let s = analyze(&t, NOW);
        let find = |title: &str| s.iter().find(|x| x.title == title).unwrap_or_else(|| panic!("no {title}: {s:?}"));
        let names = |x: &Suggestion| x.items.iter().map(|&i| t.name(i).to_string()).collect::<Vec<_>>();

        let installers = find("Old macOS installers");
        assert_eq!(names(installers), ["Install macOS Sequoia.app"], "the small stub is left out");
        assert!(matches!(installers.action, Action::Trash { preselect: false }));
        let ipsw = find("iPhone and iPad software downloads");
        assert_eq!(names(ipsw), ["iPhone17,1_26.0_Restore.ipsw"]);
        assert!(matches!(ipsw.action, Action::Trash { preselect: true }));
        assert!(risk::assess(&t, ipsw.items[0]).is_none(), "pre-selected, so nothing to warn about");
        // Backups, the hibernation image and Docker's disk are only explained.
        for title in ["iPhone and iPad backups", "Hibernation image", "Docker Desktop's disk", "Messages attachments", "Xcode archives", "Photos library"] {
            assert!(matches!(find(title).action, Action::Explain { .. }), "{title}");
        }
        assert!(!s.iter().any(|x| x.title == "Mail downloads"), "too small to mention");
        assert_eq!(find("Photos library").bytes, 40 * GB);
        assert_eq!(find("Docker Desktop's disk").bytes, 22 * GB, "space on disk, not the sparse file's size");
        assert_eq!(names(find("iPhone and iPad backups")), ["Backup"]);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn snapshot_list() {
        let text = "Snapshots for disk /:\ncom.apple.TimeMachine.2026-09-29-101502.local\n\
                    com.apple.TimeMachine.2026-09-29-111503.local\ncom.apple.os.update-ABCDEF\n";
        assert_eq!(count_snapshots(text), 2);
        assert_eq!(count_snapshots("Snapshots for disk /:\n"), 0);
    }
}
