//! iCloud Drive: removing the downloaded copy of a file that's also in
//! iCloud, as Finder's "Remove Download" does. The file stays in iCloud Drive
//! and in its folder here, and downloads again when it's opened, so this
//! frees space without deleting anything.
//!
//! Only files iCloud says are fully uploaded and whose local copy is the
//! current version qualify, so no change that exists only on this Mac can be
//! lost.

use std::path::Path;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSFileManager, NSString, NSURLResourceKey, NSURL};

/// What iCloud says about a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Not in iCloud Drive (or macOS didn't say).
    Local,
    /// Only in iCloud: nothing downloaded to remove.
    NotDownloaded,
    /// Still uploading, or with a conflict to sort out: the local copy may
    /// hold the only up-to-date version.
    Busy,
    /// In iCloud and downloaded here, the same version in both places.
    Downloaded,
}

fn url(path: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

fn value(url: &NSURL, key: &NSURLResourceKey) -> Option<Retained<AnyObject>> {
    let mut v = None;
    // SAFETY: `v` receives whatever type the key has, and is only read as that below.
    unsafe { url.getResourceValue_forKey_error(&mut v, key) }.ok()?;
    v
}

/// A key whose value is an NSNumber holding a boolean.
fn flag(url: &NSURL, key: &NSURLResourceKey) -> Option<bool> {
    let v = value(url, key)?;
    Some(unsafe { objc2::msg_send![&*v, boolValue] })
}

/// Whether `path` is in iCloud Drive, and whether its local copy can go.
pub fn state(path: &Path) -> State {
    objc2::rc::autoreleasepool(|_| {
        let u = url(path);
        // SAFETY: the keys are constant NSStrings from Foundation.
        let (ubiquitous, uploaded, uploading, conflicts, status) = unsafe {
            use objc2_foundation::*;
            (
                flag(&u, NSURLIsUbiquitousItemKey),
                flag(&u, NSURLUbiquitousItemIsUploadedKey),
                flag(&u, NSURLUbiquitousItemIsUploadingKey),
                flag(&u, NSURLUbiquitousItemHasUnresolvedConflictsKey),
                value(&u, NSURLUbiquitousItemDownloadingStatusKey)
                    .and_then(|v| v.downcast::<NSString>().ok())
                    .map(|s| s.to_string()),
            )
        };
        if ubiquitous != Some(true) {
            return State::Local;
        }
        classify(uploaded, uploading, conflicts, status.as_deref())
    })
}

/// `status` is `NSURLUbiquitousItemDownloadingStatusKey`'s value.
fn classify(uploaded: Option<bool>, uploading: Option<bool>, conflicts: Option<bool>, status: Option<&str>) -> State {
    match status {
        Some("NSURLUbiquitousItemDownloadingStatusNotDownloaded") => State::NotDownloaded,
        Some("NSURLUbiquitousItemDownloadingStatusCurrent")
            if uploaded == Some(true) && uploading != Some(true) && conflicts != Some(true) =>
        {
            State::Downloaded
        }
        _ => State::Busy,
    }
}

/// Whether a folder is in iCloud Drive (so its files may be).
pub fn in_icloud(path: &Path) -> bool {
    objc2::rc::autoreleasepool(|_| NSFileManager::defaultManager().isUbiquitousItemAtURL(&url(path)))
}

/// Remove the local copy of a file that's in iCloud, after checking again
/// that iCloud has the same version. The file stays where it is and
/// downloads again when it's opened.
pub fn remove_download(path: &Path) -> Result<(), String> {
    match state(path) {
        State::Downloaded => {}
        State::Local => return Err("it isn't in iCloud Drive".into()),
        State::NotDownloaded => return Err("it isn't downloaded".into()),
        State::Busy => return Err("iCloud hasn't finished syncing it".into()),
    }
    objc2::rc::autoreleasepool(|_| {
        NSFileManager::defaultManager()
            .evictUbiquitousItemAtURL_error(&url(path))
            .map_err(|e| e.localizedDescription().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_synced_copies_can_go() {
        let current = Some("NSURLUbiquitousItemDownloadingStatusCurrent");
        assert_eq!(classify(Some(true), Some(false), Some(false), current), State::Downloaded);
        assert_eq!(classify(Some(true), None, None, current), State::Downloaded);
        assert_eq!(classify(Some(false), Some(true), None, current), State::Busy, "still uploading");
        assert_eq!(classify(None, None, None, current), State::Busy, "upload state unknown");
        assert_eq!(classify(Some(true), None, Some(true), current), State::Busy, "conflict");
        assert_eq!(classify(Some(true), None, None, Some("NSURLUbiquitousItemDownloadingStatusDownloaded")), State::Busy, "an older version");
        assert_eq!(classify(Some(true), None, None, Some("NSURLUbiquitousItemDownloadingStatusNotDownloaded")), State::NotDownloaded);
    }

    /// Read-only: what iCloud says about the files at the top of your
    /// iCloud Drive. Run with `--ignored --nocapture` to see.
    #[test]
    #[ignore]
    fn icloud_drive_states() {
        let home = crate::platform::home_dir().unwrap();
        let drive = Path::new(&home).join("Library/Mobile Documents/com~apple~CloudDocs");
        assert!(in_icloud(&drive));
        for e in std::fs::read_dir(&drive).unwrap().flatten() {
            eprintln!("{:?}  {}", state(&e.path()), e.path().display());
        }
    }

    /// Read-only.
    #[test]
    fn ordinary_files_arent_in_icloud() {
        let exe = std::env::current_exe().unwrap();
        assert_eq!(state(&exe), State::Local);
        assert!(!in_icloud(exe.parent().unwrap()));
        assert!(remove_download(&exe).is_err());
    }
}
