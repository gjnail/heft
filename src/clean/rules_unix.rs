//! The macOS and Linux cleaning catalog.
//!
//! `%CACHE%` is `~/Library/Caches` on macOS and `$XDG_CACHE_HOME` (usually
//! `~/.cache`) on Linux; `%CONFIG%` is `~/Library/Application Support` or
//! `$XDG_CONFIG_HOME`; `%DATA%` is `$XDG_DATA_HOME`. On macOS, `%TEMP%` and
//! `%DARWIN_CACHE%` are your temporary and cache folders under
//! `/var/folders`, and `%LIBRARY%` is `/Library`. Where the two systems use
//! different folder names, `os!` picks the right one.

use super::rules::{Group::*, Rule, Target::*};
use super::unix;
use super::Special::Tool;
#[cfg(target_os = "macos")]
use super::{
    mac,
    Special::{Clipboard, Prefs, Restart},
};

#[cfg(target_os = "macos")]
macro_rules! os {
    (mac: $m:expr, linux: $l:expr) => {
        $m
    };
}
#[cfg(not(target_os = "macos"))]
macro_rules! os {
    (mac: $m:expr, linux: $l:expr) => {
        $l
    };
}

/// Cache folders every Chromium-based browser (and Electron app) keeps in
/// its user-data folder, at the top or in each profile, plus any `$extra`
/// targets.
macro_rules! chromium_cache {
    ($ud:expr $(, $extra:expr)*) => {
        &[
            $($extra,)*
            Chromium($ud, "Cache"),
            Chromium($ud, "Code Cache"),
            Chromium($ud, "GPUCache"),
            Chromium($ud, "DawnCache"),
            Chromium($ud, "DawnGraphiteCache"),
            Chromium($ud, "DawnWebGPUCache"),
            Chromium($ud, "Service Worker/CacheStorage"),
            Chromium($ud, "Service Worker/ScriptCache"),
            Chromium($ud, "ShaderCache"),
            Chromium($ud, "GrShaderCache"),
            Chromium($ud, "GraphiteDawnCache"),
            Chromium($ud, "Crashpad/reports"),
        ]
    };
}

/// Cache, cookies, history and session rules for one Chromium browser: the
/// same files as on Windows. `$disk` is where the browser keeps its disk
/// cache, outside the user-data folder.
macro_rules! chromium_browser {
    ($prefix:literal, $app:literal, $close:expr, $ud:expr, $disk:expr) => {
        [
            Rule {
                id: concat!($prefix, ".cache"),
                group: Browsers,
                app: $app,
                name: "Cache",
                about: "Downloaded copies of web pages, images and scripts. Sites load them again as needed.",
                default_on: true,
                admin: false,
                warning: None,
                close: $close,
                targets: chromium_cache!($ud, Path($disk)),
            },
            Rule {
                id: concat!($prefix, ".cookies"),
                group: Browsers,
                app: $app,
                name: "Cookies",
                about: "Site cookies and logins.",
                default_on: false,
                admin: false,
                warning: Some("Signs you out of every website."),
                close: $close,
                targets: &[
                    Chromium($ud, "Network/Cookies"),
                    Chromium($ud, "Network/Cookies-journal"),
                    Chromium($ud, "Cookies"),
                    Chromium($ud, "Cookies-journal"),
                ],
            },
            Rule {
                id: concat!($prefix, ".history"),
                group: Browsers,
                app: $app,
                name: "History",
                about: "Browsing history, download history and address-bar suggestions. Bookmarks are kept.",
                default_on: false,
                admin: false,
                warning: None,
                close: $close,
                targets: &[
                    Chromium($ud, "History"),
                    Chromium($ud, "History-journal"),
                    Chromium($ud, "Visited Links"),
                    Chromium($ud, "Top Sites"),
                    Chromium($ud, "Top Sites-journal"),
                    Chromium($ud, "Shortcuts"),
                    Chromium($ud, "Shortcuts-journal"),
                ],
            },
            Rule {
                id: concat!($prefix, ".session"),
                group: Browsers,
                app: $app,
                name: "Last session",
                about: "The tabs and windows that reopen when the browser starts.",
                default_on: false,
                admin: false,
                warning: Some("Tabs from the last session can't be restored afterwards."),
                close: $close,
                targets: &[
                    Chromium($ud, "Sessions"),
                    Chromium($ud, "Current Session"),
                    Chromium($ud, "Current Tabs"),
                    Chromium($ud, "Last Session"),
                    Chromium($ud, "Last Tabs"),
                ],
            },
        ]
    };
}

#[cfg(target_os = "macos")]
const SYSTEM: [Rule; 7] = [
    Rule {
        id: "sys.temp",
        group: System,
        app: "macOS",
        name: "Temporary files",
        about: "Files apps left in your temporary folder. Only files untouched for 24 hours are removed, so apps that are running aren't disturbed; they create new ones when they need them.",
        default_on: true,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Older("%TEMP%", 24)],
    },
    Rule {
        id: "sys.trash",
        group: System,
        app: "macOS",
        name: "Trash",
        about: "Everything in the Trash. (Heft needs Full Disk Access in System Settings to see inside it.)",
        default_on: false,
        admin: false,
        warning: Some("Permanently deletes everything in the Trash."),
        close: &[],
        targets: &[Path("%HOME%/.Trash")],
    },
    Rule {
        id: "sys.quicklook",
        group: System,
        app: "macOS",
        name: "Quick Look thumbnail cache",
        about: "Picture, video and document previews that Finder and Quick Look show. Rebuilt as you browse folders. Runs `qlmanage -r cache`; Heft needs Full Disk Access to show the size.",
        default_on: true,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Special(Tool(mac::quicklook))],
    },
    Rule {
        id: "sys.metal",
        group: System,
        app: "macOS",
        name: "Metal shader caches",
        about: "Graphics shaders macOS compiled for each app and game, kept in your cache folder. macOS compiles them again as apps need them.",
        default_on: false,
        admin: false,
        warning: Some("Games may stutter briefly while shaders rebuild."),
        close: &[],
        targets: &[
            Path("%DARWIN_CACHE%/com.apple.metal"),
            Path("%DARWIN_CACHE%/com.apple.metalfe"),
            EachDir("%DARWIN_CACHE%", "com.apple.metal"),
            EachDir("%DARWIN_CACHE%", "com.apple.metalfe"),
        ],
    },
    Rule {
        id: "sys.logs",
        group: System,
        app: "macOS",
        name: "Old logs & crash reports",
        about: "Log files and crash reports your apps wrote more than a week ago. Apps start new logs as they need them.",
        default_on: true,
        admin: false,
        warning: None,
        close: &[],
        // Crash reports are judged one by one, even when a newer one exists.
        targets: &[Older("%HOME%/Library/Logs", 168), Older("%HOME%/Library/Logs/DiagnosticReports", 168)],
    },
    Rule {
        id: "sys.syslogs",
        group: System,
        app: "macOS",
        name: "System logs & crash reports",
        about: "Logs and crash reports that system services and installers wrote to /Library/Logs more than a week ago. Services start new ones as needed, and their folders are kept.",
        default_on: false,
        admin: true,
        warning: None,
        close: &[],
        targets: &[Older("%LIBRARY%/Logs", 168), Older("%LIBRARY%/Logs/DiagnosticReports", 168)],
    },
    Rule {
        id: "sys.ipsw",
        group: System,
        app: "macOS",
        name: "iPhone & iPad software updates",
        about: "Software Finder downloaded to update or restore an iPhone, iPad or iPod. Finder downloads it again when it's needed.",
        default_on: false,
        admin: false,
        warning: Some("Finder downloads it again (several GB) the next time you update or restore a device."),
        close: &[],
        targets: &[
            Glob("%HOME%/Library/iTunes/iPhone Software Updates", "*.ipsw"),
            Glob("%HOME%/Library/iTunes/iPad Software Updates", "*.ipsw"),
            Glob("%HOME%/Library/iTunes/iPod Software Updates", "*.ipsw"),
        ],
    },
];

#[cfg(target_os = "linux")]
const SYSTEM: [Rule; 7] = [
    Rule {
        id: "sys.thumbs",
        group: System,
        app: "Linux",
        name: "Thumbnail cache",
        about: "Picture and video previews. File managers recreate them as you browse.",
        default_on: true,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Path("%CACHE%/thumbnails")],
    },
    Rule {
        id: "sys.trash",
        group: System,
        app: "Linux",
        name: "Trash",
        about: "Everything in your Trash.",
        default_on: false,
        admin: false,
        warning: Some("Permanently deletes everything in the Trash."),
        close: &[],
        targets: &[Path("%DATA%/Trash/files"), Path("%DATA%/Trash/info"), Path("%DATA%/Trash/expunged")],
    },
    Rule {
        id: "sys.apt",
        group: System,
        app: "Linux",
        name: "APT package cache",
        about: "Installer packages apt already used. Runs `apt-get clean`.",
        default_on: true,
        admin: true,
        warning: None,
        close: &["apt", "apt-get", "dpkg", "unattended-upgr"],
        targets: &[Special(Tool(unix::apt))],
    },
    Rule {
        id: "sys.dnf",
        group: System,
        app: "Linux",
        name: "DNF package cache",
        about: "Installer packages dnf kept. Runs `dnf clean packages`.",
        default_on: true,
        admin: true,
        warning: None,
        close: &["dnf", "packagekitd"],
        targets: &[Special(Tool(unix::dnf))],
    },
    Rule {
        id: "sys.journal",
        group: System,
        app: "Linux",
        name: "Old system logs",
        about: "Archived systemd journal files older than two weeks. Runs `journalctl --vacuum-time=2weeks`.",
        default_on: true,
        admin: true,
        warning: None,
        close: &[],
        targets: &[Special(Tool(unix::journald))],
    },
    Rule {
        id: "sys.snap",
        group: System,
        app: "Linux",
        name: "Old snap revisions",
        about: "Snap keeps the previous version of every snap after it updates. Removes those disabled revisions.",
        default_on: false,
        admin: true,
        warning: Some("Those snaps can't be rolled back to their previous version afterwards."),
        close: &[],
        targets: &[Special(Tool(unix::snap))],
    },
    Rule {
        id: "sys.flatpak",
        group: System,
        app: "Linux",
        name: "Unused Flatpak runtimes",
        about: "Runtimes and extensions no installed app needs. Runs `flatpak uninstall --unused`; the size is Heft's estimate.",
        default_on: true,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Special(Tool(unix::flatpak))],
    },
];

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const SYSTEM: [Rule; 0] = [];

const CHROME: [Rule; 4] = chromium_browser!(
    "chrome",
    "Google Chrome",
    &["google chrome", "chrome"],
    os!(mac: "%CONFIG%/Google/Chrome", linux: "%CONFIG%/google-chrome"),
    os!(mac: "%CACHE%/Google/Chrome", linux: "%CACHE%/google-chrome")
);
const EDGE: [Rule; 4] = chromium_browser!(
    "edge",
    "Microsoft Edge",
    &["microsoft edge", "msedge"],
    os!(mac: "%CONFIG%/Microsoft Edge", linux: "%CONFIG%/microsoft-edge"),
    os!(mac: "%CACHE%/Microsoft Edge", linux: "%CACHE%/microsoft-edge")
);
const BRAVE: [Rule; 4] = chromium_browser!(
    "brave",
    "Brave",
    &["brave browser", "brave"],
    "%CONFIG%/BraveSoftware/Brave-Browser",
    "%CACHE%/BraveSoftware/Brave-Browser"
);
const VIVALDI: [Rule; 4] = chromium_browser!(
    "vivaldi",
    "Vivaldi",
    &["vivaldi", "vivaldi-bin"],
    os!(mac: "%CONFIG%/Vivaldi", linux: "%CONFIG%/vivaldi"),
    os!(mac: "%CACHE%/Vivaldi", linux: "%CACHE%/vivaldi")
);
// Opera keeps its profile in the user-data folder itself.
const OPERA: [Rule; 4] = chromium_browser!(
    "opera",
    "Opera",
    &["opera"],
    os!(mac: "%CONFIG%/com.operasoftware.Opera", linux: "%CONFIG%/opera"),
    os!(mac: "%CACHE%/com.operasoftware.Opera", linux: "%CACHE%/opera")
);
#[cfg(target_os = "macos")]
const OPERA_GX: [Rule; 4] = chromium_browser!(
    "operagx",
    "Opera GX",
    &["opera gx", "opera"],
    "%CONFIG%/com.operasoftware.OperaGX",
    "%CACHE%/com.operasoftware.OperaGX"
);
const CHROMIUM: [Rule; 4] = chromium_browser!(
    "chromium",
    "Chromium",
    &["chromium"],
    os!(mac: "%CONFIG%/Chromium", linux: "%CONFIG%/chromium"),
    os!(mac: "%CACHE%/Chromium", linux: "%CACHE%/chromium")
);

/// Where Firefox keeps its profiles, and its crash reports.
const FIREFOX_PROFILES: &str = os!(mac: "%CONFIG%/Firefox/Profiles", linux: "%HOME%/.mozilla/firefox");
const FIREFOX_CACHE: &str = os!(mac: "%CACHE%/Firefox/Profiles", linux: "%CACHE%/mozilla/firefox");

const FIREFOX: [Rule; 5] = [
    Rule {
        id: "firefox.cache",
        group: Browsers,
        app: "Mozilla Firefox",
        name: "Cache",
        about: "Downloaded copies of web pages, images and scripts. Sites load them again as needed.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["firefox", "firefox-bin"],
        targets: &[
            EachDir(FIREFOX_CACHE, "cache2"),
            EachDir(FIREFOX_CACHE, "startupCache"),
            EachDir(FIREFOX_CACHE, "thumbnails"),
        ],
    },
    Rule {
        id: "firefox.cookies",
        group: Browsers,
        app: "Mozilla Firefox",
        name: "Cookies",
        about: "Site cookies and logins.",
        default_on: false,
        admin: false,
        warning: Some("Signs you out of every website."),
        close: &["firefox", "firefox-bin"],
        targets: &[
            EachDir(FIREFOX_PROFILES, "cookies.sqlite"),
            EachDir(FIREFOX_PROFILES, "cookies.sqlite-wal"),
            EachDir(FIREFOX_PROFILES, "cookies.sqlite-shm"),
        ],
    },
    Rule {
        id: "firefox.forms",
        group: Browsers,
        app: "Mozilla Firefox",
        name: "Form history",
        about: "Text you typed into web forms and search boxes. (Firefox keeps browsing history and bookmarks in one database, so Heft leaves history alone.)",
        default_on: false,
        admin: false,
        warning: None,
        close: &["firefox", "firefox-bin"],
        targets: &[EachDir(FIREFOX_PROFILES, "formhistory.sqlite")],
    },
    Rule {
        id: "firefox.session",
        group: Browsers,
        app: "Mozilla Firefox",
        name: "Last session",
        about: "The tabs and windows that reopen when Firefox starts.",
        default_on: false,
        admin: false,
        warning: Some("Tabs from the last session can't be restored afterwards."),
        close: &["firefox", "firefox-bin"],
        targets: &[EachDir(FIREFOX_PROFILES, "sessionstore.jsonlz4"), EachDir(FIREFOX_PROFILES, "sessionstore-backups")],
    },
    Rule {
        id: "firefox.crashes",
        group: Browsers,
        app: "Mozilla Firefox",
        name: "Crash reports",
        about: "Reports saved after Firefox crashed.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["firefox", "firefox-bin"],
        targets: &[
            Path(os!(mac: "%CONFIG%/Firefox/Crash Reports/submitted", linux: "%HOME%/.mozilla/firefox/Crash Reports/submitted")),
            Path(os!(mac: "%CONFIG%/Firefox/Crash Reports/pending", linux: "%HOME%/.mozilla/firefox/Crash Reports/pending")),
            EachDir(FIREFOX_PROFILES, "minidumps"),
        ],
    },
];

/// Safari's cache. Its cookies, history and last session are in
/// `SAFARI_PRIVATE`.
#[cfg(target_os = "macos")]
const SAFARI: [Rule; 1] = [Rule {
    id: "safari.cache",
    group: Browsers,
    app: "Safari",
    name: "Cache",
    about: "Safari's web cache. (Heft needs Full Disk Access in System Settings to see it.)",
    default_on: true,
    admin: false,
    warning: None,
    close: &["safari"],
    targets: &[Path("%CACHE%/com.apple.Safari")],
}];

/// Safari's cookies (in its app sandbox), history and last session. macOS
/// protects them like Mail and Messages, so these are only listed when Heft
/// has Full Disk Access; without it Heft could neither see nor remove them.
#[cfg(target_os = "macos")]
const SAFARI_PRIVATE: [Rule; 3] = [
    Rule {
        id: "safari.cookies",
        group: Browsers,
        app: "Safari",
        name: "Cookies",
        about: "Site cookies and logins.",
        default_on: false,
        admin: false,
        warning: Some("Signs you out of every website in Safari."),
        close: &["safari"],
        targets: &[Path("%HOME%/Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies")],
    },
    Rule {
        id: "safari.history",
        group: Browsers,
        app: "Safari",
        name: "History",
        about: "Browsing history and address-bar suggestions. Bookmarks and Reading List are kept.",
        default_on: false,
        admin: false,
        warning: Some(
            "If Safari syncs through iCloud, your other devices send the history back. To clear it everywhere, use History › Clear History in Safari instead.",
        ),
        close: &["safari"],
        targets: &[
            Path("%HOME%/Library/Safari/History.db"),
            Path("%HOME%/Library/Safari/History.db-wal"),
            Path("%HOME%/Library/Safari/History.db-shm"),
            Path("%HOME%/Library/Safari/History.db-lock"),
        ],
    },
    Rule {
        id: "safari.session",
        group: Browsers,
        app: "Safari",
        name: "Last session",
        about: "The windows and tabs History › Reopen All Windows from Last Session brings back, and recently closed tabs. Tab groups are kept.",
        default_on: false,
        admin: false,
        warning: Some("Tabs from the last session can't be restored afterwards."),
        close: &["safari"],
        targets: &[Path("%HOME%/Library/Safari/LastSession.plist"), Path("%HOME%/Library/Safari/RecentlyClosedTabs.plist")],
    },
];

/// New Teams keeps its cache in its app sandbox, in the Caches folder macOS
/// gives every sandboxed app. Only listed with Full Disk Access, like
/// `SAFARI_PRIVATE`.
#[cfg(target_os = "macos")]
const TEAMS2: Rule = Rule {
    id: "app.teams2",
    group: Apps,
    app: "Microsoft Teams",
    name: "Cache (new Teams)",
    about: "Images, files and web pages new Teams downloaded, which it downloads again as needed. You stay signed in.",
    default_on: true,
    admin: false,
    warning: None,
    close: &["msteams", "microsoft teams"],
    targets: &[Path("%HOME%/Library/Containers/com.microsoft.teams2/Data/Library/Caches")],
};

const APPS: [Rule; 9] = [
    Rule {
        id: "app.discord",
        group: Apps,
        app: "Discord",
        name: "Cache",
        about: "Images, videos and scripts Discord downloaded.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["discord"],
        targets: chromium_cache!("%CONFIG%/discord"),
    },
    Rule {
        id: "app.slack",
        group: Apps,
        app: "Slack",
        name: "Cache",
        about: "Files and pages Slack downloaded.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["slack"],
        targets: chromium_cache!("%CONFIG%/Slack"),
    },
    Rule {
        id: "app.teams",
        group: Apps,
        app: "Microsoft Teams",
        name: "Cache",
        about: os!(
            mac: "Web cache of classic Teams, which downloads it again as needed. (New Teams has its own rule, shown when Heft has Full Disk Access.)",
            linux: "Web cache of classic Teams, which downloads it again as needed."
        ),
        default_on: true,
        admin: false,
        warning: None,
        close: &["teams", "microsoft teams", "msteams"],
        targets: chromium_cache!(os!(mac: "%CONFIG%/Microsoft/Teams", linux: "%CONFIG%/Microsoft/Microsoft Teams")),
    },
    Rule {
        id: "app.spotify",
        group: Apps,
        app: "Spotify",
        name: "Cache",
        about: "Streaming cache. Songs you downloaded for offline listening are kept.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["spotify"],
        targets: &[Path(os!(mac: "%CACHE%/com.spotify.client/Data", linux: "%CACHE%/spotify/Data"))],
    },
    Rule {
        id: "app.vscode",
        group: Apps,
        app: "Visual Studio Code",
        name: "Cache & old logs",
        about: "Editor caches, downloaded extension packages, and logs older than a week.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["code", "electron"],
        targets: &[
            Path("%CONFIG%/Code/Cache"),
            Path("%CONFIG%/Code/CachedData"),
            Path("%CONFIG%/Code/Code Cache"),
            Path("%CONFIG%/Code/GPUCache"),
            Path("%CONFIG%/Code/CachedExtensionVSIXs"),
            Path("%CONFIG%/Code/Service Worker/CacheStorage"),
            Older("%CONFIG%/Code/logs", 168),
        ],
    },
    Rule {
        id: "app.cursor",
        group: Apps,
        app: "Cursor",
        name: "Cache & old logs",
        about: "Editor caches, downloaded extension packages, and logs older than a week.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["cursor"],
        targets: &[
            Path("%CONFIG%/Cursor/Cache"),
            Path("%CONFIG%/Cursor/CachedData"),
            Path("%CONFIG%/Cursor/Code Cache"),
            Path("%CONFIG%/Cursor/GPUCache"),
            Path("%CONFIG%/Cursor/CachedExtensionVSIXs"),
            Path("%CONFIG%/Cursor/Service Worker/CacheStorage"),
            Older("%CONFIG%/Cursor/logs", 168),
        ],
    },
    Rule {
        id: "app.steam",
        group: Apps,
        app: "Steam",
        name: "Web caches & old logs",
        about: "The Steam client's web caches and logs older than a week. Steam downloads what it needs again; games are not touched.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["steam", "steamwebhelper", "steam_osx"],
        targets: chromium_cache!(
            os!(mac: "%CONFIG%/Steam/config/htmlcache", linux: "%DATA%/Steam/config/htmlcache"),
            Path(os!(mac: "%CONFIG%/Steam/appcache/httpcache", linux: "%DATA%/Steam/appcache/httpcache")),
            Older(os!(mac: "%CONFIG%/Steam/logs", linux: "%DATA%/Steam/logs"), 168)
        ),
    },
    Rule {
        id: "app.java",
        group: Apps,
        app: "Java",
        name: "Deployment cache",
        about: "Applets and Web Start programs Java downloaded. Java downloads them again the next time they run.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["java", "javaws"],
        targets: &[Path(os!(mac: "%CONFIG%/Oracle/Java/Deployment/cache", linux: "%HOME%/.java/deployment/cache"))],
    },
    Rule {
        id: "app.jetbrains",
        group: Apps,
        app: "JetBrains IDEs",
        name: "Caches & indexes",
        about: "IntelliJ IDEA, PyCharm, Rider, WebStorm, CLion, GoLand and RustRover caches.",
        default_on: false,
        admin: false,
        warning: Some("Projects are re-indexed the next time you open them."),
        close: &["idea", "pycharm", "rider", "webstorm", "clion", "goland", "rustrover", "java"],
        targets: &[Path("%CACHE%/JetBrains")],
    },
];

#[cfg(target_os = "macos")]
const MAC_APPS: [Rule; 2] = [
    Rule {
        id: "app.epic",
        group: Apps,
        app: "Epic Games Launcher",
        name: "Web cache",
        about: "The launcher's web cache, which it downloads again as needed. Games are not touched.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["epicgameslauncher", "epicgameslauncher-mac-shipping", "epicwebhelper"],
        targets: &[Glob("%CACHE%/com.epicgames.EpicGamesLauncher", "webcache*")],
    },
    Rule {
        id: "app.adobe",
        group: Apps,
        app: "Adobe",
        name: "Media cache",
        about: "Conformed audio and preview files Premiere Pro, After Effects and Media Encoder create. They create them again from your footage.",
        default_on: false,
        admin: false,
        warning: Some("Projects take a while to re-conform the next time you open them."),
        close: &["adobe premiere pro*", "after effects", "adobe media encoder*"],
        targets: &[Path("%CONFIG%/Adobe/Common/Media Cache Files"), Path("%CONFIG%/Adobe/Common/Media Cache")],
    },
];

const DEVELOPER: [Rule; 13] = [
    Rule {
        id: "dev.homebrew",
        group: Developer,
        app: "Homebrew",
        name: "Downloads & old versions",
        about: "Homebrew's download cache and superseded versions of installed packages. Runs `brew cleanup --prune=all`.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["brew"],
        targets: &[Special(Tool(unix::homebrew))],
    },
    Rule {
        id: "dev.npm",
        group: Developer,
        app: "npm",
        name: "Package cache",
        about: "Every package npm has downloaded, plus npx installs and logs.",
        default_on: false,
        admin: false,
        warning: Some("Packages download again the next time a project installs them."),
        close: &[],
        targets: &[Path("%HOME%/.npm/_cacache"), Path("%HOME%/.npm/_npx"), Path("%HOME%/.npm/_logs")],
    },
    Rule {
        id: "dev.yarn",
        group: Developer,
        app: "Yarn",
        name: "Package cache",
        about: "Yarn 1 and Yarn Berry global caches.",
        default_on: false,
        admin: false,
        warning: Some("Packages download again the next time a project installs them."),
        close: &[],
        targets: &[Path(os!(mac: "%CACHE%/Yarn", linux: "%CACHE%/yarn")), Path("%HOME%/.yarn/berry/cache")],
    },
    Rule {
        id: "dev.bun",
        group: Developer,
        app: "Bun",
        name: "Package cache",
        about: "Bun's global install cache.",
        default_on: false,
        admin: false,
        warning: Some("Packages download again the next time a project installs them."),
        close: &[],
        targets: &[Path("%HOME%/.bun/install/cache")],
    },
    Rule {
        id: "dev.pip",
        group: Developer,
        app: "pip",
        name: "Package cache",
        about: "Wheels and downloads pip keeps between installs.",
        default_on: false,
        admin: false,
        warning: Some("Packages download again the next time you install them."),
        close: &[],
        targets: &[Path("%CACHE%/pip")],
    },
    Rule {
        id: "dev.cargo",
        group: Developer,
        app: "Cargo (Rust)",
        name: "Registry & git caches",
        about: "Downloaded crates, their unpacked sources, and git checkouts. Build folders (target) are listed under Disk usage › Build junk.",
        default_on: false,
        admin: false,
        warning: Some("Crates download again the next time you build."),
        close: &["cargo"],
        targets: &[
            Path("%CARGO_HOME%/registry/cache"),
            Path("%CARGO_HOME%/registry/src"),
            Path("%CARGO_HOME%/git/checkouts"),
        ],
    },
    Rule {
        id: "dev.go",
        group: Developer,
        app: "Go",
        name: "Build cache",
        about: "Compiled packages the go command reuses between builds.",
        default_on: false,
        admin: false,
        warning: Some("The next build of each project is slower."),
        close: &["go"],
        targets: &[Path("%CACHE%/go-build")],
    },
    Rule {
        id: "dev.gradle",
        group: Developer,
        app: "Gradle",
        name: "Caches",
        about: "Downloaded dependencies and build caches in the Gradle user home.",
        default_on: false,
        admin: false,
        warning: Some("Dependencies download again on the next build."),
        close: &[],
        targets: &[Path("%GRADLE_USER_HOME%/caches")],
    },
    Rule {
        id: "dev.nuget",
        group: Developer,
        app: "NuGet",
        name: "HTTP & plugin caches",
        about: "NuGet's download caches; it fills them again on the next restore. The global packages folder is left alone.",
        default_on: false,
        admin: false,
        warning: None,
        close: &[],
        targets: &[
            Path(os!(mac: "%HOME%/.local/share/NuGet/v3-cache", linux: "%DATA%/NuGet/v3-cache")),
            Path(os!(mac: "%HOME%/.local/share/NuGet/plugins-cache", linux: "%DATA%/NuGet/plugins-cache")),
        ],
    },
    Rule {
        id: "dev.deno",
        group: Developer,
        app: "Deno",
        name: "Module cache",
        about: "Remote modules, npm packages and compiled code Deno cached.",
        default_on: false,
        admin: false,
        warning: Some("Modules download again the next time they're imported."),
        close: &["deno"],
        targets: &[
            Path("%CACHE%/deno/remote"),
            Path("%CACHE%/deno/deps"),
            Path("%CACHE%/deno/npm"),
            Path("%CACHE%/deno/gen"),
        ],
    },
    Rule {
        id: "dev.electron",
        group: Developer,
        app: "Electron",
        name: "Download caches",
        about: "Electron binaries downloaded by electron and electron-builder.",
        default_on: false,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Path("%CACHE%/electron"), Path("%CACHE%/electron-builder")],
    },
    Rule {
        id: "dev.composer",
        group: Developer,
        app: "Composer",
        name: "Package cache",
        about: "PHP packages Composer downloaded. It downloads them again when a project needs them.",
        default_on: false,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Path("%CACHE%/composer")],
    },
    Rule {
        id: "dev.docker",
        group: Developer,
        app: "Docker",
        name: "Build cache & dangling images",
        about: "Build cache and untagged images Docker no longer uses, when Docker is running. Runs `docker builder prune` and `docker image prune`; containers, volumes and tagged images are kept, and the size is Heft's estimate.",
        default_on: false,
        admin: false,
        warning: Some("Images take longer to build the next time."),
        close: &[],
        targets: &[Special(Tool(unix::docker))],
    },
];

#[cfg(target_os = "macos")]
const XCODE: [Rule; 5] = [
    Rule {
        id: "xcode.derived",
        group: Developer,
        app: "Xcode",
        name: "DerivedData & previews",
        about: "Build products, indexes and SwiftUI preview caches.",
        default_on: false,
        admin: false,
        warning: Some("Projects rebuild from scratch and re-index the next time you open them."),
        close: &["xcode"],
        targets: &[
            Path("%HOME%/Library/Developer/Xcode/DerivedData"),
            Path("%HOME%/Library/Developer/Xcode/UserData/Previews"),
        ],
    },
    Rule {
        id: "xcode.simulators",
        group: Developer,
        app: "Xcode",
        name: "Unavailable simulators",
        about: "Simulators whose iOS, watchOS or tvOS runtime is no longer installed, so they can't run anyway. Runs `xcrun simctl delete unavailable`.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["simulator"],
        targets: &[Special(Tool(unix::simulators))],
    },
    Rule {
        id: "xcode.simcache",
        group: Developer,
        app: "Xcode",
        name: "Simulator caches",
        about: "Caches the simulators rebuild when they next start.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["simulator"],
        targets: &[Path("%HOME%/Library/Developer/CoreSimulator/Caches")],
    },
    Rule {
        id: "xcode.devicesupport",
        group: Developer,
        app: "Xcode",
        name: "Device support files",
        about: "Debug symbols Xcode copied from every iPhone, iPad, Watch and Apple TV you've connected.",
        default_on: false,
        admin: false,
        warning: Some("Xcode copies them again (a few minutes) the next time you connect that device."),
        close: &["xcode"],
        targets: &[
            Path("%HOME%/Library/Developer/Xcode/iOS DeviceSupport"),
            Path("%HOME%/Library/Developer/Xcode/watchOS DeviceSupport"),
            Path("%HOME%/Library/Developer/Xcode/tvOS DeviceSupport"),
        ],
    },
    Rule {
        id: "dev.cocoapods",
        group: Developer,
        app: "CocoaPods",
        name: "Pod cache",
        about: "Pods CocoaPods downloaded.",
        default_on: false,
        admin: false,
        warning: Some("Pods download again the next time you run pod install."),
        close: &[],
        targets: &[Path("%CACHE%/CocoaPods")],
    },
];

/// Where macOS keeps the Recent Items lists. `sharedfilelistd` owns them and
/// would write its copy back, so it's restarted once they're gone.
#[cfg(target_os = "macos")]
const SHARED_FILE_LIST: &str = "%CONFIG%/com.apple.sharedfilelist";

#[cfg(target_os = "macos")]
const PRIVACY: [Rule; 4] = [
    Rule {
        id: "priv.recent",
        group: Privacy,
        app: "macOS",
        name: "Recent items",
        about: "Apple menu › Recent Items (apps, documents and servers) and each app's File › Open Recent list. macOS starts new lists as you open things. (Heft needs Full Disk Access in System Settings to see them.)",
        default_on: false,
        admin: false,
        warning: None,
        close: &[],
        targets: &[
            Glob(SHARED_FILE_LIST, "com.apple.LSSharedFileList.RecentApplications.sfl*"),
            Glob(SHARED_FILE_LIST, "com.apple.LSSharedFileList.RecentDocuments.sfl*"),
            Glob(SHARED_FILE_LIST, "com.apple.LSSharedFileList.RecentServers.sfl*"),
            Glob("%CONFIG%/com.apple.sharedfilelist/com.apple.LSSharedFileList.ApplicationRecentDocuments", "*.sfl*"),
            Special(Restart("sharedfilelistd")),
        ],
    },
    Rule {
        id: "priv.finder",
        group: Privacy,
        app: "macOS",
        name: "Finder recent folders",
        about: "Finder's Go › Recent Folders and Go to Folder history. Finder starts new lists as you browse.",
        default_on: false,
        admin: false,
        warning: Some("Finder restarts to forget them, which closes its windows."),
        close: &[],
        targets: &[
            Special(Prefs("com.apple.finder", &["FXRecentFolders", "GoToField", "GoToFieldHistory"])),
            Special(Restart("Finder")),
        ],
    },
    Rule {
        id: "priv.clipboard",
        group: Privacy,
        app: "macOS",
        name: "Clipboard",
        about: "Whatever is currently copied.",
        default_on: false,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Special(Clipboard)],
    },
    Rule {
        id: "priv.dns",
        group: Privacy,
        app: "macOS",
        name: "DNS cache",
        about: "Recently looked-up website addresses. Frees no disk space. Runs `dscacheutil -flushcache` and restarts mDNSResponder, which fills the cache again as you browse.",
        default_on: false,
        admin: true,
        warning: None,
        close: &[],
        targets: &[Special(Tool(mac::dns))],
    },
];

/// All rules for this OS, in display order.
pub fn catalog() -> Vec<Rule> {
    let mut v = Vec::new();
    v.extend(SYSTEM);
    v.extend(CHROME);
    v.extend(EDGE);
    v.extend(FIREFOX);
    v.extend(BRAVE);
    v.extend(OPERA);
    #[cfg(target_os = "macos")]
    v.extend(OPERA_GX);
    v.extend(VIVALDI);
    v.extend(CHROMIUM);
    // Rules for protected places are only listed when Heft can reach them.
    #[cfg(target_os = "macos")]
    let full_disk_access = mac::has_full_disk_access() == Some(true);
    #[cfg(target_os = "macos")]
    v.extend(SAFARI);
    #[cfg(target_os = "macos")]
    if full_disk_access {
        v.extend(SAFARI_PRIVATE);
    }
    v.extend(APPS);
    #[cfg(target_os = "macos")]
    if full_disk_access {
        v.push(TEAMS2);
    }
    #[cfg(target_os = "macos")]
    v.extend(MAC_APPS);
    #[cfg(target_os = "macos")]
    v.extend(XCODE);
    v.extend(DEVELOPER);
    #[cfg(target_os = "macos")]
    v.extend(PRIVACY);
    v
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// The rules only listed with Full Disk Access, which the catalog-wide
    /// tests don't see when they run without it.
    #[test]
    fn protected_rules_resolve() {
        let listed = mac::has_full_disk_access() == Some(true);
        for r in SAFARI_PRIVATE.iter().chain([&TEAMS2]) {
            assert_eq!(super::super::rules::by_id(r.id).is_some(), listed, "{}", r.id);
            for t in r.targets {
                let crate::clean::rules::Target::Path(p) = t else { panic!("{}: unexpected target", r.id) };
                assert!(super::super::resolve(p).is_some(), "{}: {p} doesn't resolve to a safe place", r.id);
            }
        }
    }
}
