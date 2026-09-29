# Contributing

Bug reports, fixes, new cleaning rules and platform testing are all welcome.

## Rules for code that removes or changes data

Heft can delete files and edit the Windows registry, so these apply to every
change in that area:

1. Nothing is removed or changed without a confirmation that says what will
   happen. Use the Recycle Bin / Trash where possible. If something has to be
   deleted permanently, as the junk cleaner does with caches, the dialog must
   say so.
2. Don't guess. Cleaning rules name exact locations. Registry fixes only touch
   entries whose target is definitely gone. Build junk is only reported when
   the surrounding project confirms what the folder is.
3. Back up every registry change to a `.reg` file first.
4. Never follow symlinks or junctions when deleting. Skip files that are in
   use instead of forcing them.
5. Keep sizes correct. If two scanners disagree, find out why; `heft --compare`
   exists for that.
6. Don't add network access. The only exception is winget, when the user asks
   for updates.

## Building and testing

You need stable Rust (<https://rustup.rs>).

```
cargo build
cargo test
cargo clippy --all-targets
```

On Windows you also need the MSVC build tools (Visual Studio "Desktop
development with C++"). If linking fails with `LNK1104` because the checkout
is in a very deep folder, set `CARGO_TARGET_DIR` to a shorter path, for
example `%USERPROFILE%\.heft-target`. macOS and Linux need nothing else.

`HEFT_DEMO=1 cargo run` opens Heft on a made-up disk. Use it for UI work and
screenshots so real file names don't end up in issues or pull requests.

Debug builds also read `HEFT_DEBUG_TAB`, `HEFT_DEBUG_ZOOM`,
`HEFT_DEBUG_SELECT`, `HEFT_DEBUG_COLOR`, `HEFT_DEBUG_DUPES` and
`HEFT_DEBUG_DIFF`, which open a given view after a scan.

### Other platforms

A lot of code is behind `#[cfg(...)]`, so a change can build on your machine
and break another OS. Check the targets you can't run:

```
rustup target add x86_64-unknown-linux-gnu aarch64-apple-darwin x86_64-pc-windows-msvc
cargo check --all-targets --target x86_64-unknown-linux-gnu
cargo check --all-targets --target aarch64-apple-darwin
```

CI builds and tests on Windows, macOS and Ubuntu for every push and pull
request.

The Windows-only modules (MFT scanner, cleaner, startup, programs, registry,
winget) are declared with `#[cfg(windows)]` in `main.rs`. Shared code must not
call them directly.

## Source layout

```
src/
  main.rs               entry point, window setup
  cli.rs                command line: --bench, --compare, --render, --clean, --icon
  demo.rs               the made-up demo disk (HEFT_DEMO)
  tree.rs               arena tree of the scan results
  scan/mft.rs           NTFS master file table reader (Windows)
  scan/walk/            parallel directory scanner, with Win32 and std::fs listers
  treemap.rs            treemap layout, shading and render thread
  history.rs            scan snapshots and the Changes comparison
  dupes.rs              duplicate finder
  devjunk.rs            build junk detection
  colors.rs             file categories and color schemes
  icon.rs               app icon, drawn at any size
  platform/             OS integration: drives, trash, file manager, dates
  clean/                junk cleaner and its rule catalog (Windows)
  startup.rs            startup programs (Windows)
  programs.rs           installed programs and leftovers (Windows)
  regclean.rs, reg.rs   registry issues, registry access and .reg backups (Windows)
  winget.rs             updates through winget (Windows)
  winsys.rs             Windows helpers for the maintenance tools
  app/                  user interface; tools/ holds the Windows maintenance pages
packaging/linux/        desktop entry
scripts/                macOS app bundle and Linux install scripts
```

## Pull requests

- Keep each pull request to one fix or feature.
- Add tests for new logic, especially anything that decides what gets deleted.
- Follow the style of the surrounding code.
- Run `cargo test` and `cargo clippy --all-targets` before pushing.
- Add user-visible changes to `CHANGELOG.md` under "Unreleased".
- For a new cleaning rule in `src/clean/rules.rs`, explain in the pull request
  what the location contains, why removing it is safe, and which program
  recreates it.

## Reporting bugs

Include your OS, the Heft version or commit, what you did and what happened.
For wrong sizes, the output of `heft --bench <path>` helps, and on Windows as
administrator so does `heft --compare C:`. Check that anything you paste
doesn't show file names you'd rather keep private.

Report anything that could delete or change the wrong files privately, as
described in [SECURITY.md](SECURITY.md).

## License

Contributions are licensed under the [MIT License](LICENSE), like the rest of
the project.
