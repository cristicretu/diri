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
`dist/linux`. `linux-release.json` records the version, source commit,
platform, architecture, format, size, and SHA-256 digest of each artifact.

The AppImage and Debian package have the same internal layout: executables in
`usr/bin`, manifests and the browser sidecar in `usr/lib/diri`, desktop
metadata in `usr/share/applications`, and the icon under the hicolor icon
tree. The Debian package declares the glibc 2.35, fontconfig, GLib, Vulkan,
Wayland, X11/XCB, and xkbcommon runtime dependencies.

Linux packages are always built on Ubuntu 22.04 to establish the oldest
supported glibc floor, then installed and smoke-tested on clean Ubuntu 22.04
and 24.04 CI jobs. The smoke covers X11 launch, package upgrade and uninstall,
a live shell session, Engine restart/holder adoption, CLI hooks, and MCP. The
workspace job additionally launches the GUI against headless Wayland. Manual
native-GPU QA remains a release gate; virtual displays cannot validate real
drivers, multiple monitors, suspend/resume, or fractional scaling.

`DIRI_DIST_DIR`, `CARGO_TARGET_DIR`, `DIRI_VERSION`, and
`DIRI_LINUX_FORMATS=appimage,deb` can override the defaults. Linux release
artifacts must come from the CI run for the exact release commit. Set
`DIRI_LINUX_DIST` to that downloaded artifact directory when running the
macOS release script; the release is then created once with both platforms'
immutable assets.

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

The committed `assets/icon.icns` and `assets/dev-icon.icns` are the release and
development icon inputs.

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

The cargo-packager-compatible aliases `APPLE_ID`, `APPLE_PASSWORD`, and `APPLE_TEAM_ID` are also accepted. The script signs with the hardened runtime, then notarizes in two passes: the `.app` first (submitted as a zip, then stapled), and the DMG second, built from that already-stapled bundle. Both artifacts therefore carry a ticket that validates offline — which the in-app updater requires, since it assesses the bundle it extracts rather than the DMG. It never reads or submits credentials unless one of these notarization configurations is explicitly present.

Notarizing also produces `dist/diri-<version>-universal.zip`, the artifact diri's updater downloads. `DIRI_CREATE_ZIP=1` builds it without notarization too. The version in both artifact names comes from `crates/diri-app/Cargo.toml` unless `DIRI_VERSION` overrides it, because the updater compares against `CARGO_PKG_VERSION`.

## Distribution checklist

Before a public release:

1. Install the real Developer ID Application certificate and configure a notarytool keychain profile or CI secrets.
2. Run the signed/notarized DMG flow and test it on a second Mac outside the build environment.
3. Publish through the release host with `scripts/release.sh <version>`, which wraps this script and also writes the update feed — see [UPDATING.md](UPDATING.md).
