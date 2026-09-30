//! Exporting a scan: CSV for spreadsheets, JSON for scripts.

use std::fmt::Write as _;
use std::io::{self, Write};

use crate::platform;
use crate::tree::{flags, NodeId, Tree, ROOT};

/// One row per item under `root` (or per folder only), biggest first within
/// each folder.
pub fn csv(tree: &Tree, root: NodeId, folders_only: bool, out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "path,type,size,size_on_disk,files,modified,extension")?;
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let n = tree.node(id);
        if n.flags & flags::DELETED != 0 {
            continue;
        }
        if n.is_dir() || !folders_only {
            let kind = if n.is_dir() { "folder" } else { "file" };
            let ext = if n.is_dir() { "" } else { tree.ext_name(id) };
            writeln!(
                out,
                "{},{kind},{},{},{},{},{}",
                csv_field(&tree.path(id)),
                n.size,
                n.alloc,
                n.files,
                iso_date(n.mtime),
                csv_field(ext)
            )?;
        }
        if n.is_dir() {
            // Reversed so the biggest child comes out first.
            stack.extend(tree.children(id).iter().rev());
        }
    }
    Ok(())
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn iso_date(unix: i64) -> String {
    match platform::local_time(unix) {
        Some(t) => format!("{:04}-{:02}-{:02}T{:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute),
        None => String::new(),
    }
}

/// A summary of the scan for scripts: totals, the biggest children of `root`,
/// the largest files and the largest file types.
pub fn json_summary(tree: &Tree, root: NodeId) -> String {
    let n = tree.node(root);
    let mut s = String::from("{\n");
    let _ = writeln!(s, "  \"path\": {},", json_str(&tree.path(root)));
    let _ = writeln!(s, "  \"scanned_at\": {},", json_str(&iso_date(tree.info.finished_at)));
    let _ = writeln!(s, "  \"scan_mode\": {},", json_str(tree.info.mode.label()));
    let _ = writeln!(s, "  \"duration_ms\": {},", tree.info.duration_ms);
    let _ = writeln!(s, "  \"size\": {},", n.size);
    let _ = writeln!(s, "  \"size_on_disk\": {},", n.alloc);
    let _ = writeln!(s, "  \"files\": {},", n.files);
    if root == ROOT
        && let Some((total, free)) = platform::free_space(&tree.root_path)
    {
        let _ = writeln!(s, "  \"disk_total\": {total},");
        let _ = writeln!(s, "  \"disk_free\": {free},");
    }

    s.push_str("  \"children\": [");
    let kids: Vec<String> = tree
        .children(root)
        .iter()
        .take(50)
        .map(|&c| {
            let k = tree.node(c);
            format!(
                "\n    {{\"name\": {}, \"type\": \"{}\", \"size\": {}, \"size_on_disk\": {}, \"files\": {}}}",
                json_str(tree.name(c)),
                if k.is_dir() { "folder" } else { "file" },
                k.size,
                k.alloc,
                k.files
            )
        })
        .collect();
    s.push_str(&kids.join(","));
    s.push_str("\n  ],\n");

    s.push_str("  \"largest_files\": [");
    let files: Vec<String> = tree
        .largest_files(root, 50, |_, _| true)
        .into_iter()
        .map(|f| {
            let k = tree.node(f);
            format!(
                "\n    {{\"path\": {}, \"size\": {}, \"modified\": {}}}",
                json_str(&tree.path(f)),
                k.size,
                json_str(&iso_date(k.mtime))
            )
        })
        .collect();
    s.push_str(&files.join(","));
    s.push_str("\n  ],\n");

    let mut exts: Vec<_> = tree.exts.iter().filter(|e| e.count > 0).collect();
    exts.sort_by_key(|e| std::cmp::Reverse(e.size));
    s.push_str("  \"types\": [");
    let types: Vec<String> = exts
        .iter()
        .take(30)
        .map(|e| {
            format!(
                "\n    {{\"extension\": {}, \"category\": {}, \"size\": {}, \"files\": {}}}",
                json_str(&e.name),
                json_str(e.category.label()),
                e.size,
                e.count
            )
        })
        .collect();
    s.push_str(&types.join(","));
    s.push_str("\n  ]\n}\n");
    s
}

pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::tests::root;
    use crate::tree::{ScanInfo, ScanMode, TreeBuilder};

    fn tree() -> Tree {
        let mut b = TreeBuilder::new(root());
        let d = b.add(ROOT, "My, \"quoted\" folder", flags::DIR, 0, 0, 0);
        b.add(d, "a.txt", 0, 5, 4096, 0);
        b.add(ROOT, "big.bin", 0, 100, 4096, 0);
        let info = ScanInfo { mode: ScanMode::Walk, duration_ms: 7, finished_at: 0, unreadable_dirs: 0, note: None, phases: Vec::new() };
        b.finish(root().into(), info)
    }

    #[test]
    fn csv_escapes_and_orders() {
        let t = tree();
        let mut buf = Vec::new();
        csv(&t, ROOT, false, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 5);
        assert!(lines[2].contains("big.bin,file,100,4096,1,"), "{}", lines[2]);
        assert!(lines[3].starts_with('"') && lines[3].contains("\"\"quoted\"\""), "{}", lines[3]);

        let mut folders = Vec::new();
        csv(&t, ROOT, true, &mut folders).unwrap();
        assert_eq!(String::from_utf8(folders).unwrap().lines().count(), 3);
    }

    #[test]
    fn json_is_well_formed_enough() {
        let t = tree();
        let j = json_summary(&t, ROOT);
        assert!(j.contains("\"size\": 105"));
        assert!(j.contains("\"name\": \"My, \\\"quoted\\\" folder\""));
        assert_eq!(j.matches('{').count(), j.matches('}').count());
        assert_eq!(json_str("a\u{1}b"), "\"a\\u0001b\"");
    }
}
