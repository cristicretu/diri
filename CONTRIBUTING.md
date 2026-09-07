# Contributing

Diri values reliable sessions, a compact interface, and clear behavior. A good
contribution solves a specific problem and keeps those properties intact.

## Choose a change

New here? Browse the [good first issues](https://github.com/cristicretu/diri/issues?q=is%3Aissue%20is%3Aopen%20label%3A%22good%20first%20issue%22).
Each one includes a starting point, a bounded scope, and verification steps.

- **Bugs:** include reproduction steps, expected behavior, and your environment
  in a [bug report](https://github.com/cristicretu/diri/issues/new?template=bug_report.yml).
- **Fixes and docs:** open a focused PR. An issue is useful context, not a
  prerequisite for a small change.
- **Agent support:** start with the [manifest guide](docs/AGENT-MANIFESTS.md).
  Launch commands and status rules live in JSON under
  [`diri-engine/manifests`](diri/crates/diri-engine/manifests/).
- **Larger changes:** discuss the problem first, especially when adding a new
  trust boundary, persistent format, dependency, or compatibility commitment.
  Use [Discussions](https://github.com/cristicretu/diri/discussions) for early
  ideas and a [feature request](https://github.com/cristicretu/diri/issues/new?template=feature_request.yml)
  for a concrete proposal.

## Set up

Fork and clone the repository. Install Rust through rustup; the workspace's
[`rust-toolchain.toml`](diri/rust-toolchain.toml) selects the compiler.

On macOS, use macOS 15 or newer with the Xcode command-line tools. On Linux,
follow the [native dependency setup](diri/LINUX.md#build-from-source).
Node.js 20 or newer is needed for browser-sidecar work and packaging.

```sh
cd diri
cargo build --workspace
cargo test -p diri-engine    # choose the package you changed
```

The first build compiles GPUI from a pinned Zed revision. Subsequent builds
are incremental. On macOS, run `./scripts/dev.sh` from `diri/` to try your change
in an app bundle. The dev app shares sessions and preferences with the installed
app; see the [development guide](diri/README.md#build-and-run).

## Find the code

All desktop behavior lives in the Rust workspace under [`diri/`](diri/).
Read [AGENTS.md](AGENTS.md) before changing it.

| Area | Crate under `diri/crates/` |
| :--- | :--- |
| Desktop interface | `diri-app` |
| Sessions, worktrees, status, and orchestration | `diri-engine` |
| Wire types and local client | `diri-proto`, `diri-client` |
| Terminal rendering and shared parsing | `diri-term`, `diri-terminal-state` |
| Remote Helper | `diri-remote` |
| Automation CLI and MCP server | `dirijor-mcp` |

The Engine owns session records; Holders keep the PTYs and agent processes
alive. Read the [remote architecture](diri/REMOTE_PORT.md) before changing
remote sessions, SSH, Holders, terminal state, or packaging.

## Verify

Start with the narrowest relevant package or test. Before handing off Rust
changes, run these from `diri/`:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

From the repository root, `./scripts/check.sh` runs formatting, Clippy, tests,
shell/release guards, and the dependency-license policy. Add `--browser` for
sidecar integration tests; this also installs sidecar dependencies and
Playwright browsers. The release build above is a separate check.

Engine tests create real PTYs, processes, and Git repositories. On a loaded
machine, `DIRIJOR_TEST_TIMEOUT_SCALE` can extend their liveness waits. Tests
against a real SSH host must be opt-in and document setup and cleanup.

For documentation-only changes, check links and rendered output. Explain any
checks you could not run. Never include private prompts, credentials, or raw
session logs in fixtures or screenshots.

## Open a pull request

Keep one purpose per PR. Describe the problem, the resulting behavior, and
how you verified it. Link an issue when one exists. Include a screenshot or
short recording for interface changes.

If you change session lifecycle or persistence, explain what happens to running
sessions during restart, reconnect, and upgrade. If you change a protocol or
stored format, document compatibility. Update the relevant user guide when
behavior or setup changes.

CI must pass before merge. Reviews weigh correctness and session continuity
first, then performance and interface clarity. Keep new controls and
configuration justified by the problem they solve.

Contributions use [Apache 2.0](LICENSE); there is no CLA.
[Governance](GOVERNANCE.md) covers project decisions, the
[Code of Conduct](CODE_OF_CONDUCT.md) covers participation, and
[Security](SECURITY.md) explains private vulnerability reporting.
