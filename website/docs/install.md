# Install

> Install diri on macOS with the DMG or Homebrew, understand how updates arrive, try the Linux and iPhone betas, and remove every local file.

diri is a native app for macOS 15 or newer. A Linux beta runs on x86_64 Ubuntu, and an iPhone companion is in beta. Agents are installed separately; diri runs the CLIs and accounts already on your machine.

## macOS
The macOS build is universal (Apple silicon and Intel), signed with a Developer ID, and notarized.

### Homebrew

```sh
brew install --cask cristicretu/diri/diri
```

### DMG
1. Download the latest DMG from [GitHub Releases](https://github.com/cristicretu/diri/releases/latest).
2. Open it and drag diri to Applications.
3. Open diri. The first launch checks which agents are installed and offers to install one if none are. See the [Quickstart](/docs/quickstart/).

| Requirement | Value |
| --- | --- |
| macOS | 15 or newer |
| Architecture | Apple silicon or Intel (one universal app) |
| Agents | Installed separately, for example Claude Code or Codex |

## Updates
diri updates itself from the same GitHub release feed. It checks 20 seconds after launch and then every 6 hours. Downloads are verified before they are offered: the code signature, the Team ID and bundle identifier must match the running app, Gatekeeper must accept the notarization, and the version must match the one the feed promised. Only newer versions are offered.

diri never quits or relaunches by itself, because it is holding your sessions. When an update is ready:
1. The sidebar footer shows **Restart to update to** followed by the version.
2. Click it to install and relaunch now, or just quit diri normally. A normal quit installs the update without reopening the app.
3. Running agents keep running. The background Engine is replaced with the new one, and every session reconnects.

To check by hand, open the command palette (<kbd>⌘</kbd><kbd>K</kbd>) and run **Check for Updates…**. It reports "up to date" instead of staying silent.

### Update settings
Open **Settings → General → Software updates**.

| Setting | What it does |
| --- | --- |
| Update automatically | Download verified GitHub releases and install when diri quits. |
| Check Now | Run a check right away. Becomes **Download** or **Restart** when an update is waiting. |
| Skip this version | Hide the offered release until a newer one is available. |

To install a specific release, including an older one, **Option-click** the update button. A **Switch version** list shows the releases the feed still carries, with **Install** next to each. Choosing one turns off **Update automatically** so it is not replaced on the next check. Turn it back on to return to the latest release.

> [!NOTE]
> A build that is not inside a signed `.app`, such as one you built from source, shows "Updates off for this build". If diri sits in a folder your user cannot write to, download the DMG by hand instead.

## Linux beta
diri runs on x86_64 and arm64 Ubuntu 22.04 and 24.04 under Wayland or X11. Linux packages are not included in every release, so check the [release list](https://github.com/cristicretu/diri/releases) for one with Linux assets.

| Requirement | Value |
| --- | --- |
| Distribution | Ubuntu 22.04 or 24.04, x86_64 or arm64 (glibc 2.35 or newer) |
| Display | Wayland or X11 |
| Graphics | A Vulkan 1.3 driver |
| Packages | `.deb` and `.AppImage` |

Verify the download against the `SHA256SUMS` file from the same release:

```sh
sha256sum --ignore-missing --check SHA256SUMS
```

Each Linux file also has a Sigstore bundle. The [Linux guide](https://github.com/cristicretu/diri/blob/main/diri/LINUX.md) has the `cosign verify-blob` command that proves a file was built by the project's CI.

Install the Debian package with APT so its dependencies resolve. It adds the desktop entry and the `diri`, `dirijor` and `dirijor-mcp` commands:

```sh
sudo apt install ./diri_<version>_amd64.deb    # x86_64
sudo apt install ./diri_<version>_arm64.deb    # arm64
```

Or run the AppImage directly:

```sh
chmod +x diri_<version>_x86_64.AppImage    # or diri_<version>_aarch64.AppImage on arm64
./diri_<version>_x86_64.AppImage
```

diri does not update itself on Linux. Settings shows the installed version; update by installing a newer package the same way.

### Linux limits
- No native tray or notification actions. Approvals and status still work inside diri.
- No automatic in-app updates.
- No iPhone companion or remote port forwarding.
- Start at login is hidden.

Shortcuts use Ctrl where macOS uses ⌘. See [Keyboard shortcuts](/docs/keyboard-shortcuts/).

## iPhone companion (beta)
The iPhone app starts, watches and answers sessions on your Mac over [Tailscale](https://tailscale.com). It needs a signed build of the app; there is no App Store release.
1. On the Mac, open **Settings → Phone access** and click **Check this Mac**. Follow the Tailscale guidance until the check passes.
2. On the iPhone, install Tailscale and sign in with the same account.
3. On the Mac, click **Enable phone access & show code**. On the iPhone, tap **Scan pairing code**, or paste the link from **Copy pairing link**.
4. Keep diri open and the Mac plugged in with the lid open. Closing the lid or quitting diri disconnects the phone.

The pairing code controls every session on that Mac, so do not share it. Turning access off closes existing connections, and enabling it again makes a new code. Build details are in [ios/README.md](https://github.com/cristicretu/diri/blob/main/ios/README.md).

## Local data
| Platform | Path | Contents |
| --- | --- | --- |
| macOS | `~/Library/Application Support/Dirijor` | Engine state, session records, logs |
| macOS | `~/Library/Application Support/diri` | App data |
| macOS | `~/Library/Caches/diri/updates` | Downloaded updates and `install.log` |
| Linux | `~/.local/share/diri` | Data and PTY holders |
| Linux | `~/.local/state/diri` | Session state and logs |
| Linux | `~/.config/diri` | Host config and manifest overrides |
| Linux | `~/.cache/diri` | Cache |

On Linux the usual `XDG_*` variables override these roots.

To check the Engine, agent discovery and state file without opening the window, run `dirijor doctor`. On macOS the command lives inside the app:

```sh
/Applications/diri.app/Contents/Resources/bin/dirijor doctor
```

> [!WARNING]
> Logs can contain terminal output, paths and anything a process printed, including secrets. Redact them before attaching them to an issue.

## Uninstall
Quitting diri does not stop your agents. Each session runs in its own background process so it survives the app closing.
1. Close the sessions you want to stop (<kbd>⌘</kbd><kbd>W</kbd>), then quit diri.
2. On macOS, delete diri from Applications, or run `brew uninstall --cask diri` if you used Homebrew. On Linux, run `sudo apt remove diri` or delete the AppImage.
3. To remove your sessions and settings too, delete the data folders listed above.

Package removal never deletes your sessions or preferences on its own. Your project folders and any git worktrees are not touched.
