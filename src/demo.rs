//! A made-up disk, for screenshots and for trying the UI without scanning
//! anything (`HEFT_DEMO=1 heft`). Every name here is invented, and the
//! generator is deterministic so screenshots are reproducible.

use crate::platform;
use crate::tree::{flags, NodeId, ScanInfo, ScanMode, Tree, TreeBuilder, ROOT};

const MB: u64 = 1 << 20;
const GB: u64 = 1 << 30;
const DAY: i64 = 86_400;

/// Small xorshift generator. Plenty for fake data, and no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo).max(1)
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[(self.next() % items.len() as u64) as usize]
    }
}

struct Gen {
    b: TreeBuilder,
    r: Rng,
    now: i64,
}

impl Gen {
    fn dir(&mut self, parent: NodeId, name: &str) -> NodeId {
        self.b.add(parent, name, flags::DIR, 0, 0, 0)
    }

    fn hidden_dir(&mut self, parent: NodeId, name: &str) -> NodeId {
        self.b.add(parent, name, flags::DIR | flags::HIDDEN, 0, 0, 0)
    }

    fn file(&mut self, parent: NodeId, name: &str, size: u64, max_age_days: u64) {
        let age = self.r.range(0, max_age_days.max(1)) as i64;
        let alloc = size.div_ceil(4096) * 4096;
        self.b.add(parent, name, 0, size, alloc, self.now - age * DAY - self.r.range(0, 86_000) as i64);
    }

    /// `count` files named by `name(i)` with sizes in `lo..hi`.
    fn files(&mut self, parent: NodeId, count: usize, name: impl Fn(usize, &mut Rng) -> String, lo: u64, hi: u64, age: u64) {
        for i in 0..count {
            let n = name(i, &mut self.r);
            let size = self.r.range(lo, hi);
            self.file(parent, &n, size, age);
        }
    }
}

const WORDS: [&str; 24] = [
    "Neon", "Harbor", "Glass", "Tiger", "Velvet", "Orbit", "Paper", "Lantern", "Echo", "Summit", "Copper", "Meadow",
    "Static", "Bloom", "Atlas", "Ember", "Silver", "Canyon", "Pulse", "Willow", "North", "Drift", "Coral", "Signal",
];

fn title(r: &mut Rng) -> String {
    format!("{} {}", r.pick(&WORDS), r.pick(&WORDS))
}

pub fn tree() -> Tree {
    let windows = cfg!(windows);
    let root_path = if windows { "C:\\" } else { "/" };
    let mut g = Gen { b: TreeBuilder::new(root_path), r: Rng(0x2545_F491_4F6C_DD1D), now: platform::now_unix() };

    let users = g.dir(ROOT, if windows { "Users" } else { "home" });
    let home = g.dir(users, "alex");

    // Videos: a few big projects and a lot of phone clips.
    let videos = g.dir(home, "Videos");
    for trip in ["2024 Iceland", "2025 Lisbon", "Family", "Screen recordings"] {
        let d = g.dir(videos, trip);
        let count = g.r.range(8, 30) as usize;
        g.files(d, count, |i, r| format!("clip_{i:03}.{}", r.pick(&["mp4", "mov", "mkv"])), 150 * MB, 3 * GB, 900);
    }
    let edits = g.dir(videos, "Edits");
    g.files(edits, 6, |i, _| format!("final_cut_v{}.mp4", i + 1), 2 * GB, 9 * GB, 400);

    // Pictures: camera roll + exports.
    let pictures = g.dir(home, "Pictures");
    for year in 2019..=2026 {
        let d = g.dir(pictures, &year.to_string());
        let age = ((2026 - year) * 365 + 60) as u64;
        let count = g.r.range(250, 700) as usize;
        g.files(d, count, |i, r| format!("IMG_{i:04}.{}", r.pick(&["jpg", "jpg", "heic", "png"])), 2 * MB, 9 * MB, age);
    }
    let raw = g.dir(pictures, "RAW");
    g.files(raw, 420, |i, r| format!("DSC_{i:04}.{}", r.pick(&["cr3", "nef", "arw"])), 20 * MB, 60 * MB, 1500);

    // Music: artist / album / tracks.
    let music = g.dir(home, "Music");
    for _ in 0..28 {
        let artist_name = title(&mut g.r);
        let artist = g.dir(music, &artist_name);
        for _ in 0..g.r.range(1, 4) {
            let album_name = title(&mut g.r);
            let album = g.dir(artist, &album_name);
            let ext = g.r.pick(&["flac", "mp3", "m4a"]);
            let (lo, hi) = if ext == "flac" { (20 * MB, 60 * MB) } else { (4 * MB, 12 * MB) };
            g.files(album, 12, |i, _| format!("{:02} Track.{ext}", i + 1), lo, hi, 2500);
        }
    }

    // Documents: work, taxes, and a web project with its node_modules.
    let docs = g.dir(home, "Documents");
    let work = g.dir(docs, "Work");
    g.files(work, 180, |i, r| format!("report-{i:03}.{}", r.pick(&["pdf", "docx", "xlsx", "pptx"])), 50 << 10, 30 * MB, 1200);
    let taxes = g.dir(docs, "Taxes");
    g.files(taxes, 40, |i, _| format!("statement-{i:02}.pdf"), 100 << 10, 4 * MB, 2500);
    let projects = g.dir(docs, "Projects");
    for p in ["portfolio-site", "budget-app", "game-jam"] {
        let proj = g.dir(projects, p);
        g.file(proj, "package.json", 2 << 10, 90);
        let src = g.dir(proj, "src");
        g.files(src, 60, |i, r| format!("module{i}.{}", r.pick(&["ts", "tsx", "css", "json"])), 1 << 10, 80 << 10, 90);
        let nm = g.dir(proj, "node_modules");
        for _ in 0..70 {
            let name = format!("{}-{}", g.r.pick(&WORDS), g.r.pick(&WORDS)).to_lowercase();
            let pkg = g.dir(nm, &name);
            let count = g.r.range(5, 60) as usize;
            g.files(pkg, count, |i, r| format!("file{i}.{}", r.pick(&["js", "js", "map", "json", "d.ts"])), 1 << 10, 400 << 10, 200);
        }
    }

    // Downloads: installers, disk images, archives.
    let dl = g.dir(home, "Downloads");
    let installers = if windows { ["exe", "msi", "zip"] } else { ["deb", "appimage", "zip"] };
    g.files(dl, 60, |i, r| format!("download-{i:02}.{}", r.pick(&installers)), 5 * MB, 900 * MB, 1400);
    g.files(dl, 5, |i, _| format!("os-image-{i}.iso"), 3 * GB, 7 * GB, 2000);

    // Caches: temp files, package caches, browser cache.
    let cache = if windows {
        let appdata = g.hidden_dir(home, "AppData");
        g.dir(appdata, "Local")
    } else {
        g.hidden_dir(home, ".cache")
    };
    let temp = g.dir(cache, "Temp");
    g.files(temp, 900, |i, r| format!("tmp{i:05}.{}", r.pick(&["tmp", "log", "dat"])), 4 << 10, 40 * MB, 300);
    for (name, count, lo, hi) in [("npm-cache", 1500, 10 << 10, 2 * MB), ("pip", 400, 50 << 10, 30 * MB), ("BrowserCache", 2500, 2 << 10, 900 << 10)] {
        let d = g.dir(cache, name);
        g.files(d, count, |i, _| format!("{i:08x}"), lo, hi, 120);
    }

    // Games: a few very large installs.
    let games = if windows { g.dir(ROOT, "Games") } else { g.dir(home, "Games") };
    for game in ["Starfall Arena", "Horizon Voyage", "Pixel Farm", "Deep Signal"] {
        let d = g.dir(games, game);
        let content = g.dir(d, "Content");
        let count = g.r.range(4, 14) as usize;
        g.files(content, count, |i, _| format!("data{i:02}.pak"), GB, 12 * GB, 500);
        g.files(d, 30, |i, r| format!("lib{i:02}.{}", r.pick(&["dll", "bin"])), MB, 80 * MB, 500);
    }

    // The operating system.
    if windows {
        let win = g.dir(ROOT, "Windows");
        let sxs = g.dir(win, "WinSxS");
        for _ in 0..300 {
            let name = format!("amd64_{}_{:x}", g.r.pick(&WORDS).to_lowercase(), g.r.next() & 0xFFFF_FFFF);
            let d = g.dir(sxs, &name);
            let count = g.r.range(1, 8) as usize;
            g.files(d, count, |i, r| format!("component{i}.{}", r.pick(&["dll", "dll", "cat", "manifest"])), 20 << 10, 15 * MB, 800);
        }
        let sys = g.dir(win, "System32");
        g.files(sys, 1800, |i, r| format!("sys{i:04}.{}", r.pick(&["dll", "exe", "mui", "sys"])), 10 << 10, 25 * MB, 700);
        let installer = g.dir(win, "Installer");
        g.files(installer, 120, |i, r| format!("{i:x}.{}", r.pick(&["msi", "msp"])), 5 * MB, 400 * MB, 1500);
        let pf = g.dir(ROOT, "Program Files");
        for app in ["Photo Studio", "Office Suite", "Code Editor", "Music Maker", "Browser"] {
            let d = g.dir(pf, app);
            let count = g.r.range(40, 200) as usize;
            g.files(d, count, |i, r| format!("part{i:03}.{}", r.pick(&["dll", "exe", "dat", "pak"])), 50 << 10, 200 * MB, 600);
        }
        g.file(ROOT, "pagefile.sys", 16 * GB, 1);
        g.file(ROOT, "hiberfil.sys", 12 * GB, 3);
    } else {
        let usr = g.dir(ROOT, "usr");
        for (name, count) in [("lib", 2500), ("bin", 900), ("share", 3000)] {
            let d = g.dir(usr, name);
            g.files(d, count, |i, r| format!("{name}{i:04}.{}", r.pick(&["so", "bin", "png", "txt"])), 4 << 10, 30 * MB, 700);
        }
        let var = g.dir(ROOT, "var");
        let log = g.dir(var, "log");
        g.files(log, 200, |i, _| format!("syslog.{i}.gz"), 100 << 10, 200 * MB, 120);
        g.file(ROOT, "swapfile", 8 * GB, 1);
    }

    let info = ScanInfo {
        mode: ScanMode::Walk,
        duration_ms: 0,
        finished_at: platform::now_unix(),
        unreadable_dirs: 0,
        note: Some("Demo data: made-up files, nothing on this computer was scanned.".into()),
        phases: Vec::new(),
    };
    g.b.finish(root_path.to_string(), info)
}

#[cfg(test)]
mod tests {
    #[test]
    fn demo_tree_is_big_and_deterministic() {
        let a = super::tree();
        let b = super::tree();
        assert!(a.node(crate::tree::ROOT).size > 200 * super::GB);
        assert!(a.len() > 20_000);
        assert_eq!(a.node(crate::tree::ROOT).size, b.node(crate::tree::ROOT).size);
    }
}
