# Contributing to diri

Bug reports, fixes, and new agent support are all welcome. There is no CLA —
contributions are Apache 2.0, same as the project.

## What you need

- macOS 15 or newer, on Apple silicon or Intel
- Xcode command-line tools (Swift 6)
- Rust — the toolchain is pinned in `diri/rust-toolchain.toml` and rustup will
  fetch it for you
- Node 20 or newer, only if you touch the browser sidecar

The first Rust build compiles GPUI from a pinned Zed revision and takes a while.
Later builds are incremental.

## The two halves

diri is one app made of two codebases, and which one you touch depends on what
you are changing:

- **`diri/`** — the Rust + GPUI desktop app. Window, sidebar, terminal
  rendering, command palette, usage accounting.
- **`Sources/`** — the Swift engine. `dirijord` owns the PTYs and child agent
  processes so sessions outlive the app; `dirijor` is the CLI and MCP shim.

They talk over a Unix socket. The app never owns a session directly — if you are
changing what a session *does*, you are probably in `Sources/`.

## Build and test

The one-command contributor check runs shell/release guards, the Swift suite,
Rust formatting, Clippy, Rust tests, and the dependency-license policy:

```sh
./scripts/check.sh
```

Pass `--browser` to also install the sidecar dependencies and run Playwright's
browser integration tests. The default stays self-contained after toolchains and
dependencies have been fetched once.

To run one half while iterating:

```sh
swift build && swift test          # engine
(cd diri && cargo build && cargo test)   # app
```

Before opening a pull request, run what CI runs:

```sh
swift test
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
- CI runs `swift test --no-parallel`. Tests that block a thread while holding a
  PTY can starve the cooperative pool on a small runner.
- Browser tests are opt-in behind `DIRIJOR_RUN_BROWSER_TESTS=1` and need
  `npx playwright install` first.
- Two tests are skipped on CI because they hang there (#1). They still run
  locally. To take a stack from a runner instead of guessing, run the manual
  `Hang repro` workflow: it sets `DIRIJOR_RUN_HANGING_TESTS=1` and runs
  `scripts/sample-hung-tests.sh` alongside the suite, which samples the test
  binary and everything it spawned. That script works on any stuck run —
  `./scripts/sample-hung-tests.sh 60 1 0` while a local `swift test` is wedged
  prints the same thing.

## Adding an agent

This is the easiest place to start and needs no Swift or Rust. Agent support is
data: each agent is one JSON file in `Sources/DirijorCore/Resources/manifests/`
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
