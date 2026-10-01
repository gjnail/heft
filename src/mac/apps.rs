//! Installed apps on macOS: listing and measuring app bundles, moving an app
//! to the Trash or running its own uninstaller, and finding what it left
//! behind. The Mac counterpart of `programs.rs`; updates through Homebrew
//! are in `brew`.

pub mod brew;
pub mod orphans;
pub mod updaters;

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use rayon::prelude::*;

use super::{bundle_info, plist_str, read_plist, Domain, LaunchJob};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    AppStore,
    Homebrew,
    Other,
}

/// An app's own uninstaller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uninstaller {
    /// As found: an app, or a link or Finder alias to one.
    pub path: PathBuf,
    /// The app it opens.
    pub app: PathBuf,
    /// Named after the app ("Uninstall Foo"), not just sitting in its folder.
    pub named: bool,
}

#[derive(Clone, Debug)]
pub struct App {
    /// The bundle. For a link in Applications, the bundle it points to.
    pub path: PathBuf,
    /// The link in Applications, when the app itself lives elsewhere.
    pub link: Option<PathBuf>,
    /// The folder it was found in: an Applications folder or a vendor
    /// folder inside one.
    pub folder: PathBuf,
    /// The name Finder shows: the bundle's (or its link's) file name.
    pub name: String,
    /// The name inside the bundle, when it differs ("Live" for "Ableton Live 12 Suite").
    pub bundle_name: Option<String>,
    pub id: Option<String>,
    pub version: String,
    pub copyright: Option<String>,
    /// Finder's "Date Added" (Unix time), else when the bundle was created.
    pub added: i64,
    /// When it was last opened, according to Spotlight.
    pub last_used: Option<i64>,
    /// Spotlight's size for the bundle, until Heft measures it.
    pub estimated: u64,
    pub measured: Option<u64>,
    pub source: Source,
    /// The Homebrew cask that installed it.
    pub cask: Option<String>,
    /// Read in the background with `codesign`.
    pub signature: Option<Signature>,
    pub uninstaller: Option<Uninstaller>,
    /// The app's own updater, if it has one.
    pub updater: Option<updaters::Updater>,
}

impl App {
    pub fn size(&self) -> u64 {
        self.measured.filter(|&m| m > 0).unwrap_or(self.estimated)
    }

    /// Who made it: the Developer ID it's signed with, Apple for Apple's own
    /// apps, else the copyright line (App Store apps are all signed by Apple).
    pub fn publisher(&self) -> String {
        let sig = self.signature.as_ref();
        sig.and_then(Signature::developer)
            .or_else(|| sig.is_some_and(Signature::is_apple).then(|| "Apple".to_string()))
            .or_else(|| self.copyright.as_deref().and_then(copyright_holder))
            .unwrap_or_default()
    }

    pub fn is_from_app_store(&self) -> bool {
        self.source == Source::AppStore || self.signature.as_ref().is_some_and(Signature::is_app_store)
    }

    /// Where it shows up in Finder: its link in Applications if it has one.
    pub fn shown_path(&self) -> &Path {
        self.link.as_deref().unwrap_or(&self.path)
    }

    /// Moving it to the Trash needs an administrator password (apps from
    /// installer packages and the App Store usually belong to root).
    pub fn needs_admin(&self) -> bool {
        needs_admin(&self.path)
    }

    pub fn identity(&self) -> Identity {
        Identity {
            id: self.id.as_ref().map(|i| i.to_lowercase()),
            names: names_of(&[Some(&self.name), self.bundle_name.as_ref()], &self.path, self.link.as_deref()),
            paths: [Some(self.path.clone()), self.link.clone()].into_iter().flatten().collect(),
        }
    }
}

// ----------------------------------------------------------------------
// Listing

/// Folders apps are installed in. `/System/Applications` is left out: those apps are part of macOS, on its
/// read-only system volume, and can't be removed.
fn app_folders() -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from("/Applications")];
    if let Some(h) = crate::platform::home_dir() {
        out.push(Path::new(&h).join("Applications"));
    }
    out
}

fn has_app_ext(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("app"))
}

fn stem(p: &Path) -> String {
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if has_app_ext(p) { name[..name.len() - 4].to_string() } else { name }
}

fn is_uninstaller_name(stem: &str) -> bool {
    stem.to_lowercase().contains("uninstall")
}

/// What an uninstaller is for: "Uninstall Foo" and "Foo Uninstaller" → "foo".
fn uninstaller_subject(stem: &str) -> String {
    let lower = stem.to_lowercase().replace("uninstaller", " ").replace("uninstall", " ").replace(['-', '_'], " ");
    let words: Vec<&str> = lower.split_whitespace().filter(|w| *w != "for").collect();
    words.join(" ")
}

/// Something found in an Applications folder.
struct Found {
    path: PathBuf,
    link: Option<PathBuf>,
    folder: PathBuf,
}

/// A possible uninstaller and the folder it was found in.
struct Candidate {
    folder: PathBuf,
    path: PathBuf,
    /// Right in an Applications folder, rather than with a vendor's apps.
    loose: bool,
}

/// Apps in `dir`, and in folders one level down (vendor folders such as
/// `/Applications/Utilities` or `Adobe Photoshop 2026`). Bundles aren't
/// entered, and links are only followed to see whether they lead to an app.
fn scan_folder(dir: &Path, depth: usize, found: &mut Vec<Found>, uninstallers: &mut Vec<Candidate>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if name.starts_with('.') {
            continue;
        }
        let Ok(md) = std::fs::symlink_metadata(&path) else { continue };
        let is_app = has_app_ext(&path);
        if is_uninstaller_name(&stem(&path)) {
            if md.is_dir() && !is_app {
                // A folder of uninstallers next to the apps they remove.
                let Ok(rd) = std::fs::read_dir(&path) else { continue };
                let mut inner: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
                inner.sort();
                for p in inner.into_iter().filter(|p| is_uninstaller_name(&stem(p))) {
                    uninstallers.push(Candidate { folder: dir.to_path_buf(), path: p, loose: false });
                }
            } else {
                uninstallers.push(Candidate { folder: dir.to_path_buf(), path, loose: depth == 0 });
            }
            continue;
        }
        if md.file_type().is_symlink() {
            if let Some(target) = linked_app(&path) {
                found.push(Found { path: target, link: Some(path), folder: dir.to_path_buf() });
            }
        } else if md.is_dir() {
            if is_app {
                found.push(Found { path, link: None, folder: dir.to_path_buf() });
            } else if depth == 0 {
                scan_folder(&path, 1, found, uninstallers);
            }
        }
    }
}

/// The app a link in Applications points to, unless it's part of macOS
/// (Safari's link into the system volume).
fn linked_app(link: &Path) -> Option<PathBuf> {
    let target = std::fs::canonicalize(link).ok()?;
    (has_app_ext(&target) && target.is_dir() && !is_system_path(&target)).then_some(target)
}

fn is_system_path(p: &Path) -> bool {
    p.starts_with("/System")
}

/// Resolve an uninstaller entry to the app it opens: the app itself, or
/// the target of a link or Finder alias.
fn resolve_app(path: &Path) -> Option<PathBuf> {
    let md = std::fs::symlink_metadata(path).ok()?;
    if md.is_dir() {
        return has_app_ext(path).then(|| path.to_path_buf());
    }
    let target = resolve_alias(path)?;
    (has_app_ext(&target) && target.is_dir() && !is_system_path(&target)).then_some(target)
}

/// Where a symbolic link or Finder alias leads, without showing UI or
/// mounting anything.
fn resolve_alias(path: &Path) -> Option<PathBuf> {
    use objc2_foundation::{NSString, NSURLBookmarkResolutionOptions, NSURL};
    objc2::rc::autoreleasepool(|_| {
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        let opts = NSURLBookmarkResolutionOptions::WithoutUI | NSURLBookmarkResolutionOptions::WithoutMounting;
        let resolved = NSURL::URLByResolvingAliasFileAtURL_options_error(&url, opts).ok()?;
        let p = PathBuf::from(resolved.path()?.to_string());
        (p != path).then_some(p)
    })
}

/// Pick the uninstaller that belongs to an app: one named after it in its
/// folder, any uninstaller in a vendor folder holding only this app, or an
/// uninstaller app inside its bundle.
fn find_uninstaller(names: &[String], folder: &Path, alone: bool, bundle: &Path, candidates: &[Candidate]) -> Option<Uninstaller> {
    let here: Vec<&Candidate> = candidates.iter().filter(|c| c.folder == folder).collect();
    for named in [true, false] {
        if !named && !alone {
            break;
        }
        for c in &here {
            let subject = uninstaller_subject(&stem(&c.path));
            if named && !names.contains(&subject) {
                continue;
            }
            if let Some(app) = resolve_app(&c.path)
                && app != bundle
            {
                return Some(Uninstaller { path: c.path.clone(), app, named });
            }
        }
    }
    for sub in ["Contents/Resources", "Contents/SharedSupport", "Contents/Helpers", "Contents/Library", "Contents/MacOS"] {
        let Ok(rd) = std::fs::read_dir(bundle.join(sub)) else { continue };
        let mut apps: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.path())
            .filter(|p| has_app_ext(p) && is_uninstaller_name(&stem(p)))
            .collect();
        apps.sort();
        if let Some(p) = apps.into_iter().next() {
            return Some(Uninstaller { path: p.clone(), app: p, named: true });
        }
    }
    None
}

/// Names an app goes by, lower-case: the given names, its file name, the
/// name of its link, and each without a trailing version.
fn names_of(names: &[Option<&String>], path: &Path, link: Option<&Path>) -> Vec<String> {
    let mut all: Vec<String> = names.iter().flatten().map(|n| n.to_string()).collect();
    all.push(stem(path));
    all.extend(link.map(stem));
    let bases: Vec<String> = all.iter().map(|n| base_name(n)).collect();
    let mut out: Vec<String> = Vec::new();
    for n in all.into_iter().chain(bases) {
        let n = n.trim().to_lowercase();
        if !n.is_empty() && !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// "Reason 13" → "Reason", "Adobe Photoshop 2026" → "Adobe Photoshop".
pub fn base_name(name: &str) -> String {
    let mut s = name.trim().to_string();
    while let Some((head, last)) = s.rsplit_once(' ') {
        let is_version = last.trim_start_matches(['v', 'V']).chars().all(|c| c.is_ascii_digit() || c == '.')
            && last.chars().any(|c| c.is_ascii_digit());
        if !is_version {
            break;
        }
        s = head.trim_end_matches([' ', '-']).to_string();
    }
    s
}

/// iPhone and iPad apps on a Mac keep their details in a wrapper:
/// `Foo.app/Wrapper/Foo.app/Info.plist`.
fn wrapped_info(app: &Path) -> Option<plist::Dictionary> {
    let wrapper = app.join("Wrapper");
    if !std::fs::symlink_metadata(&wrapper).ok()?.is_dir() {
        return None;
    }
    let inner = std::fs::read_dir(&wrapper).ok()?.flatten().map(|e| e.path()).find(|p| has_app_ext(p))?;
    let plist = inner.join("Info.plist");
    if !std::fs::symlink_metadata(&plist).ok()?.is_file() {
        return None;
    }
    read_plist(&plist)?.into_dictionary()
}

/// Installed apps in `/Applications` and `~/Applications`.
pub fn list() -> Vec<App> {
    list_in(&app_folders(), &brew::installed_casks())
}

fn list_in(roots: &[PathBuf], casks: &[brew::Cask]) -> Vec<App> {
    let mut found = Vec::new();
    let mut candidates = Vec::new();
    for root in roots {
        scan_folder(root, 0, &mut found, &mut candidates);
    }
    // The same app reached directly and through a link is listed once.
    found.sort_by_key(|f| f.link.is_some());
    let mut seen = HashSet::new();
    found.retain(|f| seen.insert(std::fs::canonicalize(&f.path).unwrap_or_else(|_| f.path.clone())));

    let mut apps: Vec<App> = found.par_iter().map(|f| describe(f, casks)).collect();
    let mut used: HashSet<PathBuf> = HashSet::new();
    for i in 0..apps.len() {
        let a = &apps[i];
        let names = a.identity().names;
        let is_root = roots.contains(&a.folder) || a.folder == Path::new("/Applications/Utilities");
        let alone = !is_root && apps.iter().filter(|o| o.folder == a.folder).count() == 1;
        let u = find_uninstaller(&names, &a.folder, alone, &a.path, &candidates);
        if let Some(u) = &u {
            used.insert(u.path.clone());
        }
        apps[i].uninstaller = u;
    }
    // An app right in Applications that merely has "uninstall" in its name
    // and belongs to nothing listed is an app like any other. Unclaimed
    // uninstallers in vendor folders are for plug-ins and the like.
    let leftover: Vec<Found> = candidates
        .iter()
        .filter(|c| c.loose && !used.contains(&c.path) && has_app_ext(&c.path))
        .filter(|c| std::fs::symlink_metadata(&c.path).is_ok_and(|m| m.is_dir()))
        .map(|c| Found { path: c.path.clone(), link: None, folder: c.folder.clone() })
        .collect();
    apps.extend(leftover.iter().map(|f| describe(f, casks)));
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

fn describe(f: &Found, casks: &[brew::Cask]) -> App {
    let b = bundle_info(&f.path).unwrap_or_default();
    let wrapped = if b.id.is_none() { wrapped_info(&f.path) } else { None };
    let get = |k: &str| wrapped.as_ref().and_then(|d| plist_str(d, k));
    // Finder shows the file name, so that's what people know an app by.
    let name = stem(f.link.as_deref().unwrap_or(&f.path));
    let inner = if wrapped.is_some() { get("CFBundleDisplayName").or_else(|| get("CFBundleName")) } else { Some(b.name.clone()) };
    let spot = spotlight::info(&f.path);
    let cask = brew::cask_for(casks, &f.path, f.link.as_deref()).map(|c| c.token.clone());
    let receipt = f.path.join("Contents/_MASReceipt/receipt").is_file() || f.path.join("Wrapper/iTunesMetadata.plist").is_file();
    let added_at = f.link.as_deref().unwrap_or(&f.path);
    App {
        path: f.path.clone(),
        link: f.link.clone(),
        folder: f.folder.clone(),
        bundle_name: inner.filter(|n| !n.is_empty() && *n != name),
        name,
        id: b.id.clone().or_else(|| get("CFBundleIdentifier")),
        version: b.version.clone().or_else(|| get("CFBundleShortVersionString")).unwrap_or_default(),
        copyright: b.copyright.clone(),
        added: added_time(added_at).or_else(|| birth_time(added_at)).unwrap_or(0),
        last_used: spot.last_used,
        estimated: spot.size,
        measured: None,
        source: if receipt {
            Source::AppStore
        } else if cask.is_some() {
            Source::Homebrew
        } else {
            Source::Other
        },
        updater: updaters::updater(&f.path),
        cask,
        signature: None,
        uninstaller: None,
    }
}

/// Finder's "Date Added": when the item was put in its folder.
fn added_time(path: &Path) -> Option<i64> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut al: libc::attrlist = unsafe { std::mem::zeroed() };
    al.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    al.commonattr = libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_ADDEDTIME;
    // u32 length, attribute_set_t (5 × u32), then a timespec.
    let mut buf = [0u8; 64];
    let r = unsafe {
        libc::getattrlist(
            c.as_ptr(),
            (&mut al as *mut libc::attrlist).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::FSOPT_NOFOLLOW,
        )
    };
    let returned = u32::from_ne_bytes(buf[4..8].try_into().ok()?);
    if r != 0 || returned & libc::ATTR_CMN_ADDEDTIME == 0 {
        return None;
    }
    let secs = i64::from_ne_bytes(buf[24..32].try_into().ok()?);
    (secs > 0).then_some(secs)
}

fn birth_time(path: &Path) -> Option<i64> {
    let t = std::fs::symlink_metadata(path).ok()?.created().ok()?;
    t.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs() as i64)
}

/// Spotlight's details for a file, through the Metadata framework (no
/// process to start, and it only reads Spotlight's index).
mod spotlight {
    use std::ffi::c_void;
    use std::path::Path;

    type CFTypeRef = *const c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithBytes(alloc: CFTypeRef, bytes: *const u8, len: isize, encoding: u32, external: u8) -> CFTypeRef;
        fn CFRelease(cf: CFTypeRef);
        fn CFGetTypeID(cf: CFTypeRef) -> usize;
        fn CFDateGetTypeID() -> usize;
        fn CFDateGetAbsoluteTime(date: CFTypeRef) -> f64;
        fn CFNumberGetTypeID() -> usize;
        fn CFNumberGetValue(number: CFTypeRef, kind: isize, value: *mut c_void) -> u8;
    }

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn MDItemCreate(alloc: CFTypeRef, path: CFTypeRef) -> CFTypeRef;
        fn MDItemCopyAttribute(item: CFTypeRef, name: CFTypeRef) -> CFTypeRef;
    }

    const UTF8: u32 = 0x0800_0100;
    const SINT64: isize = 4;
    /// Core Foundation dates count from 2001-01-01.
    const CF_EPOCH: f64 = 978_307_200.0;

    #[derive(Default)]
    pub struct Info {
        pub last_used: Option<i64>,
        /// Bytes on disk (`kMDItemPhysicalSize`), or 0.
        pub size: u64,
    }

    /// Owns a Core Foundation object and releases it.
    struct Cf(CFTypeRef);

    impl Drop for Cf {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0) };
        }
    }

    fn cf_string(s: &str) -> Option<Cf> {
        owned(unsafe { CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as isize, UTF8, 0) })
    }

    /// Only a non-null object is wrapped: dropping a `Cf` releases it.
    fn owned(r: CFTypeRef) -> Option<Cf> {
        if r.is_null() { None } else { Some(Cf(r)) }
    }

    fn attribute(item: &Cf, name: &str) -> Option<Cf> {
        let name = cf_string(name)?;
        owned(unsafe { MDItemCopyAttribute(item.0, name.0) })
    }

    pub fn info(path: &Path) -> Info {
        let Some(p) = cf_string(&path.to_string_lossy()) else { return Info::default() };
        let Some(item) = owned(unsafe { MDItemCreate(std::ptr::null(), p.0) }) else { return Info::default() };
        let last_used = attribute(&item, "kMDItemLastUsedDate")
            .filter(|d| unsafe { CFGetTypeID(d.0) == CFDateGetTypeID() })
            .map(|d| (unsafe { CFDateGetAbsoluteTime(d.0) } + CF_EPOCH) as i64);
        let size = attribute(&item, "kMDItemPhysicalSize")
            .filter(|n| unsafe { CFGetTypeID(n.0) == CFNumberGetTypeID() })
            .and_then(|n| {
                let mut v: i64 = 0;
                let ok = unsafe { CFNumberGetValue(n.0, SINT64, (&mut v as *mut i64).cast()) };
                (ok != 0 && v > 0).then_some(v as u64)
            })
            .unwrap_or(0);
        Info { last_used, size }
    }
}

/// Apps that come with macOS, only so leftovers named after them are never
/// suggested.
pub fn system_apps() -> Vec<Identity> {
    let dirs = [
        "/System/Applications",
        "/System/Applications/Utilities",
        "/System/Library/CoreServices",
        "/System/Cryptexes/App/System/Applications",
    ];
    dirs.iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|rd| rd.flatten().map(|e| e.path()))
        .filter(|p| has_app_ext(p))
        .filter_map(|p| {
            let b = bundle_info(&p)?;
            Some(Identity { id: b.id.map(|i| i.to_lowercase()), names: names_of(&[Some(&b.name)], &p, None), paths: vec![p] })
        })
        .collect()
}

// ----------------------------------------------------------------------
// Code signatures

/// What `codesign -dv` says about an app.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Signature {
    /// The signing certificate: "Developer ID Application: Foo Inc. (ABCDE12345)".
    pub authority: Option<String>,
    pub team: Option<String>,
    pub adhoc: bool,
}

impl Signature {
    /// The developer named in a Developer ID or development certificate.
    pub fn developer(&self) -> Option<String> {
        let a = self.authority.as_deref()?;
        let (kind, who) = a.split_once(": ")?;
        if !["Developer ID Application", "Apple Development", "Apple Distribution", "Mac Developer", "3rd Party Mac Developer Application"]
            .contains(&kind)
        {
            return None;
        }
        let who = match who.rsplit_once(" (") {
            Some((w, team)) if team.ends_with(')') => w,
            _ => who,
        };
        Some(who.trim().to_string()).filter(|w| !w.is_empty())
    }

    /// Signed for the Mac App Store (by Apple, on the developer's behalf).
    pub fn is_app_store(&self) -> bool {
        self.authority.as_deref() == Some("Apple Mac OS Application Signing")
    }

    /// One of Apple's own apps.
    pub fn is_apple(&self) -> bool {
        self.authority.as_deref() == Some("Software Signing")
    }
}

/// Parse `codesign -dv --verbose=2` (it prints to stderr).
pub fn parse_codesign(text: &str) -> Signature {
    let mut s = Signature::default();
    for line in text.lines() {
        if let Some(a) = line.strip_prefix("Authority=") {
            // The first authority is the signing certificate; the rest is its chain.
            s.authority.get_or_insert_with(|| a.trim().to_string());
        } else if let Some(t) = line.strip_prefix("TeamIdentifier=") {
            let t = t.trim();
            if t != "not set" && !t.is_empty() {
                s.team = Some(t.to_string());
            }
        } else if line.trim() == "Signature=adhoc" {
            s.adhoc = true;
        }
    }
    s
}

/// Read an app's code signature. Bundles without a normal `Contents`
/// folder (iPhone app wrappers, whose files can be links into another
/// app's container) are skipped, so `codesign` never reads another app's data.
pub fn signature(app: &Path) -> Signature {
    let contents = app.join("Contents");
    let normal = std::fs::symlink_metadata(&contents).is_ok_and(|m| m.is_dir())
        && std::fs::symlink_metadata(contents.join("Info.plist")).is_ok_and(|m| m.is_file());
    if !normal {
        return Signature::default();
    }
    match Command::new("/usr/bin/codesign").args(["-dv", "--verbose=2"]).arg(app).output() {
        Ok(out) => parse_codesign(&String::from_utf8_lossy(&out.stderr)),
        Err(_) => Signature::default(),
    }
}

/// The holder in a copyright line: "Copyright © 2003–2025 Apple Inc. All
/// rights reserved." → "Apple Inc."
pub fn copyright_holder(line: &str) -> Option<String> {
    let line = line.lines().next()?.trim();
    // ASCII lower-casing keeps byte offsets, so `cut` is a char boundary.
    let cut = line.to_ascii_lowercase().find("all rights reserved").unwrap_or(line.len());
    let mut s = &line[..cut];
    loop {
        let before = s;
        s = s.trim_start_matches(|c: char| c.is_whitespace() || c.is_ascii_digit() || "-–—,.©".contains(c));
        for p in ["Copyright", "copyright", "COPYRIGHT", "(c)", "(C)"] {
            s = s.strip_prefix(p).unwrap_or(s);
        }
        if s == before {
            break;
        }
    }
    let mut s = s.trim().trim_end_matches([',', ';', ' ']).to_string();
    // "Inc." and "L.P." keep their dot; a sentence's full stop goes.
    let last = s.rsplit(' ').next().unwrap_or_default().to_lowercase();
    let keeps_dot = ["inc.", "ltd.", "co.", "corp.", "llc.", "gmbh."].contains(&last.as_str()) || last.trim_end_matches('.').contains('.');
    if s.ends_with('.') && !keeps_dot {
        s.pop();
    }
    let s = s.trim().to_string();
    (!s.is_empty() && s.chars().count() <= 60 && s.chars().any(char::is_alphabetic)).then_some(s)
}

// ----------------------------------------------------------------------
// Sizes

/// Space an item takes on disk: allocated blocks, symbolic links not
/// followed, and hard-linked files counted once.
pub fn size_on_disk(path: &Path) -> u64 {
    let Ok(md) = std::fs::symlink_metadata(path) else { return 0 };
    let own = md.blocks() * 512;
    if !md.is_dir() {
        return own;
    }
    let seen = Mutex::new(HashSet::new());
    own + folder_size(path, &seen)
}

fn folder_size(dir: &Path, seen: &Mutex<HashSet<(u64, u64)>>) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let entries: Vec<_> = rd.flatten().collect();
    entries
        .par_iter()
        .map(|e| {
            // DirEntry::metadata doesn't follow links.
            let Ok(md) = e.metadata() else { return 0 };
            let own = md.blocks() * 512;
            if md.is_dir() {
                own + folder_size(&e.path(), seen)
            } else if md.nlink() > 1 && !seen.lock().unwrap().insert((md.dev(), md.ino())) {
                0
            } else {
                own
            }
        })
        .sum()
}

// ----------------------------------------------------------------------
// Uninstalling

/// Paths of the programs running now. Processes of other users that macOS
/// doesn't let Heft inspect are left out.
fn process_paths() -> Vec<PathBuf> {
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0 as libc::pid_t; n as usize + 64];
    let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    pids[..n.max(0) as usize]
        .iter()
        .filter(|&&pid| pid > 0)
        .filter_map(|&pid| {
            let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
            (len > 0).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..len as usize])))
        })
        .collect()
}

/// Whether the app, or anything inside its bundle (helpers, agents), is running.
pub fn is_running(app: &App) -> bool {
    let bundle = std::fs::canonicalize(&app.path).unwrap_or_else(|_| app.path.clone());
    process_paths().iter().any(|p| p.starts_with(&bundle))
}

/// Whether the app is still where it was.
pub fn still_installed(app: &App) -> bool {
    std::fs::symlink_metadata(&app.path).is_ok()
}

/// Open an app's own uninstaller and wait until it quits.
pub fn run_uninstaller(u: &Uninstaller) -> Result<(), String> {
    let status = Command::new("/usr/bin/open").args(["-W", "-n"]).arg(&u.app).status().map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err(format!("{} couldn't be opened", stem(&u.app))) }
}

fn in_group(gid: u32) -> bool {
    let mut groups = vec![0 as libc::gid_t; 64];
    let n = unsafe { libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr()) };
    let primary = unsafe { libc::getegid() };
    primary == gid || groups[..n.max(0) as usize].contains(&gid)
}

/// Writable by the current user going by owner and permission bits. Not
/// `access()`: that also says no when macOS protects an app from being
/// changed by other apps (App Management), which a password doesn't fix.
fn writable_by_mode(md: &std::fs::Metadata) -> bool {
    let uid = super::uid();
    let mode = md.mode();
    if uid == 0 {
        true
    } else if md.uid() == uid {
        mode & 0o200 != 0
    } else if in_group(md.gid()) {
        mode & 0o020 != 0
    } else {
        mode & 0o002 != 0
    }
}

/// Moving `path` needs root: its folder isn't writable, it's a folder that
/// isn't (moving a folder rewrites its `..` entry), or it sits in a sticky
/// folder and belongs to someone else.
pub fn needs_admin(path: &Path) -> bool {
    let Ok(md) = std::fs::symlink_metadata(path) else { return false };
    let Some(parent) = path.parent().and_then(|p| std::fs::metadata(p).ok()) else { return true };
    let uid = super::uid();
    let sticky = parent.mode() & 0o1000 != 0 && uid != 0 && md.uid() != uid && parent.uid() != uid;
    !writable_by_mode(&parent) || sticky || (md.is_dir() && !writable_by_mode(&md))
}

/// Move items to the Trash, and say where each one landed there so the
/// Removed page can put it back. Items only root can move (apps installed by
/// packages or the App Store, things in /Library) go into your Trash with
/// `mv` as root, after one administrator password prompt. Nothing is ever
/// deleted. `why` finishes "Heft wants to …" in that prompt.
///
/// Launch agents and daemons among them are stopped once they're in the
/// Trash, so what they run doesn't keep going until the next restart.
/// Daemons are stopped by root, under the same password prompt.
pub fn trash(paths: &[PathBuf], why: &str) -> Vec<(PathBuf, Result<Option<PathBuf>, String>)> {
    let mut out = Vec::new();
    let mut as_root = Vec::new();
    let home = crate::platform::home_dir().map(PathBuf::from);
    let home_dev = home.as_ref().and_then(|h| std::fs::metadata(h).ok()).map(|m| m.dev());
    // Read before they move.
    let jobs: HashMap<PathBuf, LaunchJob> = paths.iter().filter_map(|p| Some((p.clone(), launch_job_at(p)?))).collect();
    for p in paths {
        let Ok(md) = std::fs::symlink_metadata(p) else {
            out.push((p.clone(), Err("it's already gone".to_string())));
            continue;
        };
        match crate::platform::move_to_trash(p) {
            Ok(landed) => {
                if let Some(j) = jobs.get(p) {
                    stop_in_session(j);
                }
                out.push((p.clone(), Ok(landed)));
            }
            // Moving to another disk's Trash would mean copying, so only
            // items on the home folder's disk are moved as root.
            Err(_) if needs_admin(p) && home_dev == Some(md.dev()) => as_root.push(p.clone()),
            Err(e) => out.push((p.clone(), Err(e))),
        }
    }
    let Some(home) = home.filter(|_| !as_root.is_empty()) else { return out };
    let (script, dests) = admin_move_script(
        &as_root,
        &home.join(".Trash"),
        |p| std::fs::symlink_metadata(p).is_ok(),
        |p| jobs.get(p).filter(|j| j.domain == Domain::Daemon).map(|j| stop_daemon_command(&j.label)),
    );
    let result = super::run_as_admin(&script, why);
    for (p, dest) in as_root.into_iter().zip(dests) {
        let r = if std::fs::symlink_metadata(&p).is_err() {
            if let Some(j) = jobs.get(&p).filter(|j| j.domain != Domain::Daemon) {
                stop_in_session(j);
            }
            Ok(std::fs::symlink_metadata(&dest).is_ok().then_some(dest))
        } else {
            match &result {
                Err(e) if e == super::CANCELLED => Err("cancelled".to_string()),
                Err(e) => Err(e.clone()),
                Ok(_) => Err("it couldn't be moved".to_string()),
            }
        };
        out.push((p, r));
    }
    out
}

/// The launch agent or daemon whose .plist this is, if it's in one of the
/// folders launchd loads them from.
fn launch_job_at(path: &Path) -> Option<LaunchJob> {
    let parent = path.parent()?;
    let domain = Domain::ALL.into_iter().find(|d| d.folder().as_deref() == Some(parent))?;
    super::parse_launch_job(path, domain)
}

/// Stop a launch agent in this login session. Fine if it isn't loaded.
fn stop_in_session(job: &LaunchJob) {
    let _ = super::startup::launchctl(&["bootout", &format!("gui/{}/{}", super::uid(), job.label)]);
}

/// Stops a launch daemon, as root. Fine if it isn't loaded.
fn stop_daemon_command(label: &str) -> String {
    format!("/bin/launchctl bootout {} 2>/dev/null", super::sh_quote(&format!("system/{label}")))
}

/// A shell script that moves each item into `trash` under a name that's
/// free there, and those names. `mv -n` never replaces anything. `then`
/// gives a command to run once an item has moved.
fn admin_move_script(
    items: &[PathBuf],
    trash: &Path,
    taken: impl Fn(&Path) -> bool,
    then: impl Fn(&Path) -> Option<String>,
) -> (String, Vec<PathBuf>) {
    let mut planned: HashSet<PathBuf> = HashSet::new();
    let mut dests = Vec::new();
    let mut lines = vec!["fail=0".to_string()];
    for src in items {
        let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "item".into());
        let (base, ext) = match name.rsplit_once('.') {
            Some((b, e)) if !b.is_empty() && src.extension().is_some() && !src.is_dir() || has_app_ext(src) => (b.to_string(), format!(".{e}")),
            _ => (name.clone(), String::new()),
        };
        let dest = (1..)
            .map(|n| trash.join(if n == 1 { name.clone() } else { format!("{base} {n}{ext}") }))
            .find(|d| !planned.contains(d) && !taken(d))
            .unwrap();
        planned.insert(dest.clone());
        dests.push(dest.clone());
        let mv = format!("/bin/mv -n -- {} {}", super::sh_quote(&src.to_string_lossy()), super::sh_quote(&dest.to_string_lossy()));
        lines.push(match then(src) {
            Some(then) => format!("if {mv}; then {then}; else fail=1; fi"),
            None => format!("{mv} || fail=1"),
        });
    }
    lines.push("exit $fail".into());
    (lines.join("; "), dests)
}

// ----------------------------------------------------------------------
// Leftovers

/// What an app is known by, for telling whose files are whose.
#[derive(Clone, Debug, Default)]
pub struct Identity {
    /// Bundle id, lower-case.
    pub id: Option<String>,
    /// Lower-case names (see `names_of`).
    pub names: Vec<String>,
    /// The bundle, and its link in Applications if it has one.
    pub paths: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Leftover {
    pub path: PathBuf,
    pub size: u64,
    /// Why Heft thinks it belongs to the app.
    pub reason: &'static str,
    /// Ticked by default: only for items named exactly after the app's
    /// bundle id, or that point into the removed app.
    pub confident: bool,
    /// Moving it needs an administrator password.
    pub admin: bool,
    /// `size` is known. Another app's data container isn't measured
    /// without Full Disk Access (see `Libraries::is_app_data`).
    pub measured: bool,
}

/// Where apps keep things: `~/Library` and `/Library`.
pub struct Libraries {
    pub user: PathBuf,
    pub system: PathBuf,
}

impl Libraries {
    pub fn real() -> Option<Libraries> {
        let home = crate::platform::home_dir()?;
        Some(Libraries { user: Path::new(&home).join("Library"), system: PathBuf::from("/Library") })
    }

    /// Inside another app's data container. Since macOS 14, looking in one
    /// makes macOS ask "Heft would like to access data from other apps",
    /// unless Heft has Full Disk Access.
    fn is_app_data(&self, path: &Path) -> bool {
        path.starts_with(self.user.join("Containers")) || path.starts_with(self.user.join("Group Containers"))
    }
}

/// How items in a folder are named.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Style {
    /// Named after the bundle id or the app (Application Support, Caches, Logs).
    IdOrName,
    /// Named after the bundle id, as a folder or `<id>.binarycookies`.
    Id,
    /// App groups: `group.<id>`, `<TEAMID>.<id>`, or the bare id.
    Group,
    /// `<id>.plist`
    Plist,
    /// `<id>.<hardware UUID>.plist`
    ByHost,
    /// `<id>.savedState`
    SavedState,
    /// `<id>.binarycookies`
    Cookies,
    /// Launch agents and daemons, matched by what they run.
    Launchd(Domain),
    /// Privileged helpers, named after their own bundle id.
    Helper,
}

const USER_PLACES: [(&str, Style); 13] = [
    ("Application Support", Style::IdOrName),
    ("Caches", Style::IdOrName),
    ("Logs", Style::IdOrName),
    ("Containers", Style::Id),
    ("Group Containers", Style::Group),
    ("Application Scripts", Style::Group),
    ("Preferences", Style::Plist),
    ("Preferences/ByHost", Style::ByHost),
    ("Saved Application State", Style::SavedState),
    ("HTTPStorages", Style::Id),
    ("WebKit", Style::Id),
    ("Cookies", Style::Cookies),
    ("LaunchAgents", Style::Launchd(Domain::UserAgent)),
];

const SYSTEM_PLACES: [(&str, Style); 7] = [
    ("Application Support", Style::IdOrName),
    ("Caches", Style::IdOrName),
    ("Logs", Style::IdOrName),
    ("Preferences", Style::Plist),
    ("LaunchAgents", Style::Launchd(Domain::Agent)),
    ("LaunchDaemons", Style::Launchd(Domain::Daemon)),
    ("PrivilegedHelperTools", Style::Helper),
];

/// Names too general to match a folder on (they'd hit shared folders).
const TOO_GENERAL: [&str; 24] = [
    "apple", "google", "adobe", "microsoft", "mozilla", "jetbrains", "app", "apps", "helper", "update", "updater",
    "cache", "caches", "logs", "data", "shared", "common", "system", "library", "crashreporter", "plugins",
    "plug-ins", "support", "installer",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
    /// Exactly the bundle id.
    Exact,
    /// `<id>.<more>`: an extension or helper of it.
    Family,
    /// Only the app's name.
    Name,
}

fn id_hit(key: &str, id: &str) -> Option<Hit> {
    if key == id {
        Some(Hit::Exact)
    } else if key.len() > id.len() + 1 && key.starts_with(id) && key.as_bytes()[id.len()] == b'.' {
        Some(Hit::Family)
    } else {
        None
    }
}

/// `group.<rest>` or `<TEAMID>.<rest>` (ten letters and digits) → `<rest>`.
fn strip_group(key: &str) -> &str {
    if let Some(rest) = key.strip_prefix("group.") {
        return rest;
    }
    match key.split_once('.') {
        Some((team, rest)) if team.len() == 10 && team.chars().all(|c| c.is_ascii_alphanumeric()) => rest,
        _ => key,
    }
}

/// ByHost files end in the Mac's hardware UUID (or, on old systems, its
/// Ethernet address).
fn strip_host(key: &str) -> &str {
    match key.rsplit_once('.') {
        Some((head, host)) if host.len() >= 12 && host.chars().all(|c| c.is_ascii_hexdigit() || c == '-') => head,
        _ => key,
    }
}

/// The bundle id part of an entry name, lower-case, for a folder's style.
fn key_for(name: &str, style: Style) -> Option<String> {
    let lower = name.to_lowercase();
    let k = match style {
        Style::IdOrName | Style::Helper | Style::Group => lower.as_str(),
        Style::Id => lower.strip_suffix(".binarycookies").unwrap_or(&lower),
        Style::Plist => lower.strip_suffix(".plist")?,
        Style::ByHost => strip_host(lower.strip_suffix(".plist")?),
        Style::SavedState => lower.strip_suffix(".savedstate")?,
        Style::Cookies => lower.strip_suffix(".binarycookies")?,
        Style::Launchd(_) => return None,
    };
    Some(k.to_string())
}

/// Everything the other apps (still installed, or part of macOS) are known
/// by, to keep their files out of the suggestions.
struct Others<'a> {
    list: &'a [Identity],
}

impl Others<'_> {
    /// Another app has this bundle id, or a more specific one that covers
    /// `key` (`com.foo.app.pro.helper` belongs to `com.foo.app.pro`, not `com.foo.app`).
    fn claims_id(&self, key: &str, id: &str) -> bool {
        self.list.iter().filter_map(|o| o.id.as_deref()).any(|o| key == o || (o.len() > id.len() && id_hit(key, o).is_some()))
    }

    /// Another app goes by this name, or by a longer one starting with it
    /// ("Foo Pro" may share the "Foo" folder).
    fn claims_name(&self, key: &str) -> bool {
        let prefix = format!("{key} ");
        self.list.iter().any(|o| o.id.as_deref() == Some(key) || o.names.iter().any(|n| n == key || n.starts_with(&prefix)))
    }

    /// A folder holds something of another app: its bundle, or an item
    /// named after it (a vendor folder shared by several apps). Only the
    /// first is checked when `look_inside` is off.
    fn uses_folder(&self, dir: &Path, key: &str, look_inside: bool) -> bool {
        if self.list.iter().flat_map(|o| &o.paths).any(|p| p.starts_with(dir) || dir.starts_with(p)) {
            return true;
        }
        if !look_inside {
            return false;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return false };
        rd.flatten().any(|e| {
            let child = e.file_name().to_string_lossy().to_lowercase();
            let child = child.strip_suffix(".plist").unwrap_or(&child).to_string();
            let vendor_product = format!("{key} {child}");
            self.list.iter().any(|o| {
                o.id.as_deref().is_some_and(|id| child == id || id_hit(&child, id).is_some())
                    || o.names.iter().any(|n| *n == child || *n == vendor_product)
            })
        })
    }
}

/// Things an app left behind after it was removed: files and folders named
/// after its bundle id or name in the usual Library folders, launch agents
/// and daemons that start it, its Applications link, the vendor folder it
/// was installed in, and its uninstaller.
///
/// `others` lists every app still installed (and macOS's own). Nothing that
/// matches another app's bundle id or name, holds one of their bundles, or
/// is named after one of them inside is ever suggested: those can be shared,
/// and removing them would break what's left. Only exact bundle id matches
/// (and what points into the removed app) are ticked by default.
///
/// Without Full Disk Access, data containers are found by name but not
/// looked inside or measured, so macOS doesn't ask about them.
pub fn find_leftovers(app: &App, others: &[Identity]) -> Vec<Leftover> {
    let app_data = super::has_full_disk_access() != Some(false);
    match Libraries::real() {
        Some(libs) => find_in(&libs, &app_folders(), app, others, app_data),
        None => Vec::new(),
    }
}

/// `app_data`: look inside other apps' data containers.
fn find_in(libs: &Libraries, roots: &[PathBuf], app: &App, others: &[Identity], app_data: bool) -> Vec<Leftover> {
    let me = app.identity();
    let others = Others { list: others };
    // Another copy of the same app (a second version, a beta) shares its
    // data, so nothing is suggested by bundle id or name at all.
    let twin_id = me.id.as_ref().is_some_and(|id| others.list.iter().any(|o| o.id.as_ref() == Some(id)));
    let id = me.id.clone().filter(|_| !twin_id);
    let names: Vec<String> = me
        .names
        .iter()
        .filter(|n| n.chars().count() >= 4 && !TOO_GENERAL.contains(&n.as_str()) && !others.claims_name(n))
        .cloned()
        .collect();

    let mut out: Vec<Leftover> = Vec::new();
    let push = |path: PathBuf, reason: &'static str, confident: bool, out: &mut Vec<Leftover>| {
        if !out.iter().any(|l| l.path == path) {
            let admin = path.starts_with(&libs.system) || needs_admin(&path);
            out.push(Leftover { path, size: 0, reason, confident, admin, measured: false });
        }
    };

    let places = USER_PLACES.iter().map(|(p, s)| (libs.user.join(p), *s)).chain(SYSTEM_PLACES.iter().map(|(p, s)| (libs.system.join(p), *s)));
    let mut helpers: Vec<(PathBuf, bool)> = Vec::new();
    for (dir, style) in places {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = e.path();
            if let Style::Launchd(domain) = style {
                if let Some((confident, helper)) = launchd_hit(&path, domain, app, id.as_deref(), &others) {
                    let reason = if domain == Domain::Daemon { "launch daemon for the app" } else { "launch agent for the app" };
                    push(path, reason, confident, &mut out);
                    helpers.extend(helper.map(|h| (h, confident)));
                }
                continue;
            }
            let Some(key) = key_for(&name, style) else { continue };
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
            let id_key = if style == Style::Group { strip_group(&key) } else { &key };
            let hit = id.as_deref().and_then(|id| id_hit(id_key, id)).filter(|_| !others.claims_id(id_key, id.as_deref().unwrap_or_default()));
            let hit = hit.or_else(|| {
                (style == Style::IdOrName && names.contains(&key) && !others.claims_id(&key, "")).then_some(Hit::Name)
            });
            let Some(hit) = hit else { continue };
            if is_dir && others.uses_folder(&path, &key, app_data || !libs.is_app_data(&path)) {
                continue;
            }
            let (reason, confident) = match hit {
                Hit::Exact => ("named after the app's bundle id", true),
                Hit::Family => ("named after one of the app's extensions or helpers", false),
                Hit::Name => ("named after the app", false),
            };
            push(path, reason, confident, &mut out);
        }
    }
    for (h, confident) in helpers {
        if h.starts_with(libs.system.join("PrivilegedHelperTools")) && std::fs::symlink_metadata(&h).is_ok() {
            out.retain(|l| l.path != h || l.confident || !confident);
            push(h, "helper tool its launch daemon runs", confident, &mut out);
        }
    }

    // Its link in Applications, now pointing nowhere.
    if let Some(link) = &app.link
        && std::fs::symlink_metadata(link).is_ok_and(|m| m.file_type().is_symlink())
        && !link.exists()
    {
        push(link.clone(), "link to the app in Applications", true, &mut out);
    }
    // The vendor folder it was installed in, when nothing else lives there.
    let standard = roots.contains(&app.folder) || app.folder == Path::new("/Applications/Utilities");
    if !standard
        && app.folder.is_dir()
        && !others.list.iter().flat_map(|o| &o.paths).any(|p| p.starts_with(&app.folder))
        && !holds_other_apps(&app.folder, app, 0)
    {
        let folder_name = stem(&app.folder).to_lowercase();
        let named = me.names.contains(&folder_name);
        push(app.folder.clone(), "folder it was installed in", named, &mut out);
    }
    // Its uninstaller, if it was left next to it.
    if let Some(u) = &app.uninstaller
        && !u.path.starts_with(&app.path)
        && std::fs::symlink_metadata(&u.path).is_ok()
    {
        push(u.path.clone(), "its uninstaller", u.named, &mut out);
    }

    // Keep the outermost item when one sits inside another.
    let all: Vec<PathBuf> = out.iter().map(|l| l.path.clone()).collect();
    out.retain(|l| !all.iter().any(|p| *p != l.path && l.path.starts_with(p)));
    out.par_iter_mut().filter(|l| app_data || !libs.is_app_data(&l.path)).for_each(|l| {
        l.size = size_on_disk(&l.path);
        l.measured = true;
    });
    out.sort_by(|a, b| b.confident.cmp(&a.confident).then(a.path.cmp(&b.path)));
    out
}

/// A launch agent or daemon that belongs to the removed app: it runs
/// something inside the app, or it's labelled with the app's bundle id.
/// Returns whether that's certain, and the helper tool it runs from
/// `/Library/PrivilegedHelperTools`, if any.
fn launchd_hit(plist: &Path, domain: Domain, app: &App, id: Option<&str>, others: &Others) -> Option<(bool, Option<PathBuf>)> {
    if plist.extension().is_none_or(|e| e != "plist") {
        return None;
    }
    let job = super::parse_launch_job(plist, domain)?;
    let bundles: Vec<&Path> = [Some(app.path.as_path()), app.link.as_deref()].into_iter().flatten().collect();
    let runs: Vec<&str> = job.program.iter().map(String::as_str).chain(job.args.iter().map(String::as_str)).collect();
    let inside = runs.iter().any(|r| r.starts_with('/') && bundles.iter().any(|b| Path::new(r).starts_with(b)));
    let label = job.label.to_lowercase();
    let file = plist.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    let for_other = job.bundles.iter().any(|b| others.list.iter().any(|o| o.id.as_deref() == Some(&b.to_lowercase())))
        || runs.iter().any(|r| others.list.iter().flat_map(|o| &o.paths).any(|p| Path::new(r).starts_with(p)));
    if for_other {
        return None;
    }
    let by_id = id.and_then(|id| {
        let mine = job.bundles.iter().any(|b| b.eq_ignore_ascii_case(id));
        let best = [id_hit(&label, id), id_hit(&file, id)].into_iter().flatten().min_by_key(|h| *h != Hit::Exact);
        if mine { Some(Hit::Exact) } else { best.filter(|_| !others.claims_id(&label, id)) }
    });
    let confident = inside || by_id == Some(Hit::Exact);
    if !confident && by_id.is_none() {
        return None;
    }
    let helper = job.program.as_deref().map(PathBuf::from).filter(|p| p.to_string_lossy().contains("/PrivilegedHelperTools/"));
    Some((confident, helper))
}

/// Whether a vendor folder holds apps other than this one and its
/// uninstaller (a few levels down, links not followed).
fn holds_other_apps(dir: &Path, app: &App, depth: usize) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    rd.flatten().any(|e| {
        let p = e.path();
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
        if p == app.path || app.uninstaller.as_ref().is_some_and(|u| u.path == p || u.app == p) {
            return false;
        }
        if has_app_ext(&p) {
            return is_dir;
        }
        is_dir && depth < 2 && holds_other_apps(&p, app, depth + 1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("heft-apps-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A minimal app bundle with an Info.plist.
    fn make_app(path: &Path, id: &str, name: &str) {
        std::fs::create_dir_all(path.join("Contents/MacOS")).unwrap();
        std::fs::write(
            path.join("Contents/Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundleName</key><string>{name}</string>
<key>CFBundleShortVersionString</key><string>1.2</string>
<key>NSHumanReadableCopyright</key><string>Copyright © 2024 Example Corp. All rights reserved.</string>
</dict></plist>"#
            ),
        )
        .unwrap();
        std::fs::write(path.join("Contents/MacOS/bin"), vec![1u8; 10_000]).unwrap();
    }

    #[test]
    fn codesign_output() {
        let dev = "Executable=/Applications/Blender.app/Contents/MacOS/Blender\nIdentifier=org.blenderfoundation.blender\n\
                   Format=app bundle with Mach-O universal (x86_64 arm64)\nCodeDirectory v=20500 size=1 flags=0x10000(runtime) hashes=1+7 location=embedded\n\
                   Signature size=8987\nAuthority=Developer ID Application: Stichting Blender Foundation (68UA947AUU)\n\
                   Authority=Developer ID Certification Authority\nAuthority=Apple Root CA\nTimestamp=Apr 13, 2026 at 7:29:31 PM\n\
                   Info.plist entries=33\nTeamIdentifier=68UA947AUU\nRuntime Version=14.0.0\n";
        let s = parse_codesign(dev);
        assert_eq!(s.developer().as_deref(), Some("Stichting Blender Foundation"));
        assert_eq!(s.team.as_deref(), Some("68UA947AUU"));
        assert!(!s.is_app_store() && !s.adhoc);

        let mas = "Identifier=com.lemon.lvoverseas\nAuthority=Apple Mac OS Application Signing\n\
                   Authority=Apple Worldwide Developer Relations Certification Authority\nAuthority=Apple Root CA\nTeamIdentifier=22MMUN2RN5\n";
        let s = parse_codesign(mas);
        assert!(s.is_app_store());
        assert_eq!(s.developer(), None);

        let apple = parse_codesign("Authority=Software Signing\nAuthority=Apple Code Signing Certification Authority\nTeamIdentifier=not set\n");
        assert!(apple.is_apple());
        assert_eq!(apple.team, None);

        let adhoc = parse_codesign("Identifier=foo\nSignature=adhoc\nTeamIdentifier=not set\n");
        assert!(adhoc.adhoc && adhoc.authority.is_none());
        assert_eq!(parse_codesign("/Applications/X.app: code object is not signed at all\n"), Signature::default());

        let mut app = App {
            path: "/Applications/CapCut.app".into(),
            link: None,
            folder: "/Applications".into(),
            name: "CapCut".into(),
            bundle_name: None,
            id: None,
            version: String::new(),
            copyright: Some("Copyright © 2022 ByteDance. All rights reserved.".into()),
            added: 0,
            last_used: None,
            estimated: 0,
            measured: None,
            source: Source::Other,
            cask: None,
            signature: Some(parse_codesign(mas)),
            uninstaller: None,
            updater: None,
        };
        assert_eq!(app.publisher(), "ByteDance");
        assert!(app.is_from_app_store());
        app.signature = Some(parse_codesign(dev));
        assert_eq!(app.publisher(), "Stichting Blender Foundation");
        app.signature = Some(apple);
        assert_eq!(app.publisher(), "Apple");
    }

    #[test]
    fn copyright_holders() {
        assert_eq!(copyright_holder("© 2003–2025 Apple Inc. All rights reserved.").as_deref(), Some("Apple Inc."));
        assert_eq!(copyright_holder("Copyright © 2022 ByteDance. All rights reserved.").as_deref(), Some("ByteDance"));
        assert_eq!(copyright_holder("Copyright (c) 2019-2024, Foo, LLC").as_deref(), Some("Foo, LLC"));
        assert_eq!(copyright_holder("2024 Someone").as_deref(), Some("Someone"));
        assert_eq!(copyright_holder("© 2025 HP Development Company, L.P.").as_deref(), Some("HP Development Company, L.P."));
        assert_eq!(copyright_holder("© 2024"), None);
        assert_eq!(copyright_holder(""), None);
    }

    #[test]
    fn uninstaller_names() {
        assert_eq!(uninstaller_subject("Uninstall Adobe Photoshop 2026"), "adobe photoshop 2026");
        assert_eq!(uninstaller_subject("Creative Cloud Uninstaller"), "creative cloud");
        assert_eq!(uninstaller_subject("Uninstaller for Foo"), "foo");
        assert_eq!(uninstaller_subject("Uninstall"), "");
        assert!(is_uninstaller_name("Uninstallers") && !is_uninstaller_name("Installer"));
        assert_eq!(base_name("Reason 13"), "Reason");
        assert_eq!(base_name("Ableton Live 12 Suite"), "Ableton Live 12 Suite");
    }

    #[test]
    fn lists_apps_and_their_uninstallers() {
        let base = temp("list");
        let apps = base.join("Applications");
        let elsewhere = base.join("Shared/Game.app");
        make_app(&apps.join("Solo.app"), "com.example.solo", "Solo Player");
        make_app(&apps.join("Uninstall Solo.app"), "com.example.solo.uninstaller", "Uninstall Solo");
        make_app(&apps.join("Vendor Suite/Writer.app"), "com.vendor.writer", "Writer");
        make_app(&apps.join("Vendor Suite/Uninstallers/Uninstall Writer.app"), "com.vendor.writer.u", "Uninstall Writer");
        make_app(&apps.join("Vendor Suite/Painter.app"), "com.vendor.painter", "Painter");
        make_app(&apps.join("One Tool/Activate Thing.app"), "com.one.activate", "Activate Thing");
        make_app(&apps.join("One Tool/Uninstall Thing.app"), "com.one.uninstall", "Uninstall Thing");
        make_app(&apps.join("Handy App Uninstaller.app"), "com.tools.uninstaller", "Handy App Uninstaller");
        make_app(&apps.join("Deep/Deeper/Hidden.app"), "com.deep", "Hidden");
        make_app(&elsewhere, "com.example.game", "Game");
        make_app(&apps.join("Bundled.app"), "com.example.bundled", "Bundled");
        make_app(&apps.join("Bundled.app/Contents/Resources/Uninstall Bundled.app"), "com.example.bundled.u", "Uninstall");
        std::os::unix::fs::symlink(&elsewhere, apps.join("Game.app")).unwrap();
        std::os::unix::fs::symlink(apps.join("Solo.app"), apps.join("Solo Link.app")).unwrap();
        std::os::unix::fs::symlink(base.join("missing.app"), apps.join("Broken.app")).unwrap();
        std::fs::write(apps.join(".hidden.app"), "").unwrap();

        let list = list_in(std::slice::from_ref(&apps), &[]);
        let names: Vec<&str> = list.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Activate Thing", "Bundled", "Game", "Handy App Uninstaller", "Painter", "Solo", "Writer"]);
        let by = |n: &str| list.iter().find(|a| a.name == n).unwrap();

        let solo = by("Solo");
        assert_eq!(solo.link, None, "listed directly, not through its second link");
        assert_eq!(solo.id.as_deref(), Some("com.example.solo"));
        assert_eq!(solo.bundle_name.as_deref(), Some("Solo Player"), "listed by its file name, like Finder");
        assert!(solo.identity().names.contains(&"solo player".to_string()));
        assert_eq!(solo.version, "1.2");
        assert_eq!(solo.publisher(), "Example Corp.");
        assert!(solo.added > 0);
        assert_eq!(solo.uninstaller.as_ref().map(|u| (u.app.clone(), u.named)), Some((apps.join("Uninstall Solo.app"), true)));

        let game = by("Game");
        assert_eq!(game.path, std::fs::canonicalize(&elsewhere).unwrap());
        assert_eq!(game.link.as_deref(), Some(apps.join("Game.app").as_path()));

        let writer = by("Writer");
        assert_eq!(writer.folder, apps.join("Vendor Suite"));
        assert_eq!(writer.uninstaller.as_ref().map(|u| u.app.clone()), Some(apps.join("Vendor Suite/Uninstallers/Uninstall Writer.app")));
        assert!(by("Painter").uninstaller.is_none(), "a shared vendor folder's uninstallers go by name only");
        let activate = by("Activate Thing").uninstaller.clone().unwrap();
        assert!(!activate.named, "the only app in its vendor folder gets its uninstaller");
        assert_eq!(by("Bundled").uninstaller.as_ref().map(|u| u.app.clone()), Some(apps.join("Bundled.app/Contents/Resources/Uninstall Bundled.app")));
        assert!(by("Handy App Uninstaller").uninstaller.is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sizes_on_disk() {
        let base = temp("size");
        std::fs::create_dir_all(base.join("a/b")).unwrap();
        std::fs::write(base.join("a/one"), vec![1u8; 100_000]).unwrap();
        std::fs::write(base.join("a/b/two"), vec![1u8; 50_000]).unwrap();
        std::fs::hard_link(base.join("a/one"), base.join("a/b/one-again")).unwrap();
        let outside = temp("size-outside");
        std::fs::write(outside.join("big"), vec![1u8; 1_000_000]).unwrap();
        std::os::unix::fs::symlink(outside.join("big"), base.join("a/link")).unwrap();
        let size = size_on_disk(&base.join("a"));
        assert!((150_000..300_000).contains(&size), "hard link once, link not followed: {size}");
        assert!(size_on_disk(&base.join("a/one")) >= 100_000);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn admin_needed_by_permissions() {
        use std::os::unix::fs::PermissionsExt;
        if crate::mac::uid() == 0 {
            return;
        }
        let base = temp("perm");
        let mode = |p: &Path, m: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
        std::fs::create_dir_all(base.join("open/Mine.app")).unwrap();
        std::fs::create_dir_all(base.join("locked/Theirs.app")).unwrap();
        std::fs::write(base.join("locked/file.plist"), "x").unwrap();
        assert!(!needs_admin(&base.join("open/Mine.app")));
        mode(&base.join("locked"), 0o555);
        assert!(needs_admin(&base.join("locked/Theirs.app")) && needs_admin(&base.join("locked/file.plist")));
        mode(&base.join("locked"), 0o755);
        mode(&base.join("locked/Theirs.app"), 0o555);
        assert!(needs_admin(&base.join("locked/Theirs.app")), "moving a folder rewrites its `..`");
        assert!(!needs_admin(&base.join("locked/file.plist")));
        mode(&base.join("locked/Theirs.app"), 0o755);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn admin_moves_never_overwrite() {
        let trash = Path::new("/Users/me/.Trash");
        let items = [
            PathBuf::from("/Applications/Foo.app"),
            PathBuf::from("/Library/Application Support/Foo"),
            PathBuf::from("/Library/Caches/Foo"),
            PathBuf::from("/Library/LaunchDaemons/com.foo.helper.plist"),
        ];
        let taken = |p: &Path| p == trash.join("Foo.app") || p == trash.join("com.foo.helper.plist");
        let stop = |p: &Path| p.starts_with("/Library/LaunchDaemons").then(|| stop_daemon_command("com.foo.helper"));
        let (s, dests) = admin_move_script(&items, trash, taken, stop);
        assert_eq!(dests[0], trash.join("Foo 2.app"));
        assert_eq!(
            s,
            "fail=0; \
             /bin/mv -n -- '/Applications/Foo.app' '/Users/me/.Trash/Foo 2.app' || fail=1; \
             /bin/mv -n -- '/Library/Application Support/Foo' '/Users/me/.Trash/Foo' || fail=1; \
             /bin/mv -n -- '/Library/Caches/Foo' '/Users/me/.Trash/Foo 2' || fail=1; \
             if /bin/mv -n -- '/Library/LaunchDaemons/com.foo.helper.plist' '/Users/me/.Trash/com.foo.helper 2.plist'; \
             then /bin/launchctl bootout 'system/com.foo.helper' 2>/dev/null; else fail=1; fi; \
             exit $fail"
        );
        assert!(!s.contains("rm "));
    }

    /// A fake `~/Library` and `/Library` with an app's leftovers mixed in
    /// with other apps' files.
    #[test]
    fn leftovers_only_the_removed_apps() {
        let base = temp("leftovers");
        let libs = Libraries { user: base.join("home/Library"), system: base.join("Library") };
        let apps_dir = base.join("Applications");
        let u = &libs.user;
        let s = &libs.system;
        let dirs = [
            // Foo's own, by bundle id: ticked.
            u.join("Application Support/com.foo.app"),
            u.join("Caches/com.foo.app"),
            u.join("Containers/com.foo.app"),
            u.join("Group Containers/ABCDE12345.com.foo.app"),
            u.join("Group Containers/group.com.foo.app"),
            u.join("HTTPStorages/com.foo.app"),
            u.join("WebKit/com.foo.app"),
            u.join("Saved Application State/com.foo.app.savedState"),
            u.join("Application Scripts/com.foo.app"),
            s.join("Application Support/com.foo.app"),
            // An extension of Foo: listed, not ticked.
            u.join("Containers/com.foo.app.ShareExtension"),
            // Named after Foo: listed, not ticked.
            u.join("Application Support/Foo Studio"),
            u.join("Logs/Foo Studio"),
            // Foo Pro is still installed: its data and its "family" stay.
            u.join("Containers/com.foo.app.pro"),
            u.join("Containers/com.foo.app.pro.helper"),
            // Named after Foo, but holding another app's folder: shared.
            u.join("Caches/Foo Studio/Bar"),
            // Someone else's.
            u.join("Application Support/com.bar.app"),
            u.join("Application Support/Bar"),
            u.join("Caches/com.foo.application"),
            // Holds another app's bundle.
            u.join("Application Support/com.foo.app.runtime/Bar.app"),
            u.join("Preferences/ByHost"),
            u.join("Cookies"),
            u.join("LaunchAgents"),
            s.join("LaunchDaemons"),
            s.join("PrivilegedHelperTools"),
            apps_dir.join("Foo Studio"),
        ];
        for d in &dirs {
            std::fs::create_dir_all(d).unwrap();
        }
        let files = [
            u.join("Preferences/com.foo.app.plist"),
            u.join("Preferences/com.foo.app.LSSharedFileList.plist"),
            u.join("Preferences/com.bar.app.plist"),
            u.join("Preferences/ByHost/com.foo.app.0D7A1B2C-3D4E-5F60-7182-93A4B5C6D7E8.plist"),
            u.join("Cookies/com.foo.app.binarycookies"),
            u.join("HTTPStorages/com.foo.app.binarycookies"),
            s.join("PrivilegedHelperTools/com.foo.app.helper"),
            s.join("PrivilegedHelperTools/com.bar.helper"),
        ];
        for f in &files {
            std::fs::write(f, "x").unwrap();
        }
        let foo_path = apps_dir.join("Foo Studio/Foo Studio.app");
        let plist = |label: &str, program: &str| {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict>
<key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>{program}</string></array>
</dict></plist>"#
            )
        };
        let agents = u.join("LaunchAgents");
        std::fs::write(agents.join("com.foo.updater.plist"), plist("com.foo.updater", &format!("{}/Contents/MacOS/updater", foo_path.display()))).unwrap();
        std::fs::write(agents.join("com.bar.agent.plist"), plist("com.bar.agent", "/Applications/Bar.app/Contents/MacOS/bar")).unwrap();
        std::fs::write(agents.join("com.foo.app.sync.plist"), plist("com.foo.app.sync", "/usr/local/bin/foosync")).unwrap();
        let daemon = s.join("LaunchDaemons/com.foo.app.helper.plist");
        std::fs::write(&daemon, plist("com.foo.app", &s.join("PrivilegedHelperTools/com.foo.app.helper").to_string_lossy())).unwrap();
        std::fs::write(s.join("LaunchDaemons/com.bar.helper.plist"), plist("com.bar.helper", &s.join("PrivilegedHelperTools/com.bar.helper").to_string_lossy())).unwrap();
        // The dangling link it left in Applications, and the uninstaller in its folder.
        let link = apps_dir.join("Foo Link.app");
        std::os::unix::fs::symlink(&foo_path, &link).unwrap();
        std::fs::write(apps_dir.join("Foo Studio/Uninstall Foo Studio"), "alias").unwrap();

        let foo = App {
            path: foo_path.clone(),
            link: Some(link.clone()),
            folder: apps_dir.join("Foo Studio"),
            name: "Foo Studio".into(),
            bundle_name: Some("Foo".into()),
            id: Some("com.foo.app".into()),
            version: String::new(),
            copyright: None,
            added: 0,
            last_used: None,
            estimated: 0,
            measured: None,
            source: Source::Other,
            cask: None,
            signature: None,
            uninstaller: Some(Uninstaller { path: apps_dir.join("Foo Studio/Uninstall Foo Studio"), app: base.join("u.app"), named: true }),
            updater: None,
        };
        let bar_path = u.join("Application Support/com.foo.app.runtime/Bar.app");
        let others = vec![
            Identity { id: Some("com.foo.app.pro".into()), names: vec!["foo pro".into()], paths: vec![apps_dir.join("Foo Pro.app")] },
            Identity { id: Some("com.bar.app".into()), names: vec!["bar".into()], paths: vec![bar_path] },
        ];
        let found = find_in(&libs, std::slice::from_ref(&apps_dir), &foo, &others, true);
        let got = |p: &Path| found.iter().find(|l| l.path == p);
        let ticked: HashSet<PathBuf> = found.iter().filter(|l| l.confident).map(|l| l.path.clone()).collect();
        let unticked: HashSet<PathBuf> = found.iter().filter(|l| !l.confident).map(|l| l.path.clone()).collect();

        let expect_ticked: HashSet<PathBuf> = [
            u.join("Application Support/com.foo.app"),
            u.join("Caches/com.foo.app"),
            u.join("Containers/com.foo.app"),
            u.join("Group Containers/ABCDE12345.com.foo.app"),
            u.join("Group Containers/group.com.foo.app"),
            u.join("HTTPStorages/com.foo.app"),
            u.join("HTTPStorages/com.foo.app.binarycookies"),
            u.join("WebKit/com.foo.app"),
            u.join("Saved Application State/com.foo.app.savedState"),
            u.join("Application Scripts/com.foo.app"),
            u.join("Preferences/com.foo.app.plist"),
            u.join("Preferences/ByHost/com.foo.app.0D7A1B2C-3D4E-5F60-7182-93A4B5C6D7E8.plist"),
            u.join("Cookies/com.foo.app.binarycookies"),
            u.join("LaunchAgents/com.foo.updater.plist"),
            s.join("Application Support/com.foo.app"),
            daemon.clone(),
            s.join("PrivilegedHelperTools/com.foo.app.helper"),
            link.clone(),
            // Named after the app, with nothing else in it: its uninstaller is inside.
            apps_dir.join("Foo Studio"),
        ]
        .into_iter()
        .collect();
        assert_eq!(ticked, expect_ticked, "found: {found:#?}");
        let expect_unticked: HashSet<PathBuf> = [
            u.join("Containers/com.foo.app.ShareExtension"),
            u.join("Preferences/com.foo.app.LSSharedFileList.plist"),
            u.join("LaunchAgents/com.foo.app.sync.plist"),
            u.join("Application Support/Foo Studio"),
            u.join("Logs/Foo Studio"),
        ]
        .into_iter()
        .collect();
        assert_eq!(unticked, expect_unticked, "found: {found:#?}");
        assert!(got(&s.join("Application Support/com.foo.app")).unwrap().admin, "system items need a password");
        assert!(!got(&u.join("Caches/com.foo.app")).unwrap().admin);
        assert!(found.iter().all(|l| l.measured));

        // Without Full Disk Access: the same items, but data containers
        // aren't looked inside or measured.
        let blind = find_in(&libs, std::slice::from_ref(&apps_dir), &foo, &others, false);
        let paths = |v: &[Leftover]| v.iter().map(|l| l.path.clone()).collect::<HashSet<_>>();
        assert_eq!(paths(&blind), paths(&found));
        for l in &blind {
            let container = l.path.starts_with(u.join("Containers")) || l.path.starts_with(u.join("Group Containers"));
            assert_eq!(l.measured, !container, "{}", l.path.display());
        }

        // A second copy of the app still installed: nothing by id or name,
        // only the agent that runs the removed copy.
        let twin = Identity { id: Some("com.foo.app".into()), names: vec!["foo studio".into()], paths: vec![base.join("Other/Foo Studio.app")] };
        let found = find_in(&libs, std::slice::from_ref(&apps_dir), &foo, &[twin], true);
        let in_library: Vec<&Path> = found.iter().map(|l| l.path.as_path()).filter(|p| p.starts_with(u) || p.starts_with(s)).collect();
        assert_eq!(in_library, [agents.join("com.foo.updater.plist").as_path()]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn lists_this_macs_apps() {
        // Read-only: lists /Applications and ~/Applications.
        let all = list();
        assert!(all.iter().all(|a| !a.name.is_empty() && !a.path.starts_with("/System")));
        assert!(all.iter().all(|a| a.path.is_dir()));
    }
}
