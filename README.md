# diri

**A native workspace for coding agents.**

Run Claude Code, Codex, Cursor, Gemini, and other terminal agents side by side.
See what needs you, give each task its own worktree, and review the changes in
place. Built with Rust and GPUI for macOS, with Linux in beta.

[Download](https://github.com/cristicretu/diri/releases/latest) ·
[Getting started](docs/GETTING_STARTED.md) ·
[Guides](https://diri.sh/guides/) ·
[Documentation](docs/README.md) ·
[Contributing](CONTRIBUTING.md)

![Diri with agent sessions in the sidebar, a terminal in the center, and a code diff alongside it](docs/images/diri.png)

## Install

```sh
brew install --cask cristicretu/diri/diri
```

macOS 15 or newer. Apple silicon and Intel. Signed and notarized.
You can also download the [DMG](https://github.com/cristicretu/diri/releases/latest)
and drag Diri to Applications.

**Linux beta:** x86_64 Ubuntu 22.04 / 24.04, X11 or Wayland, Vulkan 1.3.
See the [Linux guide](diri/LINUX.md) for packages, source builds, and limitations.
Linux packages are not included in every release.

Install your agent CLIs separately. Diri uses the tools and accounts already
on your machine; Claude Code and Codex have the deepest status and resume
integration.

## Work in parallel

- **Know what needs you.** Live status and notifications distinguish working,
  waiting, and finished sessions.
- **Keep tasks separate.** Give agents their own Git worktrees and branches.
- **Review in context.** Inspect diffs, stage changes, commit, and follow pull
  request checks beside the session.
- **Come back to your work.** Local sessions keep running when you close the
  app or the Engine restarts.
- **Use your own machines.** Run locally or on an SSH host you control.
  Diri handles remote Helper setup.

Your agents run under your user account, in real terminals. No Diri account or
hosted relay is required. Remote session persistence depends on the host;
Diri reports its persistence capabilities.

## Contribute

Small, well-tested changes are welcome. Start with a reproducible bug, a focused
fix, clearer documentation, or an [agent manifest](docs/AGENT-MANIFESTS.md).
Read the [contributor guide](CONTRIBUTING.md) for setup and review expectations.

[Report a bug](https://github.com/cristicretu/diri/issues/new?template=bug_report.yml) ·
[Discuss an idea](https://github.com/cristicretu/diri/discussions) ·
[Roadmap](ROADMAP.md)

---

[Apache 2.0](LICENSE) · [Third-party notices](NOTICE) ·
[Privacy](PRIVACY.md) · [Security](SECURITY.md)
