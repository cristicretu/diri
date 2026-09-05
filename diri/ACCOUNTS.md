# Account profiles

Settings → Accounts manages named Claude Code and Codex launch profiles. Each
profile chooses a provider configuration directory on This Mac or a saved SSH
host. Diri stores the name, Agent, host, directory, and default selection, never
provider credentials or authentication responses.

1. Add a profile, such as Work, and select its Agent and host.
2. Choose an existing configuration directory or a new one such as
   `~/.codex-work` or `~/.claude-work`. Diri creates missing directories with
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

## Continue a Claude conversation with another account

When Claude reaches an account limit, choose **Continue with another account…**
from the session menu or click its account badge. Select a different, already
signed-in Claude profile on the same machine. The same Diri session restarts with
native `claude --resume <conversation-id>` under that profile. Your working folder,
worktree, title, and conversation ID stay the same. Claude restores the saved
conversation; enter your next message to continue. An interrupted tool process
is stopped, not transferred as a running task.

Diri checks the destination and saved transcript before stopping Claude, then
reads its final transcript after shutdown. It copies only the main conversation
JSONL (up to 64 MiB), never credentials or provider configuration. It does not
copy subagent or file-rewind sidecars; this is same-machine conversation resume,
not a portable checkpoint. The selected profile's own settings apply. Diri does
not verify remaining quota or automatically rotate accounts.

Switching back updates the older copy only when its contents are an exact prefix
of the current conversation. Conflicting history is rejected without overwriting
it. Missing or invalid history fails before stopping the current Agent. A failure
after shutdown leaves the saved conversation intact and reports the stopped
session; check its account badge and use Resume to retry. Once committed, the new
account binding is persisted before launch and is used by crash recovery too.

This action supports Claude Code sessions with a known conversation ID, locally
and on the same saved SSH host. Codex account profiles support launching, resume,
and fork with their recorded account; cross-account Codex continuation and
cross-host conversation transfer are not included.

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
returns the updated SessionRecord. Lifecycle operations reserve the session
through preparation, shutdown, transcript installation, and relaunch.

Remote paths resolve `~/` against the remote login environment. Directory setup
uses the Engine's existing bounded fixed-script SSH seam, passing the path as
stdin data. Agent launch stays structured argv/environment over the existing
Remote PTY Holder protocol. No credential transfer or optional node is involved.

The product pattern is inspired by [T3 Code](https://github.com/pingdotgg/t3code).
This implementation is native to Diri's Rust Engine and GPUI app; it does not
import T3's server, SDK, or authentication machinery.
