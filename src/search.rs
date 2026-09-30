//! Search the whole scan by name, size, age and kind.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::tree::{flags, NodeId, Tree};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Any,
    Files,
    Folders,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Query {
    /// Substring, `*`/`?` wildcard pattern, or `.ext`. Case-insensitive.
    pub text: String,
    pub min_size: u64,
    /// Only items modified at least this many days ago.
    pub older_than_days: u32,
    /// Only items modified within this many days (0 = no limit).
    pub newer_than_days: u32,
    pub kind: Kind,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.min_size == 0 && self.older_than_days == 0 && self.newer_than_days == 0
    }
}

enum Pattern {
    Any,
    Ext(String),
    Glob(Vec<char>),
    Contains(String),
}

fn pattern(text: &str) -> Pattern {
    let t = text.trim().to_lowercase();
    if t.is_empty() {
        Pattern::Any
    } else if let Some(ext) = t.strip_prefix("*.").or_else(|| t.strip_prefix('.')).filter(|e| !e.contains(['*', '?', '.'])) {
        Pattern::Ext(ext.to_string())
    } else if t.contains(['*', '?']) {
        Pattern::Glob(t.chars().collect())
    } else {
        Pattern::Contains(t)
    }
}

/// `*` matches any run of characters, `?` exactly one. `pat` is lowercase.
fn glob(pat: &[char], name: &str) -> bool {
    let s: Vec<char> = name.to_lowercase().chars().collect();
    let (mut p, mut i) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while i < s.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == s[i]) {
            p += 1;
            i += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            mark = i;
            p += 1;
        } else if let Some(sp) = star {
            p = sp + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    pat[p..].iter().all(|&c| c == '*')
}

/// Matching items under `root`, biggest first, at most `limit`.
pub fn run(tree: &Tree, root: NodeId, q: &Query, now: i64, limit: usize) -> Vec<NodeId> {
    let pat = pattern(&q.text);
    let ext_id = match &pat {
        Pattern::Ext(e) => match tree.exts.iter().position(|x| &x.name == e) {
            Some(i) => Some(i),
            None => return Vec::new(),
        },
        _ => None,
    };
    let older = (q.older_than_days > 0).then(|| now - q.older_than_days as i64 * 86_400);
    let newer = (q.newer_than_days > 0).then(|| now - q.newer_than_days as i64 * 86_400);

    let matches = |id: NodeId| {
        let n = tree.node(id);
        if n.flags & flags::DELETED != 0 || n.size < q.min_size {
            return false;
        }
        match q.kind {
            Kind::Files if n.is_dir() => return false,
            Kind::Folders if !n.is_dir() => return false,
            _ => {}
        }
        if older.is_some_and(|t| n.mtime > t) || newer.is_some_and(|t| n.mtime < t) {
            return false;
        }
        match &pat {
            Pattern::Any => true,
            Pattern::Ext(_) => !n.is_dir() && Some(n.ext as usize) == ext_id,
            Pattern::Glob(g) => glob(g, tree.name(id)),
            Pattern::Contains(t) => contains(tree.name(id), t),
        }
    };

    let mut heap: BinaryHeap<Reverse<(u64, NodeId)>> = BinaryHeap::with_capacity(limit + 1);
    let mut stack: Vec<NodeId> = tree.children(root).to_vec();
    while let Some(id) = stack.pop() {
        let n = tree.node(id);
        // Children are sorted biggest first, so a full heap lets us skip the
        // rest of a folder once its items get too small to matter.
        if heap.len() >= limit && heap.peek().is_some_and(|m| n.size <= m.0.0) {
            continue;
        }
        if matches(id) {
            heap.push(Reverse((n.size, id)));
            if heap.len() > limit {
                heap.pop();
            }
        }
        if n.is_dir() {
            stack.extend_from_slice(tree.children(id));
        }
    }
    let mut v: Vec<(u64, NodeId)> = heap.into_iter().map(|r| r.0).collect();
    v.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    v.into_iter().map(|(_, id)| id).collect()
}

fn contains(hay: &str, needle: &str) -> bool {
    if hay.is_ascii() && needle.is_ascii() {
        let (h, n) = (hay.as_bytes(), needle.as_bytes());
        return h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n));
    }
    hay.to_lowercase().contains(needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::tests::root;
    use crate::tree::{ScanInfo, ScanMode, TreeBuilder, ROOT};

    fn tree() -> Tree {
        let mut b = TreeBuilder::new(root());
        let photos = b.add(ROOT, "Photos", flags::DIR, 0, 0, 0);
        b.add(photos, "IMG_0001.jpg", 0, 3_000, 0, 1_000);
        b.add(photos, "IMG_0002.JPG", 0, 5_000, 0, 9_000_000);
        b.add(photos, "notes.txt", 0, 10, 0, 9_000_000);
        let backup = b.add(ROOT, "Photo backup", flags::DIR, 0, 0, 0);
        b.add(backup, "archive.zip", 0, 90_000, 0, 1_000);
        let info = ScanInfo { mode: ScanMode::Walk, duration_ms: 0, finished_at: 0, unreadable_dirs: 0, note: None, phases: Vec::new() };
        b.finish(root().into(), info)
    }

    fn names(t: &Tree, q: Query) -> Vec<String> {
        run(t, ROOT, &q, 10_000_000, 100).into_iter().map(|id| t.name(id).to_string()).collect()
    }

    #[test]
    fn globs() {
        let g: Vec<char> = "img_*.jp?".chars().collect();
        assert!(glob(&g, "IMG_0001.jpg"));
        assert!(!glob(&g, "IMG_0001.jpeg"));
        assert!(glob(&['*'], ""));
        assert!(!glob(&['a', '?'], "a"));
    }

    #[test]
    fn queries() {
        let t = tree();
        assert_eq!(names(&t, Query { text: ".jpg".into(), ..Default::default() }), ["IMG_0002.JPG", "IMG_0001.jpg"]);
        assert_eq!(names(&t, Query { text: "photo".into(), kind: Kind::Folders, ..Default::default() }), ["Photo backup", "Photos"]);
        assert_eq!(names(&t, Query { text: "img_*".into(), min_size: 4_000, ..Default::default() }), ["IMG_0002.JPG"]);
        // Modified at t=1000, "now" is 10,000,000 s later (115 days): older than 100 days.
        assert_eq!(
            names(&t, Query { older_than_days: 100, kind: Kind::Files, ..Default::default() }),
            ["archive.zip", "IMG_0001.jpg"]
        );
        assert_eq!(names(&t, Query { newer_than_days: 30, kind: Kind::Files, ..Default::default() }), ["IMG_0002.JPG", "notes.txt"]);
        assert!(names(&t, Query { text: ".nope".into(), ..Default::default() }).is_empty());
    }
}
