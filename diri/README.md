# diri

For first-party VPS execution, per-node Claude/Codex accounts, fleet usage,
and transactional local↔cloud handoff, see [NODE.md](NODE.md).

`diri` is the Rust + GPUI desktop app, shipped self-contained: the app bundle
carries the Engine (`dirijord-rs`), the session holders that keep agents alive
across daemon restarts and upgrades, the `dirijor` CLI, and the MCP proxy. The
workspace holds the protocol/client core, the session engine, terminal
renderer, shared design system, session store, usage accounting, and
window/sidebar shell. [`PLAN.md`](PLAN.md) is the historical record of the
port from the retired Swift client, kept for its architecture and coexistence
notes.

## Engine

Sessions are owned by *holder* processes, not the daemon: the daemon can
crash, upgrade, or be swapped out and every live agent keeps running, to be
adopted by whatever daemon starts next.

`dirijord-rs` is the authoritative cross-platform Engine
([`crates/diri-engine`](crates/diri-engine)). It owns the control socket and
on-disk state, while `diri-holder` owns local PTYs across Engine restarts.
[`PORT.md`](PORT.md) records the completed migration layer by layer.

Holder recovery does not depend solely on the daemon's global registry. Before
a local Holder starts, the Engine writes a small per-session recovery capsule
beside its provider storage. Hooks and notifications update a separate,
privacy-minimized activity seed before attempting the control socket. A new
daemon can therefore adopt a surviving Holder, reconstruct its identity, and
replay its last lifecycle signal even when the registry write was interrupted.

Authoritative lifecycle transitions are also appended to
`activity-log.jsonl`. Entries contain status and session metadata, not terminal
output, prompts, or tool input. The automation CLI exposes the recent history
and manifest-supported native forks:

```sh
dirijor activity --limit 25
dirijor activity --json
dirijor session fork <session-id-or-title>
```

Fork support is capability-driven: it is offered only when the selected agent
manifest declares a native fork command.

## Install

```sh
brew install --cask cristicretu/diri/diri
```

Or download the DMG from [the latest release](https://github.com/cristicretu/diri/releases/latest).
Either way you get the same universal build, signed and notarized, so it opens
without a Gatekeeper prompt.

The cask lives in [cristicretu/homebrew-diri](https://github.com/cristicretu/homebrew-diri)
rather than `homebrew-cask`, which requires a notability threshold the project
does not meet yet. It declares `auto_updates true`, so Homebrew installs diri
once and then leaves it alone — diri updates itself after that, and
`brew upgrade` will not clobber a build the app moved itself to. See
[UPDATING.md](UPDATING.md) for how that works.

## Toolchain and GPUI pin

- Rust: `1.95.0` (stable, pinned by `rust-toolchain.toml`)
- GPUI source: [`zed-industries/zed`](https://github.com/zed-industries/zed)
- GPUI revision: [`dc2a339d5d043da448a3f7ddc7c0a85c63864aad`](https://github.com/zed-industries/zed/commit/dc2a339d5d043da448a3f7ddc7c0a85c63864aad)
- Revision date: 2026-07-22

The git revision is intentionally exact. Upgrade it deliberately and update this record when doing so.

## Build and run

```sh
cargo build
cargo clippy --workspace -- -D warnings
./scripts/build.sh
```

Run the app under development through `dev.sh`, which builds a throwaway app
bundle with a commit-specific name, bundle id, window title, Dock icon, and
in-window build marker. It also removes agent-session environment variables
that would otherwise stop diri from finding or launching the shared daemon:

```sh
./scripts/dev.sh
./scripts/dev.sh --release
./scripts/dev.sh --settings remote
./scripts/dev.sh -- --features audio-playback
```

The dev and installed apps deliberately share the daemon, socket, sessions,
preferences, and Application Support directory. That makes the dev build useful
against real sessions, but do not focus the same session in both apps at
different terminal sizes: each client can resize the shared PTY.

The app uses blurred window backing, a translucent persistent-width sidebar, an opaque Dirijor Dark terminal card, full-size content under transparent titlebar chrome, adjusted traffic lights, and a 900×560 minimum size.

### Sidebar preview fixtures

Deterministic sidebar fixtures render without connecting to the daemon. Run any scenario with:

```sh
env DIRIJOR_SIDEBAR_PREVIEW=1 DIRIJOR_SIDEBAR_SCENARIO=typical ./scripts/dev.sh
env DIRIJOR_SIDEBAR_PREVIEW=1 DIRIJOR_SIDEBAR_SCENARIO=stress ./scripts/dev.sh
env DIRIJOR_SIDEBAR_PREVIEW=1 DIRIJOR_SIDEBAR_SCENARIO=empty ./scripts/dev.sh
env DIRIJOR_SIDEBAR_PREVIEW=1 DIRIJOR_SIDEBAR_SCENARIO=artifacts ./scripts/dev.sh
```

Preview mode uses deterministic mock dates, account identity, and usage values. It never opens a daemon connection or reads local account/transcript data.

## Remote hosts

Add, edit, or remove execution hosts from **Settings → Remote**. The catalog is
stored per installation in
`~/Library/Application Support/Dirijor/hosts.json`. `forge` is the current
shared host, not a built-in server type or reserved id. Each installation can
use its own SSH user and can add any number of other SSH-reachable machines:

```json
{
  "hosts": [
    {
      "id": "forge",
      "name": "Forge",
      "ssh": "you@forge",
      "defaultCwd": "~/code"
    },
    {
      "id": "studio",
      "name": "Studio Mac",
      "ssh": "studio.local",
      "defaultCwd": "~/Developer"
    }
  ]
}
```

`id` is the stable value persisted with sessions, `name` is presentation only,
and `ssh` accepts either an SSH destination or an alias from `~/.ssh/config`.
Removing the file leaves the app in local-only mode. Tailscale IPv4 addresses
and MagicDNS names work like any other SSH destination when OpenSSH can resolve
them; Diri neither requires nor configures Tailscale for Remote Holder sessions.

## Agent preferences

The default agent is stored in
`~/Library/Application Support/diri/prefs.json` as the agent manifest's stable
`id`. Preferences written before the manifest catalog (`claudeCode`, `codex`,
`cursor`, or `gemini`) remain readable and are rewritten with the canonical id
on the next save. If a saved agent is removed or is no longer available, Diri
chooses the first installed first-class agent; when none are installed it falls
back to the terminal shell.

## Coexistence

`diri` launches the daemon bundled beside it when none is running, and
otherwise talks to whichever Rust Engine owns the socket. A packaged Engine
upgrade may replace an outdated daemon after holders have preserved their
sessions. Until the protocol gains multi-desktop geometry arbitration, do not
focus the same session in two desktop clients at different terminal sizes;
both would resize the same PTY, and input sent from both is interleaved.
