# Security

Heft reads raw disk metadata (as administrator on Windows), deletes files and
edits the Windows registry. Bugs in those areas can destroy data, so they're
treated as security issues.

## What to report privately

- Anything that deletes, moves or changes files or registry entries the user
  didn't confirm, or that goes beyond what the rule or dialog described.
- Any way to make Heft follow a symlink or junction while deleting.
- Problems with elevation: "Restart as administrator", the MFT reader, or the
  scheduled weekly clean task.
- Anything that sends data off the machine. Heft shouldn't do that at all; the
  only network access is winget, when the user asks for updates.

Crashes, wrong sizes and UI bugs can go in public issues.

## How to report

Use GitHub's private vulnerability reporting: open the **Security** tab of the
repository and choose **Report a vulnerability**. Include your OS, the Heft
version and steps to reproduce. Please don't open a public issue until a fix
is released.

Expect a reply within a week. Fixes ship in the next release, and the release
notes credit the reporter unless they'd rather not be named.

## Supported versions

Only the latest release receives fixes.
