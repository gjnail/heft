//! Keeping an eye on free space while Heft runs, with one desktop
//! notification per drive each time it gets close to full.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;

use crate::platform::{self, DriveInfo};
use crate::util::fmt_size;

const GB: u64 = 1 << 30;

/// The free-space limits offered in the menu.
pub const LIMITS: [u64; 4] = [5 * GB, 10 * GB, 20 * GB, 50 * GB];

/// How often drives are checked.
const EVERY: Duration = Duration::from_secs(60);

pub struct Settings {
    pub enabled: AtomicBool,
    pub limit: AtomicU64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Low {
    pub root: String,
    pub name: String,
    pub free: u64,
    pub total: u64,
}

/// Free space below which a drive counts as almost full: `limit`, or a
/// tenth of the drive when that's less, so a 32 GB stick with 4 GB free
/// doesn't count.
pub fn threshold(total: u64, limit: u64) -> u64 {
    limit.min(total / 10)
}

/// "C: (Data)", or the mount's name elsewhere.
pub fn display_name(d: &DriveInfo) -> String {
    if cfg!(windows) {
        let letter = d.root.trim_end_matches('\\');
        if d.label.is_empty() { letter.to_string() } else { format!("{letter} ({})", d.label) }
    } else {
        d.label.clone()
    }
}

/// Which of `drives` are almost full, and which of the warned ones have
/// recovered enough (a fifth above the threshold) to be warned about again.
fn assess(drives: &[DriveInfo], limit: u64, warned: &mut HashSet<String>) -> (Vec<Low>, Vec<Low>) {
    let low: Vec<Low> = drives
        .iter()
        .filter(|d| d.total > 0 && d.free < threshold(d.total, limit))
        .map(|d| Low { root: d.root.clone(), name: display_name(d), free: d.free, total: d.total })
        .collect();
    warned.retain(|root| drives.iter().any(|d| &d.root == root && d.free < threshold(d.total, limit) / 5 * 6));
    let fresh = low.iter().filter(|l| warned.insert(l.root.clone())).cloned().collect();
    (low, fresh)
}

/// Check local drives now and then every minute, on a background thread.
/// Each check sends the drives that are almost full; a drive that has just
/// become so also gets a desktop notification. `wake` is called after each
/// check so the window can update.
pub fn start(settings: Arc<Settings>, wake: impl Fn() + Send + 'static) -> Receiver<Vec<Low>> {
    let (tx, rx) = crossbeam_channel::bounded(8);
    let _ = std::thread::Builder::new().name("free space".into()).spawn(move || {
        let mut warned = HashSet::new();
        std::thread::sleep(Duration::from_secs(3));
        loop {
            let low = if settings.enabled.load(Ordering::Relaxed) {
                let mut drives: Vec<DriveInfo> =
                    platform::list_drives().into_iter().filter(|d| d.kind == "Local disk").collect();
                // Debug builds: HEFT_DEBUG_LOW pretends every drive is down to 1 GB.
                if cfg!(debug_assertions) && std::env::var_os("HEFT_DEBUG_LOW").is_some() {
                    drives.iter_mut().for_each(|d| d.free = d.free.min(GB));
                }
                let limit = settings.limit.load(Ordering::Relaxed);
                #[cfg(target_os = "macos")]
                crate::mac::menubar::set_watched(Some((&drives, limit)));
                let (low, fresh) = assess(&drives, limit, &mut warned);
                for l in &fresh {
                    notify(l);
                }
                low
            } else {
                #[cfg(target_os = "macos")]
                crate::mac::menubar::set_watched(None);
                warned.clear();
                Vec::new()
            };
            // The window may be hidden and not reading; dropping a report is fine.
            if let Err(crossbeam_channel::TrySendError::Disconnected(_)) = tx.try_send(low) {
                break; // the window is gone
            }
            wake();
            std::thread::sleep(EVERY);
        }
    });
    rx
}

fn notify(l: &Low) {
    let title = format!("{} is almost full", l.name);
    let body = format!("{} free of {}. Open Heft to see what's using the space.", fmt_size(l.free), fmt_size(l.total));
    #[cfg(windows)]
    crate::tray::notify(&title, &body);
    #[cfg(target_os = "macos")]
    crate::mac::notify::send(&title, &body);
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("notify-send")
            .args(["--app-name=Heft", "--urgency=normal", &title, &body])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(root: &str, free: u64, total: u64) -> DriveInfo {
        DriveInfo { root: root.into(), label: String::new(), fs: String::new(), kind: "Local disk", total, free, purgeable: 0 }
    }

    #[test]
    fn warns_once_until_the_drive_recovers() {
        let mut warned = HashSet::new();
        let limit = 10 * GB;
        let (low, fresh) = assess(&[drive("C", 4 * GB, 500 * GB), drive("D", 300 * GB, 500 * GB)], limit, &mut warned);
        assert_eq!((low.len(), fresh.len()), (1, 1));
        // Still low: listed, but no second notification.
        let (low, fresh) = assess(&[drive("C", 3 * GB, 500 * GB)], limit, &mut warned);
        assert_eq!((low.len(), fresh.len()), (1, 0));
        // Just above the limit isn't enough to warn again later...
        let (low, _) = assess(&[drive("C", 11 * GB, 500 * GB)], limit, &mut warned);
        assert!(low.is_empty());
        assert_eq!(assess(&[drive("C", 9 * GB, 500 * GB)], limit, &mut warned).1.len(), 0);
        // ...but clearly recovering is.
        assess(&[drive("C", 40 * GB, 500 * GB)], limit, &mut warned);
        assert_eq!(assess(&[drive("C", 9 * GB, 500 * GB)], limit, &mut warned).1.len(), 1);
    }

    #[test]
    fn small_drives_use_a_tenth() {
        assert_eq!(threshold(32 * GB, 10 * GB), 32 * GB / 10);
        assert_eq!(threshold(2000 * GB, 10 * GB), 10 * GB);
        let mut warned = HashSet::new();
        assert!(assess(&[drive("E", 4 * GB, 32 * GB)], 10 * GB, &mut warned).0.is_empty());
    }
}
