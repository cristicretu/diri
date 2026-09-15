# Documentation

## Use Diri

| Guide | Covers |
| :--- | :--- |
| [Getting started](GETTING_STARTED.md) | Install, launch your first agent, use worktrees, and connect an SSH host. |
| [Workflow guides](https://diri.sh/guides/) | Parallel agents in worktrees and persistent sessions over SSH. |
| [Keyboard shortcuts](KEYBOARD_SHORTCUTS.md) | Navigation, the command palette, and terminal input. |
| [Linux beta](../diri/LINUX.md) | Packages, source builds, graphics setup, and platform limits. |
| [iPhone companion](../ios/README.md) | Beta setup and builds for access through your Tailscale network. |
| [Support](../SUPPORT.md) | Troubleshooting, diagnostics, and bug reports. |
| [Security model](SECURITY-MODEL.md) | Process permissions and trust boundaries. |

## Build Diri

Start with [Contributing](../CONTRIBUTING.md) for local setup and review
expectations. The [Rust workspace guide](../diri/README.md) covers development
builds and preview fixtures.

| Reference | Covers |
| :--- | :--- |
| [Agent manifests](AGENT-MANIFESTS.md) | Launch commands, status rules, safe fixtures, and validation. |
| [Remote architecture](../diri/REMOTE_PORT.md) | The current SSH transport, PTY Holders, and persistence guarantees. |
| [Remote nodes](../diri/NODE.md) | Optional enhanced node mode, fleet usage, and handoff. |
| [Packaging](../diri/PACKAGING.md) | App bundles, signing, and notarization. |
| [Updates and releases](../diri/UPDATING.md) | Updater behavior and publishing. |
| [Performance](../diri/PERF.md) | Budgets and measurement. |

## Project

[Roadmap](../ROADMAP.md) · [Governance](../GOVERNANCE.md) ·
[Code of Conduct](../CODE_OF_CONDUCT.md) · [Privacy](../PRIVACY.md) ·
[Security reporting](../SECURITY.md) · [Third-party licenses](third-party/README.md)

The [Rust migration record](../diri/PORT.md) is historical context.
[AGENTS.md](../AGENTS.md) and the remote architecture describe the current
implementation baseline.
