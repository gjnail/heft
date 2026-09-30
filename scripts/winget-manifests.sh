#!/usr/bin/env bash
# Write the winget manifests for a release, ready to submit to
# microsoft/winget-pkgs. The release workflow runs this; see docs/releasing.md.
#   bash scripts/winget-manifests.sh <version> <windows zip> <output folder>
set -euo pipefail

if [ $# -ne 3 ]; then
    echo "usage: $0 <version> <windows zip> <output folder>" >&2
    exit 2
fi
version=$1
zip=$2
out=$3

id=gjnail.Heft
schema=1.10.0
repo=https://github.com/gjnail/heft
url="$repo/releases/download/v$version/$(basename "$zip")"
sha256=$(sha256sum < "$zip" | cut -d' ' -f1 | tr a-f A-F)

mkdir -p "$out"

cat > "$out/$id.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.version.$schema.schema.json
PackageIdentifier: $id
PackageVersion: $version
DefaultLocale: en-US
ManifestType: version
ManifestVersion: $schema
EOF

cat > "$out/$id.installer.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.installer.$schema.schema.json
PackageIdentifier: $id
PackageVersion: $version
InstallerType: zip
NestedInstallerType: portable
NestedInstallerFiles:
- RelativeFilePath: heft.exe
  PortableCommandAlias: heft
ReleaseDate: $(date -u +%Y-%m-%d)
Installers:
- Architecture: x64
  InstallerUrl: $url
  InstallerSha256: $sha256
ManifestType: installer
ManifestVersion: $schema
EOF

cat > "$out/$id.locale.en-US.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.defaultLocale.$schema.schema.json
PackageIdentifier: $id
PackageVersion: $version
PackageLocale: en-US
Publisher: Greg Nail
PublisherUrl: https://github.com/gjnail
PublisherSupportUrl: $repo/issues
Author: Greg Nail and the Heft contributors
PackageName: Heft
PackageUrl: $repo
License: MIT
LicenseUrl: $repo/blob/main/LICENSE
Copyright: Copyright (c) 2026 Greg Nail and the Heft contributors
ShortDescription: Disk usage analyzer and cleanup tool
Description: |-
  Heft shows where your disk space went, with a treemap of every file, a folder tree,
  a largest files list, a duplicate finder and scan history. On Windows it also has a
  junk cleaner, startup programs, installed programs with leftover checks, and a narrow
  registry check. It runs locally and doesn't send data anywhere.
Moniker: heft
Tags:
- disk-usage
- disk-space
- disk-cleanup
- treemap
- duplicate-finder
- cleaner
ReleaseNotesUrl: $repo/releases/tag/v$version
ManifestType: defaultLocale
ManifestVersion: $schema
EOF

echo "Wrote winget manifests for $id $version to $out"
