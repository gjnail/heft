//! Headless commands, handy for benchmarking and verifying the scanners.
//!
//!   heft --bench <path> [--walk] [--json] [--out <file>]
//!   heft --export <path> <out.csv|out.json> [--folders] [--walk]
//!   heft --compare <path> [--out <file>]      MFT vs standard scan, side by side
//!   heft --render <path> <out.png> [--size WxH] [--mode type|category|age] [--walk]
//!   heft --icon <out.png> [--size N]            app icon, for packaging
//!   heft --clean [--dry-run] [--out <file>]     junk cleaner with the saved selection
//!   heft --check-refresh <drive>                 test change-journal rescans (Windows, admin)
//!   heft --sensors [--rounds N]                 read every hardware sensor and print them (Windows, Linux)

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
    matches!(arg, "--bench" | "--export" | "--compare" | "--render" | "--icon" | "--help" | "--clean" | "--check-refresh")
        || (cfg!(any(windows, target_os = "linux")) && arg == "--sensors")
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
            if matches!(a.as_str(), "--out" | "--size" | "--mode" | "--rounds") {
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
            Some(p) if flag("--json") => bench_json(p, !flag("--walk"), &mut out),
            Some(p) => bench(p, !flag("--walk"), &mut out),
            None => usage(&mut out),
        },
        "--export" => match (positional.first(), positional.get(1)) {
            (Some(p), Some(dest)) => export(p, dest, flag("--folders"), !flag("--walk"), &mut out),
            _ => usage(&mut out),
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
        "--clean" => clean(flag("--dry-run"), &mut out),
        "--check-refresh" => match positional.first() {
            Some(p) => check_refresh(p, &mut out),
            None => usage(&mut out),
        },
        #[cfg(any(windows, target_os = "linux"))]
        "--sensors" => {
            let rounds = value("--rounds").and_then(|s| s.parse().ok()).unwrap_or(3u32).clamp(1, 600);
            sensors(rounds, &mut out)
        }
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
         heft --bench <path> [--walk] [--json] [--out f]  scan and print a summary (JSON with --json)\n\
         heft --export <path> <out.csv|out.json> [--folders]  scan and export every item as CSV, or a JSON summary\n\
         heft --compare <path> [--out f]             MFT vs standard scan, side by side\n\
         heft --render <path> <out.png> [--size WxH] [--mode type|category|age] [--walk]\n\
         heft --icon <out.png> [--size N]            write the app icon (for packaging)\n",
    );
    out.push_str(
        "heft --clean [--dry-run] [--out f]          run the junk cleaner with the selection saved in the GUI\n\
         heft --check-refresh <drive> [--out f]      test change-journal rescans (Windows, as administrator)\n",
    );
    #[cfg(any(windows, target_os = "linux"))]
    out.push_str("heft --sensors [--rounds N] [--out f]       read every hardware sensor and print them\n");
    ExitCode::FAILURE
}

/// Every sensor after a few rounds (rates and power need two readings).
#[cfg(any(windows, target_os = "linux"))]
fn sensors(rounds: u32, out: &mut String) -> ExitCode {
    use crate::sensors::{self, Driver, Kind};
    let snap = sensors::collect(rounds, std::time::Duration::from_secs(1));
    let driver = match &snap.driver {
        Driver::NotNeeded => "not needed".to_string(),
        Driver::NotInstalled => "PawnIO not installed".to_string(),
        Driver::NeedsAdmin => "PawnIO installed, run as administrator to use it".to_string(),
        Driver::Active(uses) => format!("PawnIO: {}", uses.join(", ")),
        Driver::Failed(e) => e.clone(),
    };
    let _ = writeln!(out, "driver    {driver}");
    let _ = writeln!(out, "read in   {} ms per round\n", snap.sample_cost.as_millis());
    for d in &snap.devices {
        let detail = if d.detail.is_empty() { String::new() } else { format!("  {}", d.detail) };
        let _ = writeln!(out, "{}  [{}]{detail}", d.name, d.class.label());
        for kind in Kind::ALL {
            for s in d.of_kind(kind) {
                let v = s.value.map(|v| kind.format(v, false)).unwrap_or_else(|| "-".into());
                let _ = writeln!(
                    out,
                    "  {:<12} {:<26} {:>14}   min {:>14}  max {:>14}",
                    kind.group(),
                    s.label,
                    v,
                    kind.format(s.min, false),
                    kind.format(s.max, false)
                );
            }
        }
        let _ = writeln!(out);
    }
    for n in &snap.notes {
        let _ = writeln!(out, "note: {}", n.text);
    }
    ExitCode::SUCCESS
}

fn scan_blocking(path: &str, allow_mft: bool) -> Result<Tree, String> {
    let p = Progress::default();
    match scan::run(&scan::normalize_root(path), allow_mft, &p) {
        ScanOutcome::Done(t, _) => Ok(t),
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

fn bench_json(path: &str, allow_mft: bool, out: &mut String) -> ExitCode {
    match scan_blocking(path, allow_mft) {
        Ok(t) => {
            out.push_str(&crate::export::json_summary(&t, ROOT));
            ExitCode::SUCCESS
        }
        Err(e) => {
            let _ = writeln!(out, "{{\"error\": {}}}", crate::export::json_str(&e));
            ExitCode::FAILURE
        }
    }
}

/// Scan `path` and write every item as CSV, or a JSON summary if `dest` ends
/// in `.json`.
fn export(path: &str, dest: &str, folders_only: bool, allow_mft: bool, out: &mut String) -> ExitCode {
    let tree = match scan_blocking(path, allow_mft) {
        Ok(t) => t,
        Err(e) => {
            let _ = writeln!(out, "error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = std::fs::File::create(dest).and_then(|f| {
        let mut w = std::io::BufWriter::new(f);
        if dest.to_lowercase().ends_with(".json") {
            std::io::Write::write_all(&mut w, crate::export::json_summary(&tree, ROOT).as_bytes())?;
        } else {
            crate::export::csv(&tree, ROOT, folders_only, &mut w)?;
        }
        std::io::Write::flush(&mut w)
    });
    match result {
        Ok(()) => {
            let _ = writeln!(out, "wrote {dest}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            let _ = writeln!(out, "error writing {dest}: {e}");
            ExitCode::FAILURE
        }
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
            by_alloc: false,
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
fn clean(dry_run: bool, out: &mut String) -> ExitCode {
    use crate::clean::{self, rules};
    let t0 = Instant::now();
    let selected = clean::load_selection();
    let p = clean::Progress::default();
    let (tx, rx) = crossbeam_channel::unbounded();
    clean::analyze_all(selected.iter().copied().collect(), tx, &p);
    let mut found: Vec<clean::Found> = rx.try_iter().collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.bytes));

    let running = clean::running_processes();
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

/// Scan a drive, change some files in a scratch folder on it, bring the scan
/// up to date from the change journal, and compare with a fresh full scan.
/// Only the scratch folder it creates is touched, and it is removed after.
#[cfg(windows)]
fn check_refresh(path: &str, out: &mut String) -> ExitCode {
    use crate::scan::RefreshOutcome;
    use std::fs;

    let root = scan::normalize_root(path);
    let p = Progress::default();
    let (tree, state) = match scan::run(&root, true, &p) {
        ScanOutcome::Done(t, Some(s)) => (t, s),
        ScanOutcome::Done(..) => {
            let _ = writeln!(out, "no MFT scan (run as administrator on an NTFS drive)");
            return ExitCode::FAILURE;
        }
        ScanOutcome::Cancelled => return ExitCode::FAILURE,
        ScanOutcome::Failed(e) => {
            let _ = writeln!(out, "scan failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut state = *state;
    let _ = writeln!(out, "full scan   {} ({} files)", fmt_duration_ms(tree.info.duration_ms), fmt_count(tree.node(ROOT).files as u64));

    let temp = std::env::temp_dir();
    let base = if platform::names_eq(&scan::normalize_root(&temp.to_string_lossy())[..2], &root[..2]) {
        temp
    } else {
        std::path::PathBuf::from(&root)
    };
    let dir = base.join(format!("heft-refresh-check-{}", std::process::id()));
    let dir_s = dir.to_string_lossy().trim_end_matches('\\').to_string();
    let write = || -> std::io::Result<u64> {
        fs::create_dir_all(dir.join("sub").join("deeper"))?;
        let mut total = 0;
        for i in 0..40u64 {
            let len = 1000 + i * 777;
            fs::write(dir.join(format!("f{i}.bin")), vec![7u8; len as usize])?;
            total += len;
        }
        fs::write(dir.join("sub").join("deeper").join("big.bin"), vec![1u8; 5 << 20])?;
        total += 5 << 20;
        // Rename, delete and grow a few, so the journal has more than creates.
        fs::rename(dir.join("f1.bin"), dir.join("sub").join("moved.bin"))?;
        fs::remove_file(dir.join("f2.bin"))?;
        total -= 1000 + 2 * 777;
        let mut grown = fs::read(dir.join("f3.bin"))?;
        grown.extend(vec![9u8; 300_000]);
        fs::write(dir.join("f3.bin"), grown)?;
        total += 300_000;
        Ok(total)
    };
    let expect = match write() {
        Ok(n) => n,
        Err(e) => {
            let _ = fs::remove_dir_all(&dir);
            let _ = writeln!(out, "cannot write test files in {dir_s}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut ok = true;
    let check = |label: &str, t: &Tree, want: Option<(u64, u32)>, out: &mut String| {
        let got = t.find(&dir_s).map(|id| (t.node(id).size, t.node(id).files));
        let pass = got == want;
        let show = |v: Option<(u64, u32)>| v.map(|(s, f)| format!("{s} bytes, {f} files")).unwrap_or_else(|| "absent".into());
        let _ = writeln!(out, "{label:<12}{}  (expected {})  {}", show(got), show(want), if pass { "ok" } else { "MISMATCH" });
        pass
    };

    let t0 = Instant::now();
    let refreshed = scan::refresh(&mut state, &p);
    let refresh_ms = t0.elapsed().as_millis() as u64;
    match &refreshed {
        RefreshOutcome::Updated(t) => {
            let _ = writeln!(out, "refresh     {} ({})", fmt_duration_ms(refresh_ms), t.info.note.clone().unwrap_or_default());
            for (name, ms) in &t.info.phases {
                let _ = writeln!(out, "  phase     {name:<26}{}", fmt_duration_ms(*ms));
            }
            ok &= check("refreshed", t, Some((expect, 40)), out);
        }
        RefreshOutcome::Unchanged => {
            ok = false;
            let _ = writeln!(out, "refresh saw no changes: MISMATCH");
        }
        RefreshOutcome::NeedFullScan(why) => {
            ok = false;
            let _ = writeln!(out, "refresh gave up: {why}");
        }
    }
    if let (RefreshOutcome::Updated(t), Ok(full)) = (&refreshed, scan::mft_scan(&root, &p)) {
        ok &= check("full scan", &full, Some((expect, 40)), out);
        let (a, b) = (t.node(ROOT), full.node(ROOT));
        let _ = writeln!(
            out,
            "whole drive refreshed {} / {} files, full {} / {} files (other programs may have written in between)",
            fmt_size(a.size),
            fmt_count(a.files as u64),
            fmt_size(b.size),
            fmt_count(b.files as u64)
        );
    }

    let removed = fs::remove_dir_all(&dir);
    if let Err(e) = &removed {
        let _ = writeln!(out, "could not remove {dir_s}: {e}");
    }
    match scan::refresh(&mut state, &p) {
        RefreshOutcome::Updated(t) => ok &= check("after rm", &t, None, out),
        RefreshOutcome::Unchanged => {
            ok = false;
            let _ = writeln!(out, "second refresh saw no changes: MISMATCH");
        }
        RefreshOutcome::NeedFullScan(why) => {
            ok = false;
            let _ = writeln!(out, "second refresh gave up: {why}");
        }
    }
    let _ = writeln!(out, "{}", if ok { "PASS" } else { "FAIL" });
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

#[cfg(not(windows))]
fn check_refresh(_path: &str, out: &mut String) -> ExitCode {
    let _ = writeln!(out, "change-journal rescans are Windows only");
    ExitCode::FAILURE
}
