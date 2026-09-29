//! Build junk: folders that build tools and package managers regenerate
//! (`node_modules`, Cargo `target`, game-engine caches and the like) found in
//! a scanned tree.
//!
//! A folder only counts when the project around it proves what it is (a
//! `package.json` next to `node_modules`, a `CACHEDIR.TAG` inside `target`,
//! a `.uproject` next to `Intermediate`…), so an unrelated folder that
//! happens to be called `build` or `Library` is never listed.

use crate::tree::{flags, NodeId, Tree};

#[derive(Clone, Debug)]
pub struct Junk {
    pub id: NodeId,
    pub kind: &'static str,
}

/// Siblings of a folder, by lower-case name.
fn sibling_names(tree: &Tree, id: NodeId) -> Vec<String> {
    let parent = tree.node(id).parent;
    tree.children(parent).iter().map(|&c| tree.name(c).to_lowercase()).collect()
}

fn has_child(tree: &Tree, id: NodeId, name: &str) -> bool {
    tree.children(id).iter().any(|&c| tree.name(c).eq_ignore_ascii_case(name))
}

/// What kind of build junk `id` is, if any.
pub fn classify(tree: &Tree, id: NodeId) -> Option<&'static str> {
    let name = tree.name(id).to_lowercase();
    let siblings = || sibling_names(tree, id);
    let sibling = |n: &str| siblings().iter().any(|s| s == n);
    let sibling_ext = |ext: &str| siblings().iter().any(|s| s.ends_with(ext));
    match name.as_str() {
        "node_modules" if sibling("package.json") => return Some("npm packages"),
        ".next" | ".nuxt" | ".turbo" | ".parcel-cache" | ".svelte-kit" | ".angular" | ".vite" if sibling("package.json") => {
            return Some("JavaScript build cache");
        }
        "library" if sibling("assets") && sibling("projectsettings") => return Some("Unity library cache"),
        "intermediate" | "deriveddatacache" if sibling_ext(".uproject") => return Some("Unreal build cache"),
        ".godot" if sibling("project.godot") => return Some("Godot import cache"),
        "bin" | "obj" if sibling_ext(".csproj") || sibling_ext(".fsproj") || sibling_ext(".vbproj") => {
            return Some(".NET build output");
        }
        "build" if sibling("build.gradle") || sibling("build.gradle.kts") => return Some("Gradle build output"),
        ".gradle" if sibling("settings.gradle") || sibling("settings.gradle.kts") || sibling("build.gradle") => {
            return Some("Gradle project cache");
        }
        ".tox" | ".nox" if sibling("tox.ini") || sibling("noxfile.py") || sibling("pyproject.toml") || sibling("setup.py") => {
            return Some("Python test environments");
        }
        _ => {}
    }
    // The cache-directory standard: tools mark folders that are safe to drop
    // (Cargo target, pytest, mypy, ruff, …).
    if has_child(tree, id, "CACHEDIR.TAG") {
        return Some(if name == "target" { "Rust build output" } else { "Tool cache" });
    }
    None
}

/// Every build-junk folder under `root`, largest first. Nested junk (a
/// `node_modules` inside another) is part of its outermost match.
pub fn find(tree: &Tree, root: NodeId) -> Vec<Junk> {
    let skip = flags::LINK | flags::DELETED | flags::MOUNT | flags::SEEN | flags::CLOUD;
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        for &c in tree.children(id) {
            let n = tree.node(c);
            if !n.is_dir() || n.flags & skip != 0 {
                continue;
            }
            match classify(tree, c) {
                Some(kind) if n.size > 0 => out.push(Junk { id: c, kind }),
                Some(_) => {}
                None => stack.push(c),
            }
        }
    }
    out.sort_by_key(|j| std::cmp::Reverse(tree.node(j.id).size));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{ScanInfo, ScanMode, TreeBuilder, ROOT};

    fn info() -> ScanInfo {
        ScanInfo { mode: ScanMode::Walk, duration_ms: 0, finished_at: 0, unreadable_dirs: 0, note: None, phases: Vec::new() }
    }

    #[test]
    fn finds_only_proven_junk() {
        let mut b = TreeBuilder::new("X:\\");
        let dir = flags::DIR;
        // A JS project: node_modules counts once, the nested one is part of it.
        let web = b.add(ROOT, "web", dir, 0, 0, 0);
        b.add(web, "package.json", 0, 1, 1, 0);
        let nm = b.add(web, "node_modules", dir, 0, 0, 0);
        let inner = b.add(nm, "left-pad", dir, 0, 0, 0);
        b.add(inner, "package.json", 0, 1, 1, 0);
        let nested = b.add(inner, "node_modules", dir, 0, 0, 0);
        b.add(nested, "x.js", 0, 500, 500, 0);
        b.add(nm, "index.js", 0, 1000, 1000, 0);
        // Rust: target with CACHEDIR.TAG.
        let rs = b.add(ROOT, "rs", dir, 0, 0, 0);
        let target = b.add(rs, "target", dir, 0, 0, 0);
        b.add(target, "CACHEDIR.TAG", 0, 177, 177, 0);
        b.add(target, "app.exe", 0, 4000, 4000, 0);
        // Look-alikes that must be ignored.
        let other = b.add(ROOT, "other", dir, 0, 0, 0);
        let nm2 = b.add(other, "node_modules", dir, 0, 0, 0); // no package.json
        b.add(nm2, "a", 0, 10, 10, 0);
        let lib = b.add(other, "Library", dir, 0, 0, 0); // not a Unity project
        b.add(lib, "b", 0, 10, 10, 0);
        let bld = b.add(other, "build", dir, 0, 0, 0);
        b.add(bld, "c", 0, 10, 10, 0);
        // Unity project.
        let game = b.add(ROOT, "game", dir, 0, 0, 0);
        b.add(game, "Assets", dir, 0, 0, 0);
        b.add(game, "ProjectSettings", dir, 0, 0, 0);
        let ulib = b.add(game, "Library", dir, 0, 0, 0);
        b.add(ulib, "ArtifactDB", 0, 2000, 2000, 0);
        let t = b.finish("X:\\".into(), info());

        let found = find(&t, ROOT);
        let names: Vec<(&str, &str, &str)> =
            found.iter().map(|j| (t.name(t.node(j.id).parent), t.name(j.id), j.kind)).collect();
        assert_eq!(
            names,
            [
                ("rs", "target", "Rust build output"),
                ("game", "Library", "Unity library cache"),
                ("web", "node_modules", "npm packages"),
            ]
        );
    }
}
