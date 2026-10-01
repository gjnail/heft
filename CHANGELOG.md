# Changelog

Notable changes to Heft. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[semantic versioning](https://semver.org/).

## [Unreleased]

## [1.1.0] - 2026-09-30

The Mac version catches up with Windows: every page Windows has now has a Mac
counterpart, tested on an Apple silicon Mac.

### Added

- macOS: Hardware monitor. Temperatures, loads, clocks and power for Apple
  silicon's performance and efficiency cores and GPU, the Neural Engine and
  memory, fan speeds and whole-system power, drive temperature and SSD wear,
  memory, network and battery, all without an administrator password.
  `heft --sensors` works on macOS too.
- macOS: Login items page, the counterpart of Startup. Login items and launch
  agents and daemons, with launchd's own on/off switch, removal with a backup,
  who signed each program, and helpers inside apps for reference.
- macOS: Apps page, the counterpart of Programs. Installed apps with measured
  sizes, publisher, date added and last opened. Uninstalls through the app's
  own uninstaller, Homebrew or the Trash, then checks `~/Library` and
  `/Library` for leftovers. Updates through Homebrew, one app or all of them,
  in a Terminal window.
- macOS: Broken items page, the counterpart of the registry check. Launch
  agents, login items, command-line links, Open With entries and package
  receipts that point at apps and files that no longer exist, with a backup
  before every change and a Backups list that restores them.
- macOS: free space alerts can keep watching from the menu bar after the
  window is closed, and start at login (a launch agent in
  `~/Library/LaunchAgents`). The menu shows each drive's free space.
  Notifications go through Notification Center when Heft runs from Heft.app,
  and clicking one opens Heft. `heft --tray` starts hidden in the menu bar.
- macOS: quick rescans and View › Update automatically, through FSEvents:
  after a scan of a local disk, Rescan only lists the folders that changed,
  and automatic updates start as soon as macOS reports a change.
  `heft --check-refresh` works on macOS and gains `--wait`.
- macOS: Compress and Uncompress folders on APFS and HFS+, with compression
  suggestions for apps and Steam games. Each file is checked against the
  original before it's swapped in. Apps installed for all users (App Store,
  installer packages) can be included after the administrator password.
- macOS: the Removed tab can put items back from the Trash. Heft now moves
  things to the Trash itself instead of asking Finder, so it no longer needs
  permission to control Finder.
- macOS: suggestions for old macOS installers, iPhone and iPad software
  downloads and backups, Time Machine's local snapshots, the hibernation image,
  Docker Desktop's disk, Messages attachments, Xcode archives, Mail downloads
  and Photos libraries (explained, with the right place to deal with each),
  and warnings for more Mac locations (Mail, Messages, music libraries,
  iCloud and other cloud folders, app bundles).
- macOS Cleaner: temporary files, Quick Look thumbnails, Metal shader caches,
  iPhone and iPad updates, system logs (administrator password), Docker build
  cache, and privacy rules for recent items, Finder's recent folders, the
  clipboard and the DNS cache. Teams, Epic Games Launcher, Java, Adobe, NuGet,
  Deno and Composer caches. Weekly cleaning with a launch agent, and shortcuts
  to Storage settings and to thinning Time Machine's local snapshots.
- Cleaner on macOS and Linux: cookies, history, form history and last-session
  rules for Chromium browsers and Firefox, as on Windows.
- macOS Cleaner: Safari's cookies, history and last session, and new Teams'
  cache. macOS protects them, so they're only listed when Heft has Full Disk
  Access. Safari's history rule warns that iCloud syncs it back.
- macOS: Remove download for iCloud Drive files and folders, in the
  right-click menu and as a suggestion for big files that haven't changed in
  a month. The files stay in iCloud and download again when opened, as with
  Finder's Remove Download. Only files iCloud has finished syncing are
  touched.
- macOS Apps: Leftovers of deleted apps. Data containers and files in
  `~/Library` from apps that are no longer installed anywhere on the Mac,
  found by bundle id only and checked against every app, extension and
  helper macOS knows. Nothing is ticked by default.
- `heft --sensors --report` adds the Mac's model, chip and macOS version and
  the raw SMC and HID sensor data, for fixing readings on Macs Heft hasn't
  been tested on. The bug report form asks for it.
- macOS: menus at the top of the screen. Heft (About, Settings… ⌘,), File
  (Scan Folder… ⌘O, Rescan ⌘R, Close Window), View (each page with ⌘1 to
  ⌘6, Dark Mode, Enter Full Screen), Window and Help.
- Settings: appearance, free space alerts and starting at login, and on
  macOS moving to the Trash through Finder, update checks and Full Disk
  Access.
- macOS: Finder's Put Back for what Heft moves to the Trash, when Settings ›
  Move items to the Trash through Finder is on (macOS asks once to let Heft
  control Finder). Heft's own Removed tab puts items back either way.
- macOS Apps: App Store updates through `mas` when it's installed. Apps with
  their own updater (Sparkle, Squirrel) say "updates itself" and have Open to
  update; only if it's turned on in Settings, Heft asks Sparkle apps' update
  feeds for newer versions.
- macOS Login items: Show all background items lists everything macOS tracks
  in Login Items & Extensions (`sfltool dumpbtm`, after the administrator
  password), with whether each is allowed in the background.
- macOS Broken items: Dock icons for apps and folders that no longer exist,
  with a backup of each icon. Apps that were only moved don't count, since
  the Dock follows them.
- macOS: purgeable space. The start screen and the status bar show how much
  more macOS can free by itself (local Time Machine snapshots, downloaded
  iCloud files, caches), which Finder counts as available, so the two
  figures no longer seem to disagree.
- macOS: a note on the start screen explains what Heft can't see without Full
  Disk Access, with a button to the right settings page. It goes away once
  access is given, or when dismissed.
- macOS: the start screen links to Login items and Apps as well as the
  Cleaner.
- A button in the top-right corner switches between light and dark mode: a
  moon in light mode, a sun in dark mode. Heft remembers the choice; until
  it's used, Heft follows the system theme.

### Changed

- Suggestions no longer pre-select anything that carries a warning.
- Cleaner: files a program has open are skipped on macOS and Linux too, and
  everything that needs a password is done after one prompt per clean
  (`pkexec` on Linux, the administrator prompt on macOS).
- Disk images, installer packages and `.ipsw` files are skipped when
  compressing; `.hds` and `Docker.raw` count as virtual disks.
- macOS: quick rescans no longer keep a second copy of the scan in memory,
  roughly halving Heft's memory use after scanning a whole disk.
- macOS Apps: launch agents and daemons moved to the Trash with an app's
  leftovers are stopped too, instead of running until the next restart.
  Daemons are stopped under the same password prompt as the move.

### Fixed

- macOS Apps: looking for an app's leftovers could make macOS ask to let Heft
  access data from other apps. Without Full Disk Access, other apps' data
  containers are now listed without looking inside them or measuring them.
- macOS: Start at login and weekly cleaning stopped working when Heft.app was
  moved. Heft now points them at its new place when it's next opened.
- macOS: the startup disk wasn't listed on the start screen or in Scan drive,
  and free space alerts watched no drive at all, on macOS 11 and later
  (the system volume is mounted from a snapshot).
- A Unity project's `Library` folder was flagged as the Mac's Library folder.

## [1.0.1] - 2026-09-29

### Fixed

- File types tab: the percentage no longer overlaps the size.
- The downloads now include the licenses of the libraries Heft is built with
  (`THIRD-PARTY-LICENSES.txt`) and, on Windows, of the PawnIO modules
  (`PAWNIO-LICENSE.txt`). On macOS they're in the app's Resources folder.

## [1.0.0] - 2026-09-29

First public version.

### Disk usage (Windows, macOS, Linux)

- Treemap of every file with cushion shading, zooming, arrow-key navigation,
  hover details and names on large rectangles. Color by file type, category,
  age, or growth since an earlier scan; size by file size or space on disk.
- Folder tree with sizes, percentages, file counts and dates, and a breakdown
  by file type that highlights matching files in the treemap.
- Suggestions page: old installers and repeated downloads, build folders in
  inactive projects, Steam games not played in a year, big logs and crash
  dumps, big old files, Windows.old and hibernation files, a full Recycle
  Bin, large virtual disks, the Cleaner's temporary files, and programs worth
  compressing. Each says why and how to deal with it, and only clearly
  disposable items are pre-selected.
- Search the whole scan by name, extension or pattern, size, age and type.
  With no search text it lists the largest files.
- Duplicate finder that compares size, then sampled content, then a full hash,
  and ignores hard links. Extra copies can be recycled, or replaced with hard
  links or with copy-on-write clones (ReFS, APFS, Btrfs, XFS) after a fresh
  byte-for-byte comparison.
- Build junk finder for `node_modules`, JavaScript build caches, Rust
  `target` and other `CACHEDIR.TAG` folders, .NET, Gradle, Python test
  environments, and Unity, Unreal and Godot caches. It only lists folders
  whose project confirms what they are.
- Scan history: a snapshot after each scan, and a Changes tab with the folders
  that grew or shrank and a chart of a folder's size over time.
- Delete to the Recycle Bin or Trash, with plain-language warnings for system
  files, installed programs, app data, cloud folders, keys, mailboxes and
  saved games in the tree, tooltips, right-click menu and delete dialog.
  Deleting anything that could stop the computer from starting needs an extra
  confirmation.
- Removed page: everything Heft moved to the Recycle Bin or Trash, with
  restore on Windows and Linux.
- Move a folder to another drive and leave a junction or symbolic link. Every
  file is checked against the original before the swap.
- Export to CSV or JSON from the Export menu, `heft --export`, and
  `heft --bench --json`.
- Free space alerts in the window and as desktop notifications. On Windows,
  Heft can stay in the notification area after closing and start with
  Windows.
- Accurate sizes: hard links counted once, online-only cloud files never
  read, virtual file systems and second paths to the same folder skipped.
- Screen reader support through AccessKit.
- Windows: MFT scanner (administrator, NTFS). Rescan reads the NTFS change
  journal instead of scanning again, and View > Update automatically keeps
  the map current. Compress rarely changed folders the same way as
  `compact /EXE`.
- macOS and Linux: parallel directory scanners (`getattrlistbulk` on macOS).
  The MFT record parser is fuzzed in CI.

### Cleaner (Windows, macOS, Linux)

- Junk cleaner based on a catalog of exact locations for the system,
  browsers, applications and developer tools, with optional privacy items on
  Windows. It shows exactly which files each rule would remove before
  cleaning, skips files in use and rules whose program is running, and never
  follows links.
- Linux: system caches are cleaned by their own tools (apt, dnf, journalctl,
  snap, flatpak) through `pkexec`. macOS: Homebrew, Xcode and CocoaPods
  caches, old logs and the Trash.
- Windows: weekly cleaning through a scheduled task, compacting WSL 2 and
  Docker Desktop virtual disks, and shortcuts to Storage Sense, Disk Cleanup
  and DISM component store cleanup.

### Hardware monitor (Windows, Linux)

- A dashboard of CPU and GPU temperature, load and power, memory and the
  hottest drive, a card per device, and a table of every sensor with value,
  minimum, maximum, average and a two-minute trend.
- Click any reading for a history chart (1, 5 or 10 minutes); Ctrl-click to
  compare readings of the same kind. Log all readings to CSV. Celsius or
  Fahrenheit, and a choice of update interval.
- Windows without extra software: per-core load and clocks, NVIDIA cards
  through NVML, other graphics cards through the graphics kernel, drive
  temperatures (SMART for older SATA drives when run as administrator),
  disk and network throughput, memory and batteries.
- Windows with PawnIO installed and Heft running as administrator: CPU
  temperature and package power (AMD Zen, Intel), and fans, fan drive,
  voltages and board temperatures from Nuvoton NCT679x and NCT6701D chips.
  Heft never installs the driver itself.
- Linux: the same page from `/proc` and hwmon, with no extra driver.

### Windows tools

- Startup programs: Run keys, Startup folders, and logon/boot scheduled tasks,
  switched off the same way Task Manager does it, or removed with a backup.
- Installed programs: measured sizes, uninstall, a leftover check, and
  updates through winget ("Update all" asks first). Leftover suggestions are
  checked against the programs still installed. A folder is never taken from
  a launcher link (such as a Steam game's uninstall command), never shared
  with or nested inside another program, and only the installer's recorded
  folder is pre-selected.
- Registry issues: a narrow check for entries pointing at missing files, with
  a `.reg` backup before every change and a Backups list to restore them.
  Only uninstall, startup, App Paths and shared DLL issues are pre-selected.
  An entry only counts as missing on a real "not found" error; "access
  denied" counts as unknown.

### Command line

- `--bench`, `--export`, `--render`, `--clean` and `--icon` everywhere;
  `--sensors` on Windows and Linux; `--compare`, `--check-refresh` and
  `--tray` on Windows.
