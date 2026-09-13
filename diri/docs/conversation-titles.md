# Conversation titles

## September 2026 diagnosis

The screenshot failures were reproducible at `Registry::fold_session_view`:
`Action Required | dirijor` replaced a useful first-prompt title. Once the record
became `AgentProvided`, later terminal names were ignored. The affected saved
records had no Codex thread ID, so native store refresh could not rescue them.
Separately, `Session::view` preferred a captured first prompt over every later OSC
title, and native store reads could promote a copy of the first prompt as if it
were a generated name.

## Source comparison

- [Codex terminal-title renderer](https://github.com/openai/codex/blob/main/codex-rs/tui/src/chatwidget/status_surfaces.rs)
  composes activity, thread name and project name, including transient activity
  and title-generation presentation. Its terminal title is not itself a durable
  conversation name.
- [Codex thread metadata](https://github.com/openai/codex/blob/main/codex-rs/state/src/model/thread_metadata.rs)
  distinguishes an explicit `name` from the best-effort `title` and first user
  message. Diri preserves that distinction when deciding title precedence.
- [herdr terminal-title synchronization](https://github.com/Mihailorama/herdr-terminal/blob/main/src/app/terminal_titles.rs)
  keeps raw and stripped terminal titles separate and observes later updates.
  Its [title-sync plugin](https://github.com/winoooops/herdr-agent-title-sync)
  reads Codex's `session_index.jsonl` `thread_name` using conversation identity.
- [herdr auto-title](https://github.com/sh1ma/herdr-auto-title) generates names
  through an additional `codex exec` or `claude -p` invocation. Diri instead uses
  the names the running Agent already supplies.
- [cmux workspace title state](https://github.com/manaflow-ai/cmux/blob/main/Sources/Workspace.swift)
  distinguishes process titles, custom titles and custom-title provenance,
  protecting user-owned names from automatic updates.

These links describe the public source inspected during this fix; the Codex
comparison covers its public Rust TUI and metadata implementation.

## Diri behavior

Title priority is manual/Diri-assigned name, native conversation name,
provisional terminal name, first prompt, then placeholder. Native names follow
provider renames. Terminal names follow later useful OSC updates, including
before the first Codex completion notification supplies a thread ID.

Codex activity and pending-name labels are rejected. Braille activity spinners
and a matching cwd suffix are removed. A temporary label never erases the last
useful name. Raw terminal titles remain available to status detection unchanged.
Old transient Agent-provided titles are repaired on load; manual names survive.
`TitleSource::TerminalTitle` uses additive wire value 5.

Native metadata stays profile-scoped and identity-bound. A database `title`
equal to `first_user_message` is a prompt fallback, not a native name. Remote
sessions use existing terminal output and prompt capture; they do not read a
local Codex store or gain remote thread discovery.

## Regression coverage

Registry tests cover the screenshot labels, name generation and subsequent
rename, native and manual precedence, persisted-state repair, and a real PTY
whose names change after the first prompt was captured. History tests distinguish
native names from prompt previews and retain profile isolation and batched reads.

Run `cargo test -p diri-engine --lib` from `diri/`. Socket and PTY tests require
an execution environment that permits ordinary local Unix sockets and PTYs.

### Verification

On the PR branch based on `dea0a4f`:

- `cargo fmt --all -- --check` passed.
- `cargo test -p diri-engine -p diri-proto --lib -- --test-threads=4`
  passed: 397 Engine tests and 52 protocol tests; 4 opt-in Engine tests ignored.
- `cargo clippy -p diri-engine -p diri-proto --all-targets -- -D warnings` passed.
- Regression coverage includes the screenshot labels, subsequent name updates,
  native/manual precedence, persisted recovery, and actual PTY output after
  first-prompt capture.

The implementation checkout also passed workspace Clippy and a workspace
release build. Its broader suite encountered intermittent daemon marker,
remote cleanup/reconnect/environment, and MCP cleanup fixture failures, so a
clean full-workspace test run is not claimed for this change.

The rebuilt application must be run for the changes to take effect. Live user
session records are not edited directly.
