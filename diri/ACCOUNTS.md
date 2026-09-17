# Account profiles

The bottom-left account menu switches local Codex logins while keeping one shared
conversation home, `~/.codex`. It replaces only `auth.json` and resumes open Diri
tabs with the same native conversation IDs. No transcript migration runs.

## Set up and switch

1. Open the bottom-left account menu → **Add or manage accounts…**.
2. Add a local Codex profile with a meaningful name. Use **Save current login**
   to remember the account currently signed into `~/.codex`.
3. Add another profile and choose **Sign in**. Diri opens a login-only terminal
   with an isolated credential directory. Complete Codex's browser login; the
   login process exits when finished. Close that setup tab.
4. Choose the saved account in the bottom-left menu. Diri stops running Codex
   conversations open in its tabs/split panes, swaps the shared login once, and
   resumes their existing native IDs. Sleeping processes restart and return to
   sleep; stopped tabs stay stopped. The login becomes the default for new tabs.

Profile names are user labels, not verified email addresses. **Save current
login** replaces that profile's saved credential. It does not switch accounts.
Signing in does not change the shared login until you choose the account.

Closed and archived sessions are excluded from restart. Since authentication is
shared, their next resume also uses the current shared login. CLI processes
outside Diri are not restarted and may retain cached credentials; do not switch
while independently managed processes are writing the same authentication file.
Running tools are interrupted, not replayed. A native conversation ID is required
before any open tab is restarted. Separate-home conversations created by earlier
builds are left in their original home and counted as unchanged. Their history is
never copied. A legacy profile's login alone can be imported on first switch.

## What happens to MCPs

Local MCP definitions, OAuth stores, plugin files, project configuration and
conversation databases remain in the same home and are not rewritten or copied.
This preserves local configuration; it does **not** transfer hosted connector
installations or authorizations between provider accounts. For example, Slack
can report **not installed** after a Codex account switch even when all local
plugin files still exist. Install/connect that plugin for the selected account.
Diri cannot turn an account-side grant into a portable credential file.

## Storage and recovery

The Engine stores the catalog in `accounts.json` beside its socket (version 1,
0600). Local Codex logins are stored under `codex-logins/<profile-id>/auth.json`
beside that catalog, in owner-only directories and files. These are credentials:
protect this directory like Codex's own auth file. They are never returned in
control responses, logged or passed as process arguments. Removing a catalog
profile does not delete provider directories or its saved login slot.

The switch accepts Codex's file credential backend. Explicit Keychain, auto or
encrypted backends fail with an actionable error without changing configuration.
File reads are bounded, reject symlinks/nonregular files and require owner-only
credential permissions. Writes use fsync and atomic replacement with a concurrent
change check. Before replacement, the current credential is retained in
`codex-logins/previous-auth.json`. Refreshed credentials update saved slots only
when the token's account ID and user subject match. No refresh protocol is
reimplemented by Diri.

The batch reserves affected sessions and excludes concurrent launches and account
edits. Preparation fails before stopping processes. A later failure reports the
login/default error and attempts to resume stopped tabs; failed relaunches can be
retried with Resume. This is not an atomic multi-process transaction.

`account.codex.login` and `account.codex.capture` take `{id}` and return a login
SessionRecord or the catalog. `account.switch_all` takes `{accountProfileId}` and
returns updated sessions, unchanged IDs, failures and the default-save outcome.
No credential material is part of these responses.

## Other profiles

Claude and remote profiles retain their existing directory-based launch behavior.
The Engine binds `CLAUDE_CONFIG_DIR` or `CODEX_HOME`, clears ambient provider
authentication/routing overrides and reasserts the binding after shell startup.
They can use **Open Agent** for sign-in. The shared-login switch applies only to
local Codex. Existing explicit Claude single-conversation continuation remains
available; it does not transfer MCP grants. Remote profiles stay scoped to one
Agent and host, and use the existing structured Remote PTY Holder launch path.

## Verification

The `codex_accounts` tests exercise credential permissions, malformed files,
symlinks, unsupported backends, refreshed-login switch-back, running/sleeping/
stopped open tabs and excluded closed records. A fixture with an 800 MiB invalid
history file verifies switching never parses or copies history and leaves tool
configuration and credentials unchanged. Tests use synthetic auth and fake Agents.
A real hosted-connector connection must be checked separately for each account.

`toml_edit` (already present in the workspace lockfile) parses only the credential
backend setting. It never rewrites MCP configuration.
