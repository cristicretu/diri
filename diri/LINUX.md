# Linux beta

Diri supports Ubuntu 22.04 and 24.04 on x86_64 and on 64-bit ARM (aarch64,
Debian's `arm64`) under native Wayland and X11. The desktop renderer requires a
Vulkan 1.3-capable driver. Both architectures are built natively on Ubuntu
22.04, so both have a glibc 2.35 floor; neither requires Swift, SwiftPM, Xcode,
or a macOS application bundle.

| Architecture | AppImage | Debian package | glibc | CI-tested on |
|---|---|---|---|---|
| x86_64 (Intel, AMD) | `diri_<version>_x86_64.AppImage` | `diri_<version>_amd64.deb` | 2.35 or newer | Ubuntu 22.04, 24.04 |
| aarch64 (64-bit ARM) | `diri_<version>_aarch64.AppImage` | `diri_<version>_arm64.deb` | 2.35 or newer | Ubuntu 22.04, 24.04 (`ubuntu-*-arm` runners) |

`uname -m` prints the name to pick. aarch64 packages are published from the
first release after 0.9.1; earlier releases are x86_64 only. Other
distributions with glibc 2.35 or newer and a Vulkan 1.3 driver, such as Asahi
Linux on Apple silicon with Mesa's Honeykrisp driver, are expected to run the
AppImage but are not in the tested matrix. The AppImage is the format for
distributions without APT.

Scripts should pick files from the release's `linux-release.json`: its
`builds` list has one entry per `architecture` (`x86_64`, `aarch64`) with that
build's `debArchitecture` and `artifacts`. The top-level `architecture` and
`artifacts` fields are kept for older readers and always describe x86_64.

## Install

Download both the artifact and `SHA256SUMS` from the same GitHub release, then
verify the download:

```sh
sha256sum --ignore-missing --check SHA256SUMS
```

A checksum only proves the download matches the list next to it. To prove the
files were built by this repository's CI, verify their Sigstore signatures.
Every Linux release file has a `<file>.sigstore.json` bundle beside it. Install
[cosign](https://docs.sigstore.dev/cosign/system_config/installation/) (3.x
is tested), download the artifact and its bundle, then run:

```sh
cosign verify-blob \
  --bundle diri_<version>_x86_64.AppImage.sigstore.json \
  --certificate-identity https://github.com/cristicretu/diri/.github/workflows/nightly.yml@refs/heads/main \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  diri_<version>_x86_64.AppImage
```

`Verified OK` means the file is byte-for-byte what the `nightly.yml` workflow
on `main` signed, and that the signature is recorded in the public Sigstore
transparency log. Any other signer, a modified file, or a missing bundle
fails. Use the same command for `diri_<version>_amd64.deb`, or for the
aarch64 files `diri_<version>_aarch64.AppImage` and `diri_<version>_arm64.deb`.

To check every Linux file at once, verify the signed Linux checksum list and
then check against it. `SHA256SUMS-linux` lists the packages of every
architecture; `--ignore-missing` checks the ones you downloaded:

```sh
cosign verify-blob \
  --bundle SHA256SUMS-linux.sigstore.json \
  --certificate-identity https://github.com/cristicretu/diri/.github/workflows/nightly.yml@refs/heads/main \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  SHA256SUMS-linux
sha256sum --ignore-missing --check SHA256SUMS-linux
```

There is no long-lived signing key or fingerprint to import: Sigstore issues a
short-lived certificate to the CI job, and the identity above is the trust
anchor. The Debian package is not `dpkg-sig` signed and there is no APT
repository yet, so `apt` itself does not check a signature; verify the `.deb`
with cosign before installing it.

For Ubuntu or another Debian-based system, install the package with APT so its
runtime dependencies are resolved:

```sh
sudo apt install ./diri_<version>_amd64.deb    # x86_64
sudo apt install ./diri_<version>_arm64.deb    # aarch64
```

This installs the desktop entry and the `diri`, `dirijor`, and `dirijor-mcp`
commands. Upgrade by installing the newer package the same way. Remove the
program with `sudo apt remove diri`, or remove it and its system package
metadata with `sudo apt purge diri`. User sessions and preferences are not
deleted by package removal.

The AppImage needs no installation:

```sh
chmod +x diri_<version>_x86_64.AppImage    # or diri_<version>_aarch64.AppImage
./diri_<version>_x86_64.AppImage
```

Diri does not replace packages from inside the app on Linux. Settings shows
the installed version and directs you to update through APT or a newer GitHub
release.

## Build from source

On Ubuntu, install the native GPUI dependencies before running Cargo:

```sh
sudo apt update
sudo apt install build-essential clang cmake libasound2-dev libfontconfig-dev \
  libglib2.0-dev libssl-dev libvulkan1 libwayland-dev libx11-xcb-dev \
  libxkbcommon-x11-dev mesa-vulkan-drivers pkg-config
(cd diri && cargo build --workspace)
```

Creating distribution artifacts additionally needs Node.js 20 or newer and
`cargo-packager` 0.11.8, then `diri/scripts/package-linux.sh`.

## User files

Diri follows the XDG base-directory specification. The defaults are:

| Purpose | Default location |
|---|---|
| Data, PTY holders, injected helpers | `~/.local/share/diri` |
| Session state and logs | `~/.local/state/diri` |
| Host config and manifest overrides | `~/.config/diri` |
| Cache | `~/.cache/diri` |
| Control socket and daemon lock | `$XDG_RUNTIME_DIR/diri` |

When `XDG_RUNTIME_DIR` is unavailable, the runtime directory is
`~/.local/state/diri/run`. `XDG_DATA_HOME`, `XDG_STATE_HOME`,
`XDG_CONFIG_HOME`, and `XDG_CACHE_HOME` override the corresponding roots.
`DIRIJOR_APP_SUPPORT=/absolute/path` deliberately puts every root beneath one
directory; it is useful for isolated test instances. The daemon creates its
private directories with mode `0700` and its Unix socket with mode `0600`.

The main daemon log is normally
`~/.local/state/diri/logs/dirijord.log`. Run `dirijor doctor` to check the
daemon, agent discovery, state file, and active socket without opening the UI.

## Optional integrations

- Coding-agent executables must be installed separately and visible on the
  login shell's `PATH`. Diri ships 23 agent definitions and shows the installed
  CLIs it detects. Claude Code and Codex have the deepest status and resume
  integration; every supported CLI still runs in a real terminal.
- Status sounds use the first available command among `pw-play`, `paplay`, and
  `aplay`. Diri remains fully usable when none is installed.
- SSH password or key-passphrase dialogs use `zenity`, with `kdialog` as a
  fallback. Key-based SSH works without either program.
- Browser test artifacts need a system Node.js 20 or newer plus Playwright's
  browser engines. The reviewed sidecar and its JavaScript dependencies ship
  in both packages; browsers remain an opt-in developer dependency.

## Troubleshooting graphics and display startup

Check Vulkan independently with `vulkaninfo` or `vkcube` from your
distribution's Vulkan tools package. On a hybrid-GPU system, the standard
`DRI_PRIME=1` or Mesa device-selection variables can select another GPU.

Diri follows the active desktop session. To force the X11 path from a Wayland
session, launch it with an empty `WAYLAND_DISPLAY`:

```sh
WAYLAND_DISPLAY= diri
```

On X11, `GPUI_X11_SCALE_FACTOR=1.5 diri` can override incorrect DPI detection.
When reporting a Linux launch or rendering bug, include the Diri version,
package format, distro, kernel, display server, desktop environment, GPU and
driver, plus the privacy-safe diagnostics from Settings.

## Beta limitations

The beta intentionally does not provide native tray or notification actions, automatic in-app package replacement, mobile-companion
connectivity, or remote port forwarding. Approval and status workflows remain
available inside Diri. Start-at-login is hidden until a desktop-neutral
autostart implementation exists.

CI launches a real GPUI window through Xvfb and a headless Weston compositor,
and package smoke tests cover install, upgrade, uninstall, a live shell,
daemon restart/adoption, hooks, and MCP on clean Ubuntu 22.04 and 24.04 jobs
for both x86_64 and aarch64.
Those virtual displays do not replace the manual release matrix for multiple
monitors, fractional scaling, suspend/resume, and native GPU drivers.
