//! Parsers for the output of macOS and Linux cleanup tools. Kept free of
//! OS calls so they're tested on every platform.

/// "67.4MB", "1.1 GB", "512 kB", "3 bytes" → bytes. `si` picks 1000-based
/// units (flatpak) over 1024-based ones (Homebrew).
pub fn size(text: &str, si: bool) -> Option<u64> {
    let t = text.trim().replace(',', "");
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().ok()?;
    let base: f64 = if si { 1000.0 } else { 1024.0 };
    let pow = match unit.trim().to_ascii_lowercase().as_str() {
        "b" | "byte" | "bytes" => 0,
        "kb" | "k" | "kib" => 1,
        "mb" | "m" | "mib" => 2,
        "gb" | "g" | "gib" => 3,
        "tb" | "t" | "tib" => 4,
        _ => return None,
    };
    Some((n * base.powi(pow)) as u64)
}

/// `snap list --all` → (name, revision) of disabled (superseded) revisions.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn snap_disabled(list: &str) -> Vec<(String, String)> {
    list.lines()
        .skip_while(|l| !l.starts_with("Name"))
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let notes = f.last()?;
            (f.len() >= 4 && notes.split(',').any(|n| n == "disabled")).then(|| (f[0].to_string(), f[2].to_string()))
        })
        .collect()
}

/// `brew cleanup --dry-run` → (bytes it would free, entries it would remove).
pub fn brew_dry_run(out: &str) -> (u64, u64) {
    let items = out.lines().filter(|l| l.starts_with("Would remove")).count() as u64;
    let bytes = out
        .lines()
        .find_map(|l| {
            let rest = l.split("approximately ").nth(1)?;
            size(rest.split(" of disk space").next()?, false)
        })
        .unwrap_or(0);
    (bytes, items)
}

/// `xcrun simctl list devices unavailable --json` → device UDIDs.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn simctl_udids(json: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = json;
    while let Some(i) = rest.find("\"udid\"") {
        rest = &rest[i + 6..];
        let Some(open) = rest.find('"') else { break };
        let value = &rest[open + 1..];
        let Some(close) = value.find('"') else { break };
        let id = &value[..close];
        if !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            out.push(id.to_string());
        }
        rest = &value[close + 1..];
    }
    out
}

/// A row of `flatpak list --columns=ref,size`: tab-separated when piped.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn flatpak_row(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    match line.split_once('\t') {
        Some((r, s)) => Some((r.trim(), s.trim())),
        None => line.split_once(char::is_whitespace).map(|(r, s)| (r, s.trim())),
    }
}

/// Estimate what `flatpak uninstall --unused` removes: runtimes that no
/// installed app uses, directly or as an extension of its runtime
/// (`org.freedesktop.Platform.GL.default` belongs to `org.freedesktop.Platform`).
/// Only an estimate; the removal itself is flatpak's own logic.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn flatpak_unused(runtimes: &str, app_runtimes: &str) -> Vec<(String, u64)> {
    let used: Vec<(&str, &str)> = app_runtimes
        .lines()
        .filter_map(|l| {
            let mut p = l.trim().split('/');
            let id = p.next()?;
            let branch = p.nth(1).unwrap_or("");
            (!id.is_empty()).then_some((id, branch))
        })
        .collect();
    runtimes
        .lines()
        .filter_map(flatpak_row)
        .filter_map(|(r, s)| {
            let mut p = r.split('/');
            let (id, branch) = (p.next()?, p.nth(1)?);
            let in_use = used.iter().any(|&(uid, ubranch)| {
                (id == uid && branch == ubranch) || id.starts_with(&format!("{uid}."))
            });
            (!in_use).then(|| (r.to_string(), size(s, true).unwrap_or(0)))
        })
        .collect()
}

/// Archived systemd journal files (`system@….journal`, `user-1000@….journal~`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn is_archived_journal(name: &str) -> bool {
    name.contains('@') && (name.ends_with(".journal") || name.ends_with(".journal~"))
}

/// Quote an argument for `sh -c`.
pub fn shell_quote(arg: &str) -> String {
    if !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@".contains(c)) {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// One `sh` script for everything that needs root, so the password is asked
/// for once. Each job's commands run in order even if one fails; then the
/// script prints `heft-status <job> <status>`, where the status is that of
/// the job's last failing command, or 0.
pub fn root_script(jobs: &[Vec<Vec<String>>]) -> String {
    let mut parts = Vec::new();
    for (i, cmds) in jobs.iter().enumerate() {
        if cmds.is_empty() {
            continue;
        }
        parts.push("s=0".to_string());
        for c in cmds {
            let line = c.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
            parts.push(format!("{line} || s=$?"));
        }
        parts.push(format!("echo heft-status {i} $s"));
    }
    parts.join("; ")
}

/// The statuses `root_script` printed, by job. osascript turns line breaks
/// into carriage returns, so both count.
pub fn root_statuses(out: &str) -> std::collections::HashMap<usize, i32> {
    out.split(['\n', '\r'])
        .filter_map(|l| {
            let mut f = l.trim().strip_prefix("heft-status ")?.split_whitespace();
            Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?))
        })
        .collect()
}

/// A field of one line of `docker system df --format '{{json .}}'`. Values
/// are plain strings there, so no JSON parser is needed.
fn json_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = &line[line.find(&format!("\"{key}\":\""))? + key.len() + 4..];
    rest.split('"').next()
}

/// `docker system df --format '{{json .}}'` → (reclaimable bytes, unused
/// entries) of the build cache.
pub fn docker_build_cache(df: &str) -> (u64, u64) {
    let Some(line) = df.lines().find(|l| json_field(l, "Type") == Some("Build Cache")) else { return (0, 0) };
    // "1.2GB (50%)": Docker's sizes are 1000-based.
    let bytes = json_field(line, "Reclaimable")
        .and_then(|r| size(r.split(" (").next().unwrap_or(r), true))
        .unwrap_or(0);
    let count = |k| json_field(line, k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let unused = count("TotalCount").saturating_sub(count("Active"));
    (bytes, if bytes > 0 { unused.max(1) } else { unused })
}

/// `docker images --format '{{.Size}}'` → (total bytes, images).
pub fn docker_image_sizes(list: &str) -> (u64, u64) {
    list.lines().filter_map(|l| size(l, true)).fold((0, 0), |(b, n), s| (b + s, n + 1))
}

/// `tmutil listlocalsnapshots /` → how many local Time Machine snapshots.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn tm_snapshots(out: &str) -> usize {
    out.lines().filter(|l| l.trim().starts_with("com.apple.TimeMachine.")).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(size("67.4MB", false), Some((67.4 * 1024.0 * 1024.0) as u64));
        assert_eq!(size("1.1 GB", true), Some(1_100_000_000));
        assert_eq!(size("512 kB", true), Some(512_000));
        assert_eq!(size("3 bytes", true), Some(3));
        assert_eq!(size("1,024KB", false), Some(1024 * 1024));
        assert_eq!(size("lots", true), None);
    }

    #[test]
    fn snap() {
        let text = "\
Name      Version   Rev    Tracking         Publisher    Notes
core20    20230801  2015   latest/stable    canonical**  base,disabled
core20    20230908  2105   latest/stable    canonical**  base
firefox   118.0.1   3206   latest/stable/…  mozilla**    disabled
snapd     2.60      20092  latest/stable    canonical**  snapd
";
        assert_eq!(snap_disabled(text), [("core20".into(), "2015".into()), ("firefox".into(), "3206".into())]);
        assert!(snap_disabled("error: cannot communicate with server").is_empty());
    }

    #[test]
    fn brew() {
        let out = "\
Would remove: /Users/x/Library/Caches/Homebrew/wget--1.21.bottle.tar.gz (1.3MB)
Would remove: /usr/local/Cellar/python@3.11/3.11.4 (3,262 files, 62.5MB)
==> This operation would free approximately 67.4MB of disk space.
";
        assert_eq!(brew_dry_run(out), ((67.4 * 1024.0 * 1024.0) as u64, 2));
        assert_eq!(brew_dry_run(""), (0, 0));
    }

    #[test]
    fn simctl() {
        let json = r#"{ "devices" : { "com.apple.CoreSimulator.SimRuntime.iOS-16-0" : [
          { "dataPath" : "/x", "udid" : "8A1B2C3D-0000-4E5F-9A8B-112233445566", "isAvailable" : false, "name" : "iPhone 14" },
          { "udid" : "BAD\" }", "name" : "odd" } ] } }"#;
        assert_eq!(simctl_udids(json), ["8A1B2C3D-0000-4E5F-9A8B-112233445566"]);
    }

    #[test]
    fn flatpak() {
        let runtimes = "\
org.freedesktop.Platform/x86_64/23.08\t206.1 MB
org.freedesktop.Platform.GL.default/x86_64/23.08\t453.9 MB
org.freedesktop.Platform/x86_64/22.08\t190.0 MB
org.gnome.Sdk/x86_64/45\t1.2 GB
org.gnome.Platform/x86_64/45\t1.1 GB
";
        let apps = "org.gnome.Platform/x86_64/45\norg.freedesktop.Platform/x86_64/23.08\n";
        let unused = flatpak_unused(runtimes, apps);
        assert_eq!(
            unused,
            [("org.freedesktop.Platform/x86_64/22.08".to_string(), 190_000_000), ("org.gnome.Sdk/x86_64/45".to_string(), 1_200_000_000)]
        );
    }

    #[test]
    fn journals_and_quoting() {
        assert!(is_archived_journal("system@0005f2a1b2c3d4e5-1a2b3c4d5e6f7a8b.journal"));
        assert!(is_archived_journal("user-1000@abc.journal~"));
        assert!(!is_archived_journal("system.journal"));
        assert_eq!(shell_quote("apt-get"), "apt-get");
        assert_eq!(shell_quote("--revision=12"), "--revision=12");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("a b"), "'a b'");
    }

    #[test]
    fn root_scripts() {
        let v = |c: &[&str]| c.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let jobs = vec![
            vec![v(&["/usr/bin/dscacheutil", "-flushcache"]), v(&["/usr/bin/killall", "-HUP", "mDNSResponder"])],
            vec![],
            vec![v(&["/bin/rm", "-f", "--", "/Library/Logs/old one.log"])],
        ];
        assert_eq!(
            root_script(&jobs),
            "s=0; /usr/bin/dscacheutil -flushcache || s=$?; /usr/bin/killall -HUP mDNSResponder || s=$?; echo heft-status 0 $s; \
             s=0; /bin/rm -f -- '/Library/Logs/old one.log' || s=$?; echo heft-status 2 $s"
        );
        let st = root_statuses("heft-status 0 0\rrm: x: Operation not permitted\rheft-status 2 1\r");
        assert_eq!(st.get(&0), Some(&0));
        assert_eq!(st.get(&2), Some(&1));
        assert_eq!(st.len(), 2);
        assert!(root_statuses("heft-status x 0\nheft-status 1\n").is_empty());
    }

    #[test]
    fn docker() {
        let df = r#"{"Active":"2","Reclaimable":"1.2GB (50%)","Size":"2.4GB","TotalCount":"5","Type":"Images"}
{"Active":"0","Reclaimable":"0B","Size":"0B","TotalCount":"0","Type":"Containers"}
{"Active":"0","Reclaimable":"0B","Size":"0B","TotalCount":"0","Type":"Local Volumes"}
{"Active":"3","Reclaimable":"512.5MB","Size":"600MB","TotalCount":"31","Type":"Build Cache"}
"#;
        assert_eq!(docker_build_cache(df), (512_500_000, 28));
        assert_eq!(docker_build_cache(""), (0, 0));
        assert_eq!(docker_build_cache(r#"{"Active":"0","Reclaimable":"0B","Size":"0B","TotalCount":"0","Type":"Build Cache"}"#), (0, 0));
        assert_eq!(docker_image_sizes("1.2GB\n512kB\n\n"), (1_200_512_000, 2));
    }

    #[test]
    fn time_machine() {
        let out = "Snapshots for disk /:\ncom.apple.TimeMachine.2026-09-29-101010.local\ncom.apple.TimeMachine.2026-09-29-111010.local\n";
        assert_eq!(tm_snapshots(out), 2);
        assert_eq!(tm_snapshots("Snapshots for disk /:\n"), 0);
    }
}
