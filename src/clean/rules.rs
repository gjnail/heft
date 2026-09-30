//! The catalog of things Heft knows how to clean.
//!
//! Every rule names exact locations; there is no "search the disk for
//! *.tmp" style guessing. Paths use `%VAR%` placeholders that are resolved at
//! run time (see `super::resolve`); a rule whose variables can't be resolved
//! is skipped rather than guessed.
//!
//! This file holds the Windows catalog; `rules_unix.rs` has macOS and Linux.

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum Group {
    /// The operating system itself: temp files, logs, caches.
    System,
    Browsers,
    Apps,
    Developer,
    Privacy,
}

impl Group {
    pub const ALL: [Group; 5] = [Group::System, Group::Browsers, Group::Apps, Group::Developer, Group::Privacy];

    pub fn label(self) -> &'static str {
        match self {
            Group::System => super::SYSTEM,
            Group::Browsers => "Browsers",
            Group::Apps => "Applications",
            Group::Developer => "Developer tools",
            Group::Privacy => "Privacy",
        }
    }
}

#[derive(Debug)]
pub enum Target {
    /// A folder's contents (the folder itself stays), or a single file.
    Path(&'static str),
    /// Like `Path`, but only files untouched for at least this many hours.
    Older(&'static str, u32),
    /// Entries of a folder whose names match a `*` pattern (files, or whole
    /// subfolders).
    #[cfg_attr(not(windows), allow(dead_code))]
    Glob(&'static str, &'static str),
    /// Chromium user-data folder + a path inside each profile in it
    /// (`Default`, `Profile 1`, …, and the folder itself for Opera).
    #[cfg_attr(not(windows), allow(dead_code))]
    Chromium(&'static str, &'static str),
    /// A path inside every subfolder of a folder (Firefox profiles, IDEs).
    EachDir(&'static str, &'static str),
    /// Something only this OS has: the Recycle Bin, a package manager, …
    Special(super::Special),
}

#[derive(Debug)]
pub struct Rule {
    /// Stable id, used for saved selections.
    pub id: &'static str,
    pub group: Group,
    /// Program the rule belongs to ("Google Chrome", "Windows").
    pub app: &'static str,
    pub name: &'static str,
    pub about: &'static str,
    pub default_on: bool,
    /// Only works as administrator.
    pub admin: bool,
    /// Side effect worth knowing before ticking the box.
    pub warning: Option<&'static str>,
    /// Executables that must not be running while cleaning.
    pub close: &'static [&'static str],
    pub targets: &'static [Target],
}

#[cfg(windows)]
use super::Special::{Clipboard, DnsCache, RecycleBin, RegistryValues};
#[cfg(windows)]
use Group::*;
#[cfg(windows)]
use Target::*;

/// Cache folders every Chromium-based browser (and Electron app) shares.
#[cfg(windows)]
macro_rules! chromium_cache {
    ($($ud:literal),+) => {
        &[$(
            Chromium($ud, "Cache"),
            Chromium($ud, "Code Cache"),
            Chromium($ud, "GPUCache"),
            Chromium($ud, "DawnCache"),
            Chromium($ud, "DawnGraphiteCache"),
            Chromium($ud, "DawnWebGPUCache"),
            Chromium($ud, r"Service Worker\CacheStorage"),
            Chromium($ud, r"Service Worker\ScriptCache"),
            Path(concat!($ud, r"\ShaderCache")),
            Path(concat!($ud, r"\GrShaderCache")),
            Path(concat!($ud, r"\GraphiteDawnCache")),
            Path(concat!($ud, r"\Crashpad\reports")),
        )+]
    };
}

#[cfg(windows)]
macro_rules! chromium_cookies {
    ($ud:literal) => {
        &[
            Chromium($ud, r"Network\Cookies"),
            Chromium($ud, r"Network\Cookies-journal"),
            Chromium($ud, "Cookies"),
            Chromium($ud, "Cookies-journal"),
        ]
    };
}

#[cfg(windows)]
macro_rules! chromium_history {
    ($ud:literal) => {
        &[
            Chromium($ud, "History"),
            Chromium($ud, "History-journal"),
            Chromium($ud, "Visited Links"),
            Chromium($ud, "Top Sites"),
            Chromium($ud, "Top Sites-journal"),
            Chromium($ud, "Shortcuts"),
            Chromium($ud, "Shortcuts-journal"),
        ]
    };
}

#[cfg(windows)]
macro_rules! chromium_session {
    ($ud:literal) => {
        &[
            Chromium($ud, "Sessions"),
            Chromium($ud, "Current Session"),
            Chromium($ud, "Current Tabs"),
            Chromium($ud, "Last Session"),
            Chromium($ud, "Last Tabs"),
        ]
    };
}

/// Cache, cookies, history and session rules for one Chromium browser.
#[cfg(windows)]
macro_rules! chromium_browser {
    ($prefix:literal, $app:literal, $exe:literal, $cache:expr, $ud:literal) => {
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
                close: &[$exe],
                targets: $cache,
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
                close: &[$exe],
                targets: chromium_cookies!($ud),
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
                close: &[$exe],
                targets: chromium_history!($ud),
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
                close: &[$exe],
                targets: chromium_session!($ud),
            },
        ]
    };
}

#[cfg(windows)]
const CHROME: [Rule; 4] = chromium_browser!(
    "chrome",
    "Google Chrome",
    "chrome.exe",
    chromium_cache!(r"%LOCALAPPDATA%\Google\Chrome\User Data"),
    r"%LOCALAPPDATA%\Google\Chrome\User Data"
);
#[cfg(windows)]
const EDGE: [Rule; 4] = chromium_browser!(
    "edge",
    "Microsoft Edge",
    "msedge.exe",
    chromium_cache!(r"%LOCALAPPDATA%\Microsoft\Edge\User Data"),
    r"%LOCALAPPDATA%\Microsoft\Edge\User Data"
);
#[cfg(windows)]
const BRAVE: [Rule; 4] = chromium_browser!(
    "brave",
    "Brave",
    "brave.exe",
    chromium_cache!(r"%LOCALAPPDATA%\BraveSoftware\Brave-Browser\User Data"),
    r"%LOCALAPPDATA%\BraveSoftware\Brave-Browser\User Data"
);
#[cfg(windows)]
const VIVALDI: [Rule; 4] = chromium_browser!(
    "vivaldi",
    "Vivaldi",
    "vivaldi.exe",
    chromium_cache!(r"%LOCALAPPDATA%\Vivaldi\User Data"),
    r"%LOCALAPPDATA%\Vivaldi\User Data"
);
#[cfg(windows)]
const OPERA: [Rule; 4] = chromium_browser!(
    "opera",
    "Opera",
    "opera.exe",
    chromium_cache!(r"%LOCALAPPDATA%\Opera Software\Opera Stable", r"%APPDATA%\Opera Software\Opera Stable"),
    r"%APPDATA%\Opera Software\Opera Stable"
);
#[cfg(windows)]
const OPERA_GX: [Rule; 4] = chromium_browser!(
    "operagx",
    "Opera GX",
    "opera.exe",
    chromium_cache!(r"%LOCALAPPDATA%\Opera Software\Opera GX Stable", r"%APPDATA%\Opera Software\Opera GX Stable"),
    r"%APPDATA%\Opera Software\Opera GX Stable"
);
#[cfg(windows)]
const CHROMIUM: [Rule; 4] = chromium_browser!(
    "chromium",
    "Chromium",
    "chrome.exe",
    chromium_cache!(r"%LOCALAPPDATA%\Chromium\User Data"),
    r"%LOCALAPPDATA%\Chromium\User Data"
);

#[cfg(windows)]
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
        close: &["firefox.exe"],
        targets: &[
            EachDir(r"%LOCALAPPDATA%\Mozilla\Firefox\Profiles", "cache2"),
            EachDir(r"%LOCALAPPDATA%\Mozilla\Firefox\Profiles", "startupCache"),
            EachDir(r"%LOCALAPPDATA%\Mozilla\Firefox\Profiles", "thumbnails"),
            EachDir(r"%LOCALAPPDATA%\Mozilla\Firefox\Profiles", "jumpListCache"),
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
        close: &["firefox.exe"],
        targets: &[
            EachDir(r"%APPDATA%\Mozilla\Firefox\Profiles", "cookies.sqlite"),
            EachDir(r"%APPDATA%\Mozilla\Firefox\Profiles", "cookies.sqlite-wal"),
            EachDir(r"%APPDATA%\Mozilla\Firefox\Profiles", "cookies.sqlite-shm"),
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
        close: &["firefox.exe"],
        targets: &[EachDir(r"%APPDATA%\Mozilla\Firefox\Profiles", "formhistory.sqlite")],
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
        close: &["firefox.exe"],
        targets: &[
            EachDir(r"%APPDATA%\Mozilla\Firefox\Profiles", "sessionstore.jsonlz4"),
            EachDir(r"%APPDATA%\Mozilla\Firefox\Profiles", "sessionstore-backups"),
        ],
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
        close: &["firefox.exe"],
        targets: &[
            Path(r"%APPDATA%\Mozilla\Firefox\Crash Reports\submitted"),
            Path(r"%APPDATA%\Mozilla\Firefox\Crash Reports\pending"),
            EachDir(r"%APPDATA%\Mozilla\Firefox\Profiles", "minidumps"),
        ],
    },
];

#[cfg(windows)]
const WINDOWS: [Rule; 14] = [
    Rule {
        id: "win.temp",
        group: System,
        app: "Windows",
        name: "Temporary files",
        about: "Files programs left in your temp folder. Only files untouched for 24 hours are removed, so running installers aren't disturbed.",
        default_on: true,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Older(r"%TEMP%", 24)],
    },
    Rule {
        id: "win.systemp",
        group: System,
        app: "Windows",
        name: "Windows temporary files",
        about: "The system-wide temp folder (files untouched for 24 hours).",
        default_on: true,
        admin: true,
        warning: None,
        close: &[],
        targets: &[Older(r"%SystemRoot%\Temp", 24)],
    },
    Rule {
        id: "win.recycle",
        group: System,
        app: "Windows",
        name: "Recycle Bin",
        about: "Everything in the Recycle Bin on every drive.",
        default_on: false,
        admin: false,
        warning: Some("Permanently deletes everything in the Recycle Bin."),
        close: &[],
        targets: &[Special(RecycleBin)],
    },
    Rule {
        id: "win.thumbs",
        group: System,
        app: "Windows",
        name: "Thumbnail cache",
        about: "Explorer's picture and video previews. Rebuilt as you browse folders; files Explorer has open are skipped.",
        default_on: true,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Glob(r"%LOCALAPPDATA%\Microsoft\Windows\Explorer", "thumbcache_*.db")],
    },
    Rule {
        id: "win.inetcache",
        group: System,
        app: "Windows",
        name: "Internet cache",
        about: "Web cache used by Windows components, Internet Explorer mode and older apps.",
        default_on: true,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Path(r"%LOCALAPPDATA%\Microsoft\Windows\INetCache")],
    },
    Rule {
        id: "win.reports",
        group: System,
        app: "Windows",
        name: "Error reports & crash dumps",
        about: "Problem reports and application crash dumps for your account.",
        default_on: true,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Path(r"%LOCALAPPDATA%\Microsoft\Windows\WER"), Path(r"%LOCALAPPDATA%\CrashDumps")],
    },
    Rule {
        id: "win.sysreports",
        group: System,
        app: "Windows",
        name: "System error reports",
        about: "Windows Error Reporting archives and queues for the whole PC.",
        default_on: true,
        admin: true,
        warning: None,
        close: &[],
        targets: &[
            Path(r"%ProgramData%\Microsoft\Windows\WER\ReportArchive"),
            Path(r"%ProgramData%\Microsoft\Windows\WER\ReportQueue"),
            Path(r"%ProgramData%\Microsoft\Windows\WER\Temp"),
        ],
    },
    Rule {
        id: "win.memdumps",
        group: System,
        app: "Windows",
        name: "Memory dumps",
        about: "Crash dumps Windows writes after a blue screen or a driver hang. Often several GB.",
        default_on: true,
        admin: true,
        warning: Some("Keep them if you're troubleshooting crashes."),
        close: &[],
        targets: &[
            Path(r"%SystemRoot%\MEMORY.DMP"),
            Path(r"%SystemRoot%\Minidump"),
            Path(r"%SystemRoot%\LiveKernelReports"),
        ],
    },
    Rule {
        id: "win.delivery",
        group: System,
        app: "Windows",
        name: "Delivery Optimization cache",
        about: "Update files Windows keeps to share with other PCs.",
        default_on: true,
        admin: true,
        warning: None,
        close: &[],
        targets: &[Path(
            r"%SystemRoot%\ServiceProfiles\NetworkService\AppData\Local\Microsoft\Windows\DeliveryOptimization\Cache",
        )],
    },
    Rule {
        id: "win.logs",
        group: System,
        app: "Windows",
        name: "Setup & servicing logs",
        about: "Component servicing (CBS) and DISM logs older than a week.",
        default_on: true,
        admin: true,
        warning: None,
        close: &[],
        targets: &[Older(r"%SystemRoot%\Logs\CBS", 168), Older(r"%SystemRoot%\Logs\DISM", 168)],
    },
    Rule {
        id: "win.update",
        group: System,
        app: "Windows",
        name: "Windows Update downloads",
        about: "Update packages already downloaded. Also a common fix for stuck updates.",
        default_on: false,
        admin: true,
        warning: Some("Updates that are downloaded but not installed yet will download again."),
        close: &[],
        targets: &[Path(r"%SystemRoot%\SoftwareDistribution\Download")],
    },
    Rule {
        id: "win.dxcache",
        group: System,
        app: "Windows",
        name: "DirectX shader cache",
        about: "Compiled graphics shaders. Rebuilt as games and apps need them.",
        default_on: true,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Path(r"%LOCALAPPDATA%\D3DSCache")],
    },
    Rule {
        id: "win.gpucache",
        group: System,
        app: "Windows",
        name: "GPU driver shader caches",
        about: "NVIDIA, AMD and Intel driver shader caches.",
        default_on: false,
        admin: false,
        warning: Some("Games may stutter briefly while shaders rebuild."),
        close: &[],
        targets: &[
            Path(r"%LOCALAPPDATA%\NVIDIA\DXCache"),
            Path(r"%LOCALAPPDATA%\NVIDIA\GLCache"),
            Path(r"%LOCALLOW%\NVIDIA\PerDriverVersion\DXCache"),
            Path(r"%LOCALAPPDATA%\AMD\DxCache"),
            Path(r"%LOCALAPPDATA%\AMD\DxcCache"),
            Path(r"%LOCALAPPDATA%\AMD\GLCache"),
            Path(r"%LOCALAPPDATA%\AMD\VkCache"),
            Path(r"%LOCALAPPDATA%\Intel\ShaderCache"),
        ],
    },
    Rule {
        id: "win.nvidia",
        group: System,
        app: "Windows",
        name: "NVIDIA driver downloads",
        about: "Driver installers the NVIDIA app downloaded and no longer needs.",
        default_on: true,
        admin: true,
        warning: None,
        close: &[],
        targets: &[Path(r"%ProgramData%\NVIDIA Corporation\Downloader")],
    },
];

#[cfg(windows)]
const APPS: [Rule; 11] = [
    Rule {
        id: "app.discord",
        group: Apps,
        app: "Discord",
        name: "Cache",
        about: "Images, videos and scripts Discord downloaded.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["discord.exe"],
        targets: chromium_cache!(r"%APPDATA%\discord"),
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
        close: &["slack.exe"],
        targets: chromium_cache!(r"%APPDATA%\Slack"),
    },
    Rule {
        id: "app.teams",
        group: Apps,
        app: "Microsoft Teams",
        name: "Cache",
        about: "Web cache for new and classic Teams.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["ms-teams.exe", "teams.exe"],
        targets: chromium_cache!(
            r"%LOCALAPPDATA%\Packages\MSTeams_8wekyb3d8bbwe\LocalCache\Microsoft\MSTeams\EBWebView",
            r"%APPDATA%\Microsoft\Teams"
        ),
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
        close: &["spotify.exe"],
        targets: &[
            Path(r"%LOCALAPPDATA%\Spotify\Data"),
            Path(r"%LOCALAPPDATA%\Spotify\Browser\Cache"),
            Path(r"%LOCALAPPDATA%\Packages\SpotifyAB.SpotifyMusic_zpdnekdrzrea0\LocalCache\Spotify\Data"),
        ],
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
        close: &["code.exe"],
        targets: &[
            Path(r"%APPDATA%\Code\Cache"),
            Path(r"%APPDATA%\Code\CachedData"),
            Path(r"%APPDATA%\Code\Code Cache"),
            Path(r"%APPDATA%\Code\GPUCache"),
            Path(r"%APPDATA%\Code\CachedExtensionVSIXs"),
            Path(r"%APPDATA%\Code\Service Worker\CacheStorage"),
            Older(r"%APPDATA%\Code\logs", 168),
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
        close: &["cursor.exe"],
        targets: &[
            Path(r"%APPDATA%\Cursor\Cache"),
            Path(r"%APPDATA%\Cursor\CachedData"),
            Path(r"%APPDATA%\Cursor\Code Cache"),
            Path(r"%APPDATA%\Cursor\GPUCache"),
            Path(r"%APPDATA%\Cursor\CachedExtensionVSIXs"),
            Path(r"%APPDATA%\Cursor\Service Worker\CacheStorage"),
            Older(r"%APPDATA%\Cursor\logs", 168),
        ],
    },
    Rule {
        id: "app.steam",
        group: Apps,
        app: "Steam",
        name: "Web cache, dumps & old logs",
        about: "The Steam client's web cache, crash dumps and logs older than a week. Games are not touched.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["steam.exe", "steamwebhelper.exe"],
        targets: &[
            Path(r"%STEAM%\appcache\httpcache"),
            Path(r"%STEAM%\dumps"),
            Older(r"%STEAM%\logs", 168),
            Path(r"%LOCALAPPDATA%\Steam\htmlcache"),
        ],
    },
    Rule {
        id: "app.epic",
        group: Apps,
        app: "Epic Games Launcher",
        name: "Web cache & old logs",
        about: "The launcher's web cache and logs older than a week. Games are not touched.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["epicgameslauncher.exe"],
        targets: &[
            Glob(r"%LOCALAPPDATA%\EpicGamesLauncher\Saved", "webcache*"),
            Older(r"%LOCALAPPDATA%\EpicGamesLauncher\Saved\Logs", 168),
        ],
    },
    Rule {
        id: "app.java",
        group: Apps,
        app: "Java",
        name: "Deployment cache",
        about: "Applets and Web Start programs Java downloaded.",
        default_on: true,
        admin: false,
        warning: None,
        close: &["javaw.exe", "javaws.exe"],
        targets: &[Path(r"%LOCALLOW%\Sun\Java\Deployment\cache")],
    },
    Rule {
        id: "app.adobe",
        group: Apps,
        app: "Adobe",
        name: "Media cache",
        about: "Conformed audio and preview files Premiere Pro, After Effects and Media Encoder create.",
        default_on: false,
        admin: false,
        warning: Some("Projects take a while to re-conform the next time you open them."),
        close: &["adobe premiere pro.exe", "afterfx.exe", "adobe media encoder.exe"],
        targets: &[
            Path(r"%APPDATA%\Adobe\Common\Media Cache Files"),
            Path(r"%APPDATA%\Adobe\Common\Media Cache"),
        ],
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
        close: &[
            "idea64.exe",
            "pycharm64.exe",
            "rider64.exe",
            "webstorm64.exe",
            "clion64.exe",
            "goland64.exe",
            "rustrover64.exe",
            "phpstorm64.exe",
            "datagrip64.exe",
            "rubymine64.exe",
        ],
        targets: &[EachDir(r"%LOCALAPPDATA%\JetBrains", "caches")],
    },
];

#[cfg(windows)]
const DEVELOPER: [Rule; 11] = [
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
        targets: &[
            Path(r"%LOCALAPPDATA%\npm-cache\_cacache"),
            Path(r"%LOCALAPPDATA%\npm-cache\_npx"),
            Path(r"%LOCALAPPDATA%\npm-cache\_logs"),
        ],
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
        targets: &[Path(r"%LOCALAPPDATA%\Yarn\Cache"), Path(r"%LOCALAPPDATA%\Yarn\Berry\cache")],
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
        targets: &[Path(r"%USERPROFILE%\.bun\install\cache")],
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
        targets: &[Path(r"%LOCALAPPDATA%\pip\Cache")],
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
        close: &["cargo.exe"],
        targets: &[
            Path(r"%CARGO_HOME%\registry\cache"),
            Path(r"%CARGO_HOME%\registry\src"),
            Path(r"%CARGO_HOME%\git\checkouts"),
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
        close: &["go.exe"],
        targets: &[Path(r"%LOCALAPPDATA%\go-build")],
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
        targets: &[Path(r"%GRADLE_USER_HOME%\caches")],
    },
    Rule {
        id: "dev.nuget",
        group: Developer,
        app: "NuGet",
        name: "HTTP & plugin caches",
        about: "NuGet's download caches. The global packages folder is left alone.",
        default_on: false,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Path(r"%LOCALAPPDATA%\NuGet\v3-cache"), Path(r"%LOCALAPPDATA%\NuGet\plugins-cache")],
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
        close: &["deno.exe"],
        targets: &[
            Path(r"%LOCALAPPDATA%\deno\remote"),
            Path(r"%LOCALAPPDATA%\deno\deps"),
            Path(r"%LOCALAPPDATA%\deno\npm"),
            Path(r"%LOCALAPPDATA%\deno\gen"),
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
        targets: &[Path(r"%LOCALAPPDATA%\electron\Cache"), Path(r"%LOCALAPPDATA%\electron-builder\Cache")],
    },
    Rule {
        id: "dev.composer",
        group: Developer,
        app: "Composer",
        name: "Package cache",
        about: "PHP packages Composer downloaded.",
        default_on: false,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Path(r"%LOCALAPPDATA%\Composer")],
    },
];

#[cfg(windows)]
const PRIVACY: [Rule; 5] = [
    Rule {
        id: "priv.recent",
        group: Privacy,
        app: "Windows",
        name: "Recent items",
        about: "Shortcuts to recently opened files (Quick access › Recent). Pinned jump-list items are kept.",
        default_on: false,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Glob(r"%APPDATA%\Microsoft\Windows\Recent", "*.lnk")],
    },
    Rule {
        id: "priv.runmru",
        group: Privacy,
        app: "Windows",
        name: "Run dialog history",
        about: "Commands typed into Win+R.",
        default_on: false,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Special(RegistryValues(r"Software\Microsoft\Windows\CurrentVersion\Explorer\RunMRU"))],
    },
    Rule {
        id: "priv.typedpaths",
        group: Privacy,
        app: "Windows",
        name: "Explorer address bar history",
        about: "Paths typed into File Explorer's address bar.",
        default_on: false,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Special(RegistryValues(r"Software\Microsoft\Windows\CurrentVersion\Explorer\TypedPaths"))],
    },
    Rule {
        id: "priv.clipboard",
        group: Privacy,
        app: "Windows",
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
        app: "Windows",
        name: "DNS cache",
        about: "Recently looked-up website addresses. Frees no disk space.",
        default_on: false,
        admin: false,
        warning: None,
        close: &[],
        targets: &[Special(DnsCache)],
    },
];

/// All rules for this OS, in display order.
pub fn all() -> &'static [Rule] {
    use std::sync::OnceLock;
    static ALL: OnceLock<Vec<Rule>> = OnceLock::new();
    ALL.get_or_init(catalog)
}

#[cfg(windows)]
fn catalog() -> Vec<Rule> {
    let mut v = Vec::new();
    v.extend(WINDOWS);
    v.extend(CHROME);
    v.extend(EDGE);
    v.extend(FIREFOX);
    v.extend(BRAVE);
    v.extend(OPERA);
    v.extend(OPERA_GX);
    v.extend(VIVALDI);
    v.extend(CHROMIUM);
    v.extend(APPS);
    v.extend(DEVELOPER);
    v.extend(PRIVACY);
    v
}

#[cfg(unix)]
fn catalog() -> Vec<Rule> {
    super::rules_unix::catalog()
}

pub fn by_id(id: &str) -> Option<usize> {
    all().iter().position(|r| r.id == id)
}
