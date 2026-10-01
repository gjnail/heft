#!/usr/bin/env bash
# Build Heft.app. Produces a universal (Apple Silicon + Intel) binary when
# both Rust targets are installed:
#   rustup target add aarch64-apple-darwin x86_64-apple-darwin
# Set HEFT_BUNDLE_ID to use your own bundle identifier, and
# HEFT_SIGN_IDENTITY to sign with a Developer ID certificate from your
# keychain instead of ad hoc (see docs/releasing.md).
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
target_dir="${CARGO_TARGET_DIR:-target}"

targets=()
for t in aarch64-apple-darwin x86_64-apple-darwin; do
    if rustup target list --installed 2>/dev/null | grep -qx "$t"; then
        targets+=("$t")
    fi
done
if [ ${#targets[@]} -eq 0 ]; then
    targets=("$(rustc -vV | sed -n 's/^host: //p')")
fi

bins=()
for t in "${targets[@]}"; do
    cargo build --release --target "$t"
    bins+=("$target_dir/$t/release/heft")
done

app="$target_dir/Heft.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
if [ ${#bins[@]} -gt 1 ]; then
    lipo -create -output "$app/Contents/MacOS/heft" "${bins[@]}"
else
    cp "${bins[0]}" "$app/Contents/MacOS/heft"
fi

# Icon: render every size the .icns format wants from the binary itself.
iconset="$(mktemp -d)/heft.iconset"
mkdir -p "$iconset"
for s in 16 32 128 256 512; do
    "$app/Contents/MacOS/heft" --icon "$iconset/icon_${s}x${s}.png" --size "$s"
    "$app/Contents/MacOS/heft" --icon "$iconset/icon_${s}x${s}@2x.png" --size "$((s * 2))"
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/heft.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Heft</string>
    <key>CFBundleDisplayName</key><string>Heft</string>
    <key>CFBundleIdentifier</key><string>${HEFT_BUNDLE_ID:-local.heft.Heft}</string>
    <key>CFBundleVersion</key><string>${version}</string>
    <key>CFBundleShortVersionString</key><string>${version}</string>
    <key>CFBundleExecutable</key><string>heft</string>
    <key>CFBundleIconFile</key><string>heft</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSAppleEventsUsageDescription</key><string>Heft asks System Events to list and change your login items when macOS doesn't let it read them directly.</string>
</dict>
</plist>
PLIST

# License texts. THIRD-PARTY-LICENSES.txt only exists when the release
# workflow has generated it. They go in before signing, which seals the app.
for f in LICENSE THIRD-PARTY-LICENSES.txt; do
    if [ -f "$f" ]; then
        cp "$f" "$app/Contents/Resources/"
    fi
done

if [ -n "${HEFT_SIGN_IDENTITY:-}" ]; then
    # Developer ID signature with the hardened runtime and a secure
    # timestamp, which notarization requires. The identity is the
    # certificate's name, like "Developer ID Application: Name (TEAMID)".
    codesign --force --options runtime --timestamp \
        --entitlements packaging/macos/heft.entitlements \
        --sign "$HEFT_SIGN_IDENTITY" "$app"
    codesign --verify --strict --verbose=2 "$app"
else
    # Ad-hoc signature so a locally built app runs without Gatekeeper complaints.
    codesign --force --deep --sign - "$app" >/dev/null 2>&1 || true
fi

echo "Built $app"
