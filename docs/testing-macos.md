# Testing Heft on a Mac by hand

The automated tests cover parsing, planning and every step that can run
without changing the Mac. What's left needs a person: anything behind the
administrator password, changes to login items, Dock, Finder and iCloud,
notifications, and the Start at login and weekly cleaning agents. This list
walks through each on throwaway items, so nothing you use is touched.

Tick items off as you go. Each section ends with how to clean up. Commands go
in Terminal; the ones with `sudo` ask for your password there.

## Before you start

- [ ] Build the app and put it in Applications, so notifications, the menu
      bar icon and Start at login behave as they will for people:

      ```
      bash scripts/bundle-macos.sh
      ditto target/Heft.app /Applications/Heft.app
      ```

- [ ] Open `/Applications/Heft.app`. Check the Heft, File, View, Window and
      Help menus at the top of the screen, and that ⌘, opens Settings, ⌘1 to
      ⌘6 switch pages, ⌘O asks for a folder, and View › Dark Mode is ticked
      when dark mode is on.
- [ ] Make a throwaway app with its own bundle id. It shows a dialog when
      opened and does nothing else:

      ```
      osacompile -o "/Applications/Heft Test.app" -e 'display dialog "Heft test app"'
      plutil -replace CFBundleIdentifier -string com.heftdev.test "/Applications/Heft Test.app/Contents/Info.plist"
      codesign --force --sign - "/Applications/Heft Test.app"
      ```

Backups of everything Heft changes are in
`~/Library/Application Support/Heft/backups`.

## Login items

- [ ] Add the test app to your login items:

      ```
      osascript -e 'tell application "System Events" to make login item at end with properties {path:"/Applications/Heft Test.app", hidden:false}'
      ```

- [ ] Login items page: Heft Test is listed and ticked. Untick it: it
      disappears from System Settings › General › Login Items & Extensions.
      Tick it again: it's back.
- [ ] Remove it with the bin button: it's gone, and the toast names the
      backup. Broken items › Backups › Restore… puts it back.
- [ ] Add a user launch agent that keeps running:

      ```
      mkdir -p ~/Library/LaunchAgents
      cat > ~/Library/LaunchAgents/com.heftdev.test.agent.plist <<'EOF'
      <?xml version="1.0" encoding="UTF-8"?>
      <plist version="1.0"><dict>
      <key>Label</key><string>com.heftdev.test.agent</string>
      <key>ProgramArguments</key><array><string>/bin/sleep</string><string>86400</string></array>
      <key>RunAtLoad</key><true/>
      <key>AssociatedBundleIdentifiers</key><string>com.heftdev.test</string>
      </dict></plist>
      EOF
      launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.heftdev.test.agent.plist
      ```

- [ ] Refresh: the agent shows as running. Untick it, then check
      `launchctl print-disabled gui/$(id -u) | grep heftdev` says disabled.
      Tick it again: enabled.
- [ ] Press **Show all background items…**: macOS asks for your password,
      then the list matches System Settings › Login Items & Extensions,
      with allowed and not allowed marked the same way.

Leave the agent in place for the Apps section.

## Launch daemons (administrator password)

- [ ] Add a throwaway daemon:

      ```
      sudo tee /Library/LaunchDaemons/com.heftdev.test.daemon.plist >/dev/null <<'EOF'
      <?xml version="1.0" encoding="UTF-8"?>
      <plist version="1.0"><dict>
      <key>Label</key><string>com.heftdev.test.daemon</string>
      <key>ProgramArguments</key><array><string>/bin/sleep</string><string>86400</string></array>
      <key>RunAtLoad</key><true/>
      </dict></plist>
      EOF
      sudo launchctl bootstrap system /Library/LaunchDaemons/com.heftdev.test.daemon.plist
      ```

- [ ] Login items page: untick the daemon. macOS asks for your password, and
      `sudo launchctl print-disabled system | grep heftdev` says disabled.
      Tick it again.
- [ ] Remove it: one password prompt, the file is gone from
      `/Library/LaunchDaemons`, and `sudo launchctl print system/com.heftdev.test.daemon`
      says it isn't loaded. Restore it from Broken items › Backups (password
      again) and check the file is back, owned by root.
- [ ] Clean up: `sudo launchctl bootout system/com.heftdev.test.daemon; sudo rm /Library/LaunchDaemons/com.heftdev.test.daemon.plist`

## Apps: uninstalling, leftovers and background helpers

- [ ] Give the test app some leftovers:

      ```
      mkdir -p ~/Library/"Application Support"/com.heftdev.test ~/Library/Caches/com.heftdev.test
      defaults write com.heftdev.test tested -bool true
      ```

- [ ] Apps page: Heft Test is listed. Open it, then press Uninstall: Heft
      says it's open. Quit it and press Check again.
- [ ] Move to Trash: the app goes to the Trash, and the leftovers dialog
      lists the Application Support and Caches folders, the preferences file
      and the launch agent, ticked. Move them to the Trash.
- [ ] `launchctl print gui/$(id -u)/com.heftdev.test.agent` now fails: the
      agent was stopped, not left running until the next restart.
- [ ] Removed tab: everything is listed and Put back restores it.
- [ ] Installed for all users: make the test app again (see Before you
      start), then `sudo chown -R root:wheel "/Applications/Heft Test.app"`.
      Uninstalling it asks for your password once, and it lands in your
      Trash.
- [ ] Leftovers of deleted apps: make a container for an app that was never
      installed, and some saved window state for another:

      ```
      mkdir -p ~/Library/Containers/com.heftdev.gone ~/Library/Caches/com.heftdev.gone
      mkdir -p ~/Library/"Saved Application State"/com.heftdev.old.savedState
      ```

      Press **Leftovers of deleted apps…**: both appear, nothing ticked, the
      first with its Caches folder too. Without Full Disk Access the
      container's size shows as "size?". Move them to the Trash.
- [ ] Clean up: empty those items from the Trash, and remove anything left:
      `rm -rf ~/Library/Containers/com.heftdev.gone ~/Library/Caches/com.heftdev.gone`

## Updates

- [ ] Check for updates with Homebrew installed: outdated casks and formulae
      are listed; Update runs `brew upgrade` in Terminal.
- [ ] With `mas` installed (`brew install mas`) and an App Store app that has
      an update: it's listed as App Store, and Update runs `mas upgrade` in
      Terminal.
- [ ] Apps that use Sparkle (Rectangle, IINA, many others) say "updates
      itself", and their menu has Open to update.
- [ ] Settings › Also ask apps that update themselves: then Check for
      updates lists Sparkle apps with newer versions in their own feeds, and
      without the setting, no network access happens apart from Homebrew and
      mas (check with Little Snitch or `nettop` if you have them).

## Notifications and the menu bar

- [ ] View › set the free space alert above a drive's free space (for example
      500 GB). Within a minute a Notification Center alert from Heft appears.
      Click it: Heft opens. System Settings › Notifications lists Heft.
- [ ] Tick Keep watching in the menu bar after closing, close the window: the
      icon stays, its menu shows each drive's free space, and Open Heft brings
      the window back. Quit Heft from the menu.
- [ ] Put the alert limit back.

## Start at login and weekly cleaning

- [ ] View › Start at login: `plutil -p ~/Library/LaunchAgents/io.github.gjnail.heft.plist`
      shows Heft.app and `--tray`. Log out and in: Heft is in the menu bar,
      not the Dock.
- [ ] Move Heft.app to `~/Applications`, open it from there once, and check
      the plist now points at the new place. Move it back and open it again.
- [ ] Cleaner › Clean the selected items every Sunday: `launchctl print gui/$(id -u)/io.github.gjnail.heft.weekly-clean`
      shows the job. Run it now with
      `launchctl kickstart gui/$(id -u)/io.github.gjnail.heft.weekly-clean`
      and check the Cleaner finds less afterwards.
- [ ] Turn both off: both plists are gone.

## Moving a folder to another drive

- [ ] Make a small disk to move to, and a folder to move:

      ```
      hdiutil create -size 200m -fs APFS -volname HeftTest ~/Desktop/HeftTest.dmg
      hdiutil attach ~/Desktop/HeftTest.dmg
      mkdir -p ~/HeftMoveTest && for i in 1 2 3; do dd if=/dev/urandom of=~/HeftMoveTest/f$i bs=1m count=10; done
      ```

- [ ] Scan your home folder, right-click HeftMoveTest › Move to another
      drive › HeftTest. Afterwards `ls -l ~/HeftMoveTest` is a link to
      `/Volumes/HeftTest/…`, the files open, and the original is in the
      Trash.
- [ ] Clean up: `rm ~/HeftMoveTest`, eject HeftTest, and delete the disk
      image and the folder in the Trash.

## Duplicates: replacing copies with clones

- [ ] Make two identical files:

      ```
      mkdir -p ~/HeftDupTest && dd if=/dev/urandom of=~/HeftDupTest/a bs=1m count=50 && cp ~/HeftDupTest/a ~/HeftDupTest/b
      ```

- [ ] Scan HeftDupTest, Duplicates › Find duplicates, tick the second copy,
      Replace with clones. `cmp ~/HeftDupTest/a ~/HeftDupTest/b` finds no
      difference, and free space (`df -h ~`) went up by about 50 MB.
- [ ] Change one: `echo x >> ~/HeftDupTest/b`. The other is unchanged
      (`cmp` now reports a difference). Clean up: `rm -rf ~/HeftDupTest`.

## Compressing apps

- [ ] Compress a copy of an app of your own:
      `ditto "/Applications/Heft.app" "/Applications/Heft Compress Test.app"`.
      Scan it, right-click › Compress. Afterwards
      `ls -lO "/Applications/Heft Compress Test.app/Contents/MacOS/"` shows
      `compressed`, and the app still opens. Uncompress undoes it.
- [ ] Installed for all users: `sudo chown -R root:wheel "/Applications/Heft Compress Test.app"`,
      then Compress. The dialog offers to include its files with your
      password; tick it. One prompt, then the same checks pass.
- [ ] Clean up: `sudo rm -rf "/Applications/Heft Compress Test.app"`

## Trash, Put Back and iCloud

- [ ] Settings › Move items to the Trash through Finder. Make
      `~/HeftTrashTest.txt`, scan your home folder and move it to the Trash
      from Heft: macOS asks once to let Heft control Finder. In the Trash,
      right-click it › Put Back works, and so does the Removed tab.
- [ ] Turn the setting off again.
- [ ] Pick a small file in iCloud Drive that's downloaded (no cloud icon in
      Finder). Right-click it in Heft › Remove download: Finder now shows the
      cloud icon, the file is still listed, and opening it downloads it again.

## Dock

- [ ] Make the test app again (see Before you start), open it, and choose
      Options › Keep in Dock on its Dock icon. Quit it and delete the app in
      Finder: the Dock shows a question mark.
- [ ] Broken items › Scan for issues: the icon is under Dock icons for missing
      apps, ticked. Fix it: the Dock restarts without it.
- [ ] Backups › Restore: the icon comes back where it was (still with a
      question mark, since the app is gone). Remove it from the Dock yourself.

## Full Disk Access and the Cleaner

- [ ] Without Full Disk Access, the start screen explains what Heft can't
      see; Not now hides the note for good.
- [ ] Give Heft Full Disk Access and reopen it: the note is gone, and the
      Cleaner lists Safari's cookies, history and last session (and new
      Teams' cache, if Teams is installed), none of them ticked except the
      Teams cache.

## Anything else

- [ ] The start screen and status bar show purgeable space next to free
      space, and the two add up to what Finder shows as available.
- [ ] View › Update automatically after scanning a folder: a file made in
      Finder shows up in the map within a couple of seconds, and Heft uses
      no CPU while nothing changes (Activity Monitor).
- [ ] `/Applications/Heft.app/Contents/MacOS/heft --sensors --report --out ~/Desktop/report.txt`
      writes a report with the Mac's model, chip and raw sensor data, and
      nothing personal.

When everything's done, delete `/Applications/Heft Test.app` if it's still
there, and empty the Trash of the test items.
