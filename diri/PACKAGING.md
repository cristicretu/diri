# Packaging diri

The macOS and Linux packages are built natively on their oldest supported
runner. Both consume the same Rust workspace, protocol, daemon, holder, CLI,
MCP frontend, and agent manifests.

## Linux

Run this on x86_64 Linux after installing the build dependencies in
[`LINUX.md`](LINUX.md), Node.js 20 or newer, and cargo-packager 0.11.8:

```sh
cargo install cargo-packager --version 0.11.8 --locked
scripts/package-linux.sh
scripts/verify-linux-package.sh dist/linux
```

The script builds every release binary, installs the locked browser-sidecar
dependencies, validates the repository's license policy, and passes a declared
resource manifest to cargo-packager. It emits an AppImage, a Debian package,
`SHA256SUMS`, `linux-release.json`, and the third-party license inventory under
`dist/linux`, named for the host architecture (`diri_<v>_x86_64.AppImage` and
`diri_<v>_amd64.deb`, or `diri_<v>_aarch64.AppImage` and `diri_<v>_arm64.deb`).
The script refuses to cross-build. `linux-release.json` records the version,
source commit, platform, architecture, format, size, and SHA-256 digest of each
artifact.

The AppImage and Debian package have the same internal layout: executables in
`usr/bin`, manifests and the browser sidecar in `usr/lib/diri`, desktop
metadata in `usr/share/applications`, and the icon under the hicolor icon
tree. The Debian package declares the glibc 2.35, fontconfig, GLib, Vulkan,
Wayland, X11/XCB, and xkbcommon runtime dependencies.

Linux packages are always built on Ubuntu 22.04 to establish the oldest
supported glibc floor (2.35), natively on both `ubuntu-22.04` (x86_64) and
`ubuntu-22.04-arm` (aarch64), then installed and smoke-tested on clean Ubuntu
22.04 and 24.04 CI jobs of the same architecture. A release-set job merges the
two builds with `write-linux-release-manifest.py --merge`: one `SHA256SUMS`
over all four packages, and one `linux-release.json` whose top-level
`architecture`/`artifacts` stay the x86_64 build (the shape every release
before aarch64 published) while the additive `builds` array lists every
architecture with its Debian architecture name. The smoke covers X11 launch, package upgrade and uninstall,
a live shell session, Engine restart/holder adoption, CLI hooks, and MCP. The
workspace job additionally launches the GUI against headless Wayland. Manual
native-GPU QA remains a release gate; virtual displays cannot validate real
drivers, multiple monitors, suspend/resume, or fractional scaling.

`DIRI_DIST_DIR`, `CARGO_TARGET_DIR`, `DIRI_VERSION`, and
`DIRI_LINUX_FORMATS=appimage,deb` can override the defaults. Linux release
artifacts must come from the CI run for the exact release commit. The macOS
release script fetches them from a Nightly run on that commit (dispatching one
if needed), or takes `DIRI_LINUX_DIST` pointing at a downloaded artifact
directory; the release is then created once with both platforms' immutable
assets.

### Linux signatures

The Nightly `linux-package-release` job signs every AppImage and Debian
package the merged `linux-release.json` declares, plus `SHA256SUMS` and
`linux-release.json`, with Sigstore keyless signing
(`scripts/linux-signatures.sh sign`), then verifies them before uploading.
cosign exchanges the job's GitHub OIDC token for a short-lived certificate
whose identity is
`https://github.com/cristicretu/diri/.github/workflows/nightly.yml@refs/heads/main`,
and records the signature in the public Rekor transparency log. Each file gets
a `<file>.sigstore.json` bundle. Only `main` runs sign; pull-request runs do
not. The Ubuntu 24.04 smoke jobs (x86_64 and aarch64) verify the bundles again
on machines that did not sign them, and `release.sh` verifies them with the pinned identity
before publishing, so an unsigned or wrongly signed Linux artifact cannot ship.
The release carries CI's signed Linux checksum list as `SHA256SUMS-linux`
beside the release-wide `SHA256SUMS`, which `release.sh` writes on the Mac and
therefore cannot carry a CI signature. User commands are in
[`LINUX.md`](LINUX.md#install).

Why Sigstore rather than a long-lived GPG or minisign key:

- **No key to keep.** The packages are built in CI, so the strongest statement
  available is "this workflow on `main` built these bytes", and keyless
  signing states exactly that. A GPG or minisign key would have to live in a
  GitHub secret, where anyone able to exfiltrate it could sign anything until
  it is revoked, and losing it strands every user who pinned its fingerprint.
- **Tamper evidence.** Every signature is in a public, append-only log, so a
  signature made outside this workflow would be visible.
- **Cost to users.** Verification needs `cosign`, which Ubuntu does not
  preinstall, whereas it does ship `gpg`. That is the main tradeoff, and the
  reason plain `SHA256SUMS` stays the first instruction.
- **Debian.** `dpkg-sig` is unmaintained and dpkg does not check embedded
  `.deb` signatures by default (`debsig-verify` is opt-in policy), so signing
  inside the package would protect almost nobody. The Debian-native trust path
  is a signed APT repository `InRelease` file whose key is installed with
  `signed-by`; that needs repository hosting and a long-lived GPG key, and is a
  separate decision (see "Not yet" below).
- **AppImage.** appimagetool can embed a GPG signature, but cargo-packager
  does not produce one, it again needs a long-lived key, and few tools check
  it. A detached bundle covers the same bytes.

Rehearse the path locally with a throwaway key, never a release key:

```sh
export COSIGN_PASSWORD=
cosign generate-key-pair --output-key-prefix "$TMPDIR/rehearsal"
DIRI_COSIGN_KEY="$TMPDIR/rehearsal.key" scripts/linux-signatures.sh sign <dist>
DIRI_COSIGN_PUBLIC_KEY="$TMPDIR/rehearsal.pub" scripts/linux-signatures.sh verify <dist>
```

Key mode stays offline and never uploads to the transparency log. `release.sh`
clears both variables before its own verification, and needs `cosign` on the
release Mac (`brew install cosign`).

Not yet: an APT repository with a signed `InRelease` (so `apt upgrade`
verifies updates), and GitHub build-provenance attestations.
The in-app updater does not download Linux artifacts (it tells Linux users to
update through APT or a newer download), so it has nothing to verify. If a
Linux self-updater is ever added, it must verify these bundles against the same
pinned identity, and pick its files from the `builds` entry whose
`architecture` matches the running binary, never from the top-level x86_64
fields.

## macOS

`scripts/package.sh` builds `diri` for Apple silicon and Intel, combines the two slices with `lipo`, asks cargo-packager to assemble `dist/diri.app`, and signs the result. The bundle identifier is `com.dirijor.diri`, the deployment target is macOS 15.0, and the app does not use App Sandbox.

## One-time setup

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin \
    aarch64-unknown-linux-musl x86_64-unknown-linux-musl
cargo install cargo-packager --locked
brew install zig
cargo install cargo-zigbuild --locked
```

The two musl targets, `zig`, and `cargo-zigbuild` are **required to cut a
release**, not optional. The app bundles one remote Helper binary per supported
remote platform, and `package.sh` fails rather than shipping a partial catalog:
a Helper the remote host cannot run means that host simply does not work. Only
the two Linux targets are cross-built; the Apple Helper needs a macOS builder,
which is why releases are cut on macOS. CI asserts all three artifacts are
present in the bundle.

`package.sh` uses the toolchain in `~/.cargo` and builds into `<workspace>/target`. Override either with `CARGO_HOME` or `CARGO_TARGET_DIR` — but never point `CARGO_TARGET_DIR` at a cache shared with another checkout: cross-workspace fingerprint collisions link stale crates into the shipped app.

Also required: cargo-packager 0.11 or newer, Xcode command-line tools, `lipo`, `codesign`, `sips`, and `iconutil`.

## Local package and install

```sh
scripts/package.sh
scripts/install-local.sh
```

With no signing environment, `package.sh` applies an ad-hoc hardened-runtime signature and verifies it. Set `DIRI_CREATE_DMG=1` to also create `dist/diri-<version>-universal.dmg`. `DIRI_DIST_DIR` changes the output directory, and `DIRI_VERSION` changes the DMG filename.

The app icon's source of truth is the Icon Composer documents
`assets/diri.icon` (release) and `assets/diri-dev.icon` (development builds). The
compiled outputs are committed so packaging needs no Xcode 26+: `Assets.car` /
`dev-Assets.car` are what macOS 26+ draws (light, dark, tinted and clear
appearances, named by `CFBundleIconName`), `icon.icns` / `dev-icon.icns` are the
macOS 15 fallback, and `icon.png` feeds the Linux packages. After editing either
`.icon`, regenerate them all with `scripts/build-icons.sh`.

## Developer ID signing and notarization

Set the Developer ID Application identity and choose one notarytool authentication method:

```sh
export DIRI_SIGN_IDENTITY='Developer ID Application: Example Team (TEAMID)'
export DIRI_CREATE_DMG=1
export APPLE_NOTARIZATION_KEYCHAIN_PROFILE=dirijor-notary
scripts/package.sh
```

The keychain-profile variable also accepts the legacy `NOTARY_PROFILE` name and cargo-packager's `APPLE_KEYCHAIN_PROFILE` name. Alternatively, set all three direct credential variables:

- `APPLE_NOTARIZATION_APPLE_ID`
- `APPLE_NOTARIZATION_PASSWORD` (an app-specific password)
- `APPLE_NOTARIZATION_TEAM_ID`

The cargo-packager-compatible aliases `APPLE_ID`, `APPLE_PASSWORD`, and `APPLE_TEAM_ID` are also accepted. The script signs with the hardened runtime, then notarizes once: with a DMG it submits only the DMG, whose ticket covers the app inside it, and staples both the DMG and the `.app`. The update zip is made from the stapled `.app`, so the bundle the in-app updater extracts validates offline. The DMG's own copy of the app is not stapled; Gatekeeper checks it online on first launch, while the DMG carries its stapled ticket. Without a DMG, the `.app` is submitted alone (as a zip) and stapled. It never reads or submits credentials unless one of these notarization configurations is explicitly present.

Notarizing also produces `dist/diri-<version>-universal.zip`, the artifact diri's updater downloads. `DIRI_CREATE_ZIP=1` builds it without notarization too. The version in both artifact names comes from `crates/diri-app/Cargo.toml` unless `DIRI_VERSION` overrides it, because the updater compares against `CARGO_PKG_VERSION`.

## Distribution checklist

Before a public release:

1. Install the real Developer ID Application certificate and configure a notarytool keychain profile or CI secrets.
2. Run the signed/notarized DMG flow and test it on a second Mac outside the build environment.
3. Publish through the release host with `scripts/release.sh <version>`, which wraps this script and also writes the update feed — see [UPDATING.md](UPDATING.md).
