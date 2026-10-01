#!/usr/bin/env bash
# Write the Homebrew cask for a release, so `brew install --cask heft`
# installs Heft.app and puts `heft` on the PATH. The release workflow runs
# this; see docs/releasing.md.
#   bash scripts/homebrew-cask.sh <version> <macOS zip> <output folder>
set -euo pipefail

if [ $# -ne 3 ]; then
    echo "usage: $0 <version> <macOS zip> <output folder>" >&2
    exit 2
fi
version=$1
zip=$2
out=$3

repo=https://github.com/gjnail/heft
bundle_id=io.github.gjnail.heft
sha256=$(shasum -a 256 < "$zip" | cut -d' ' -f1)
# The file name with the version swapped for Homebrew's placeholder.
file=$(basename "$zip" | sed "s/$version/#{version}/")

mkdir -p "$out"

cat > "$out/heft.rb" <<EOF
cask "heft" do
  version "$version"
  sha256 "$sha256"

  url "$repo/releases/download/v#{version}/$file"
  name "Heft"
  desc "Disk usage analyzer and cleanup tool with a treemap"
  homepage "https://gjnail.github.io/heft/"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on macos: ">= :big_sur"

  app "Heft.app"
  binary "#{appdir}/Heft.app/Contents/MacOS/heft"

  # Start at login and weekly cleaning are launch agents that point at the app.
  uninstall quit:      "$bundle_id",
            launchctl: [
              "$bundle_id",
              "$bundle_id.weekly-clean",
            ]

  zap trash: [
    "~/Library/Application Support/Heft",
    "~/Library/Saved Application State/$bundle_id.savedState",
  ]
end
EOF

echo "Wrote the Homebrew cask for Heft $version to $out/heft.rb"
