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
Turning access off closes existing connections. The code is kept owner-only
(`0600`) in Diri's application-support directory, so enabling again, or
relaunching Diri, keeps existing phones paired. **New pairing code** in the
same settings page replaces it and every phone must be paired again. Quitting
Diri turns access off.

On the phone, **New session** lets you choose this Mac or a configured SSH
host, a known project or a browsable folder, and an agent installed on that
computer. **Separate workspace** defaults to a fresh worktree from `main`.
The base must already exist on the selected computer; no implicit fetch/pull
or fallback to the currently checked-out feature branch occurs. Use **Branch
options** for a different base. The original checkout is left unchanged.
Remote SSH hosts and agent login should first be initialized from the Mac.

The session menu offers **Review changes** (tracked diff against HEAD), **Fit
text to width** and **Resize session to this phone**. Network errors preserve
the prompt draft. Mutations are never automatically retried: after a dropped
connection, inspect the session before sending or starting again.

## Terminal

A session opens on its terminal, drawn from `diri-web`'s grid stream
(`/grid`, `/history`, `/stream`; see `Terminal/GridModel.swift`): colours,
bold/dim/italic/underline/inverse/strike, the cursor, and wide characters
placed on the cell grid. It is a `UIScrollView` of recycled line layers drawn
with Core Text; a layer is redrawn only when its line changes.

Scrollback lives in an on-device cache (`LineCache`, 256-line pages, 40k lines,
least-recently-used pages evicted). The newest 10k lines are fetched on open,
and while scrolling the pages ahead of the fling are requested, up to four at a
time. Scrolling never waits for the network: an uncached line draws blank
until its page lands. Only the session on screen streams; the last eight keep
their cache and scroll position for switching back.

The session keeps its own columns. With no size chosen, text is scaled to fit
them (down to 7.5 pt, then the view scrolls sideways); pinch sets a size that
sticks. **Resize session to this phone** is the only path that calls
`/resize`. An older gateway without the stream falls back to polling `/screen`
as plain text.

Tapping the terminal (or the keyboard button) types straight into the PTY via
`/input`. The row above the keyboard is the browser terminal's: esc, a sticky
ctrl, ⇥, arrows (repeating while held), ⏎, ⇧⇥, ^C, `|`, `/`, `~`, `-`. It
scrolls sideways and a key acts on release.

A dropped stream reconnects with `?resume=COLSxROWS.<row digests>` and the
gateway answers with only the rows that changed. It reconnects at once on a
network change (the old socket is often open but dead) and on returning to the
foreground, otherwise backs off with jitter up to 4 s; 45 s of silence (two
missed heartbeats) counts as a drop. After 2 s without a frame the screen dims
and says so, but keeps what it showed, even while the computer is unreachable.
Long-press selects the word under your finger; keep holding and drag to
extend it, then Copy, Paste (into the program, bracketed when it asked) or
Select Screen. A tap clears it; ⌘C copies with a hardware keyboard. In a
full-screen program that asked for the mouse, a drag sends wheel reports.

`DiriPhoneUITests/TerminalFixtureTests` drives this against `diri-web`'s
`phone_fixture` (a fake Engine behind a real gateway). A hardware keyboard sends ctrl and option combos, arrows, esc and
tab; ⌘V pastes (bracketed when the program asked for it), ⌘K switches
session, ⌘[ / ⌘] go to the previous / next one, ⌘N starts one. The message
field below the terminal still sends whole prompts through `/send`.

Sessions are grouped by computer. On a narrow screen the list is a drawer
(left-edge swipe or the sidebar button); at 700 pt and wider it is a sidebar.

`-flingBenchmark` (with `-session <id>`) flings through the whole scrollback
and logs `DIRI_FRAME_PROBE` with frame intervals, dropped frames, main-thread
layout+draw time per frame and how many lines showed blank.

## Several computers

Pair more than one gateway (your Mac, `forge`) with **connection menu → Add a
computer…**. Gateways are kept in Keychain in priority order (older builds'
single pairing is migrated on first launch). On launch, on returning to the
foreground, on every network change and every 15 s, all of them are probed
(`/api/health`, 3 s timeout) at once. The highest-priority reachable one is
*live* — shown in the toolbar and used for new sessions — and sessions from
every reachable gateway are listed together, tagged with the gateway's name.
Writes always go to the gateway that owns the session. **Prefer this
computer** moves a gateway to the top.

## Voice mode

The waveform button in a session's message field opens voice mode, a glass
panel over the bottom of the terminal, for use with AirPods and the screen
locked. When the agent finishes a turn (`turn_finished`
on the event stream) it is read aloud — the agent's own `spoken` summary if
there is one, otherwise the last agent turn from `/transcript`, with code and
markup stripped and cut at a sentence near 600 characters. Then it listens
(on-device: `SpeechAnalyzer` on iOS 26+, otherwise `SFSpeechRecognizer` with
`requiresOnDeviceRecognition`) and sends after 1.5 s of silence. The **Talk**
capsule (or a tap anywhere on the panel) interrupts speech, sends early, or
starts talking; **Repeat** and **Mute** sit beside it. Mute stops reading aloud and the automatic listening
after it. While voice mode is open the Mac's `readAloud` setting is on and is
restored on close; the screen poll pauses. Sends are never retried — a failed
one is shown and left to you.

The microphone stays open for as long as voice mode is: an iOS app with the
`audio` background mode is suspended once its audio session goes idle, and
voice mode must still hear the next turn with the screen locked.

DEBUG builds read two launch-environment hooks for UI tests:
`DIRI_VOICE_TEST_AUDIO` (dictate this audio file instead of the microphone;
speech is then synthesized at zero volume) and `DIRI_E2E_CWD` (prefill the new
session folder). The opt-in end-to-end test is
`DiriPhoneUITests/EndToEndTests.swift`; its header documents the variables.

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
xcrun simctl launch booted dev.dirijor.ios \
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

**Deliberate departures.** The desktop row is 28pt, which is a mouse target;
the phone row is 52pt with a 15pt title, read at arm's length. Rows are grouped
by computer rather than project (the branch is the second line), and a quiet
status word ("needs you", "done", "working") stands in for the desktop's chips.
The glyphs, colours and ordering are unchanged.

## What it does

- Sessions grouped by computer, then ordered exactly as the desktop orders them:
  pinned first, then manual rank, then arrival; siblings by creation time
  *ascending*.
- Spawn lineage drawn with the same rails — a full-height line while an ancestor
  still has siblings below, a half-height elbow on the column that parents the
  row, nothing where a rail would imply a relationship that is not there.
- Per-agent status marks tinted by attention: working in the agent's own colour,
  needs-input amber (red when the pending tool call is destructive), done-unseen
  green, idle and ended progressively dimmer.
- Tap a session for its live terminal, typed into directly, plus a message
  field for whole prompts.
- Swipe to mark seen or kill; long-press for the same actions plus fold.
- Pull to refresh, with an SSE stream for liveness and a 15s polling backstop
  for when that stream dies quietly.

## Not built yet

- **Push notifications when a session needs input.** The highest-value missing
  piece: today you have to open the app to discover that something is waiting.
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
