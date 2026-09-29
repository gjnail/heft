# Changelog

Notable changes to Heft. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[semantic versioning](https://semver.org/).

## [Unreleased]

First public version.

### Disk usage (Windows, macOS, Linux)

- Treemap with zooming and hover details. Colors by file type, category, age,
  or growth since an earlier scan.
- Folder tree, file type breakdown, and a largest files list with name and age
  filters.
- Duplicate finder that compares size, then sampled content, then a full hash,
  and ignores hard links.
- Build junk finder for `node_modules`, Cargo `target` and engine caches. It
  only lists folders whose project confirms what they are.
- Scan history: a snapshot after each scan, and a Changes tab that shows which
  folders grew.
- Delete to the Recycle Bin or Trash, with a warning for system folders.
- Hard links counted once; online-only cloud files never read; virtual file
  systems and second paths to the same folder skipped.
- MFT scanner on Windows (administrator, NTFS); parallel directory scanners
  elsewhere.

### Maintenance (Windows)

- Junk cleaner based on a catalog of exact locations.
- Startup programs: Run keys, Startup folders, and logon/boot scheduled tasks,
  switched off the same way Task Manager does it.
- Installed programs: sizes, uninstall, a leftover check, and updates through
  winget ("Update all" asks first). Leftover suggestions are checked against
  the programs still installed. A folder is never taken from a launcher link
  (such as a Steam game's uninstall command), never shared with or nested
  inside another program, and only the installer's recorded folder is
  pre-selected.
- Registry issues: a narrow check for entries pointing at missing files, with
  a `.reg` backup before every change. Only uninstall, startup, App Paths and
  shared DLL issues are pre-selected. An entry only counts as missing on a
  real "not found" error; "access denied" counts as unknown.

### Command line

- `--bench`, `--compare`, `--render`, `--icon`, and `--clean` (Windows).
