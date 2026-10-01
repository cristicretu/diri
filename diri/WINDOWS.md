# Native Windows Diri

Diri's GUI and Engine run natively on Windows. Local sessions use ConPTY; WSL distributions are execution hosts using the existing Linux Remote PTY Holder. WSLg is not required.

## Install and build

The initial platform floor is **Windows 11 22H2 (build 22621)**, x64 or ARM64. This is an implementation/API floor, not a measured fidelity certification. Install the architecture-matching `diri-<version>-windows-<architecture>-setup.exe`. Installation is per-user under `%LOCALAPPDATA%\Programs\Diri`, with a Start menu shortcut. The MSVC builds link the C runtime statically, without requiring a VC++ redistributable install. No administrator, service, scheduled task, startup registry entry, or WSL configuration change is required. CI's **Windows build and package** artifacts contain unsigned review installers; release installers require Authenticode signing.

For development, install the pinned Rust toolchain, the matching Visual Studio C++ build tools/Windows SDK, and Git for Windows. From `diri/`:

```powershell
cargo build --locked -p diri-app -p diri-engine -p dirijor-mcp
.\target\debug\diri.exe
```

Keep `diri.exe`, `dirijord-rs.exe`, `diri-holder.exe`, `diri-ssh-askpass.exe`, `dirijor.exe`, and `dirijor-mcp.exe` together. The manifest catalog and Helper bundle are required for packaged builds; a development build without the exact Helper catalog fails closed for SSH/WSL sessions.

`windows.yml` builds all three Helper targets on native builders, combines their versioned manifests, builds the native Windows binaries on x64 and ARM64 runners, and packages both architectures. It executes portable library tests, ConPTY interaction tests, and native Holder adoption/tree-cleanup tests on both architectures before packaging. It also lints those test targets. Hosted runner Jobs prohibit breakaway, so `scripts/test-windows-holder.ps1` runs the detached-Holder integration binary through the existing local WMI process provider, with a minimal environment and bounded wait. This test-only harness installs no service and changes no host configuration; the production launcher still fails if its enclosing Job prohibits detachment. The existing Unix CI remains required. To package manually, supply the complete output of `scripts/build-remote-helpers.sh`:

```powershell
./scripts/package-windows.ps1 -Architecture x64 -HelperCatalog target/remote-helpers/manifest.json -Unsigned
# Signed release: certificate already installed in the builder's current-user store.
./scripts/package-windows.ps1 -Architecture arm64 -HelperCatalog target/remote-helpers/manifest.json -CertificateThumbprint <thumbprint>
```

Inno Setup 6 and Windows SDK `signtool` are build-time requirements. The package verifies all three Helper hashes/lengths and at least 20 Agent manifests. No tool is installed on a user's remote host. Signed packaging emits the installer and architecture-specific appcast JSON; publish both with the matching GitHub release. The updater checks HTTPS, declared length/SHA-256, trusted Authenticode status, Diri product/version metadata, and the running app's signer identity. For Azure Artifact Signing Public Trust, this is the same publisher subject and exact profile-specific EKU (never the shared Public Trust marker); daily leaf-certificate rotation is accepted. Other certificates retain an exact thumbprint pin. A publisher/profile change or a traditional certificate rotation requires a manually installed, trusted release. Unsigned development builds cannot enable the signed updater.

Each version installs into its own `versions/<version>` directory. Updates wait for the GUI to exit and leave earlier directories intact, so running Holders retain their binaries. Uninstall only after stopping sessions; uninstall is not a session migration mechanism.

## Sessions and WSL

The default native shell is PowerShell (PowerShell 7 when found, otherwise Windows PowerShell). `SHELL` may select `cmd.exe` or Git Bash. Shell commands explicitly supplied by the user remain shell commands. Agent launches are exact argv/env/cwd vectors: native EXE/COM, PowerShell scripts, and recognized npm Node shims. Arbitrary batch scripts fail with an unsupported-launch error; Diri never constructs an Agent command for `cmd.exe /c`.

Windows launch environments are explicit: OS/profile/temp/path variables, configured language/tool directories, proxy/certificate settings, SSH/Git settings, and selected Agent/provider variables. Session/socket identities are added by the Engine. `TERM=xterm-256color` and `COLORTERM=truecolor` are asserted. Remote environments are still captured on their host; `WSLENV` is removed from transport children.

Installed WSL distributions appear automatically next to SSH hosts (discovery cache: 30 seconds). Diri uses `wsl.exe --distribution <name> --exec /bin/sh …` only for fixed bootstrap/maintenance commands; the session channel carries Helper frames. Bootstrap selects Linux x64 or ARM64 according to the distro, verifies the exact packaged artifact, and activates it atomically. It does not install a service or require systemd.

Explorer `\\wsl$\Ubuntu\home\me\project` and `\\wsl.localhost\Ubuntu\home\me\project` paths route to the matching registered distro and `/home/me/project`. Worktree creation and Git inspection then run on that host. UNC conversion is also used for WSL preference copies and migration files. Native Git operations need Git for Windows; its bundled POSIX tools support the existing fixed repository-migration scripts. SSH preference sync remains the existing optional rsync enhancement, requiring rsync on both ends; it is not part of Helper bootstrap or session transport.

Closing or killing the local Engine does not own the detached Holder's lifetime. Each native Holder owns one ConPTY and one kill-on-close Job. Killing the Holder kills its Job tree; freeze/thaw tracks process handles and verifies Job membership. Windows exit codes retain all DWORD bits and are never reported as POSIX signals. Windows logoff and `wsl --shutdown` are outside detach guarantees; the latter terminates that VM and its sessions. The Linux Helper's existing persistence probe reports its capability without changing `.wslconfig` or system services.

## Platform decisions

IPC uses Windows AF_UNIX with IOCP for async clients. The logical endpoints remain the Engine/Holder socket paths; physical endpoints use bounded, SID-scoped hashes under `%USERPROFILE%\.diri-ipc-<SID hash>`. An exclusive lock serializes endpoint creation/recovery. This preserves byte-stream framing and the existing Rust Engine `Hello` identity check. It avoids a separate named-pipe protocol implementation; there is no TCP fallback. Windows enforces socket/parent DACLs ([Microsoft AF_UNIX documentation](https://devblogs.microsoft.com/commandline/af_unix-comes-to-windows/)).

`diri-platform` owns the file policy: protected current-user SID DACLs, no inherited broad access, final reparse-point rejection, regular-file/owner validation, and pinned directories for completed-terminal publication. The account/state/cache layout uses `%APPDATA%` and `%LOCALAPPDATA%`; executables use `.exe`. Windows does not provide the Unix directory-fsync guarantee through an ordinary user directory handle; files are flushed before publication. No stronger power-loss guarantee is claimed.

System ConPTY uses documented `CreatePseudoConsole` flags (`0`). No undocumented passthrough flag, bundled `conpty.dll`, or OpenConsole dependency is assumed. Microsoft's API floor is older than our desktop floor; that is not evidence of Agent TUI fidelity. ConPTY's synchronous input/output pipes have separate adapters, while bounded Holder queues keep attached clients off the PTY drain path. See [CreatePseudoConsole](https://learn.microsoft.com/en-us/windows/console/createpseudoconsole) and [pseudoconsole lifecycle](https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session).

## macOS feature audit

| Feature / source areas | Windows disposition |
|---|---|
| GPUI windowing, main/root, frame restore | Native Win32 GPUI windows; existing restore/layout behavior. |
| Menus, commands, held hints | Existing non-Mac shortcut mapping and command palette; no macOS menu-bar app. Terminal Ctrl input remains separate from app shortcuts. |
| Floating panels, session surfaces, tab peek/preview, sidebar motion | Existing in-window GPUI surfaces; Mica replaces the root glass material. AppKit child-panel placement/vibrancy and private window-level APIs are intentionally omitted. |
| Fonts, SF Symbols | Segoe UI, Cascadia Mono/Code, then Consolas; existing non-Mac vector/icon fallback. |
| DPI, system theme, clipboard, Explorer drops | GPUI Windows backend. OSC 52 writes go through the shared application clipboard route. ConPTY preservation of OSC 52 remains unmeasured. |
| Notifications, notification panel/store | WinRT toasts with the installed AUMID; open/reply/dismiss callbacks while Diri runs, shared reply validation and inbox. Health surface reports disabled/failed delivery. No COM background activation after the application exits; no macOS Dock badge. |
| Sounds | Win32 in-memory WAVE playback, same chime synthesis and volume scaling. |
| Haptics | Intentionally omitted: no assumed Windows trackpad haptic API. |
| WebKit inspector browser | Replaced by the default browser, with explicit UI copy. Embedded WebView2, embedded back/forward navigation, and WebKit automation are intentionally not shipped in this baseline. |
| SSH askpass | Native credential/host-confirmation dialogs; secrets go only to OpenSSH stdout. Windows SSH uses independent channels, not ControlMaster. |
| Secure input | No AppKit secure-event-input global mode. ConPTY does not expose a reliable secret-input fact; report unknown, not a fabricated echo state. |
| Updater, app identity/icon | Signed versioned per-user installer, PE version/icon resources, Start menu identity. |
| Usage/account authentication | Existing file-based non-Mac credentials; system curl for requests. macOS Keychain/Cursor decryption is intentionally absent. Native Claude account sign-in uses a fixed PowerShell script with positional data. |
| Phone access | Existing network/web endpoint. No `caffeinate` equivalent: Windows sleep policy remains user-controlled. |
| Diagnostics, telemetry | Shared collection/redaction with Windows home paths; macOS-only `sw_vers`, AppKit probes and native signposts omitted. |
| History, quick open, settings, navigation/workspaces | Shared Rust UI/storage; WSL host catalog comes from the Engine rather than local SSH-only preferences. |

All macOS-specific implementations remain isolated in `diri-app/src/macos/`. The table covers gates in `main`, `root`, `application_notifications`, `commands`, `platform`, `alerts`, `external_drop`, `floating`, `fonts`, `haptics`, `held_hints`, `inspector`, `launcher`, `menu_inbox`, `navigation`, `notification_panel`, `notifications`, `phone_access`, `secure_input`, `session_links`, `session_surfaces`, `sidebar/**`, `sounds`, `store/**`, `surface_shell`, `tab_peek*`, `tab_preview`, `telemetry`, `terminal_pane/**`, `updates`, `usage/**`, `usage_page`, `usage_share`, `window_restore`, and `workspace_workbench`. Test-only macOS fixtures remain platform-specific.

## Agent catalog audit

All 22 established manifests remain packaged. None supplies a POSIX absolute executable or shell-composed Agent argv. The Windows resolver supports each manifest's bare executable name with PATH/PATHEXT discovery; this is **launch plumbing, not a certification of the vendor's native Windows release**.

| Manifests | Native launch / setup disposition |
|---|---|
| `shell`, `generic` | PowerShell/cmd/Git Bash or explicit structured command. |
| `claude-code` | Native `claude.exe`; Windows vendor installer offered. Git for Windows remains the vendor's shell dependency for hooks/tools. |
| `codex`, `gemini` | Native executable or recognized npm shim; npm setup command offered in PowerShell. |
| `opencode`, `cursor`, `amp`, `maki`, `droid`, `cline`, `antigravity`, `grok`, `hermes`, `devin`, `aider`, `kiro`, `copilot`, `kimi`, `kilo`, `pi`, `qoder` | Discover a vendor-supplied native executable/shim when present. No POSIX installer is offered on Windows. Use vendor setup or select WSL for its Linux CLI; native TUI compatibility has not been measured. |

Native Agent exit ends that native session; it does not inject a Unix login-shell wrapper. Remote Unix sessions retain their established behavior. Native support and ConPTY fidelity must be assessed separately for each Agent version.

## Fidelity capture and outstanding evidence

The original implementation was supplied without test execution. The consolidated Windows PR now enables native test execution in CI and includes the subsequent Windows 11 x64 runtime fixes. The contributor reported interactive PowerShell, same-PID Engine-restart adoption, Job-tree termination, and GUI startup on build 26200. These observations do not certify all Agent TUIs, ARM64 runtime behavior, installer updates, or latency. See `REMOTE_PORT.md` for the recorded decision.

The manual `pty_fidelity` example records raw bytes, the canonical encoded full grid (including style/link metadata), cursor, alternate screen, bracketed paste, mouse/keyboard modes, and OSC 52 clipboard observations at scripted checkpoints. It uses the production Unix PTY or ConPTY and the same `HeadlessScreen` parser. It is compiled, not executed, in the Windows build workflow.

```powershell
cargo build -p diri-terminal-state --example pty_fidelity
# Supply the platform-specific command/cwd in scenario.json; no shell composition.
./target/debug/examples/pty_fidelity.exe scenario.json windows.json
./target/debug/examples/pty_fidelity.exe --compare macos.json windows.json
```

Example scenario (replace the executable/cwd with absolute paths on each host):

```json
{"argv":["C:/src/diri/target/debug/examples/pty_fidelity.exe","--emit"],"cwd":"C:/src/diri","steps":[{"settleMs":500},{"size":[100,32],"settleMs":500},{"size":[80,24],"settleMs":500}]}
```

Steps may include `input` (JSON escapes preserve VT bytes) for bracketed paste, Alt/meta, mouse and Agent interactions. `--emit` supplies a bounded VT sample with truecolor, wide characters, links, clipboard and mode requests. Run the equivalent scenario on macOS with the Unix example binary; compare captures and inspect each differing field. Use disposable sessions: captures intentionally contain raw terminal output and may contain prompts or credentials. Captures are owner-only local files and are not telemetry; do not upload real-session captures unreviewed.

No sequence is labeled passing or broken without captures. Passthrough availability and bundled OpenConsole remain unmeasured; neither is a runtime fallback. Before certifying a Windows release, collect native Claude/Codex and VT-stress captures, Engine-kill/reattach identity/grid evidence, Holder tree cleanup, WSL persistence outcomes, slow-client recovery, monitor-DPI/clipboard/drop screenshots, and the existing latency percentiles (snapshot p90 ≤100 ms, input p95 ≤10 ms, output p90 ≤50 ms). Terminal correctness remains a release gate.


## Release readiness checklist

All implementation and follow-up fixes belong to the consolidated Windows PR;
there is no separate runtime-fixes PR to merge afterward. A green package build
alone does not certify Windows support. Before announcing a supported release:

- [ ] Pass native x64 and ARM64 tests/lints and the existing macOS/Linux gates
      on the final merged source. Windows Git test execution disables inherited
      `core.fsmonitor`; the queue test explicitly exercises redundant wakeups.
- [ ] Capture native Claude/Codex and scripted VT fidelity against macOS,
      including resize, paste, mouse/keyboard modes, hyperlinks and clipboard.
- [ ] Verify real Engine termination/restart, slow-client recovery, and native
      latency gates. Automated Session-detach/adoption and Holder-death tests
      supplement, but do not replace, this end-to-end evidence.
- [ ] Exercise installed WSL distro discovery, exact Helper bootstrap, input,
      reattach and persistence reporting. Deterministic parsing/path/command
      tests do not certify a real WSL lifecycle.
- [ ] Test the installed GUI on x64/ARM64: mixed DPI, clipboard, Explorer drops,
      native shortcuts/caption controls, notifications and SSH askpass.
- [ ] Produce Authenticode-signed installers and verify fresh install, upgrade
      with live sessions, signature rejection and uninstall. CI installers are
      explicitly unsigned review artifacts until release signing is supplied.

Native ConPTY does not expose POSIX foreground process groups, another process's
working directory, or canonical-line wait state. The newer Unix shell job-name,
`cd` tracking and line-wait observations remain unavailable on native Windows;
unknown facts are not reported as successful probes. WSL uses the Linux behavior.


## Release signing setup

The Windows workflow packages unsigned review installers and their staged
payloads on pull requests. On `main`, `.github/workflows/windows-sign.yml`
can sign both architectures using Azure Artifact Signing and GitHub OIDC.
It signs all six application executables, assembles the installer from that
signed payload, signs and timestamps the installer, verifies signatures, then
writes the appcast hash/length from the final signed bytes. It never accepts
an arbitrary run ID or signs a pull-request artifact. The resulting
`diri-windows-<architecture>-signed` artifact contains the installer and feed;
publish both on the matching GitHub release.

A maintainer must complete the external setup before enabling this job:

1. Create an Azure Artifact Signing account, complete identity validation, and
   create a **Public Trust certificate profile**.
2. Create an Entra app with a federated credential for
   `repo:cristicretu/diri:environment:windows-signing` and the Artifact Signing
   Certificate Profile Signer role on that account.
3. Create the GitHub `windows-signing` environment, restricted to `main`, with
   `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, and `AZURE_SUBSCRIPTION_ID` secrets.
4. Set repository variables `DIRI_ARTIFACT_SIGNING_ENDPOINT`,
   `DIRI_ARTIFACT_SIGNING_ACCOUNT`, and `DIRI_ARTIFACT_SIGNING_PROFILE`.
5. Set `DIRI_WINDOWS_SIGNING=artifact-signing`, then dispatch **Windows build
   and package** on `main`. Validate a real signed install/update before release.

No private key is stored in GitHub. The profile-specific EKU persists across
leaf renewal; deleting/recreating a profile changes that identity. Microsoft's
[certificate management documentation](https://learn.microsoft.com/en-us/azure/artifact-signing/concept-certificate-management)
explains the daily renewal and stable profile EKU. Traditional certificate/HSM
builders can still use `package-windows.ps1 -CertificateThumbprint ...`.
Real signing remains unverified until the maintainer provisions the account;
policy unit tests cannot prove certificate issuance or SmartScreen behavior.
