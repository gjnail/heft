//! Quick rescans on macOS through FSEvents, the counterpart of the NTFS
//! change journal on Windows.
//!
//! macOS logs every change to a local disk (fseventsd keeps the history on
//! the disk itself), and any program can read that history back from a
//! given event id without asking for permission. A scan notes the current
//! event id before it starts and keeps its tree. A rescan reads the events
//! logged since for the scanned folder, turns them into the folders that
//! have to be listed again (see [`plan`]), and rebuilds the tree from those
//! listings plus the unchanged rest (see [`walk::update`]). When the history
//! can't be trusted (events were dropped, the disk was replaced, the folder
//! itself moved), it asks for a full scan instead.
//!
//! Event paths come back as the file system knows them: symlinks resolved
//! (`/tmp` is `/private/tmp`) and the data volume seen through its firmlinks
//! (`/Users`, not `/System/Volumes/Data/Users`), so they are mapped back to
//! the scanned path first. Each volume keeps its own history, so disks
//! mounted inside the scanned folder are read with their own stream path;
//! ones without a history (network shares, small system volumes) are read
//! again on every rescan.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

use super::walk::{self, Changes, Ids};
use super::Progress;
use crate::platform;
use crate::tree::{flags, NodeId, Tree, ROOT};

mod ffi {
    use std::ffi::{c_char, c_void};

    pub type CFTypeRef = *const c_void;
    pub type StreamRef = *mut c_void;
    pub type Queue = *mut c_void;
    pub type Callback = extern "C" fn(StreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

    #[repr(C)]
    pub struct StreamContext {
        pub version: isize,
        pub info: *mut c_void,
        pub retain: *const c_void,
        pub release: *const c_void,
        pub copy_description: *const c_void,
    }

    #[repr(C)]
    pub struct UuidBytes(pub [u8; 16]);

    /// Opaque; only its address is used.
    #[repr(C)]
    pub struct ArrayCallBacks([usize; 5]);

    pub const UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        pub fn FSEventsGetCurrentEventId() -> u64;
        pub fn FSEventsCopyUUIDForDevice(dev: libc::dev_t) -> CFTypeRef;
        pub fn FSEventStreamCreate(
            allocator: CFTypeRef,
            callback: Callback,
            context: *const StreamContext,
            paths: CFTypeRef,
            since: u64,
            latency: f64,
            flags: u32,
        ) -> StreamRef;
        pub fn FSEventStreamSetDispatchQueue(stream: StreamRef, queue: Queue);
        pub fn FSEventStreamStart(stream: StreamRef) -> u8;
        pub fn FSEventStreamStop(stream: StreamRef);
        pub fn FSEventStreamInvalidate(stream: StreamRef);
        pub fn FSEventStreamRelease(stream: StreamRef);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub static kCFTypeArrayCallBacks: ArrayCallBacks;
        pub fn CFRelease(cf: CFTypeRef);
        pub fn CFUUIDGetUUIDBytes(uuid: CFTypeRef) -> UuidBytes;
        pub fn CFStringCreateWithBytes(alloc: CFTypeRef, bytes: *const u8, len: isize, encoding: u32, external: u8) -> CFTypeRef;
        pub fn CFArrayCreate(alloc: CFTypeRef, values: *const CFTypeRef, count: isize, callbacks: *const c_void) -> CFTypeRef;
    }

    unsafe extern "C" {
        pub fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> Queue;
        pub fn dispatch_sync_f(queue: Queue, context: *mut c_void, work: extern "C" fn(*mut c_void));
        pub fn dispatch_release(object: *mut c_void);
    }
}

// Stream creation flags.
const CREATE_NO_DEFER: u32 = 0x02;
const CREATE_FILE_EVENTS: u32 = 0x10;

// Event flags.
const MUST_SCAN_SUB_DIRS: u32 = 0x01;
const USER_DROPPED: u32 = 0x02;
const KERNEL_DROPPED: u32 = 0x04;
const IDS_WRAPPED: u32 = 0x08;
const HISTORY_DONE: u32 = 0x10;
const ROOT_CHANGED: u32 = 0x20;
const MOUNT: u32 = 0x40;
const UNMOUNT: u32 = 0x80;
const CREATED: u32 = 0x100;
const REMOVED: u32 = 0x200;
const INODE_META_MOD: u32 = 0x400;
const RENAMED: u32 = 0x800;
const CHANGE_OWNER: u32 = 0x4000;

/// Events that say "look at everything below this path again".
const RESCAN: u32 = MUST_SCAN_SUB_DIRS | USER_DROPPED | KERNEL_DROPPED | ROOT_CHANGED | MOUNT | UNMOUNT;

/// More changes than this and a full scan is quicker than chasing them.
const MAX_CHANGES: usize = 250_000;

/// How long to wait for fseventsd to replay its history.
const HISTORY_TIMEOUT: Duration = Duration::from_secs(120);

/// Where the data volume is mounted; firmlinks point into it.
const DATA_VOLUME: &str = "/System/Volumes/Data";

/// One logged change.
#[derive(Clone, Debug)]
pub(super) struct Event {
    pub path: String,
    pub flags: u32,
}

/// The newest event id the system has handed out.
pub fn current_event_id() -> u64 {
    unsafe { ffi::FSEventsGetCurrentEventId() }
}

/// The id of a volume's event history, or `None` if it keeps none
/// (network shares, read-only and some system volumes).
fn history_uuid(dev: libc::dev_t) -> Option<[u8; 16]> {
    let uuid = unsafe { ffi::FSEventsCopyUUIDForDevice(dev) };
    if uuid.is_null() {
        return None;
    }
    let bytes = unsafe { ffi::CFUUIDGetUUIDBytes(uuid) };
    unsafe { ffi::CFRelease(uuid) };
    Some(bytes.0)
}

struct Sink {
    tx: crossbeam_channel::Sender<Vec<(String, u32, u64)>>,
}

extern "C" fn on_events(
    _stream: ffi::StreamRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
) {
    let sink = unsafe { &*(info as *const Sink) };
    let paths = paths as *const *const c_char;
    let batch = (0..count)
        .map(|i| unsafe {
            (CStr::from_ptr(*paths.add(i)).to_string_lossy().into_owned(), *flags.add(i), *ids.add(i))
        })
        .collect();
    let _ = sink.tx.send(batch);
}

extern "C" fn nothing(_: *mut c_void) {}

/// Every event logged under `paths` after event `since`, and the id of the
/// last one (`since` if there were none).
fn read_history(paths: &[String], since: u64) -> Result<(Vec<Event>, u64), String> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let sink = Box::new(Sink { tx });
    let context = ffi::StreamContext {
        version: 0,
        info: &*sink as *const Sink as *mut c_void,
        retain: std::ptr::null(),
        release: std::ptr::null(),
        copy_description: std::ptr::null(),
    };
    let strings: Vec<ffi::CFTypeRef> = paths
        .iter()
        .map(|p| unsafe { ffi::CFStringCreateWithBytes(std::ptr::null(), p.as_ptr(), p.len() as isize, ffi::UTF8, 0) })
        .collect();
    let stream = if strings.iter().any(|s| s.is_null()) {
        std::ptr::null_mut()
    } else {
        unsafe {
            let array = ffi::CFArrayCreate(
                std::ptr::null(),
                strings.as_ptr(),
                strings.len() as isize,
                (&raw const ffi::kCFTypeArrayCallBacks).cast(),
            );
            let stream = if array.is_null() {
                std::ptr::null_mut()
            } else {
                ffi::FSEventStreamCreate(
                    std::ptr::null(),
                    on_events,
                    &context,
                    array,
                    since,
                    0.0,
                    CREATE_FILE_EVENTS | CREATE_NO_DEFER,
                )
            };
            if !array.is_null() {
                ffi::CFRelease(array);
            }
            stream
        }
    };
    for s in strings.into_iter().filter(|s| !s.is_null()) {
        unsafe { ffi::CFRelease(s) };
    }
    if stream.is_null() {
        return Err("macOS would not read the file system events".into());
    }

    let queue = unsafe { ffi::dispatch_queue_create(c"heft.fsevents".as_ptr(), std::ptr::null()) };
    unsafe { ffi::FSEventStreamSetDispatchQueue(stream, queue) };
    let started = unsafe { ffi::FSEventStreamStart(stream) } != 0;
    let mut events = Vec::new();
    let mut result = Err("macOS would not read the file system events".to_string());
    if started {
        let deadline = Instant::now() + HISTORY_TIMEOUT;
        result = Err("macOS didn't return the file system events in time".into());
        'read: while let Ok(batch) = rx.recv_deadline(deadline) {
            for (path, fl, id) in batch {
                if fl & HISTORY_DONE != 0 {
                    result = Ok(id.max(since));
                    break 'read;
                }
                events.push(Event { path, flags: fl });
            }
            if events.len() > MAX_CHANGES {
                result = Err(format!("more than {MAX_CHANGES} items changed"));
                break;
            }
        }
        unsafe { ffi::FSEventStreamStop(stream) };
    }
    unsafe {
        ffi::FSEventStreamInvalidate(stream);
        ffi::FSEventStreamRelease(stream);
        // Wait out a callback that may still be running before `sink` goes.
        ffi::dispatch_sync_f(queue, std::ptr::null_mut(), nothing);
        ffi::dispatch_release(queue);
    }
    drop(sink);
    result.map(|last| (events, last))
}

// ---------------------------------------------------------------------------
// Watching live

/// `kFSEventStreamEventIdSinceNow`: only changes from now on.
const SINCE_NOW: u64 = u64::MAX;

struct Notify(Box<dyn Fn() + Send + Sync>);

extern "C" fn on_change(_stream: ffi::StreamRef, info: *mut c_void, _count: usize, _paths: *mut c_void, _flags: *const u32, _ids: *const u64) {
    let notify = unsafe { &*(info as *const Notify) };
    (notify.0)();
}

/// Calls its function soon after anything changes under the watched
/// folders, at most about once a second, until it's dropped. It says only
/// that something changed; the history says what (see [`refresh`]).
pub struct Watcher {
    stream: ffi::StreamRef,
    queue: ffi::Queue,
    _notify: Box<Notify>,
}

// The stream and queue are only touched by `watch` and `drop`.
unsafe impl Send for Watcher {}

/// Watch `paths` (as they'd be scanned) for changes. `None` if macOS
/// wouldn't start a stream.
pub fn watch(paths: &[String], notify: impl Fn() + Send + Sync + 'static) -> Option<Watcher> {
    let notify = Box::new(Notify(Box::new(notify)));
    let context = ffi::StreamContext {
        version: 0,
        info: &*notify as *const Notify as *mut c_void,
        retain: std::ptr::null(),
        release: std::ptr::null(),
        copy_description: std::ptr::null(),
    };
    let strings: Vec<ffi::CFTypeRef> = paths
        .iter()
        .map(|p| unsafe { ffi::CFStringCreateWithBytes(std::ptr::null(), p.as_ptr(), p.len() as isize, ffi::UTF8, 0) })
        .collect();
    let mut stream = std::ptr::null_mut();
    if !strings.is_empty() && strings.iter().all(|s| !s.is_null()) {
        unsafe {
            let array = ffi::CFArrayCreate(
                std::ptr::null(),
                strings.as_ptr(),
                strings.len() as isize,
                (&raw const ffi::kCFTypeArrayCallBacks).cast(),
            );
            if !array.is_null() {
                // Changes within a second of the first are delivered together.
                stream = ffi::FSEventStreamCreate(std::ptr::null(), on_change, &context, array, SINCE_NOW, 1.0, CREATE_NO_DEFER);
                ffi::CFRelease(array);
            }
        }
    }
    for s in strings.into_iter().filter(|s| !s.is_null()) {
        unsafe { ffi::CFRelease(s) };
    }
    if stream.is_null() {
        return None;
    }
    let queue = unsafe { ffi::dispatch_queue_create(c"heft.fsevents.watch".as_ptr(), std::ptr::null()) };
    unsafe { ffi::FSEventStreamSetDispatchQueue(stream, queue) };
    let w = Watcher { stream, queue, _notify: notify };
    (unsafe { ffi::FSEventStreamStart(stream) } != 0).then_some(w)
}

impl Drop for Watcher {
    fn drop(&mut self) {
        unsafe {
            ffi::FSEventStreamStop(self.stream);
            ffi::FSEventStreamInvalidate(self.stream);
            ffi::FSEventStreamRelease(self.stream);
            // Wait out a callback that may still be running before `notify` goes.
            ffi::dispatch_sync_f(self.queue, std::ptr::null_mut(), nothing);
            ffi::dispatch_release(self.queue);
        }
    }
}

/// Wait until the history shows a change to `path` made after event
/// `since`: macOS logs changes a moment after they happen. False if that
/// takes longer than `timeout`.
pub fn wait_logged(path: &Path, since: u64, timeout: Duration) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else { return false };
    let dir = dir.to_string_lossy().into_owned();
    let suffix = format!("/{}", name.to_string_lossy());
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok((events, _)) = read_history(std::slice::from_ref(&dir), since)
            && events.iter().any(|e| e.path.ends_with(&suffix))
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// Mounts inside the scanned folder

/// A file system mounted inside the scanned folder.
#[derive(Clone, Debug, PartialEq)]
struct Volume {
    point: String,
    dev: libc::dev_t,
    history: Option<[u8; 16]>,
    local: bool,
    read_only: bool,
    /// Not scanned at all (a virtual file system or a snapshot).
    skip: bool,
}

fn volumes_under(roots: &[String]) -> Vec<Volume> {
    let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if n <= 0 {
        return Vec::new();
    }
    let cap = n as usize + 8;
    let mut buf: Vec<libc::statfs> = Vec::with_capacity(cap);
    let size = (cap * std::mem::size_of::<libc::statfs>()) as libc::c_int;
    let n = unsafe { libc::getfsstat(buf.as_mut_ptr(), size, libc::MNT_NOWAIT) };
    if n <= 0 {
        return Vec::new();
    }
    unsafe { buf.set_len((n as usize).min(cap)) };
    let text = |chars: &[c_char]| {
        let bytes: Vec<u8> = chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let mut out: Vec<Volume> = buf
        .iter()
        .filter_map(|s| {
            let point = text(&s.f_mntonname);
            if !matches!(place(&point, roots), Place::Inside(rel) if !rel.is_empty()) {
                return None;
            }
            let skip = platform::is_virtual_fs(&text(&s.f_fstypename)) || s.f_flags & libc::MNT_SNAPSHOT as u32 != 0;
            let local = s.f_flags & libc::MNT_LOCAL as u32 != 0;
            // The first half of the file system id is its device number.
            let dev = unsafe { *(&raw const s.f_fsid).cast::<i32>() } as libc::dev_t;
            Some(Volume {
                point,
                dev,
                history: if local && !skip { history_uuid(dev) } else { None },
                local,
                read_only: s.f_flags & libc::MNT_RDONLY as u32 != 0,
                skip,
            })
        })
        .collect();
    out.sort_by(|a, b| a.point.cmp(&b.point));
    out
}

/// Compare the mounts inside the scanned folder with the last scan's.
/// Returns the mount points whose own history has to be read, and the ones
/// that have to be read again from disk: mounted, unmounted or replaced
/// since, or keeping no history that covers changes.
fn compare_volumes(before: &[Volume], now: &[Volume], root_history: &[u8; 16]) -> (Vec<String>, Vec<String>) {
    let (mut watch, mut reread) = (Vec::new(), Vec::new());
    for v in now.iter().filter(|v| !v.skip || !before.contains(v)) {
        match &v.history {
            _ if !before.contains(v) => reread.push(v.point.clone()),
            // Other computers change shares without telling this one.
            _ if !v.local => reread.push(v.point.clone()),
            Some(h) if h == root_history => {}
            Some(_) => watch.push(v.point.clone()),
            None if v.read_only => {}
            None => reread.push(v.point.clone()),
        }
    }
    for v in before {
        if !now.iter().any(|n| n.point == v.point) {
            reread.push(v.point.clone());
        }
    }
    (watch, reread)
}

// ---------------------------------------------------------------------------
// From event paths to folders of the tree

/// `/usr/share/firmlinks`: (path on the system volume, path inside the data volume).
fn firmlinks() -> Vec<(String, String)> {
    std::fs::read_to_string("/usr/share/firmlinks")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (src, dst) = l.split_once('\t')?;
            (src.starts_with('/') && !dst.is_empty()).then(|| (src.to_string(), dst.trim_matches('/').to_string()))
        })
        .collect()
}

/// `path` relative to `dir` ("" for `dir` itself), if it's `dir` or inside it.
fn strip_dir<'p>(path: &'p str, dir: &str) -> Option<&'p str> {
    if dir == "/" {
        return path.strip_prefix('/');
    }
    let rest = path.strip_prefix(dir)?;
    if rest.is_empty() {
        Some("")
    } else {
        rest.strip_prefix('/')
    }
}

/// The spellings of an event path: as reported, and through the firmlinks
/// in either direction (a scan of `/` has the data volume twice).
fn spellings(path: &str, firmlinks: &[(String, String)]) -> Vec<String> {
    let mut out = vec![path.to_string()];
    for (src, dst) in firmlinks {
        let data = format!("{DATA_VOLUME}/{dst}");
        if let Some(rest) = strip_dir(path, src) {
            out.push(join(&data, rest));
        }
        if let Some(rest) = strip_dir(path, &data) {
            out.push(join(src, rest));
        }
    }
    out
}

fn join(dir: &str, rel: &str) -> String {
    if rel.is_empty() {
        dir.to_string()
    } else if dir.ends_with('/') {
        format!("{dir}{rel}")
    } else {
        format!("{dir}/{rel}")
    }
}

#[derive(Debug, PartialEq)]
enum Place {
    /// Inside the scanned folder, at this relative path.
    Inside(String),
    /// A folder that contains the scanned folder.
    Above,
    Outside,
}

/// Where `path` is relative to the scanned folder, spelled any of `roots`.
fn place(path: &str, roots: &[String]) -> Place {
    let mut above = false;
    for r in roots {
        if let Some(rel) = strip_dir(path, r) {
            return Place::Inside(rel.to_string());
        }
        above |= strip_dir(r, path).is_some();
    }
    if above { Place::Above } else { Place::Outside }
}

/// Finds nodes of the previous tree by relative path.
struct Resolver<'a> {
    tree: &'a Tree,
    index: HashMap<NodeId, HashMap<&'a str, NodeId>>,
}

impl<'a> Resolver<'a> {
    fn new(tree: &'a Tree) -> Self {
        Resolver { tree, index: HashMap::new() }
    }

    fn child(&mut self, dir: NodeId, name: &str) -> Option<NodeId> {
        let t = self.tree;
        let kids = t.children(dir);
        let exact = if kids.len() > 32 {
            let index = self.index.entry(dir).or_insert_with(|| kids.iter().map(|&k| (t.name(k), k)).collect());
            index.get(name).copied()
        } else {
            kids.iter().copied().find(|&k| t.name(k) == name)
        };
        // Case-insensitive volumes: the history spells names as they are
        // stored, so this is only a fallback for small folders.
        exact.or_else(|| {
            (kids.len() <= 32).then(|| kids.iter().copied().find(|&k| platform::names_eq(t.name(k), name))).flatten()
        })
    }

    /// The deepest node on `rel`'s path, and whether it is `rel` itself.
    fn resolve(&mut self, rel: &str) -> (NodeId, bool) {
        let mut cur = ROOT;
        for comp in rel.split('/').filter(|c| !c.is_empty()) {
            if !self.tree.node(cur).is_dir() {
                return (cur, false);
            }
            match self.child(cur, comp) {
                Some(k) => cur = k,
                None => return (cur, false),
            }
        }
        (cur, true)
    }
}

fn lost(fl: u32) -> String {
    if fl & (USER_DROPPED | KERNEL_DROPPED) != 0 {
        "macOS dropped some file system events".into()
    } else if fl & ROOT_CHANGED != 0 {
        "the scanned folder was moved".into()
    } else if fl & (MOUNT | UNMOUNT) != 0 {
        "a disk was mounted or unmounted there".into()
    } else {
        "macOS asked for the folder to be read again".into()
    }
}

/// Turn logged events into the folders to list again. `readable` says if a
/// folder of the tree can be listed and entered now, `same` if the folder at
/// its path is still the one scanned (same identity). `Err` means only a
/// full scan will do.
fn plan(
    tree: &Tree,
    roots: &[String],
    firmlinks: &[(String, String)],
    events: &[Event],
    readable: impl Fn(NodeId) -> bool,
    same: impl Fn(NodeId) -> bool,
) -> Result<Changes, String> {
    if events.len() > MAX_CHANGES {
        return Err(format!("{} items changed", events.len()));
    }
    let mut ch = Changes::default();
    let mut r = Resolver::new(tree);
    let parent = |n: NodeId| (n != ROOT).then(|| tree.node(n).parent);
    let mut rels = Vec::new();
    for e in events {
        if e.flags & IDS_WRAPPED != 0 {
            return Err("the file system event counter started over".into());
        }
        rels.clear();
        let mut above = false;
        for s in spellings(&e.path, firmlinks) {
            match place(&s, roots) {
                Place::Inside(rel) => rels.push(rel),
                Place::Above => above = true,
                Place::Outside => {}
            }
        }
        if above && e.flags & RESCAN != 0 {
            return Err(lost(e.flags));
        }
        rels.sort();
        rels.dedup();
        for rel in &rels {
            let (node, exact) = r.resolve(rel);
            let is_dir = tree.node(node).is_dir();
            let up = parent(node);
            let up2 = up.and_then(parent);
            if !exact {
                // Something new below `node` (or below a file that became a
                // folder): its listing, and its own date in its parent's.
                if is_dir {
                    ch.dirty.insert(node);
                }
                ch.dirty.extend(up);
                if !is_dir {
                    ch.dirty.extend(up2);
                }
                continue;
            }
            if e.flags & RESCAN != 0 {
                if node == ROOT {
                    return Err(lost(e.flags));
                }
                if is_dir {
                    ch.deep.insert(node);
                }
                ch.dirty.extend(up.into_iter().chain(up2));
                continue;
            }
            if node == ROOT {
                // Its identity is checked separately; its entries may have changed.
                ch.dirty.insert(ROOT);
                if e.flags & (INODE_META_MOD | CHANGE_OWNER) != 0 && !readable(ROOT) {
                    return Err("the scanned folder can't be read any more".into());
                }
                continue;
            }
            // Its entry in its parent, and the parent's date in the grandparent.
            ch.dirty.extend(up.into_iter().chain(up2));
            if is_dir {
                // Moved away and back, or another folder now: what happened
                // inside it wasn't logged under this path. (The history merges
                // a path's flags over a while, so a folder created before the
                // scan can still say "created"; its identity tells.)
                if e.flags & RENAMED != 0 || (e.flags & (CREATED | REMOVED) != 0 && !same(node)) {
                    ch.deep.insert(node);
                } else {
                    ch.dirty.insert(node);
                    if e.flags & (INODE_META_MOD | CHANGE_OWNER) != 0 && !readable(node) {
                        ch.deep.insert(node);
                    }
                }
            }
        }
    }
    Ok(ch)
}

/// A mount point inside the scan changed: read what's there again.
fn reread_mount(tree: &Tree, roots: &[String], point: &str, ch: &mut Changes) {
    let Place::Inside(rel) = place(point, roots) else { return };
    let (node, exact) = Resolver::new(tree).resolve(&rel);
    let parent = |n: NodeId| (n != ROOT).then(|| tree.node(n).parent);
    if exact && node != ROOT {
        if tree.node(node).is_dir() {
            ch.deep.insert(node);
        }
        ch.dirty.extend(parent(node));
    } else if tree.node(node).is_dir() {
        ch.dirty.insert(node);
        ch.dirty.extend(parent(node));
    }
}

fn can_enter(path: &str) -> bool {
    CString::new(path).is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::R_OK | libc::X_OK) } == 0)
}

// ---------------------------------------------------------------------------
// Scan state

/// What a scan keeps for quick rescans.
pub struct FsState {
    root: String,
    /// The scanned path, plus its real path if symlinks lead there.
    roots: Vec<String>,
    /// Folder identities by node id, for the tree `refresh` is given: the
    /// last one it (or the scan) returned. The tree itself isn't kept here,
    /// so a whole-disk scan isn't held in memory twice.
    ids: Ids,
    /// Everything logged up to this event is in `tree`.
    since: u64,
    /// (device, inode) of the scanned folder.
    root_ident: (u64, u64),
    root_history: [u8; 16],
    volumes: Vec<Volume>,
}

fn identity(path: &str) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path).ok()?;
    md.is_dir().then(|| (md.dev(), md.ino()))
}

/// The scanned folder's identity and its disk's history id, or why quick
/// rescans can't be used there.
fn tracking(root: &str) -> Result<((u64, u64), [u8; 16]), String> {
    let ident = identity(root).ok_or("the folder can't be read")?;
    let c = CString::new(Path::new(root).as_os_str().as_bytes()).map_err(|_| "unusual path")?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return Err("the folder can't be read".into());
    }
    if s.f_flags & libc::MNT_LOCAL as u32 == 0 {
        return Err("it isn't on a local disk".into());
    }
    let history = history_uuid(ident.0 as libc::dev_t).ok_or("this disk keeps no file system event history")?;
    Ok((ident, history))
}

/// Why a scan of `root` wouldn't get quick rescans, or `None` if it would.
pub fn unavailable_reason(root: &str) -> Option<String> {
    tracking(root).err()
}

/// A full scan that also returns what [`refresh`] needs, when `root` is on a
/// disk that keeps an event history.
pub fn scan(root: &str, progress: &Progress) -> Result<(Tree, Option<FsState>), String> {
    let Ok((root_ident, root_history)) = tracking(root) else {
        return walk::scan(root, progress).map(|t| (t, None));
    };
    let mut roots = vec![root.to_string()];
    if let Ok(real) = std::fs::canonicalize(root) {
        let real = real.to_string_lossy().into_owned();
        if real != root {
            roots.push(real);
        }
    }
    // Before reading anything, so changes made during the scan are replayed.
    let since = current_event_id();
    let volumes = volumes_under(&roots);
    let (tree, ids) = walk::scan_keeping_ids(root, progress)?;
    let state = FsState { root: root.to_string(), roots, ids, since, root_ident, root_history, volumes };
    Ok((tree, Some(state)))
}

/// Bring `tree`, the last tree the scan or a refresh returned (changed only
/// by removing items and resizing files, which keep node ids), up to date
/// from the event history. `Ok(None)` means nothing changed; `Err` means the
/// history can't be used and a full scan is needed.
pub fn refresh(st: &mut FsState, tree: &Tree, progress: &Progress) -> Result<Option<Tree>, String> {
    let t0 = Instant::now();
    progress.set_phase("Reading file system events");
    if identity(&st.root) != Some(st.root_ident) {
        return Err("the scanned folder was moved, replaced or deleted".into());
    }
    if history_uuid(st.root_ident.0 as libc::dev_t) != Some(st.root_history) {
        return Err("the disk's file system event history was reset".into());
    }
    let now = current_event_id();
    let volumes = volumes_under(&st.roots);
    let (watch, reread) = compare_volumes(&st.volumes, &volumes, &st.root_history);
    let mut paths = vec![st.root.clone()];
    paths.extend(watch);
    let (events, last) = read_history(&paths, st.since)?;
    let ids = &st.ids;
    let same = |id: NodeId| {
        let was = ids.dirs.binary_search_by_key(&id, |d| d.0).ok().map(|i| ids.dirs[i].1);
        let now = identity(&tree.path(id)).map(|(dev, ino)| ((dev as u128) << 64) | ino as u128);
        was.is_some() && was == now
    };
    let mut changes = plan(tree, &st.roots, &firmlinks(), &events, |id| can_enter(&tree.path(id)), same)?;
    for p in &reread {
        reread_mount(tree, &st.roots, p, &mut changes);
    }
    let history_ms = t0.elapsed().as_millis() as u64;
    let next = st.since.max(now).max(last);
    let unchanged = |st: &mut FsState, volumes| {
        st.since = next;
        st.volumes = volumes;
        Ok(None)
    };
    if changes.is_empty() {
        return unchanged(st, volumes);
    }

    let (mut tree, ids, folders) = walk::update(tree, &st.ids, &changes, progress)?;
    if folders == 0 {
        // Only second paths and skipped mounts were named: a plain copy.
        return unchanged(st, volumes);
    }
    tree.info.note = Some(format!("Updated {folders} changed folder(s) from file system events."));
    tree.info.phases.insert(0, ("read file system events", history_ms));
    st.ids = ids;
    st.since = next;
    st.volumes = volumes;
    Ok(Some(tree))
}

// ---------------------------------------------------------------------------
// Checking

/// How two scans of the same folder differ, one line per item (at most
/// `limit` lines). Which name of a hard-linked file carries its bytes may
/// differ between two full scans too, so that alone isn't a difference.
pub fn differences(a: &Tree, b: &Tree, limit: usize) -> Vec<String> {
    let mut out = Diff { lines: Vec::new(), total: 0, limit };
    if a.info.unreadable_dirs != b.info.unreadable_dirs {
        out.push(format!("unreadable folders: {} vs {}", a.info.unreadable_dirs, b.info.unreadable_dirs));
    }
    compare_node(a, ROOT, b, ROOT, &mut out);
    if out.total > out.lines.len() {
        let more = out.total - out.lines.len();
        out.lines.push(format!("… and {more} more"));
    }
    out.lines
}

struct Diff {
    lines: Vec<String>,
    total: usize,
    limit: usize,
}

impl Diff {
    fn push(&mut self, line: String) {
        self.total += 1;
        if self.lines.len() < self.limit {
            self.lines.push(line);
        }
    }
}

/// Returns (a hard link's owner differs below, something differs below).
fn compare_node(a: &Tree, ia: NodeId, b: &Tree, ib: NodeId, out: &mut Diff) -> (bool, bool) {
    let (na, nb) = (a.node(ia), b.node(ib));
    let mut swapped = !na.is_dir() && (na.flags ^ nb.flags) & flags::HARDLINK != 0;
    let mut below = false;
    if na.is_dir() && nb.is_dir() {
        let theirs: HashMap<&str, NodeId> = b.children(ib).iter().map(|&c| (b.name(c), c)).collect();
        let mut matched = 0;
        for &ca in a.children(ia) {
            match theirs.get(a.name(ca)) {
                Some(&cb) => {
                    matched += 1;
                    let (s, d) = compare_node(a, ca, b, cb, out);
                    swapped |= s;
                    below |= d;
                }
                None => {
                    below = true;
                    out.push(format!("only in the first: {}", a.path(ca)));
                }
            }
        }
        if matched < b.children(ib).len() {
            let ours: std::collections::HashSet<&str> = a.children(ia).iter().map(|&c| a.name(c)).collect();
            for &cb in b.children(ib) {
                if !ours.contains(b.name(cb)) {
                    below = true;
                    out.push(format!("only in the second: {}", b.path(cb)));
                }
            }
        }
    }
    let mut what = Vec::new();
    if (na.flags ^ nb.flags) & !flags::HARDLINK != 0 {
        what.push(format!("flags {:#x} vs {:#x}", na.flags, nb.flags));
    }
    // A folder's totals follow from what's below it; only report them when
    // nothing below explains the difference.
    if !below {
        if !swapped && (na.size, na.alloc) != (nb.size, nb.alloc) {
            what.push(format!("size {}/{} vs {}/{}", na.size, na.alloc, nb.size, nb.alloc));
        }
        if na.files != nb.files {
            what.push(format!("files {} vs {}", na.files, nb.files));
        }
        if na.mtime != nb.mtime {
            what.push(format!("modified {} vs {}", na.mtime, nb.mtime));
        }
    }
    let differs = !what.is_empty();
    if differs {
        out.push(format!("{}: {}", a.path(ia), what.join(", ")));
    }
    (swapped, below || differs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::RefreshOutcome;
    use crate::tree::{ScanInfo, ScanMode, TreeBuilder};

    /// A real stream on a temporary folder: a new file is noticed within a
    /// few seconds, and nothing is called once the watcher is gone.
    #[test]
    fn watcher_notices_changes() {
        let dir = std::env::temp_dir().join(format!("heft-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let w = watch(&[dir.to_string_lossy().into_owned()], move || {
            let _ = tx.send(());
        })
        .expect("stream");
        std::fs::write(dir.join("new.txt"), "x").unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(10)).is_ok(), "no notice of the new file");
        drop(w);
        while rx.try_recv().is_ok() {}
        std::fs::write(dir.join("later.txt"), "x").unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        assert!(rx.try_recv().is_err(), "called after it was dropped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tree() -> Tree {
        let mut b = TreeBuilder::new("/Users/me");
        let docs = b.add(ROOT, "Documents", flags::DIR, 0, 0, 0);
        b.add(docs, "a.txt", 0, 10, 10, 0);
        let sub = b.add(docs, "Sub", flags::DIR, 0, 0, 0);
        b.add(sub, "b.txt", 0, 10, 10, 0);
        let info = ScanInfo { mode: ScanMode::Walk, duration_ms: 0, finished_at: 0, unreadable_dirs: 0, note: None, phases: Vec::new() };
        b.finish("/Users/me".into(), info)
    }

    fn ev(path: &str, flags: u32) -> Event {
        Event { path: path.into(), flags }
    }

    const FILE: u32 = 0x10000;
    const DIR: u32 = 0x20000;
    const MODIFIED: u32 = 0x1000;

    fn plan_for(events: &[Event]) -> Result<(Vec<String>, Vec<String>), String> {
        let t = tree();
        let roots = vec!["/Users/me".to_string()];
        let links = vec![("/Users".to_string(), "Users".to_string())];
        let ch = plan(&t, &roots, &links, events, |_| true, |_| true)?;
        let names = |s: &std::collections::HashSet<NodeId>| {
            let mut v: Vec<String> = s.iter().map(|&n| t.path(n)).collect();
            v.sort();
            v
        };
        Ok((names(&ch.dirty), names(&ch.deep)))
    }

    #[test]
    fn changed_file_lists_its_folder_and_the_one_above() {
        let (dirty, deep) = plan_for(&[ev("/Users/me/Documents/Sub/b.txt", MODIFIED | FILE)]).unwrap();
        assert_eq!(dirty, ["/Users/me/Documents", "/Users/me/Documents/Sub"]);
        assert!(deep.is_empty());
    }

    #[test]
    fn new_items_list_the_nearest_known_folder() {
        let (dirty, deep) = plan_for(&[ev("/Users/me/Documents/New/Deeper/c.txt", CREATED | FILE)]).unwrap();
        assert_eq!(dirty, ["/Users/me", "/Users/me/Documents"]);
        assert!(deep.is_empty());
    }

    #[test]
    fn renamed_or_recreated_folders_are_walked_again() {
        let (dirty, deep) = plan_for(&[ev("/Users/me/Documents/Sub", RENAMED | DIR)]).unwrap();
        assert_eq!(deep, ["/Users/me/Documents/Sub"]);
        assert_eq!(dirty, ["/Users/me", "/Users/me/Documents"]);
        let (dirty, deep) = plan_for(&[ev("/Users/me/Documents/Sub", 0x8000 | DIR)]).unwrap();
        assert!(deep.is_empty(), "an extended attribute only needs a listing");
        assert!(dirty.contains(&"/Users/me/Documents/Sub".to_string()));

        // "Created" on a folder that is still the one scanned is a merged,
        // older flag; on another folder under the same name it's real.
        let t = tree();
        let roots = vec!["/Users/me".to_string()];
        let sub = t.find("/Users/me/Documents/Sub").unwrap();
        let events = [ev("/Users/me/Documents/Sub", CREATED | 0x8000 | DIR)];
        let ch = plan(&t, &roots, &[], &events, |_| true, |_| true).unwrap();
        assert!(ch.deep.is_empty() && ch.dirty.contains(&sub));
        let ch = plan(&t, &roots, &[], &events, |_| true, |n| n != sub).unwrap();
        assert!(ch.deep.contains(&sub));
    }

    #[test]
    fn firmlinked_spellings_map_to_the_scan() {
        // Scanned as /System/Volumes/Data/Users/me, logged as /Users/me.
        let t = tree();
        let mut b = TreeBuilder::new("/System/Volumes/Data/Users/me");
        b.add(ROOT, "x", 0, 1, 1, 0);
        let info = t.info.clone();
        let data = b.finish("/System/Volumes/Data/Users/me".into(), info);
        let roots = vec!["/System/Volumes/Data/Users/me".to_string()];
        let links = vec![("/Users".to_string(), "Users".to_string())];
        let ch = plan(&data, &roots, &links, &[ev("/Users/me/x", MODIFIED | FILE)], |_| true, |_| true).unwrap();
        assert!(ch.dirty.contains(&ROOT));
        assert_eq!(spellings("/Users/me", &links), ["/Users/me", "/System/Volumes/Data/Users/me"]);
        assert_eq!(place("/private/tmp/x/y", &["/tmp/x".into(), "/private/tmp/x".into()]), Place::Inside("y".into()));
        assert_eq!(place("/", &["/tmp/x".into()]), Place::Above);
        assert_eq!(place("/tmp/xy", &["/tmp/x".into()]), Place::Outside);
        assert_eq!(place("/a/b", &["/".into()]), Place::Inside("a/b".into()));
    }

    #[test]
    fn untrustworthy_history_needs_a_full_scan() {
        assert!(plan_for(&[ev("/Users/me", MUST_SCAN_SUB_DIRS)]).is_err());
        assert!(plan_for(&[ev("/", MUST_SCAN_SUB_DIRS | USER_DROPPED)]).is_err());
        assert!(plan_for(&[ev("/Users/me/Documents/a.txt", IDS_WRAPPED)]).is_err());
        // Events outside the folder that don't ask for a rescan are ignored.
        assert_eq!(plan_for(&[ev("/Users/other/f", MODIFIED | FILE)]).unwrap(), (vec![], vec![]));
        // Lost events below a folder: walk that folder again.
        let (_, deep) = plan_for(&[ev("/Users/me/Documents", MUST_SCAN_SUB_DIRS | KERNEL_DROPPED)]).unwrap();
        assert_eq!(deep, ["/Users/me/Documents"]);
    }

    #[test]
    fn permission_changes_check_access() {
        let t = tree();
        let roots = vec!["/Users/me".to_string()];
        let sub = t.find("/Users/me/Documents/Sub").unwrap();
        let ch = plan(&t, &roots, &[], &[ev("/Users/me/Documents/Sub", INODE_META_MOD | DIR)], |n| n != sub, |_| true).unwrap();
        assert!(ch.deep.contains(&sub));
        assert!(plan(&t, &roots, &[], &[ev("/Users/me", CHANGE_OWNER | DIR)], |n| n != ROOT, |_| true).is_err());
    }

    #[test]
    fn mounts_inside_the_scan() {
        let vol = |point: &str, history: Option<u8>, local: bool, read_only: bool| Volume {
            point: point.into(),
            dev: 7,
            history: history.map(|h| [h; 16]),
            local,
            read_only,
            skip: false,
        };
        let root = [1u8; 16];
        let before = vec![
            vol("/Volumes/Ext", Some(2), true, false),
            vol("/System/Volumes/Data", Some(1), true, false),
            vol("/System/Volumes/VM", None, true, false),
            vol("/Volumes/Share", None, false, false),
            vol("/Volumes/Image", None, true, true),
            vol("/Volumes/Gone", Some(3), true, false),
        ];
        let mut now = before.clone();
        now.pop();
        now.push(vol("/Volumes/Plugged", Some(4), true, false));
        let (watch, reread) = compare_volumes(&before, &now, &root);
        assert_eq!(watch, ["/Volumes/Ext"]);
        assert_eq!(reread, ["/System/Volumes/VM", "/Volumes/Share", "/Volumes/Plugged", "/Volumes/Gone"]);
    }

    /// A throwaway folder in the temp folder, which is under `/var`, a
    /// symlink to `/private/var`: FSEvents reports the real path.
    fn fixture() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("heft-fsevents-{}-{nanos}", std::process::id()));
        for d in ["docs/deep/er/still/deeper", "old", "keep/inner", "gone/sub", "links", "swap", "locked"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join("docs/a.txt"), vec![1u8; 1000]).unwrap();
        std::fs::write(dir.join("docs/deep/er/still/deeper/leaf.bin"), vec![2u8; 2000]).unwrap();
        std::fs::write(dir.join("old/o.dat"), vec![3u8; 3000]).unwrap();
        std::fs::write(dir.join("keep/inner/k"), vec![4u8; 4000]).unwrap();
        std::fs::write(dir.join("gone/sub/g"), vec![5u8; 5000]).unwrap();
        std::fs::write(dir.join("links/first"), vec![6u8; 6000]).unwrap();
        std::fs::hard_link(dir.join("links/first"), dir.join("links/second")).unwrap();
        std::fs::write(dir.join("swap/s"), vec![7u8; 700]).unwrap();
        std::fs::write(dir.join("locked/l"), vec![8u8; 800]).unwrap();
        std::fs::write(dir.join(".hidden"), b"h").unwrap();
        std::os::unix::fs::symlink("docs", dir.join("to-docs")).unwrap();
        dir
    }

    /// Touch a marker last and wait until FSEvents has logged it, so every
    /// change before it is in the history too.
    fn settle(dir: &Path, n: u32) {
        let since = current_event_id();
        let marker = dir.join(format!(".marker-{n}"));
        std::fs::write(&marker, b"m").unwrap();
        assert!(wait_logged(&marker, since, Duration::from_secs(20)), "FSEvents didn't log {}", marker.display());
    }

    fn full(root: &str) -> Tree {
        walk::scan(root, &Progress::default()).unwrap()
    }

    fn updated(st: &mut FsState, tree: &Tree) -> Tree {
        match crate::scan::refresh(st, tree, &Progress::default()) {
            RefreshOutcome::Updated(t) => t,
            RefreshOutcome::Unchanged => panic!("refresh saw no changes"),
            RefreshOutcome::NeedFullScan(why) => panic!("refresh gave up: {why}"),
        }
    }

    #[test]
    fn refresh_matches_a_full_scan() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let dir = fixture();
        let root = crate::scan::normalize_root(&dir.to_string_lossy());
        if let Some(why) = unavailable_reason(&root) {
            eprintln!("skipped, no quick rescans in the temp folder: {why}");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        settle(&dir, 0);
        let (tree, st) = scan(&root, &Progress::default()).unwrap();
        let mut st = st.expect("the temp folder is on a disk with an event history");
        assert_eq!(differences(&tree, &full(&root), 20), Vec::<String>::new());

        // New, removed, resized, renamed and deep changes.
        fs::write(dir.join("docs/new.txt"), vec![9u8; 900]).unwrap();
        fs::write(dir.join("docs/a.txt"), vec![1u8; 50_000]).unwrap();
        fs::write(dir.join("docs/deep/er/still/deeper/leaf.bin"), vec![2u8; 20]).unwrap();
        fs::create_dir_all(dir.join("docs/deep/er/still/deeper/newest/x")).unwrap();
        fs::write(dir.join("docs/deep/er/still/deeper/newest/x/y"), vec![3u8; 333]).unwrap();
        fs::remove_dir_all(dir.join("gone")).unwrap();
        fs::rename(dir.join("old"), dir.join("docs/renamed")).unwrap();
        fs::create_dir_all(dir.join("fresh/one/two")).unwrap();
        fs::write(dir.join("fresh/one/two/f"), vec![4u8; 4444]).unwrap();
        // The name carrying the hard link's bytes goes away.
        fs::remove_file(dir.join("links/first")).unwrap();
        fs::hard_link(dir.join("links/second"), dir.join("docs/third")).unwrap();
        // A different folder under the same name.
        fs::rename(dir.join("swap"), dir.join("swap-was")).unwrap();
        fs::create_dir(dir.join("swap")).unwrap();
        fs::write(dir.join("swap/other"), vec![5u8; 55]).unwrap();
        fs::remove_dir_all(dir.join("swap-was")).unwrap();
        fs::set_permissions(dir.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        settle(&dir, 1);

        let t1 = updated(&mut st, &tree);
        drop(tree);
        assert_eq!(differences(&t1, &full(&root), 20), Vec::<String>::new());
        assert_eq!(t1.info.unreadable_dirs, 1);
        let at = |t: &Tree, p: &str| t.find(&format!("{root}/{p}")).map(|id| t.node(id).size);
        assert_eq!(at(&t1, "docs/renamed/o.dat"), Some(3000));
        assert_eq!(at(&t1, "gone"), None);
        assert_eq!(at(&t1, "swap"), Some(55));

        // And back: readable again, more removals, and one Heft moved away
        // itself, which it takes out of its tree at once.
        let mut t1 = t1;
        let renamed = t1.find(&format!("{root}/docs/renamed")).unwrap();
        fs::rename(dir.join("docs/renamed"), dir.with_extension("trashed")).unwrap();
        t1.remove(renamed);
        fs::set_permissions(dir.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(dir.join("docs/deep")).unwrap();
        fs::remove_file(dir.join("links/second")).unwrap();
        fs::write(dir.join("keep/inner/k"), vec![4u8; 1]).unwrap();
        settle(&dir, 2);
        let t2 = updated(&mut st, &t1);
        assert_eq!(differences(&t2, &full(&root), 20), Vec::<String>::new());
        assert_eq!(t2.info.unreadable_dirs, 0);
        assert_eq!(at(&t2, "locked/l"), Some(800));
        assert_eq!(at(&t2, "docs/third"), Some(6000));

        // Nothing since: nothing to do (or, if something wrote there after
        // all, still the same tree as a full scan).
        match crate::scan::refresh(&mut st, &t2, &Progress::default()) {
            RefreshOutcome::Unchanged => {}
            RefreshOutcome::Updated(t) => assert_eq!(differences(&t, &full(&root), 20), Vec::<String>::new()),
            RefreshOutcome::NeedFullScan(why) => panic!("refresh gave up: {why}"),
        }

        // The folder itself replaced: only a full scan will do.
        fs::rename(&dir, dir.with_extension("moved")).unwrap();
        fs::create_dir(&dir).unwrap();
        assert!(matches!(crate::scan::refresh(&mut st, &t2, &Progress::default()), RefreshOutcome::NeedFullScan(_)));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(dir.with_extension("moved"));
        let _ = fs::remove_dir_all(dir.with_extension("trashed"));
    }

    /// A scan of the whole disk: the data volume shares the startup disk's
    /// history, and `/dev` isn't scanned at all.
    #[test]
    fn whole_disk_mounts() {
        let roots = vec!["/".to_string()];
        let now = volumes_under(&roots);
        for v in &now {
            eprintln!("{v:?}");
        }
        let (_, history) = tracking("/").unwrap();
        let (watch, reread) = compare_volumes(&now, &now, &history);
        eprintln!("watch {watch:?}\nreread {reread:?}");
        if let Some(data) = now.iter().find(|v| v.point == DATA_VOLUME) {
            assert_eq!(data.history, Some(history));
        }
        assert!(!watch.iter().chain(&reread).any(|p| p == DATA_VOLUME || p == "/dev"));
    }

    #[test]
    fn differences_are_reported() {
        let a = tree();
        let mut b = TreeBuilder::new("/Users/me");
        let docs = b.add(ROOT, "Documents", flags::DIR, 0, 0, 0);
        b.add(docs, "a.txt", 0, 11, 10, 0);
        b.add(docs, "c.txt", 0, 1, 1, 0);
        let b = b.finish("/Users/me".into(), a.info.clone());
        let d = differences(&a, &b, 10);
        assert!(d.iter().any(|l| l.contains("a.txt") && l.contains("size")), "{d:?}");
        assert!(d.iter().any(|l| l.starts_with("only in the first") && l.ends_with("Sub")), "{d:?}");
        assert!(d.iter().any(|l| l.starts_with("only in the second") && l.ends_with("c.txt")), "{d:?}");
        assert!(!d.iter().any(|l| l.starts_with("/Users/me/Documents:")), "folder totals are explained below: {d:?}");
    }
}
