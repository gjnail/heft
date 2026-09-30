# Releasing Heft

Releases are built by [`.github/workflows/release.yml`](../.github/workflows/release.yml).
Nothing is published until you publish the draft release it creates.

## Try the build first

Actions > Release > Run workflow builds all three downloads without making a
release. Use it after changing the workflow, the packaging scripts or
`build.rs`. The downloads and the winget manifests are attached to the run.

## Make a release

1. Set the new version in `Cargo.toml` and run `cargo build`, which updates
   `Cargo.lock`.
2. In `CHANGELOG.md`, move everything under `## [Unreleased]` into a new
   `## [x.y.z] - YYYY-MM-DD` section. That section becomes the release notes.
3. Commit, then tag and push:

   ```
   git tag vx.y.z
   git push origin main vx.y.z
   ```

4. The workflow checks that the tag matches `Cargo.toml` and that the
   changelog has a section for it, builds on Windows, macOS and Linux, and
   opens a draft release with the downloads, `SHA256SUMS.txt` and a build
   provenance attestation for each file.
5. Look over the draft on the Releases page, download the Windows zip and try
   it, then click **Publish release**.

If something is wrong before publishing, delete the draft and the tag
(`git push origin :vx.y.z`), fix it, and tag again.

## Licenses

Every download includes `THIRD-PARTY-LICENSES.txt`, the licenses of the Rust
libraries Heft is built with, and the Windows zip also has
`PAWNIO-LICENSE.txt` for the embedded PawnIO modules. The Check version job
generates the first with [cargo-about](https://github.com/EmbarkStudios/cargo-about)
from `about.toml` and `about.hbs`, and fails if a dependency uses a license
that isn't in `about.toml`'s accepted list. If that happens, check the
license is fine to ship with Heft and add it. To see the file locally:

```
cargo install cargo-about --locked --features cli
cargo about generate --locked about.hbs -o THIRD-PARTY-LICENSES.txt
```

## winget

The Windows job writes three manifest files for the `gjnail.Heft` package and
attaches them to the run as the `winget-manifests` artifact. They point at the
release URL, so submit them only after the release is published.

Check them first. Installing from a local manifest has to be switched on once,
from a terminal running as administrator, with
`winget settings --enable LocalManifestFiles`.

```
winget validate --manifest <folder>
winget install --manifest <folder>
```

The first version has to go through a pull request to
[microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs) with the
files in `manifests/g/gjnail/Heft/<version>/`. The
[wingetcreate](https://github.com/microsoft/winget-create) tool can open it
for you:

```
wingetcreate submit --token <github token> <folder>
```

Microsoft's automated checks and a moderator review the pull request, which
can take a few days. Once `gjnail.Heft` is in winget, later versions only need:

```
wingetcreate update gjnail.Heft --version x.y.z --urls <windows zip url> --submit --token <github token>
```

## Code signing

`heft.exe` is signed through [SignPath Foundation](https://signpath.org/),
which signs open source projects for free with its own certificate. Windows
shows "SignPath Foundation" as the publisher. The README's
[Code signing policy](../README.md#code-signing-policy) section is required by
their terms; keep it up to date when the team changes.

Until the steps below are done, the workflow skips signing and releases are
unsigned.

### One-time setup

1. SignPath Foundation only takes projects that already have a release, so
   publish an unsigned release first.
2. Turn on two-factor authentication for every GitHub account with write
   access. SignPath requires it, for SignPath accounts too.
3. Apply at <https://signpath.org/apply>. Once the project is accepted,
   SignPath sets up an organization with a `heft` project and the
   `test-signing` and `release-signing` policies.
4. In SignPath:
   - add the predefined **GitHub.com** trusted build system to the
     organization, link it to the project, and install the SignPath GitHub
     App on this repository when asked;
   - set the project's default artifact configuration to
     [`packaging/windows/signpath-artifact-configuration.xml`](../packaging/windows/signpath-artifact-configuration.xml);
   - create a CI user that is allowed to submit to both policies, and copy its
     API token.
5. In this repository, under Settings > Secrets and variables > Actions:
   - secret `SIGNPATH_API_TOKEN`: the CI user's token;
   - variable `SIGNPATH_ORGANIZATION_ID`: the organization ID from SignPath;
   - variable `SIGNPATH_PROJECT_SLUG`: only if the project isn't called `heft`.
6. Run the Release workflow by hand. That signs with the test certificate, so
   it checks the whole setup without using the real one.

### With signing on

After you push a tag, the Windows job waits (up to an hour) for you to approve
the signing request in SignPath. The link is in the job log. The job then
checks the signature before packaging. Nothing else in the release steps
changes.

Once the first signed release is out, download it on a clean Windows machine
and update "The first time you open it" in the README to match what Windows
shows. SmartScreen goes by the reputation of the certificate and the file, so
a signed exe can still get a warning, just much less often.

## Signing and notarizing for macOS

A Developer ID signature and Apple's notarization stop macOS saying it can't
verify Heft. They also give every release the same code identity, so macOS
keeps Full Disk Access, Notification Center permission and App Management
permission across updates instead of asking again after each one, and
notifications from Heft.app work reliably.

Until the steps below are done, the workflow signs the app ad hoc, as a local
build does.

### One-time setup

1. Join the [Apple Developer Program](https://developer.apple.com/programs/)
   ($99 a year). An individual account is fine.
2. Create a **Developer ID Application** certificate: in Xcode, Settings >
   Accounts > Manage Certificates > + > Developer ID Application (only the
   account holder can do this). In Keychain Access, export it with its
   private key as a `.p12` file with a password.
3. Create an App Store Connect API key for notarization: App Store Connect >
   Users and Access > Integrations > App Store Connect API > +, with the
   **Developer** role. Download the `.p8` file (it can only be downloaded
   once) and note the key ID and the issuer ID above the list.
4. In this repository, under Settings > Secrets and variables > Actions:
   - secret `MACOS_CERTIFICATE`: the `.p12` file as base64
     (`base64 -i heft.p12 | pbcopy`);
   - secret `MACOS_CERTIFICATE_PASSWORD`: its password;
   - secret `APPLE_API_KEY`: the contents of the `.p8` file;
   - variable `APPLE_API_KEY_ID`: the key ID;
   - variable `APPLE_API_ISSUER_ID`: the issuer ID;
   - variable `APPLE_TEAM_ID`: your team ID (Membership details on
     developer.apple.com). Setting this one turns signing on.
5. Run the Release workflow by hand and check the macOS job's Notarize step.
   Download the zip from the run and open it on a Mac that has never run
   Heft: it should open without the "can't verify" warning.

To sign a local build the same way, pass the certificate's name from your
keychain:

```
HEFT_SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" bash scripts/bundle-macos.sh
```

The app is signed with the hardened runtime and the entitlements in
[`packaging/macos/heft.entitlements`](../packaging/macos/heft.entitlements).
If a feature that talks to another app stops working in a signed build, check
that file first.

### With signing on

Nothing changes in the release steps: the macOS job signs, notarizes (usually
a few minutes) and staples the ticket to the app before zipping it. Once the
first signed release is out, update "The first time you open it" in the
README and the macOS notes on the website.

## Homebrew

The macOS job writes a cask, `heft.rb`, and attaches it to the run as the
`homebrew-cask` artifact. It points at the release URL, so use it only after
the release is published. It installs Heft.app and links `heft` into
Homebrew's `bin` folder, so the command line works too.

Until Heft is in Homebrew itself, publish the cask in a tap of your own: a
repository called `homebrew-heft` with the file at `Casks/heft.rb`. People
then install with:

```
brew install --cask gjnail/heft/heft
```

Check the cask before pushing it:

```
brew style --fix Casks/heft.rb
brew audit --cask --new gjnail/heft/heft
brew install --cask gjnail/heft/heft
```

For plain `brew install --cask heft`, the cask has to go into
[Homebrew/homebrew-cask](https://github.com/Homebrew/homebrew-cask), whose
rules require a signed and notarized app and a reasonably well-known
project (see their [acceptable casks](https://docs.brew.sh/Acceptable-Casks)
page). Open a pull request adding `Casks/h/heft.rb`. After that, Homebrew's
autobump updates it for each new release, so it only needs doing once.
