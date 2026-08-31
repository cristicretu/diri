<h1><img src="docs/images/diri-wordmark.png" alt="diri" width="300"></h1>

[![CI](https://github.com/cristicretu/diri/actions/workflows/ci.yml/badge.svg)](https://github.com/cristicretu/diri/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![GitHub release](https://img.shields.io/github/v/release/cristicretu/diri)](https://github.com/cristicretu/diri/releases/latest)

Native desktop orchestrator for coding agents on macOS and Linux. Run Claude Code, Codex, Cursor, Gemini and plain
shells in parallel — across git worktrees or on remote hosts — each with a live status
(working / needs-you / done) and tmux-like persistence: closing the app never kills a session,
and a daemon restart brings conversations back.

![diri](docs/images/diri.png)

<p align="center"><img src="docs/images/diri-divider-status.png" alt="" width="760"></p>

## Install

### macOS

```sh
brew install --cask cristicretu/diri/diri
```

Or download the latest DMG from [Releases](https://github.com/cristicretu/diri/releases/latest),
open it, and drag diri to Applications. Either way it is the same universal build (Apple
silicon and Intel), signed and notarized. diri updates itself from there.

<p align="center"><img src="docs/images/diri-install.png" alt="Diri moving the app into the Applications folder" width="680"></p>

The tap has to be named in full — a bare `diri` resolves only against Homebrew's default
taps. The cask lives in [cristicretu/homebrew-diri](https://github.com/cristicretu/homebrew-diri)
rather than `homebrew-cask`, which requires a notability threshold diri does not meet yet.

macOS 15 or newer.

### Linux beta

Download the x86_64 AppImage or Debian package from
[Releases](https://github.com/cristicretu/diri/releases/latest). Ubuntu 22.04
and 24.04 are supported under X11 and Wayland with a Vulkan 1.3-capable GPU.

```sh
sudo apt install ./diri_<version>_amd64.deb
# or
chmod +x diri_<version>_amd64.AppImage && ./diri_<version>_amd64.AppImage
```

See the [Linux beta guide](diri/LINUX.md) for checksums, upgrade and uninstall
steps, XDG paths, graphics troubleshooting, and current limitations.

## 60-second tour

1. Add a project directory and create a session for Claude Code, Codex, another
   supported agent, or a plain shell.
2. Start several sessions, ideally in separate git worktrees when they edit the
   same repository.
3. Watch the sidebar instead of every terminal: it shows which agents are
   working, waiting for you, or done.
4. Quit and reopen diri. The daemon keeps each PTY alive and replays the session
   when you return.

The [getting-started guide](docs/GETTING_STARTED.md) covers remote hosts, MCP
orchestration, diagnostics, local data, and uninstalling.

<p align="center"><img src="docs/images/diri-divider-worktrees.png" alt="" width="760"></p>

## What it does

- **Many agents at once.** Each session is a real terminal with a real PTY. Group them by
  project, split them across git worktrees, or run them on a remote host over ssh+tmux.
- **Status you can trust.** diri reads what an agent actually painted on its screen and tells
  you which ones are working, which are waiting on you, and which are done — so you can watch
  ten sessions without reading ten terminals.
- **Sessions outlive the app.** A background daemon owns the PTYs. Quit diri, reopen it, and
  everything is still there.
- **Agents can orchestrate agents.** An MCP server lets a running agent spawn another one,
  watch it, read its output, and answer its prompts.

First-class status detection and resume are Claude Code and Codex. Cursor and Gemini run with
partial support, and anything else runs as a terminal with running/exited status.

![Claude Code, Codex, Cursor, Gemini, and shell agents](docs/images/diri-agent-lineup.png)

## Architecture

Two processes, one wire protocol:

![Diri architecture: app and CLI connect through the control socket to the engine, persistent PTY holders, and coding agents](docs/images/diri-architecture.png)

- **`diri`** — the desktop app: Rust + [GPUI](https://github.com/zed-industries/zed). Owns the
  window, sidebar, terminal renderer, command palette, and usage accounting. Lives in
  [`diri/`](diri/).
- **`dirijord-rs`** — the headless Rust engine, launched by the app and outliving it. Owns PTYs and
  child agent processes, an offset-addressed output log per session (for detach and replay), a
  headless terminal emulator for status detection, the session registry and persistence,
  worktrees, and the control socket.

`dirijor` is the automation CLI for hooks, notifications, status, and diagnostics;
`dirijor-mcp` is the MCP stdio server injected into agents. `diri-holder` owns each PTY master so
sessions survive an engine restart. Every shipped executable is built from the Rust workspace in
[`diri/`](diri/).

## Adding an agent

Agent support is data, not code. Each agent is one JSON file in
`diri/crates/diri-engine/manifests/` describing how to spawn it, how to resume, which keys
approve or deny a prompt, and the screen rules that decide whether it is working, waiting, or
done. Copy the closest existing manifest and adjust it — no code changes required. The
[manifest-authoring guide](docs/AGENT-MANIFESTS.md) explains the schema, safe capture workflow,
examples, overrides, and validation. This is the easiest way to contribute.

## Building from source

Needs Rust (pinned in `diri/rust-toolchain.toml`) plus the platform build dependencies. On macOS
that means the Xcode command-line tools; Linux dependencies and packaging commands are listed in
[`diri/LINUX.md`](diri/LINUX.md). The first
build compiles GPUI from a pinned Zed revision and takes a while.

```sh
(cd diri && cargo build)                   # app, engine, holder, CLI, MCP
(cd diri && cargo test --workspace)
(cd diri && cargo run -p diri-app)         # run the app from source

diri/scripts/package.sh                    # full bundle
diri/scripts/package-linux.sh              # AppImage + DEB, on x86_64 Linux
diri/scripts/install-local.sh
```

Run the same core checks as CI with one command:

```sh
./scripts/check.sh
```

[`diri/PACKAGING.md`](diri/PACKAGING.md) covers signing and notarization,
[`diri/UPDATING.md`](diri/UPDATING.md) the updater and release flow,
[`diri/NODE.md`](diri/NODE.md) running agents on a remote VPS node.

The [documentation index](docs/README.md) links user and engineering guides.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Bug reports, fixes, docs, and new agent
manifests are all welcome. New contributors can start with
[`good first issue`](https://github.com/cristicretu/diri/labels/good%20first%20issue)
or [`help wanted`](https://github.com/cristicretu/diri/labels/help%20wanted).

Questions belong in [Discussions](https://github.com/cristicretu/diri/discussions),
reproducible bugs in [Issues](https://github.com/cristicretu/diri/issues), and
vulnerabilities in [private security reports](SECURITY.md). See the
[roadmap](ROADMAP.md), [support guide](SUPPORT.md), [privacy notice](PRIVACY.md),
and [governance](GOVERNANCE.md) for project expectations.

## License

Diri's original source is Apache 2.0. Builds also contain third-party software
under its own licenses; see [LICENSE](LICENSE), [NOTICE](NOTICE), and the
machine-checked dependency policy in [`license-policy.json`](license-policy.json).
