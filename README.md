# diri

**Every coding agent. All at once.**

Agents are good now. Watching thirty terminals is not. diri runs Claude Code,
Codex, Cursor, Gemini, OpenCode and more than a dozen others side by side, on
your Mac or any SSH host you own, and tells you which ones need you. Native,
Rust and GPUI, no Electron.

Free and open source. No diri account, no cloud in the middle. You pay only for
the agents you already use.

[Download](https://github.com/cristicretu/diri/releases/latest) ·
[Quickstart](https://diri.sh/docs/quickstart/) ·
[Guides](https://diri.sh/guides/) ·
[Docs](https://diri.sh/docs/) ·
[Contributing](CONTRIBUTING.md)

<img width="1862" height="1194" alt="diri running several coding agents side by side" src="docs/images/diri-hero.webp" />

## Install

```sh
brew install --cask diri
```

macOS 15 or newer, Apple silicon or Intel, signed and notarized. Or download the
[DMG](https://github.com/cristicretu/diri/releases/latest) and drag diri to
Applications. Updates install themselves.

**Linux (beta):** x86_64 Ubuntu 22.04 or 24.04, X11 or Wayland, Vulkan 1.3. See
the [Linux guide](diri/LINUX.md). Not every release ships Linux packages.

diri runs the agent CLIs and accounts already on your machine. Claude Code and
Codex have the deepest status and resume support; every other agent still gets
a full terminal.

## What you get

- **See who needs you.** Each session shows working, waiting or done, live.
  Notifications and ⇧⌘J take you straight to the next agent waiting on input.
- **Sessions that outlive the app.** Every terminal has its own holder process.
  Quit diri, restart the Engine, update the app: your agents keep running.
- **A worktree per task.** Start an agent in its own Git worktree and branch so
  parallel work never collides, then review diffs, commit and follow PR checks
  next to the session that wrote the code.
- **Agents that run agents.** The built-in MCP server lets an agent spawn more
  agents, locally or over SSH, hand them tasks, wait for them and collect the
  results. Ask with "Use diri to…".
- **Notes as the plan.** Write a spec in a note, split it into to-dos and send
  any to-do to an agent. Agents write their findings back to notes.
- **Runs while you're away.** Schedule an agent for every weekday at 7 or once
  tomorrow; diri wakes your Mac for it. Folder triggers hand new files to an
  agent as they appear.
- **Computer use for every agent.** Agents can drive your Mac's apps in the
  background. Your cursor stays yours.
- **A real terminal.** Split panes, a quick terminal on a hotkey, inline images,
  search across every session's screen, scrollback and chat (⇧⌘A), and the
  history of every past conversation (⇧⌘H).
- **Your own machines.** Remote sessions bootstrap a small helper over SSH. No
  tmux, no sudo, nothing to install on the server first.

## Things people actually do with it

You are the bottleneck, not the model. These are setups where you stop being one.

- **An agent on your own iPhone.** Open iPhone Mirroring, tell Claude to run
  your TikTok outreach: open the app, find creators in your niche, send the
  DMs, log who answered in a note. It taps and scrolls in the background. You
  keep using your Mac.
- **Ten bugs, ten agents, one coffee.** Paste the issue list, say "use diri to
  fix each of these in its own worktree". Come back to ten diffs and ten PRs.
  Merge the good ones, kill the rest.
- **A spec in, a feature out.** Write the plan in a note. Each to-do becomes an
  agent. They report back into the same note, so the plan stays the source of
  truth instead of your memory.
- **A night shift.** Schedule Codex for 3am to triage new issues and Claude for
  7am to summarise what broke. diri wakes the Mac, runs them, and the results
  are waiting.
- **The big box does the work.** Point diri at a GPU server over SSH. Agents
  run there, survive your laptop closing, and show up in the same sidebar.
- **A folder as an inbox.** Drop invoices, screenshots or CSVs in a folder and
  an agent picks up each new file as it lands.

None of this needs a new app per task. It's the same agents, run more of them.

## Add an agent

Agent support is data. Each JSON file in
[`diri/crates/diri-engine/manifests/`](diri/crates/diri-engine/manifests/)
says how to launch and resume an agent, which keys answer its prompts, and how
to read its status from the screen. Copy the closest one and adjust it. The
[manifest guide](docs/AGENT-MANIFESTS.md) covers the schema and how to test it.

## Build from source

Everything that ships is built from the Rust workspace in [`diri/`](diri/).

```sh
cd diri
cargo run -p diri-app        # the app (the first build compiles GPUI and takes a while)
cargo test --workspace
```

`./scripts/check.sh` runs the same checks as CI. Packaging, signing and releases
are in [`diri/PACKAGING.md`](diri/PACKAGING.md) and
[`diri/UPDATING.md`](diri/UPDATING.md).

## Contribute

Small, well-tested changes are welcome: a reproducible bug, a focused fix,
clearer docs or a new agent manifest. Read the
[contributor guide](CONTRIBUTING.md) first.

[Report a bug](https://github.com/cristicretu/diri/issues/new?template=bug_report.yml) ·
[Discuss an idea](https://github.com/cristicretu/diri/discussions) ·
[Roadmap](ROADMAP.md)

---

Copyright 2026 Cristian Crețu and the diri contributors · [AGPL-3.0](LICENSE) ·
[Notices](NOTICE) · [Privacy](PRIVACY.md) · [Security](SECURITY.md)
