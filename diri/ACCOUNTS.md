# Account profiles

Settings → Accounts manages named Claude Code and Codex launch profiles. Each
profile chooses a provider configuration directory on This Mac or a saved SSH
host. Diri stores the name, Agent, host, directory, and default selection, never
provider credentials or authentication responses.

1. Add a profile, such as Work, and select its Agent and host.
2. Keep the automatically generated, distinct directory for the new profile,
   or select an existing provider directory. Diri creates missing directories with
   owner-only permissions when launching. A final symlink is rejected; existing
   directory contents and permissions are preserved.
3. Save, then Open Agent to complete the provider's own sign-in flow. The profile
   name is a user label, not a verified email or authentication status.
4. Select the account in the new-session launcher (⌘⇧A). A default is scoped to
   one Agent and host. CLI environment bypasses the saved default.

Saved launch recipes retain the selection. An explicit profile that is missing
or belongs to another Agent/host fails with an actionable error. A recipe using
Default resolves the current default when launched. Agents without profile
support retain their normal CLI environment.

The Engine binds `CODEX_HOME` or `CLAUDE_CONFIG_DIR`, clears ambient provider
authentication/routing overrides, and reasserts the selection after local login
shell startup. Provider settings inside the selected directory still apply.
Profiles sharing a directory share its login and configuration. Use distinct
directories for independent accounts. This is launch configuration, not a
security sandbox or a credential vault.

Sessions record a launch-profile snapshot. Editing defaults, changing
a profile directory, or removing a profile affects future launches; resume, fork,
and crash recovery retain the session's recorded binding. Only an explicit
account continuation changes that binding. A session's profile
badge describes its launch configuration. Manually running another command in
the fallback shell is outside this binding. Removing a profile never deletes
provider files or signs out running Agents. Cross-host migration of a bound
session requires destination-account mapping and is currently rejected.

Local transcript discovery and native title lookup use the bound configuration
directory. Global history import and existing usage panels retain their current
scope; this feature does not aggregate usage across profile directories.

## Switch all conversations

Open the bottom-left account menu and choose a saved account under **Switch all
conversations**. Each row identifies its provider, machine, and default status.
Progress and any failures stay in the menu. **Add or manage accounts…** opens
account setup. The switch applies to all tracked conversations for that provider
and machine, including stopped conversations.

Choose **Switch account for all conversations…** from a Codex or Claude session
menu, or **Switch all conversations** on a saved profile in Settings → Accounts.
Sign in to each profile once with **Open Agent**. Switching uses that saved login;
it never logs out the other accounts or replaces their model-provider tokens.

The action covers every tracked Diri conversation for the selected Agent on the
same execution host, including stopped and archived records. It does not change
other Agents or other hosts, or conversations managed outside Diri. Running
Agents restart with their native conversation IDs; stopped/archived conversations
remain stopped and use the new account when resumed. Working folders, worktrees,
titles, Diri IDs, and conversation history are retained. Running tool processes
are interrupted, not migrated or automatically replayed. After a fully successful
switch, the selected account becomes the default for new conversations.

Every conversation must have a known native ID and a valid saved transcript.
Codex rollout discovery is supported locally; remote Codex requires an already
known rollout path. The Engine validates the entire batch before stopping any
Agent. Missing history, conflicting destination history, duplicate writers,
conflicting MCP definitions, or an unavailable destination leave the original
Agents running. Once shutdown begins, failures are reported per conversation;
saved history stays recoverable and the UI does not claim a complete switch.
The current profile badge and Resume action describe recovery after a partial
switch. New launches and concurrent account edits cannot race the switch.

Main transcripts are bounded to 64 MiB and copied atomically, with identity and
working-directory checks for Codex. A destination copy must be an exact prefix
of the source, permitting switching back without overwriting a divergent branch.
Source transcripts are retained. The new binding is durable before relaunch;
a relaunch failure can be retried with Resume. Codex reconstructs its runtime
state from the rollout; Diri does not copy a live SQLite database. This does not
transfer provider-specific subagent/file-rewind sidecars or running tools.

### MCP preservation

Bulk switching carries direct MCP setup independently of the model account:

- Codex: `mcp_servers` and MCP OAuth settings in `config.toml`, plus direct-server
  OAuth grants in `.credentials.json`. Plugin enablement and marketplace definitions
  also follow the conversations. On this Mac, missing plugin cache assets are
  shared through owner-checked links; existing conflicting versions fail closed.
  Plugin app-server and login state are never shared. Keep source profile
  directories in place (removing a Diri profile does not delete them).
  Native direct Keychain grants remain in
  Codex's shared MCP credential store. Account-bound executor and enterprise
  identity grants are excluded.
- Claude: global and per-project `mcpServers` definitions from `.claude.json`,
  and the `mcpOAuth` portion of file-backed credentials. On this Mac, the Engine
  also merges only `mcpOAuth` into the destination profile's existing Keychain
  item using Security.framework; its `claudeAiOauth` login stays unchanged.
- Project `.mcp.json` and project Codex configuration stay in their existing
  working folders. Model settings, provider authentication, approval policies,
  and workspace trust are not imported from another account.

Settings and OAuth grants are reread after all selected Agents stop, so the
transfer includes their final token refreshes. Definitions with the same name
must agree; ambiguous grants from multiple source accounts fail closed. Files
are bounded to 1 MiB, written atomically with mode 0600, and checked for concurrent
changes. Secrets never enter the control response, logs, or process arguments.
Remote file transfer remains on the same execution host through authenticated
fixed-script SSH; no credentials move between machines.

Hosted ChatGPT/claude.ai connections are authorized by the provider account;
Diri cannot transfer that server-side authorization. Connect the desired service
directly through MCP when independent authorization is required. Remote plugin-provided
servers still require the corresponding plugin on the execution host.
Codex's optional encrypted/home-specific credential backend, remote macOS
Keychain migration, and custom credential stores are not migrated. Such
connections may require authorization in the destination. This is direct-MCP
preservation, not an MCP gateway or a new OAuth refresh implementation.
Do not keep independently managed CLI processes writing to the same profile
while switching its Diri conversations.

The existing `session.continue_with_account` API remains available for an
explicit single-conversation resume; it retains the destination's tool settings.
The desktop uses the new bulk action.

## Storage and protocol

The local Engine owns `accounts.json` beside `agents.json` (version 1, mode 0600,
atomic writes, at most 64 profiles). Invalid versions, duplicate identities,
ambiguous defaults, symlinks, and unsafe file permissions fail closed without
overwriting the file. `account.profiles.list`, `account.profiles.save`, and
`account.profiles.remove` expose the catalog over the existing local protocol.

`session.spawn.accountProfileId` is additive: absent selects the current
Agent/host default, an empty string explicitly selects the CLI environment,
and a nonempty string requires that profile. `SessionRecord.accountProfile`
and recovery capsules retain the resolved directory and profile metadata;
older records without the field remain valid.

`session.continue_with_account` takes `sessionID` and `accountProfileId` and
returns the updated SessionRecord. `account.switch_all` takes `accountProfileId`
and returns switched records, per-session failures, and the default-save outcome.
Bulk operations reserve all affected sessions and exclude concurrent launches
and account edits. Lifecycle operations reserve the session
through preparation, shutdown, transcript installation, and relaunch.

Remote paths resolve `~/` against the remote login environment. Directory setup
uses the Engine's existing bounded fixed-script SSH seam, passing the path as
stdin data. Agent launch stays structured argv/environment over the existing
Remote PTY Holder protocol. No credential transfer or optional node is involved.

The product pattern is inspired by [T3 Code](https://github.com/pingdotgg/t3code).
This implementation is native to Diri's Rust Engine and GPUI app; it does not
import T3's server, SDK, or authentication machinery.

## Verification

`cargo test -p diri-engine account_handoff --lib` covers bulk Codex switching and
switch-back, stopped sessions, preflight conflicts, login isolation, MCP merges,
OAuth scope separation, hostile paths, symlinks, concurrent file edits, and remote
fixed-script behavior. No real account or SSH host is needed. Keychain merge
semantics are covered with credential fixtures; a real signed-in Keychain switch
requires a manual check. The installed Codex 0.154.0 app-server was also checked
with an isolated synthetic rollout and a `thread/resume` request, without starting
a model turn or using account credentials.

`toml_edit` preserves comments and unrelated destination configuration while
parsing and merging only MCP settings. This existing lockfile dependency is used
directly by the Engine instead of implementing a TOML parser or rewriting config
with string substitutions. Security.framework adds no third-party dependency.

The switch/spawn exclusion uses an uncontended lifecycle `RwLock` read gate,
not a terminal-path lock. A local optimized Rust microbenchmark (2026-09-17,
three runs of one million acquire/drop operations, subtracting the empty-loop
baseline) measured 9.5–17.4 ns per operation. The switch uses nonblocking lock
acquisition; input/output and read-only requests never take this gate.
