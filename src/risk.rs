//! Plain-language warnings for files and folders that are risky to delete:
//! system files, installed programs, app data, cloud-synced folders, keys.
//!
//! The rules only look at paths and names, so they're cheap enough to run for
//! every visible row. They err on the side of warning; nothing here blocks a
//! delete by itself, it makes sure the person sees why it might be a bad idea.

use crate::platform::SEP;
use crate::tree::{NodeId, Tree};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    /// Might break an app or lose data.
    Caution,
    /// Might stop the computer from starting or working.
    Danger,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Risk {
    pub level: Level,
    /// Short label, e.g. "Part of Windows".
    pub title: &'static str,
    /// One or two sentences on what could go wrong and what to do instead.
    pub detail: &'static str,
}

const fn danger(title: &'static str, detail: &'static str) -> Option<Risk> {
    Some(Risk { level: Level::Danger, title, detail })
}

const fn caution(title: &'static str, detail: &'static str) -> Option<Risk> {
    Some(Risk { level: Level::Caution, title, detail })
}

pub fn assess(tree: &Tree, id: NodeId) -> Option<Risk> {
    if tree.node(id).flags & crate::tree::flags::HARDLINK != 0 {
        return None; // removing an extra name leaves the file itself alone
    }
    assess_path(&tree.path(id))
}

/// Warning for deleting `path`, if any.
pub fn assess_path(path: &str) -> Option<Risk> {
    let lower = path.to_lowercase();
    let comps: Vec<&str> = lower.split(SEP).filter(|c| !c.is_empty()).collect();
    system(&lower, &comps).or_else(|| everywhere(&comps))
}

// ---------------------------------------------------------------------------
// Operating system locations

#[cfg(windows)]
fn system(p: &str, comps: &[&str]) -> Option<Risk> {
    let env = |k: &str, default: &str| std::env::var(k).unwrap_or_else(|_| default.into()).to_lowercase();
    let windir = env("SystemRoot", "C:\\Windows");
    let under = |base: &str| p == base || p.starts_with(&format!("{base}\\"));

    if comps.len() <= 1 {
        return danger("A whole drive", "This is the top of a drive. Deleting it deletes everything on it.");
    }
    if under(&format!("{windir}\\winsxs")) {
        return danger(
            "Windows component store",
            "Windows needs these files to run and update. Much of this folder is shared with System32 through \
             hard links, so it looks bigger than it is. Shrink it with Component store cleanup in the Cleaner \
             page, never by hand.",
        );
    }
    if under(&format!("{windir}\\installer")) {
        return danger(
            "Windows installer cache",
            "Programs need these files to update, repair and uninstall. Deleting them leaves programs you \
             can't remove or patch.",
        );
    }
    if under(&format!("{windir}\\system32\\drivers")) {
        return danger("Device drivers", "Deleting drivers can stop Windows from starting or disable hardware.");
    }
    if under(&windir) {
        return danger(
            "Part of Windows",
            "Deleting files here can stop Windows from starting. Use Disk Cleanup or Storage Sense to free \
             space in the Windows folder.",
        );
    }

    let name = comps.last().copied().unwrap_or("");
    if comps.len() == 2 {
        // Directly on the drive.
        match name {
            "pagefile.sys" | "swapfile.sys" => {
                return danger(
                    "Virtual memory",
                    "Windows uses this file as extra memory. It can't be deleted while Windows runs; change its \
                     size in Settings > System > About > Advanced system settings instead.",
                );
            }
            "hiberfil.sys" => {
                return caution(
                    "Hibernation file",
                    "Used by hibernation and Fast Startup. If you don't use them, run `powercfg /hibernate off` \
                     as administrator and Windows removes it. Don't delete it directly.",
                );
            }
            "bootmgr" | "bootnxt" | "boot" | "efi" | "recovery" => {
                return danger("Startup files", "Windows needs these to start. Deleting them can make the PC unbootable.");
            }
            "system volume information" => {
                return danger(
                    "Restore points",
                    "Holds System Restore points and shadow copies. Manage them in System Protection settings.",
                );
            }
            "$recycle.bin" => {
                return caution("Recycle Bin", "Empty the Recycle Bin instead of deleting this folder.");
            }
            "windows.old" => {
                return caution(
                    "Previous Windows installation",
                    "Lets you roll back a Windows update. Remove it with Disk Cleanup (Previous Windows \
                     installations) rather than deleting it directly.",
                );
            }
            "program files" | "program files (x86)" | "users" => {
                return danger("System folder", "Windows and your programs expect this folder to exist.");
            }
            "programdata" => {
                return caution("Shared program data", "Settings and data that installed programs and Windows share.");
            }
            _ => {}
        }
    }
    if matches!(comps.get(1), Some(&"program files") | Some(&"program files (x86)")) {
        return caution(
            "Installed program",
            "Deleting a program's folder leaves it broken and still listed as installed. Uninstall it from the \
             Programs page or Windows Settings instead.",
        );
    }
    if comps.get(1) == Some(&"programdata") {
        return caution("Shared program data", "Settings and data that installed programs and Windows share.");
    }
    if comps.get(1) == Some(&"users") {
        if comps.len() == 3 {
            return danger("A user profile", "Everything that belongs to this user: documents, settings and apps.");
        }
        if name.starts_with("ntuser.dat") || name.starts_with("usrclass.dat") {
            return danger("User registry", "Windows can't sign this user in without it.");
        }
        if comps.len() == 4 && comps[3] == "appdata" {
            return caution("App settings and data", "Everything your apps store for you. Delete inside it selectively.");
        }
        if let Some(i) = comps.iter().position(|c| *c == "appdata") {
            // Temp folders are meant to be cleaned.
            if comps.get(i + 1) == Some(&"local") && comps.get(i + 2) == Some(&"temp") {
                return None;
            }
            return caution(
                "App data",
                "Settings, saved data or caches for an app. Deleting it can reset the app or lose things like \
                 saved logins and game saves. Caches are usually safe; close the app first.",
            );
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn system(p: &str, comps: &[&str]) -> Option<Risk> {
    let under = |base: &str| p == base || p.starts_with(&format!("{base}/"));
    if comps.is_empty() {
        return danger("The whole disk", "This is the top of the disk. Deleting it deletes everything on it.");
    }
    if under("/usr/local") || under("/opt") {
        return caution(
            "Installed tools",
            "Software installed by you, Homebrew or MacPorts. Uninstall it with the tool that installed it.",
        );
    }
    // Temporary files are meant to be cleaned.
    if under("/private/tmp") || under("/private/var/tmp") || under("/private/var/folders") {
        return None;
    }
    if p == "/private/var/vm/sleepimage" {
        return caution(
            "Hibernation image",
            "macOS saves what's in memory here so your work survives if the battery runs out during sleep. Don't \
             delete it directly; the Suggestions tab explains how to turn it off.",
        );
    }
    if under("/private/var/log") {
        return caution("System logs", "macOS trims these itself. Remove old ones selectively if they're very large.");
    }
    for base in ["/system", "/usr", "/bin", "/sbin", "/private", "/library/apple", "/cores"] {
        if under(base) {
            return danger("Part of macOS", "macOS needs these files. Deleting them can stop the Mac from starting.");
        }
    }
    if comps.len() == 1 {
        return danger("System folder", "macOS and your apps expect this folder to exist.");
    }
    if comps[0] == "volumes" && comps.len() == 2 {
        return danger("A whole drive", "This is the top of a drive. Deleting it deletes everything on it.");
    }
    if comps[0] == "users" && comps.len() == 2 {
        return danger("A user's home folder", "Everything that belongs to this user: documents, settings and apps.");
    }
    if comps.contains(&"keychains") {
        return danger("Keychain", "Saved passwords and certificates. Deleting it can lock you out of accounts.");
    }
    if comps.iter().any(|c| c.ends_with(".photoslibrary")) {
        return danger(
            "Photos library",
            "Your whole Photos library. Anything not synced to iCloud or backed up is gone for good.",
        );
    }
    if comps.iter().any(|c| c.ends_with(".musiclibrary") || c.ends_with(".tvlibrary") || *c == "itunes library.itl") {
        return caution(
            "Music or TV library",
            "Your playlists, ratings and play history. The songs and videos are in the Media folder next to it; \
             remove them in the Music or TV app instead.",
        );
    }
    if comps.contains(&"mobile documents") {
        return cloud();
    }
    if comps.contains(&"backups.backupdb") {
        return caution("Time Machine backup", "Manage old backups from Time Machine instead of deleting them here.");
    }
    // An app bundle, or something inside one.
    let apps = if comps[0] == "applications" { Some(1) } else { (comps[0] == "users" && comps.get(2) == Some(&"applications")).then_some(3) };
    if let Some(i) = apps
        && comps.len() == i + 1
    {
        return caution(
            "An app",
            "Moving an app to the Trash is how Mac apps are uninstalled, but its settings stay in ~/Library.",
        );
    }
    // Folders named after a bundle id (`com.example.app`) end in `.app` too.
    if comps.windows(2).any(|w| w[0].ends_with(".app") && w[1] == "contents") {
        return caution(
            "Part of an app",
            "Deleting files inside an app breaks it, and macOS may refuse to open it. Remove the whole app instead.",
        );
    }
    // The system's /Library, and your own ~/Library.
    let lib = if comps[0] == "library" { Some(1) } else { (comps[0] == "users" && comps.get(2) == Some(&"library")).then_some(3) };
    if let Some(i) = lib {
        let rest = &comps[i..];
        return match rest {
            ["caches" | "logs", ..] => None, // meant to be cleared
            // Copies of iOS that Finder downloads again when it needs one.
            ["itunes", .., name] if name.ends_with(".ipsw") => None,
            ["cloudstorage", ..] => cloud(),
            ["mail", ..] => caution(
                "Mail",
                "Your email as Mail keeps it. Messages that aren't on a mail server, like those in On My Mac \
                 mailboxes, are gone for good.",
            ),
            ["messages", ..] => caution(
                "Messages history",
                "Your conversations and their attachments. Delete conversations in Messages instead.",
            ),
            ["application support", "mobilesync", "backup", ..] => caution(
                "iPhone or iPad backup",
                "Backups of your devices. Delete old ones in Finder (select the device, then Manage Backups) so \
                 you can see which is which.",
            ),
            // Keys, virtual disks and the like say more than the folder does.
            _ => everywhere(comps).or_else(|| {
                caution(
                    "App settings and data",
                    "Settings and saved data for your apps. Deleting it can reset an app or lose its data.",
                )
            }),
        };
    }
    None
}

#[cfg(all(unix, not(target_os = "macos")))]
fn system(p: &str, comps: &[&str]) -> Option<Risk> {
    let under = |base: &str| p == base || p.starts_with(&format!("{base}/"));
    if comps.is_empty() {
        return danger("The whole system", "This is the root of the file system. Deleting it deletes everything.");
    }
    if under("/usr/local") || under("/opt") {
        return caution("Installed software", "Uninstall it with the tool that installed it.");
    }
    for base in ["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/etc", "/boot", "/proc", "/sys", "/dev", "/var/lib", "/snap"] {
        if under(base) {
            return danger(
                "Part of the system",
                "The system or your package manager needs these files. Remove software with your package manager instead.",
            );
        }
    }
    if under("/var/log") {
        return caution("System logs", "Trim logs with `journalctl --vacuum-size=200M` or logrotate instead.");
    }
    if comps.len() == 1 {
        return danger("System folder", "The system expects this folder to exist.");
    }
    if comps.first() == Some(&"home") && comps.len() == 2 {
        return danger("A user's home folder", "Everything that belongs to this user: documents, settings and apps.");
    }
    if comps.first() == Some(&"home") {
        if comps.get(2) == Some(&".cache") {
            return None;
        }
        if matches!(comps.get(2), Some(&".config") | Some(&".local")) {
            return caution(
                "App settings and data",
                "Settings and saved data for your apps. Deleting it can reset an app or lose its data.",
            );
        }
    }
    None
}

fn cloud() -> Option<Risk> {
    caution(
        "Synced with the cloud",
        "Deleting here also deletes it from the cloud and your other devices. It usually stays in the service's \
         online recycle bin for a while.",
    )
}

// ---------------------------------------------------------------------------
// Anywhere on any system

fn everywhere(comps: &[&str]) -> Option<Risk> {
    let name = comps.last().copied().unwrap_or("");
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");

    if comps.iter().any(|c| matches!(*c, ".ssh" | ".gnupg")) || matches!(ext, "kdbx" | "kdb" | "pem" | "ppk" | "pfx" | "p12")
        || matches!(name, "wallet.dat" | "id_rsa" | "id_ed25519" | "id_ecdsa")
    {
        return danger(
            "Keys or passwords",
            "Encryption keys, a password vault or a crypto wallet. Without a backup, deleting it can lock you out \
             for good.",
        );
    }
    if comps.iter().any(|c| {
        matches!(*c, "onedrive" | "dropbox" | "google drive" | "icloud drive" | "iclouddrive" | "box")
            || c.starts_with("onedrive - ")
    }) {
        return cloud();
    }
    if comps.contains(&".git") {
        return caution(
            "Project history",
            "A Git repository's history. Deleting it loses every commit and any work that isn't pushed.",
        );
    }
    if matches!(ext, "vhdx" | "vhd" | "vmdk" | "vdi" | "qcow2" | "avhdx" | "hds") || name == "docker.raw" {
        return caution(
            "Virtual disk",
            "A virtual machine, WSL or Docker disk. Deleting it deletes everything inside that system. Shrink it \
             instead if it's too big.",
        );
    }
    if matches!(ext, "pst" | "mbox") {
        return caution("Mailbox", "Stored email. Deleting it loses any messages that aren't on the mail server.");
    }
    if comps.iter().any(|c| matches!(*c, "saves" | "saved games" | "savegames")) {
        return caution("Saved games", "Game progress. Deleting it can't be undone unless the game syncs saves to the cloud.");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(p: &str) -> Option<Level> {
        assess_path(p).map(|r| r.level)
    }

    #[test]
    #[cfg(windows)]
    fn windows_locations() {
        assert_eq!(level("C:\\"), Some(Level::Danger));
        assert_eq!(level("C:\\Windows\\System32\\kernel32.dll"), Some(Level::Danger));
        assert_eq!(assess_path("C:\\Windows\\WinSxS").unwrap().title, "Windows component store");
        assert_eq!(level("C:\\pagefile.sys"), Some(Level::Danger));
        assert_eq!(level("C:\\hiberfil.sys"), Some(Level::Caution));
        assert_eq!(level("C:\\Program Files\\Some App"), Some(Level::Caution));
        assert_eq!(level("C:\\Users\\ana"), Some(Level::Danger));
        assert_eq!(level("C:\\Users\\ana\\NTUSER.DAT"), Some(Level::Danger));
        assert_eq!(level("C:\\Users\\ana\\AppData\\Roaming\\App"), Some(Level::Caution));
        assert_eq!(level("C:\\Users\\ana\\AppData\\Local\\Temp\\x.tmp"), None);
        assert_eq!(level("C:\\Users\\ana\\Videos\\clip.mp4"), None);
        assert_eq!(level("D:\\Games\\Big\\data.pak"), None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_locations() {
        assert_eq!(level("/"), Some(Level::Danger));
        assert_eq!(level("/usr/lib/libc.so.6"), Some(Level::Danger));
        assert_eq!(level("/var/log/syslog"), Some(Level::Caution));
        assert_eq!(level("/home/ana"), Some(Level::Danger));
        assert_eq!(level("/home/ana/.cache/thing"), None);
        assert_eq!(level("/home/ana/Videos/clip.mp4"), None);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn mac_locations() {
        let title = |p: &str| assess_path(p).map(|r| r.title);
        assert_eq!(level("/"), Some(Level::Danger));
        assert_eq!(level("/System/Library/CoreServices"), Some(Level::Danger));
        assert_eq!(level("/usr/lib/libSystem.B.dylib"), Some(Level::Danger));
        assert_eq!(level("/Library/Apple/usr"), Some(Level::Danger));
        assert_eq!(level("/private/etc/hosts"), Some(Level::Danger));
        assert_eq!(level("/private/var/db/receipts"), Some(Level::Danger));
        assert_eq!(level("/Library"), Some(Level::Danger));
        assert_eq!(level("/Volumes/Backup"), Some(Level::Danger));
        assert_eq!(level("/Users/ana"), Some(Level::Danger));
        assert_eq!(level("/Users/ana/Library/Keychains/login.keychain-db"), Some(Level::Danger));
        assert_eq!(level("/Users/ana/Pictures/Photos Library.photoslibrary"), Some(Level::Danger));
        assert_eq!(title("/private/var/vm/sleepimage"), Some("Hibernation image"));
        assert_eq!(level("/private/var/log/system.log"), Some(Level::Caution));
        assert_eq!(level("/private/var/folders/xy/T/thing.tmp"), None);
        assert_eq!(level("/opt/homebrew/Cellar"), Some(Level::Caution));
        assert_eq!(title("/Applications/Editor.app"), Some("An app"));
        assert_eq!(title("/Users/ana/Applications/Game.app"), Some("An app"));
        assert_eq!(title("/Applications/Editor.app/Contents/MacOS/Editor"), Some("Part of an app"));
        assert_eq!(title("/Users/ana/Library/Mobile Documents/com~apple~CloudDocs/a.txt"), Some("Synced with the cloud"));
        assert_eq!(title("/Users/ana/Library/CloudStorage/OneDrive-Personal/a.txt"), Some("Synced with the cloud"));
        assert_eq!(title("/Users/ana/Library/Mail/V10"), Some("Mail"));
        assert_eq!(title("/Users/ana/Library/Messages/chat.db"), Some("Messages history"));
        assert_eq!(title("/Users/ana/Music/Music/Music Library.musiclibrary"), Some("Music or TV library"));
        assert_eq!(
            title("/Users/ana/Library/Application Support/MobileSync/Backup/00008110-001A"),
            Some("iPhone or iPad backup")
        );
        assert_eq!(
            title("/Users/ana/Library/Containers/com.docker.docker/Data/vms/0/data/Docker.raw"),
            Some("Virtual disk")
        );
        assert_eq!(title("/Users/ana/Library/Application Support/Some App/prefs.json"), Some("App settings and data"));
        assert_eq!(level("/Users/ana/Library/Caches/com.example.app/blob"), None);
        assert_eq!(level("/Users/ana/Library/Logs/app.log"), None);
        assert_eq!(level("/Users/ana/Library/iTunes/iPhone Software Updates/iPhone_18.0_Restore.ipsw"), None);
        // A folder that happens to be called Library, in a Unity project, is just a folder.
        assert_eq!(level("/Users/ana/Projects/game/Library/ShaderCache"), None);
        assert_eq!(level("/Users/ana/Movies/clip.mov"), None);
    }

    #[test]
    fn anywhere() {
        let p = |parts: &[&str]| format!("{SEP}data{SEP}{}", parts.join(&SEP.to_string()));
        assert_eq!(level(&p(&["proj", ".git", "objects"])), Some(Level::Caution));
        assert_eq!(level(&p(&["keys", "vault.kdbx"])), Some(Level::Danger));
        assert_eq!(level(&p(&["OneDrive", "notes.txt"])), Some(Level::Caution));
        assert_eq!(level(&p(&["wsl", "ext4.vhdx"])), Some(Level::Caution));
        assert_eq!(level(&p(&["music", "song.mp3"])), None);
    }
}
