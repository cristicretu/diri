# Authoring agent manifests

An agent manifest is the data contract for launching a coding agent and reading
its terminal state. A basic screen-driven integration needs no Rust code:
declare the executable, then add small rules for working, idle, and needs-input
screens.

Use [Maki](../diri/crates/diri-engine/manifests/maki.json) as a compact
screen-driven example. [Claude Code](../diri/crates/diri-engine/manifests/claude-code.json)
shows the advanced hooks-driven shape.

## Minimal screen-driven manifest

JSON does not allow comments, so this example uses `jsonc` only to explain the
fields. Remove the comments in a real manifest.

```jsonc
{
  "schemaVersion": 2,                 // Schema generation understood by Diri.
  "id": "example-agent",             // Stable kebab-case id; match the filename.
  "version": "2026.08.11.1",         // Bump whenever the manifest changes.
  "statusModel": "full",             // Screen rules provide detailed status.
  "agent": {
    "id": "example-agent",           // Include for the Rust-owned catalog copy.
    "displayName": "Example Agent",  // Human-facing name.
    "shortLabel": "example",         // Compact CLI/API label.
    "glyph": "E",                    // One-character fallback mark.
    "aliases": ["example"],           // Other accepted spawn names.
    "firstClass": true,
    "statusAuthority": "screen",
    "binary": "example",
    "returnToLoginShell": true,
    "approve": { "text": "y", "submit": true },
    "deny": { "text": "n", "submit": true }
  },
  "rules": [
    {
      "id": "permission",
      "state": "blockedPermission",
      "priority": 1000,
      "region": "bottom_non_empty_lines",
      "regionLines": 8,
      "when": {
        "all": [
          { "contains": "allow this command?" },
          { "contains": "y approve" }
        ]
      },
      "flags": ["visible_blocker"],
      "capture": {
        "region": "bottom_non_empty_lines",
        "regionLines": 8,
        "maxChars": 400
      }
    },
    {
      "id": "working",
      "state": "working",
      "priority": 900,
      "region": "bottom_non_empty_lines",
      "regionLines": 3,
      "when": { "contains": "esc to cancel" }
    },
    {
      "id": "idle",
      "state": "idle",
      "priority": 500,
      "region": "bottom_non_empty_lines",
      "regionLines": 1,
      "when": { "lineRegex": "^>\\s*$" }
    }
  ]
}
```

The numbers are intentionally spaced apart. A permission form must outrank a
working marker that remains visible behind it, and a working marker must outrank
an input box that remains visible while output streams.

## Top-level fields

| Field | Meaning |
| --- | --- |
| `schemaVersion` | Required integer schema generation. New manifests use `2`. |
| `id` | Required stable, kebab-case identity. It must equal the filename without `.json`. Never rename it to change display text. |
| `version` | Required manifest revision string. A date plus revision is the existing convention. |
| `statusModel` | `full` enables rule-driven status. `processOnly` uses only process liveness and normally has no rules. |
| `agent` | Launch, display, resume, and prompt-answer behavior. |
| `rules` | Detection rules, evaluated from highest to lowest priority. An empty array is valid only for `processOnly`. |

Keys beginning with `_`, such as `_notice`, are ignored by the decoders and are
useful for provenance, tested CLI versions, and non-obvious safety constraints.
Do not use an ignored key for behavior.

## The `agent` descriptor

### Basic identity and launch fields

| Field | Required? | Meaning |
| --- | --- | --- |
| `id` | Built-ins | Repeat the top-level id in the `agent` descriptor so the catalog identity is available to clients. |
| `displayName` | Yes | Clear product name shown in the UI and diagnostics. |
| `shortLabel` | No | Compact lower-case label; defaults to the id. |
| `glyph` | No | One-character mark for places without an icon; defaults to `▸`. |
| `aliases` | No | Additional case-insensitive names accepted by spawn surfaces. Do not reuse another agent's alias. |
| `firstClass` | No | `true` when the manifest provides real detailed status. Keep this aligned with `statusModel: full`. |
| `setup` | First-class agents | Display-only setup guidance: an official HTTP(S) `url`, concise `installHint`, and optional `signInHint`. Clients show these strings but never execute them. |
| `statusAuthority` | Yes for built-ins | `screen` for ordinary TUI agents, `hooks` when a supported hook integration is primary, or `process` for liveness only. |
| `binary` | Yes for a launchable agent | `argv[0]`, such as `maki`; omit only for pseudo-agents such as `shell` and `generic`. |
| `spawnArgs` | No | Fixed argv words inserted on every launch. Each item is one word; never concatenate a shell command. |
| `returnToLoginShell` | No | After a local agent exits, return to an interactive login shell instead of ending the PTY. Most terminal agents set this to `true`. |
| `approve`, `deny` | No | Canned prompt answers: `text` is typed literally and `submit` controls whether Return follows. `deny` defaults to Escape; omit `approve` when no universal safe answer exists. |

`env` is a map of values Diri deliberately forces into the child. Use it only
for a documented compatibility switch. `envScrubPrefixes` lists prefixes that
must not leak from an outer agent into a new child. Built-in Rust manifests also
scrub `DIRI_` and `DIRIJOR_`; add an agent-specific prefix only when that CLI
exports nesting or session identity through its environment.

`foregroundExecNames` contains executable basenames or identifying path
components used when Diri adopts an agent typed into a plain shell session. It
does not change the launch command. Script-based Node or Python CLIs may need an
identifying installed-path component because their real foreground executable
can be `node` or `python`.

For `setup`, link to the agent publisher's installation page, not a package
search or third-party tutorial. Keep commands in `installHint` short enough for
an unavailable-agent row, and use `signInHint` only for the documented next
step after installation. Setup metadata is guidance: Diri does not run either
hint or open its URL without an explicit user action.

### Resume behavior

`sessionIDFlag` tells Diri that it may mint an id at launch, for example
`"--session-id"`. `resume` declares how that id is passed later:

| `resume.style` | Result |
| --- | --- |
| `flag` | `binary <token> <id>` |
| `flagJoined` | `binary <token>=<id>` |
| `subcommand` | `binary <token> <id>` where the token is a subcommand such as `resume` |
| `latest` | `binary <token>`; the CLI chooses the latest conversation for the cwd |

Declare resume only when the CLI documents it and Diri can obtain the required
id. A flag that accepts an id is not useful if the CLI never reports that id.
The engine's current built-in representation uses `flag` without an id for bare
latest-session tokens; follow the closest manifest and verify the actual argv in
tests rather than guessing.

### Injection mechanisms

`injection` is advanced and code-backed. Its booleans do not mean “inject any
config”; each selects a mechanism Diri already implements:

- `claudeHooks`: launch Claude Code with Diri's hooks settings.
- `claudeMCP`: inject Diri's MCP server into Claude Code.
- `codexNotify`: install Codex's turn-complete callback.
- `codexMCP`: inject Diri's MCP server through Codex configuration overrides.

Do not set these for a different CLI because it happens to accept a similarly
named flag. A new injection mechanism requires implementation and security
review, not just manifest data. `statusAuthority: hooks` is appropriate only
when the corresponding supported hook path reliably reports lifecycle events.

## Detection rules

Rules are stably sorted by descending `priority`; the first match wins. Equal
priorities retain file order, but distinct priorities make intent easier to
review. A useful starting convention is blockers around `1000`, working around
`900`, and idle around `500`.

Every rule has:

- `id`: a stable diagnostic name unique within the manifest.
- `state`: `working`, `idle`, `blockedPermission`, `blockedQuestion`, or `skip`.
  Use `skip` for transient screens such as a transcript viewer where Diri should
  hold its prior belief.
- `priority`: integer ordering across all rules.
- `region`: the slice of terminal state inspected.
- `regionLines`: optional count, default `5`; meaningful for
  `bottom_non_empty_lines` and captures.
- `when`: one predicate object.
- `flags`: optional annotations used by existing manifests. The current
  evaluators derive behavior from `state`; `visible_blocker` and
  `skip_state_update` document intent but must not be relied on in place of the
  correct state.
- `capture`: optional needs-input excerpt configuration.

Choose the narrowest region that reliably contains the signal:

| Region | Contents |
| --- | --- |
| `bottom_non_empty_lines` | Last `regionLines` non-blank visible rows. Best for composers, spinners, and bottom forms. |
| `whole_recent` | Last 60 non-blank visible rows. Use when a form can move or wrap, but combine multiple specific markers to avoid scrollback matches. |
| `prompt_box_body` | Text inside the bottom-most box-drawing frame, or the tail beginning at the last prompt marker. |
| `osc_title` | Last OSC 0/2 window title set by the agent. |
| `osc_progress` | OSC 9;4 state; pair it with a `progress` predicate. |

Predicate objects are recursive and must contain exactly one operator:

- `{ "contains": "text" }` — case-insensitive substring over the joined region.
- `{ "regex": "pattern" }` — regex over the joined multi-line region.
- `{ "lineRegex": "pattern" }` — succeeds when any individual region line matches.
- `{ "progress": { "state": 0 } }` — compares the OSC progress state.
- `{ "any": [ ... ] }`, `{ "all": [ ... ] }`, and `{ "not": { ... } }` — compose predicates.

Prefer several literal UI markers over a broad regex. Anchor prompt and status
rows, and make blocker rules distinguish a pending form from the answered form
that may remain in scrollback.

### Regex support

Patterns use Rust's `regex` crate, which deliberately rejects lookaround and
backreferences. Do not use `(?=...)`, `(?!...)`, `(?<=...)`, `(?<!...)`, `\1`, or named
backreferences. Remember that a backslash is escaped once for JSON: regex
`\s` is written as `"\\s"`. Prefer literal Unicode glyphs over engine-specific
escape syntax.

### Captures and prompt options

For a blocker, Diri captures `prompt_box_body` by default. Set `capture` when
the agent uses an unboxed form or the important text lives elsewhere:

- `region`: any region above.
- `regionLines`: defaults to `5`.
- `maxChars`: defaults to `400`.

Captured text is redacted and attached to the needs-input detail. Numbered
options such as `1. Yes` are also extracted from the capture. Redaction is a
safety net, not permission to capture a whole transcript: keep the region small.

## Capture realistic screens safely

Run the actual released CLI in a disposable repository and record each state
separately: fresh idle, active thinking/streaming/tool work, a permission form,
and a genuine question form. Preserve spaces, wrapping, box drawing, spinner
glyphs, option labels, and footer hints because those are what the evaluator
sees.

When Diri already has a partial rule, open **Session Inspector → Info → Why Diri
thinks this**, then use **Copy status debug info**. See the
[status-debug workflow](../SUPPORT.md#status-debug-info). Before sharing or
committing a fixture:

1. Remove prompts, model output, API keys, tokens, usernames, private paths,
   repository names, issue text, and remote hostnames.
2. Keep only the smallest contiguous rows that reproduce the state.
3. Replace sensitive payloads with neutral text without changing the UI chrome
   your predicate matches.
4. Record the CLI version and whether the fixture is a live capture or a
   constructed regression.

Never publish a full terminal transcript just to demonstrate one status row.

## Bundled manifests and user overrides

Built-in manifests live in the canonical catalog at
[`diri/crates/diri-engine/manifests/`](../diri/crates/diri-engine/manifests/).
Add one JSON file whose filename, top-level `id`, and `agent.id` agree. Treat the
nearest working manifest as the compatibility reference rather than guessing.

At startup, bundled files are loaded in filename order. User files under
`~/Library/Application Support/Dirijor/manifests/overrides/` load afterward and
replace a bundled manifest with the same `id`; an override-only id adds a local
agent. A malformed file is skipped instead of disabling the rest of the
catalog. Restart the daemon after changing an override because the catalog is
immutable for the process lifetime.

Loose Rust development binaries can point `DIRI_MANIFESTS_DIR` at a catalog.
Packaged Rust binaries prefer the `manifests` directory beside the executable,
then apply the same user override directory.

## Validation

Add golden fixtures for every state the new rules claim to recognize. These
focused commands keep the edit loop short:

```sh
(cd diri && cargo test -p diri-engine detect::tests)
```

Before opening a pull request, run the complete engine package. It decodes every
bundled manifest and proves every regex is supported:

```sh
(cd diri && cargo test -p diri-engine)
```

If the catalog roster changed, also run `diri/scripts/verify-remote-refactor.sh`
so a packaging or catalog-count guard cannot silently drop the new agent.
