#!/usr/bin/env bash
# Install Heft for the current user (no root needed): ~/.local/bin/heft, a
# desktop entry, and icons. In a source checkout this builds Heft first; in an
# extracted release download it installs the included binary.
# Uninstall: rm ~/.local/bin/heft ~/.local/share/applications/heft.desktop \
#               ~/.local/share/icons/hicolor/*/apps/heft.png
set -euo pipefail
cd "$(dirname "$0")/.."

target_dir="${CARGO_TARGET_DIR:-target}"
if [ -f heft ] && [ -x heft ]; then
    bin=heft
else
    cargo build --release
    bin="$target_dir/release/heft"
fi
bin_dir="${XDG_BIN_HOME:-$HOME/.local/bin}"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"

install -Dm755 "$bin" "$bin_dir/heft"

for size in 48 64 128 256 512; do
    icon_dir="$data_dir/icons/hicolor/${size}x${size}/apps"
    mkdir -p "$icon_dir"
    "$bin_dir/heft" --icon "$icon_dir/heft.png" --size "$size"
done

desktop="$data_dir/applications/heft.desktop"
install -Dm644 packaging/linux/heft.desktop "$desktop"
# Point at the installed binary in case ~/.local/bin isn't on PATH for the desktop session.
sed -i "s|^Exec=heft|Exec=$bin_dir/heft|" "$desktop"

command -v update-desktop-database >/dev/null && update-desktop-database -q "$data_dir/applications" || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q "$data_dir/icons/hicolor" || true

echo "Installed $bin_dir/heft. Heft is now in your app launcher."
