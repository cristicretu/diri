# Contributing to diri

Bug reports, fixes, and new agent support are all welcome. There is no CLA —
contributions are Apache 2.0, same as the project.

## What you need

- macOS 15 or newer, on Apple silicon or Intel
- Xcode command-line tools
- Rust — the toolchain is pinned in `diri/rust-toolchain.toml` and rustup will
  fetch it for you
- Node 20 or newer, only if you touch the browser sidecar

The first Rust build compiles GPUI from a pinned Zed revision and takes a while.
Later builds are incremental.

## Workspace map

diri ships from the Rust workspace under `diri/`:

- **`crates/diri-app`** — GPUI window, sidebar, terminal surfaces, and settings.
- **`crates/diri-engine`** — local and remote session orchestration, PTYs,
  holders, persistence, status detection, and the control socket.
- **`crates/dirijor-mcp`** — the Rust `dirijor` automation CLI and MCP frontend.
- **`crates/diri-proto`**, **`diri-client`**, and **`diri-term`** — shared wire
  types, app-to-engine client, and terminal renderer.

The app never owns a session directly. It talks to the Engine over a Unix
socket; holder processes retain PTYs while either process restarts.

## Build and test

The one-command contributor check runs shell/release guards, Rust formatting,
Clippy, workspace tests, and the dependency-license policy:

```sh
./scripts/check.sh
```

Pass `--browser` to also install the sidecar dependencies and run Playwright's
browser integration tests. The default stays self-contained after toolchains and
dependencies have been fetched once.

To build and test while iterating:

```sh
(cd diri && cargo build)
(cd diri && cargo test --workspace)
```

Before opening a pull request, run what CI runs:

```sh
(cd diri && cargo fmt --all -- --check)
(cd diri && cargo clippy --workspace --all-targets -- -D warnings)
(cd diri && cargo test --workspace)
```

To try your change in the real app, `diri/scripts/package.sh` builds the bundle
and `diri/scripts/install-local.sh` installs it to `~/Applications`.

## Notes on the test suite

The engine tests spawn real PTYs, child processes, and git repositories. A few
consequences worth knowing:

- They are wall-clock sensitive. `DIRIJOR_TEST_TIMEOUT_SCALE` multiplies every
  liveness wait; CI sets it to 6. Raise it locally if your machine is loaded.
- Browser tests are opt-in behind `DIRIJOR_RUN_BROWSER_TESTS=1` and need
  `npx playwright install` first.

## Adding an agent

This is the easiest place to start and needs no Rust. Agent support is data:
each agent is one JSON file in `diri/crates/diri-engine/manifests/`
describing how to spawn it, how to resume a session, which keystrokes approve or
deny, and the screen predicates that decide whether it is working, waiting on
you, or done. Copy the closest existing manifest and adjust it.

Claude Code and Codex have first-class status detection and resume. Anything
without a manifest still runs as a plain terminal.

## Pull requests

Keep the change focused, explain why in the description, and say how you tested
it. If it changes behavior the daemon owns, mention whether existing sessions
survive it — that property matters more than almost anything else here.

CI must be green before merge. Maintainers may ask for a design issue first when
a change creates a new trust boundary, persistent format, or compatibility
commitment. See [GOVERNANCE.md](GOVERNANCE.md) for how decisions are made and
[SECURITY.md](SECURITY.md) for private vulnerability reports.
