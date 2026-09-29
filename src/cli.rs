//! Headless commands, handy for benchmarking and verifying the scanners.
//!
//!   heft --bench <path> [--walk] [--out <file>]
//!   heft --compare <path> [--out <file>]      MFT vs standard scan, side by side
//!   heft --render <path> <out.png> [--size WxH] [--mode type|category|age] [--walk]
//!   heft --icon <out.png> [--size N]            app icon, for packaging
//!   heft --clean [--dry-run] [--out <file>]     junk cleaner with the saved selection (Windows)

use std::fmt::Write as _;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use crate::scan::{self, Progress, ScanOutcome};
use crate::treemap::{self, ColorMode, Highlight, Style};
use crate::tree::{NodeId, Tree, ROOT};
use crate::util::{fmt_count, fmt_duration_ms, fmt_size};
use crate::{colors, platform};

pub fn is_cli_command(arg: &str) -> bool {
    matches!(arg, "--bench" | "--compare" | "--render" | "--icon" | "--help") || (cfg!(windows) && arg == "--clean")
}

pub fn run(args: &[String]) -> ExitCode {
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let positional: Vec<&String> = {
        let mut v = Vec::new();
        let mut skip = false;
        for a in &args[1..] {
            if skip {
                skip = false;
                continue;
            }
            if matches!(a.as_str(), "--out" | "--size" | "--mode") {
                skip = true;
                continue;
            }
            if !a.starts_with("--") {
                v.push(a);
            }
        }
        v
    };

    let mut out = String::new();
    let code = match args[0].as_str() {
        "--bench" => match positional.first() {
            Some(p) => bench(p, !flag("--walk"), &mut out),
            None => usage(&mut out),
        },
        "--compare" => match positional.first() {
            Some(p) => compare(p, &mut out),
            None => usage(&mut out),
        },
        "--render" => match (positional.first(), positional.get(1)) {
            (Some(p), Some(png)) => render(p, png, value("--size"), value("--mode"), !flag("--walk"), &mut out),
            _ => usage(&mut out),
        },
        "--icon" => match positional.first() {
            Some(png) => {
                let size = value("--size").and_then(|s| s.parse().ok()).unwrap_or(256u32).clamp(16, 2048);
                match crate::icon::save_png(std::path::Path::new(png.as_str()), size) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        let _ = writeln!(out, "error: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
            None => usage(&mut out),
        },
        #[cfg(windows)]
        "--clean" => clean(flag("--dry-run"), &mut out),
        _ => usage(&mut out),
    };
    print!("{out}");
    if let Some(path) = value("--out") {
        let _ = std::fs::write(path, &out);
    }
    code
}

fn usage(out: &mut String) -> ExitCode {
    out.push_str(
        "Heft, a disk usage analyzer\n\n\
         heft [path]                                 open the GUI (optionally scanning path)\n\
         heft --bench <path> [--walk] [--out f]      scan and print a summary\n\
         heft --compare <path> [--out f]             MFT vs standard scan, side by side\n\
         heft --render <path> <out.png> [--size WxH] [--mode type|category|age] [--walk]\n\
         heft --icon <out.png> [--size N]            write the app icon (for packaging)\n",
    );
    #[cfg(windows)]
    out.push_str(
        "heft --clean [--dry-run] [--out f]          run the junk cleaner with the selection saved in the GUI\n",
    );
    ExitCode::FAILURE
}

fn scan_blocking(path: &str, allow_mft: bool) -> Result<Tree, String> {
    let p = Progress::default();
    match scan::run(&scan::normalize_root(path), allow_mft, &p) {
        ScanOutcome::Done(t) => Ok(t),
        ScanOutcome::Cancelled => Err("cancelled".into()),
        ScanOutcome::Failed(e) => Err(e),
    }
}

fn summarize(t: &Tree, out: &mut String) {
    let r = t.node(ROOT);
    let dirs = t.nodes.iter().filter(|n| n.is_dir()).count();
    let _ = writeln!(out, "root      {}", t.root_path);
    let _ = writeln!(out, "mode      {}", t.info.mode.label());
    let _ = writeln!(out, "time      {}", fmt_duration_ms(t.info.duration_ms));
    let _ = writeln!(out, "files     {}", fmt_count(r.files as u64));
    let _ = writeln!(out, "folders   {}", fmt_count(dirs as u64));
    let _ = writeln!(out, "size      {} ({} bytes)", fmt_size(r.size), r.size);
    let _ = writeln!(out, "on disk   {}", fmt_size(r.alloc));
    if t.info.unreadable_dirs > 0 {
        let _ = writeln!(out, "unreadable folders: {}", t.info.unreadable_dirs);
    }
    if let Some(n) = &t.info.note {
        let _ = writeln!(out, "note      {n}");
    }
    for (name, ms) in &t.info.phases {
        let _ = writeln!(out, "  phase   {name:<22}{}", fmt_duration_ms(*ms));
    }
    let _ = writeln!(out, "\nlargest children:");
    for &c in t.children(ROOT).iter().take(15) {
        let n = t.node(c);
        let _ = writeln!(out, "  {:>10}  {:>10} files  {}", fmt_size(n.size), fmt_count(n.files as u64), t.name(c));
    }
    let mut exts: Vec<_> = t.exts.iter().filter(|e| e.size > 0).collect();
    exts.sort_by(|a, b| b.size.cmp(&a.size));
    let _ = writeln!(out, "\nlargest types:");
    for e in exts.iter().take(10) {
        let name = if e.name.is_empty() { "(none)" } else { &e.name };
        let _ = writeln!(out, "  {:>10}  {:>10} files  .{}", fmt_size(e.size), fmt_count(e.count), name);
    }
}

fn bench(path: &str, allow_mft: bool, out: &mut String) -> ExitCode {
    let _ = writeln!(out, "elevated  {}", platform::is_elevated());
    match scan_blocking(path, allow_mft) {
        Ok(t) => {
            summarize(&t, out);
            ExitCode::SUCCESS
        }
        Err(e) => {
            let _ = writeln!(out, "error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn compare(path: &str, out: &mut String) -> ExitCode {
    let root = scan::normalize_root(path);
    if let Some(reason) = scan::mft_unavailable_reason(&root) {
        let _ = writeln!(out, "MFT scan unavailable: {reason}");
        return ExitCode::FAILURE;
    }
    let p = Progress::default();
    let t0 = Instant::now();
    let mft = match scan::mft_scan(&root, &p) {
        Ok(t) => t,
        Err(e) => {
            let _ = writeln!(out, "MFT scan failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mft_ms = t0.elapsed().as_millis() as u64;
    let t1 = Instant::now();
    let walk = match scan::walk::scan(&root, &p) {
        Ok(t) => t,
        Err(e) => {
            let _ = writeln!(out, "standard scan failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let walk_ms = t1.elapsed().as_millis() as u64;
    let (a, b) = (mft.node(ROOT), walk.node(ROOT));
    let _ = writeln!(out, "{:<12}{:>16}{:>16}", "", "MFT", "standard");
    let _ = writeln!(out, "{:<12}{:>16}{:>16}", "time", fmt_duration_ms(mft_ms), fmt_duration_ms(walk_ms));
    let _ = writeln!(out, "{:<12}{:>16}{:>16}", "files", fmt_count(a.files as u64), fmt_count(b.files as u64));
    let _ = writeln!(out, "{:<12}{:>16}{:>16}", "size", fmt_size(a.size), fmt_size(b.size));
    let _ = writeln!(out, "{:<12}{:>16}{:>16}", "on disk", fmt_size(a.alloc), fmt_size(b.alloc));
    let _ = writeln!(out, "speedup     {:.1}x", walk_ms as f64 / mft_ms.max(1) as f64);
    for (name, ms) in &mft.info.phases {
        let _ = writeln!(out, "  MFT phase  {name:<22}{}", fmt_duration_ms(*ms));
    }
    for (name, ms) in &walk.info.phases {
        let _ = writeln!(out, "  std phase  {name:<22}{}", fmt_duration_ms(*ms));
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "top-level folders (MFT vs standard):");
    for &c in mft.children(ROOT).iter().take(25) {
        let name = mft.name(c);
        let other = walk.find(&mft.path(c)).map(|id| walk.node(id).size);
        let _ = writeln!(
            out,
            "  {:>10} {:>10}  {}{}",
            fmt_size(mft.node(c).size),
            other.map(fmt_size).unwrap_or_else(|| "-".into()),
            name,
            match other {
                Some(o) if o.abs_diff(mft.node(c).size) > mft.node(c).size / 50 + (1 << 20) => "   <- differs",
                _ => "",
            }
        );
    }
    drill_down(&mft, &walk, out);
    ExitCode::SUCCESS
}

/// Follow the biggest size discrepancies between the two scans downwards.
fn drill_down(mft: &Tree, walk: &Tree, out: &mut String) {
    let size_in = |t: &Tree, path: &str| t.find(path).map(|id| t.node(id).size).unwrap_or(0);
    let diff = |id: NodeId| {
        let p = mft.path(id);
        (mft.node(id).size as i64 - size_in(walk, &p) as i64, p)
    };
    let mut tops: Vec<(i64, NodeId)> = mft.children(ROOT).iter().map(|&c| (diff(c).0, c)).collect();
    tops.sort_by_key(|t| std::cmp::Reverse(t.0.unsigned_abs()));
    let _ = writeln!(out, "\nbiggest discrepancies (MFT minus standard):");
    for &(d, top) in tops.iter().take(4) {
        if d.unsigned_abs() < 64 << 20 {
            break;
        }
        let mut cur = top;
        for depth in 0..12 {
            let (d, p) = diff(cur);
            let _ = writeln!(out, "  {}{:>+16}  {}", "  ".repeat(depth), d, p);
            let next = mft
                .children(cur)
                .iter()
                .map(|&c| (diff(c).0, c))
                .max_by_key(|t| t.0.unsigned_abs());
            match next {
                Some((cd, c)) if cd.unsigned_abs() * 2 >= d.unsigned_abs() && cd != 0 => cur = c,
                _ => break,
            }
        }
        // Items the standard scan has that the MFT scan lacks, in the last folder.
        if let Some(wid) = walk.find(&mft.path(cur)) {
            for &wc in walk.children(wid).iter().take(200) {
                if mft.find(&walk.path(wc)).is_none() && walk.node(wc).size > 1 << 20 {
                    let _ = writeln!(out, "      only in standard: {} ({})", walk.path(wc), fmt_size(walk.node(wc).size));
                }
            }
        }
    }
}

fn render(
    path: &str,
    png: &str,
    size: Option<String>,
    mode: Option<String>,
    allow_mft: bool,
    out: &mut String,
) -> ExitCode {
    let (w, h) = size
        .and_then(|s| s.split_once('x').map(|(a, b)| (a.parse().ok(), b.parse().ok())))
        .and_then(|(a, b)| Some((a?, b?)))
        .unwrap_or((1600usize, 1000usize));
    let mode = match mode.as_deref() {
        Some("category") => ColorMode::Category,
        Some("age") => ColorMode::Age,
        _ => ColorMode::Extension,
    };
    let tree = match scan_blocking(path, allow_mft) {
        Ok(t) => Arc::new(t),
        Err(e) => {
            let _ = writeln!(out, "error: {e}");
            return ExitCode::FAILURE;
        }
    };
    summarize(&tree, out);
    let t0 = Instant::now();
    let req = treemap::Request {
        seq: 1,
        tree: tree.clone(),
        root: ROOT,
        width: w,
        height: h,
        style: Style {
            mode,
            ext_colors: Arc::new(colors::extension_colors(&tree.exts)),
            now: platform::now_unix(),
            diff: None,
            highlight: Highlight::None,
        },
    };
    let r = treemap::render(&req);
    let _ = writeln!(out, "\nrendered {}x{} ({} rects) in {} ms", w, h, r.rects.len(), t0.elapsed().as_millis());
    match treemap::save_png(&r, std::path::Path::new(png)) {
        Ok(()) => {
            let _ = writeln!(out, "saved {png}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            let _ = writeln!(out, "error saving png: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Run the junk cleaner with the selection saved by the GUI. Used by the
/// weekly scheduled task; `--dry-run` only reports what would go.
#[cfg(windows)]
fn clean(dry_run: bool, out: &mut String) -> ExitCode {
    use crate::clean::{self, rules};
    let t0 = Instant::now();
    let selected = clean::load_selection();
    let p = clean::Progress::default();
    let (tx, rx) = crossbeam_channel::unbounded();
    clean::analyze_all(selected.iter().copied().collect(), tx, &p);
    let mut found: Vec<clean::Found> = rx.try_iter().collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.bytes));

    let running = crate::winsys::running_processes();
    let (mut freed, mut removed, mut skipped) = (0u64, 0u64, 0u64);
    for f in &found {
        let r = &rules::all()[f.rule];
        let name = format!("{} · {}", r.app, r.name);
        if f.items == 0 && !f.special_only() {
            continue;
        }
        if dry_run || !f.is_actionable() {
            let why = if f.needs_admin {
                "  (needs administrator)".to_string()
            } else if let Some(exe) = f.blocked_by {
                format!("  ({exe} is running)")
            } else {
                String::new()
            };
            let _ = writeln!(out, "{:>10}  {:>8} items  {name}{why}", fmt_size(f.bytes), fmt_count(f.items));
            continue;
        }
        let c = clean::clean(f, &running, &p);
        freed += c.freed;
        removed += c.removed;
        skipped += c.skipped;
        let note = c.note.map(|n| format!("  ({n})")).unwrap_or_default();
        let _ = writeln!(out, "{:>10}  {:>8} items  {name}{note}", fmt_size(c.freed), fmt_count(c.removed));
    }
    let took = fmt_duration_ms(t0.elapsed().as_millis() as u64);
    if dry_run {
        let total: u64 = found.iter().filter(|f| f.is_actionable()).map(|f| f.bytes).sum();
        let _ = writeln!(out, "\n{} can be freed (dry run, nothing deleted) · {took}", fmt_size(total));
    } else {
        let _ = writeln!(out, "\nfreed {} · {} items removed · {} in use and left alone · {took}", fmt_size(freed), fmt_count(removed), fmt_count(skipped));
    }
    ExitCode::SUCCESS
}
