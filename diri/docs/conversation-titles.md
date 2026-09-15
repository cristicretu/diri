# Conversation titles

## Follow-up: titles flipping after reconnect

The September 13 recording shows one row alternating between its original
request and the later follow-up `make pr`. The native Codex database still
contains the original request in both `title` and `first_user_message`, with
no explicit name. The title fix in #281 correctly classifies this as
`FirstPrompt`, but that exposed a conflict: `fold_session_view` could replace
an existing `FirstPrompt` with the first input captured by the current Engine
Session, which can be a later turn after adoption or resume. The native refresh
then restores the original request, and the next live fold overwrites it again.

Live prompt capture now only fills placeholder/unknown titles. Provider reads
can still repair an incorrect fallback; terminal names, native renames and
manual names retain their existing precedence. No new wire value, polling,
provider lookup or session state is needed.

Regression tests cover a saved first prompt surviving later input and a live
PTY Session interleaved with a fixture Codex database refresh. The latter checks
that refresh events, live records, list results, watcher events and persisted
state all keep the same repaired title over repeated passes. Both tests fail on
the previous implementation with `make pr` instead of the original title.

Verification on `fix/conversation-title-flapping`, based on main `975009e`:
`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, `cargo test --workspace`, and `cargo build --workspace --release`
all passed. The full test run used an environment permitting the fixture Unix
sockets; the initial sandboxed run could not bind those sockets.

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

## First-message capture before idle detection

The 0.7.3 follow-up exposed a separate gap: terminal attachment input passed
through an idle-only prompt collector, while Diri's send-text API captured its
title directly. A first message submitted before an idle screen observation,
or across a repaint to Working, could therefore leave the sidebar Untitled.
The affected local Codex sessions had no completion-supplied thread ID and
their OSC stream contained only the cwd and pending-name labels, so neither
other title source could recover the name during that first turn.

The collector now accepts composer input in Starting, Idle and Working.
Permission/question responses clear its draft and do not become titles. The
regressions use actual Session terminal writes, including bracketed paste,
individual keystrokes and a status change before Enter, then inspect the
Registry projection used by the sidebar. The startup regression failed before
the change and passed afterward. Previously missed input is not reconstructed;
existing sessions can still recover through their native completion metadata.

## Prompt stability across title publishers

The regression `codex_prompt_titles_stay_stable_across_native_refresh_and_live_views`
covers both fresh and resumed PTY sessions. Fresh input can seed an unnamed
record; native metadata can then recover the original conversation prompt once.
For a resumed session, the saved first prompt survives the attachment's input
from the start. Repeated metadata refreshes, individual/list reads, and forced
status-watcher publications must agree on the title and avoid redundant native
title events. This extends the reconnect coverage for the fix in PR #286.
