# Changelog

Notable changes to Heft. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[semantic versioning](https://semver.org/).

## [Unreleased]

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
