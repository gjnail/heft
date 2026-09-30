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

The macOS download is still unsigned. Signing and notarizing it needs a paid
Apple Developer account.
