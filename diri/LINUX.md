# Linux beta

Diri supports x86_64 Ubuntu 22.04 and 24.04 under native Wayland and X11.
The desktop renderer requires a Vulkan 1.3-capable driver. The release build
has a glibc 2.35 floor and does not require Swift, SwiftPM, Xcode, or a macOS
application bundle.

## Install

Download both the artifact and `SHA256SUMS` from the same GitHub release, then
verify the download:

```sh
sha256sum --check SHA256SUMS
```

For Ubuntu or another Debian-based system, install the package with APT so its
runtime dependencies are resolved:

```sh
sudo apt install ./diri_<version>_amd64.deb
```

This installs the desktop entry and the `diri`, `dirijor`, and `dirijor-mcp`
commands. Upgrade by installing the newer package the same way. Remove the
program with `sudo apt remove diri`, or remove it and its system package
metadata with `sudo apt purge diri`. User sessions and preferences are not
deleted by package removal.

The AppImage needs no installation:

```sh
chmod +x diri_<version>_amd64.AppImage
./diri_<version>_amd64.AppImage
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
  login shell's `PATH`. Diri currently advertises Claude Code, Codex, Cursor,
  and Gemini when their commands are available.
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

The first beta intentionally does not provide aarch64 packages, native tray or
notification actions, automatic in-app package replacement, mobile-companion
connectivity, or remote port forwarding. Approval and status workflows remain
available inside Diri. Start-at-login is hidden until a desktop-neutral
autostart implementation exists.

CI launches a real GPUI window through Xvfb and a headless Weston compositor,
and package smoke tests cover install, upgrade, uninstall, a live shell,
daemon restart/adoption, hooks, and MCP on clean Ubuntu 22.04 and 24.04 jobs.
Those virtual displays do not replace the manual release matrix for multiple
monitors, fractional scaling, suspend/resume, and native GPU drivers.
