//! Updates for apps Homebrew didn't install.
//!
//! - App Store apps, through the `mas` command-line tool when it's installed
//!   (`brew install mas`). Like Homebrew, it only runs when you check for
//!   updates, and upgrades run in a Terminal window.
//! - Apps with their own updater (Sparkle, which many Mac apps use, and
//!   Squirrel, which Electron apps use) update from inside the app, so Heft
//!   offers to open them.
//! - Only when that's turned on in Settings, Heft asks a Sparkle app's own
//!   update feed (the address inside the app) whether there's a newer
//!   version. That's the one time Heft goes online by itself besides
//!   Homebrew and `mas`, so it's off unless chosen.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::mac::{plist_str, read_plist, sh_quote};

/// Ask Sparkle apps' update feeds when checking for updates (a setting).
static CHECK_FEEDS: AtomicBool = AtomicBool::new(false);

pub fn set_check_feeds(on: bool) {
    CHECK_FEEDS.store(on, Ordering::Relaxed);
}

pub fn check_feeds() -> bool {
    CHECK_FEEDS.load(Ordering::Relaxed)
}

/// How an app updates itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Updater {
    /// Sparkle, with the feed it checks if the app names one.
    Sparkle { feed: Option<String> },
    /// Squirrel (Electron apps): checks for updates by itself as it runs.
    Squirrel,
}

impl Updater {
    pub fn label(&self) -> &'static str {
        "updates itself"
    }

    pub fn hover(&self) -> &'static str {
        match self {
            Updater::Sparkle { .. } => {
                "Has its own updater (Sparkle). Open it and use Check for Updates, usually in the app's menu, or let it update by itself."
            }
            Updater::Squirrel => "Has its own updater (Squirrel). It updates by itself while it's open; restart it to finish.",
        }
    }
}

/// The app's own updater, if it has one.
pub fn updater(app: &Path) -> Option<Updater> {
    let frameworks = app.join("Contents/Frameworks");
    if frameworks.join("Sparkle.framework").is_dir() {
        let feed = read_plist(&app.join("Contents/Info.plist"))
            .and_then(|v| v.as_dictionary().and_then(|d| plist_str(d, "SUFeedURL")))
            .filter(|f| f.starts_with("https://") || f.starts_with("http://"));
        return Some(Updater::Sparkle { feed });
    }
    frameworks.join("Squirrel.framework").is_dir().then_some(Updater::Squirrel)
}

/// Open an app so it can update itself.
pub fn open_to_update(app: &Path) -> Result<(), String> {
    let status = Command::new("/usr/bin/open").arg(app).status().map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err("it couldn't be opened".into()) }
}

// ----------------------------------------------------------------------
// App Store apps through mas

/// Where `mas` is, if it's installed (usually through Homebrew).
pub fn find_mas() -> Option<PathBuf> {
    ["/opt/homebrew/bin/mas", "/usr/local/bin/mas"].iter().map(PathBuf::from).find(|p| p.is_file())
}

/// An App Store app with an update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreUpdate {
    /// The App Store's id for the app.
    pub id: u64,
    pub name: String,
    pub installed: String,
    pub available: String,
}

/// `mas outdated`: lines like `497799835 Xcode (15.0 -> 15.1)`.
pub fn parse_mas_outdated(text: &str) -> Vec<StoreUpdate> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let (id, rest) = l.split_once(char::is_whitespace)?;
            let id = id.parse().ok()?;
            let open = rest.rfind('(')?;
            let versions = rest[open + 1..].strip_suffix(')')?;
            let (installed, available) = versions.split_once("->")?;
            Some(StoreUpdate {
                id,
                name: rest[..open].trim().to_string(),
                installed: installed.trim().to_string(),
                available: available.trim().to_string(),
            })
        })
        .collect()
}

/// Ask the App Store, through `mas`, which apps have updates.
pub fn mas_outdated(mas: &Path) -> Result<Vec<StoreUpdate>, String> {
    let out = Command::new(mas).arg("outdated").output().map_err(|e| format!("mas couldn't be started: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("mas failed").to_string());
    }
    Ok(parse_mas_outdated(&String::from_utf8_lossy(&out.stdout)))
}

/// Update one App Store app, or all of them with `None`, in Terminal.
pub fn mas_upgrade(mas: &Path, id: Option<u64>) -> Result<(), String> {
    let mut cmd = format!("{} upgrade", sh_quote(&mas.to_string_lossy()));
    if let Some(id) = id {
        cmd.push_str(&format!(" {id}"));
    }
    crate::mac::run_in_terminal("Heft - updating", &cmd)
}

// ----------------------------------------------------------------------
// Sparkle feeds (only when turned on)

/// A Sparkle app whose feed lists a newer version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedUpdate {
    pub app: PathBuf,
    pub name: String,
    pub installed: String,
    pub available: String,
}

/// The newest release in an appcast that this Mac can run: (build, version
/// shown to people). Items on a beta channel are skipped.
pub fn newest_in_appcast(xml: &str, macos: &str) -> Option<(String, Option<String>)> {
    let mut best: Option<(String, Option<String>)> = None;
    for item in xml.split("<item").skip(1) {
        let item = item.split("</item>").next().unwrap_or(item);
        if item.contains("<sparkle:channel>") {
            continue;
        }
        if let Some(min) = element(item, "sparkle:minimumSystemVersion")
            && compare(&min, macos) == std::cmp::Ordering::Greater
        {
            continue;
        }
        let Some(build) = attribute(item, "sparkle:version").or_else(|| element(item, "sparkle:version")) else { continue };
        let short = attribute(item, "sparkle:shortVersionString").or_else(|| element(item, "sparkle:shortVersionString"));
        if best.as_ref().is_none_or(|(b, _)| compare(&build, b) == std::cmp::Ordering::Greater) {
            best = Some((build, short));
        }
    }
    best
}

fn attribute(item: &str, name: &str) -> Option<String> {
    let at = item.find(&format!("{name}=\""))? + name.len() + 2;
    let end = item[at..].find('"')?;
    Some(item[at..at + end].trim().to_string()).filter(|v| !v.is_empty())
}

fn element(item: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let at = item.find(&open)? + open.len();
    let end = item[at..].find('<')?;
    Some(item[at..at + end].trim().to_string()).filter(|v| !v.is_empty())
}

/// Compare versions the way Sparkle does: runs of digits as numbers, other
/// runs as text, piece by piece (`1.10` is newer than `1.9`).
pub fn compare(a: &str, b: &str) -> std::cmp::Ordering {
    fn pieces(s: &str) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        let mut split = true;
        for c in s.chars() {
            if !c.is_ascii_alphanumeric() {
                split = true;
                continue;
            }
            let digit = c.is_ascii_digit();
            match out.last_mut() {
                Some((d, p)) if *d == digit && !split => p.push(c),
                _ => out.push((digit, c.to_string())),
            }
            split = false;
        }
        out
    }
    let (a, b) = (pieces(a), pieces(b));
    for i in 0..a.len().max(b.len()) {
        let ord = match (a.get(i), b.get(i)) {
            (Some((true, x)), Some((true, y))) => {
                let (x, y) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
                x.len().cmp(&y.len()).then_with(|| x.cmp(y))
            }
            (Some((_, x)), Some((_, y))) => x.cmp(y),
            // 1.0 and 1.0.0 are the same; 1.0 is older than 1.0.1.
            (Some((true, x)), None) => if x.trim_start_matches('0').is_empty() { std::cmp::Ordering::Equal } else { std::cmp::Ordering::Greater },
            (None, Some((true, y))) => if y.trim_start_matches('0').is_empty() { std::cmp::Ordering::Equal } else { std::cmp::Ordering::Less },
            // A letter after the number marks a pre-release (1.0b1 < 1.0).
            (Some((false, _)), None) => std::cmp::Ordering::Less,
            (None, Some((false, _))) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

/// This Mac's macOS version, like `26.1`.
fn macos_version() -> String {
    crate::mac::output("/usr/bin/sw_vers", &["-productVersion"]).map(|v| v.trim().to_string()).unwrap_or_default()
}

/// Download a feed with the system's `curl`: web addresses only, a size
/// limit and a time limit.
fn fetch(url: &str) -> Result<String, String> {
    let agent = format!("Heft/{}", env!("CARGO_PKG_VERSION"));
    let out = Command::new("/usr/bin/curl")
        .args(["--silent", "--show-error", "--fail", "--location", "--proto", "=https,http", "--proto-redir", "=https,http"])
        .args(["--max-time", "20", "--max-filesize", "5000000", "--user-agent", &agent, "--", url])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Ask each Sparkle app's feed for its newest version. `apps` are (bundle,
/// name, feed). Apps whose feed can't be read are left out.
pub fn check_feeds_of(apps: &[(PathBuf, String, String)]) -> Vec<FeedUpdate> {
    use rayon::prelude::*;
    let macos = macos_version();
    apps.par_iter()
        .filter_map(|(app, name, feed)| {
            let info = read_plist(&app.join("Contents/Info.plist"))?;
            let dict = info.as_dictionary()?;
            let build = plist_str(dict, "CFBundleVersion")?;
            let shown = plist_str(dict, "CFBundleShortVersionString").unwrap_or_else(|| build.clone());
            let (newest, newest_shown) = newest_in_appcast(&fetch(feed).ok()?, &macos)?;
            (compare(&newest, &build) == std::cmp::Ordering::Greater).then(|| FeedUpdate {
                app: app.clone(),
                name: name.clone(),
                installed: shown,
                available: newest_shown.unwrap_or(newest),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn versions() {
        assert_eq!(compare("1.10", "1.9"), Greater);
        assert_eq!(compare("2.0", "2.0.0"), Equal);
        assert_eq!(compare("2.0", "2.0.1"), Less);
        assert_eq!(compare("1.0b1", "1.0"), Less);
        assert_eq!(compare("1.0", "1.0b1"), Greater);
        assert_eq!(compare("310", "309"), Greater);
        assert_eq!(compare("1.2.3 (456)", "1.2.3 (455)"), Greater);
        assert_eq!(compare("007", "7"), Equal);
    }

    #[test]
    fn appcast() {
        let xml = r#"<?xml version="1.0"?><rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle"><channel>
<item><title>2.1</title><enclosure url="https://x/2.1.zip" sparkle:version="210" sparkle:shortVersionString="2.1" length="1"/></item>
<item><title>2.3</title><sparkle:version>230</sparkle:version><sparkle:shortVersionString>2.3</sparkle:shortVersionString>
<sparkle:minimumSystemVersion>13.0</sparkle:minimumSystemVersion><enclosure url="https://x/2.3.zip"/></item>
<item><title>3.0 beta</title><sparkle:channel>beta</sparkle:channel><enclosure sparkle:version="300"/></item>
<item><title>2.4 for new macOS</title><sparkle:minimumSystemVersion>99.0</sparkle:minimumSystemVersion><enclosure sparkle:version="240"/></item>
</channel></rss>"#;
        assert_eq!(newest_in_appcast(xml, "26.1"), Some(("230".into(), Some("2.3".into()))));
        assert_eq!(newest_in_appcast(xml, "12.7"), Some(("210".into(), Some("2.1".into()))), "2.3 needs macOS 13");
        assert_eq!(newest_in_appcast("<rss></rss>", "26.1"), None);
    }

    #[test]
    fn mas_output() {
        let text = "497799835 Xcode (15.0 -> 15.1)\n1295203466  Microsoft Remote Desktop  (10.9.3 -> 10.9.4)\nnot a line\n";
        let u = parse_mas_outdated(text);
        assert_eq!(u.len(), 2);
        assert_eq!(u[0], StoreUpdate { id: 497799835, name: "Xcode".into(), installed: "15.0".into(), available: "15.1".into() });
        assert_eq!(u[1].name, "Microsoft Remote Desktop");
        assert_eq!(u[1].available, "10.9.4");
    }

    #[test]
    fn updaters_in_bundles() {
        let base = std::env::temp_dir().join(format!("heft-updaters-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let sparkle = base.join("S.app");
        std::fs::create_dir_all(sparkle.join("Contents/Frameworks/Sparkle.framework")).unwrap();
        std::fs::write(
            sparkle.join("Contents/Info.plist"),
            r#"<?xml version="1.0"?><plist version="1.0"><dict><key>SUFeedURL</key><string>https://example.com/appcast.xml</string></dict></plist>"#,
        )
        .unwrap();
        let squirrel = base.join("E.app");
        std::fs::create_dir_all(squirrel.join("Contents/Frameworks/Squirrel.framework")).unwrap();
        let plain = base.join("P.app");
        std::fs::create_dir_all(plain.join("Contents")).unwrap();
        assert_eq!(updater(&sparkle), Some(Updater::Sparkle { feed: Some("https://example.com/appcast.xml".into()) }));
        assert_eq!(updater(&squirrel), Some(Updater::Squirrel));
        assert_eq!(updater(&plain), None);
        let _ = std::fs::remove_dir_all(&base);
    }
}
