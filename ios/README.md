# diri for iPhone

A native SwiftUI client for a Dirijor daemon. It talks to [`diri-web`](../diri/crates/diri-web),
which is a frontend on a `dirijord` control socket — the phone never speaks the
daemon's unix-socket protocol itself.

```
iPhone ──http──▶ diri-web ──unix socket──▶ dirijord ──pty──▶ claude/codex
       (tailnet)
```

That indirection is the point. The control protocol assumes a persistent
connection and an event cursor, both of which a phone loses every time it
changes cell tower. HTTP against `diri-web` is stateless per request, so
reconnecting is just the next request succeeding.

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

## Running it against a real daemon

Start `diri-web` on the host and open the link it prints:

```sh
diri-web --listen forge.your-tailnet.ts.net:7380 --label forge
diri-web url          # prints http://…/?token=…
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

The simulator is the only target this repository can verify on its own — CI has
no signing identity, so `CODE_SIGNING_ALLOWED` is off. Getting this onto a
physical phone needs a development team set in Xcode, and a free provisioning
profile expires after seven days. Until that is sorted, the
[web frontend](../diri/crates/diri-web) serves the same daemon to mobile Safari
and installs to the home screen with no signing at all.
