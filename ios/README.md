# diri for iPhone

Start, monitor, and control Diri coding-agent sessions from an iPhone. The
native SwiftUI client can create local or preconfigured SSH sessions, send
prompts, answer agent questions, follow terminal output, and review tracked
changes.

The client talks to [`diri-web`](../diri/crates/diri-web), a small HTTP frontend
on the local `dirijord` control socket. The phone never speaks the daemon's Unix
socket protocol itself.

```
iPhone ──http──▶ diri-web ──unix socket──▶ dirijord ──pty──▶ claude/codex
       (tailnet)
```

The control protocol assumes a persistent connection and an event cursor, both
of which a phone can lose as its network changes. HTTP requests to `diri-web`
are stateless, so the next successful request reconnects the client.

## Build

The Xcode project is generated, not committed — edit `project.yml` instead.

```sh
brew install xcodegen
cd ios && xcodegen generate
open DiriPhone.xcodeproj
```

From the command line:

```sh
xcodebuild -project DiriPhone.xcodeproj -scheme DiriPhone \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' test
```

## Set up phone access (no terminal required)

1. Install a signed Diri iPhone build. Distribution is described below.
2. In Diri on the Mac, open **Settings → Phone access → Check this Mac**.
   Follow the install/sign-in/connect guidance, then check again. Diri only
   reads Tailscale's status; installation and VPN permissions stay in Tailscale.
3. Follow the iPhone setup guide. It links directly to Tailscale in the App
   Store: sign in with the same account, allow the VPN configuration, and return
   to Diri. No exit node, Tailscale SSH, router changes or commands are needed.
4. On the Mac, **Enable phone access & show code**. On the phone, tap
   **Scan pairing code**. Alternatively copy the
   pairing link from the Mac and paste it on the phone. Connection is verified
   before saving credentials in Keychain.
5. Keep Diri open and the Mac plugged in with its lid open. Diri prevents idle
   sleep while access is enabled; closing the lid or explicitly sleeping still
   disconnects the phone. The display can turn off.

The gateway is embedded in the Mac app, so there is no separate server binary
to install or launch. Its bind is Tailscale-only, not your public/LAN address.
The [Tailscale CLI](https://tailscale.com/docs/reference/tailscale-cli?tab=macos)
is probed read-only with `TAILSCALE_BE_CLI=1`; Diri does not change VPN settings.
The pairing code controls all sessions exposed by this Mac: do not share it.
Turning access off closes existing connections; enabling again produces a new
code, and the phone must be paired again. Quitting Diri turns access off.

On the phone, **New session** lets you choose this Mac or a configured SSH
host, a known project or a browsable folder, and an agent installed on that
computer. **Separate workspace** defaults to a fresh worktree from `main`.
The base must already exist on the selected computer; no implicit fetch/pull
or fallback to the currently checked-out feature branch occurs. Use **Branch
options** for a different base. The original checkout is left unchanged.
Remote SSH hosts and agent login should first be initialized from the Mac.

The session menu offers **Review changes** (tracked diff against HEAD) and
**Follow output**. Network errors preserve the prompt draft and show a stale
screen warning. Mutations are never automatically retried: after a dropped
connection, inspect the session before sending or starting again.

## Developer CLI / standalone gateway

Start `diri-web` on the host and explicitly print its pairing link:

```sh
diri-web --listen forge.your-tailnet.ts.net:7380 --label forge
diri-web url --listen forge.your-tailnet.ts.net:7380
```

Paste that link into the app's first screen. It is split into a base URL and a
token, and the token is stored in the keychain — it is a bearer credential that
can start and kill processes on a real machine, so it does not go in
`UserDefaults`.

For the simulator, two launch arguments skip the setup screen:

```sh
xcrun simctl launch booted com.cristicretu.diri.phone \
  -endpoint 'http://forge:7380/?token=…' \
  -session s_1ab7417f0c93          # optional: open straight into one session
```

## Fidelity to the desktop sidebar

The session list is meant to read as the same surface as diri's sidebar, so the
design tokens, the ordering model and the lineage rails are transcribed from the
Rust rather than reinvented:

| Phone | Desktop |
| --- | --- |
| `Design/Tokens.swift` | `diri-ui/src/tokens.rs` |
| `Design/BrandMark.swift` | `diri-ui/src/brand.rs` |
| `Views/StatusGlyphView.swift` | `diri-ui/src/status.rs` |
| `Model/SidebarProjection.swift` | `diri-app/src/store/projection.rs` |
| `Views/SessionRowView.swift` | `diri-app/src/sidebar/view.rs` |

The agent marks parse the same 24×24 SVG path data the desktop parses, rather
than shipping rasterised copies that would silently go stale when the artwork
changes.

**One deliberate departure.** The desktop row is 28pt, which is a mouse target;
a finger needs 44. The row grows and nothing inside it does — type sizes, the
16pt glyph, the 12pt indent and the 7pt corner radius are all unchanged — so the
row reads identically and simply breathes more. `Tokens.Metrics.desktopRowHeight`
keeps the original value visible next to it.

## What it does

- Sessions grouped by project, ordered exactly as the desktop orders them:
  pinned first, then manual rank, then arrival; siblings by creation time
  *ascending*.
- Spawn lineage drawn with the same rails — a full-height line while an ancestor
  still has siblings below, a half-height elbow on the column that parents the
  row, nothing where a rail would imply a relationship that is not there.
- Per-agent status marks tinted by attention: working in the agent's own colour,
  needs-input amber (red when the pending tool call is destructive), done-unseen
  green, idle and ended progressively dimmer.
- Tap a session for its live screen, a composer, and a key row that writes raw
  terminal bytes so permission prompts and menus are answerable.
- Swipe to mark seen or kill; long-press for the same actions plus fold.
- Pull to refresh, with an SSE stream for liveness and a 15s polling backstop
  for when that stream dies quietly.

## Not built yet

- **Push notifications when a session needs input.** The highest-value missing
  piece: today you have to open the app to discover that something is waiting.
- **Colour and cursor in the terminal.** The view renders `read_screen`'s plain
  text. The daemon also exposes `read_scrollback_cells` and a grid protocol, so
  a faithful renderer is available, just not written.
- **Archived bucket, rename, drag-to-reorder.** The projection already computes
  the archived list and honours a manual order; no UI reaches them.

## Distribution

See [TESTFLIGHT.md](TESTFLIGHT.md) for owner setup, signing, privacy/review
notes, upload steps, and the physical-device checklist. Simulator signing is
off; device builds use automatic signing with your explicit development team.

```sh
bash scripts/testflight.sh check
DIRI_APPLE_TEAM_ID=YOURTEAMID DIRI_BUILD_NUMBER=2 \
  bash scripts/testflight.sh archive
```

The scripts never upload, invite testers, or overwrite an existing archive.
The committed app icon is generated from the code-native Diri terminal mark;
run `xcrun swift scripts/render-app-icon.swift` only when updating that artwork.
A successful unsigned build is not proof of signing, TestFlight acceptance,
camera permission behavior, or real cellular/Tailscale connectivity.
