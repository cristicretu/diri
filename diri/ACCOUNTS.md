# Account profiles

The bottom-left account menu switches the login for every open Claude or Codex
tab on this Mac while keeping one shared conversation home per Agent
(`~/.claude`, `~/.codex`). Open tabs relaunch on their existing conversations;
nothing is copied or migrated.

## Set up and switch

1. Open Settings › Accounts (bottom-left account menu → **Manage
   accounts…**). Accounts are grouped by Agent; each shows its name, email and
   plan, every plan window it has used and when each resets. A checkmark marks
   the account new tabs use.
2. The login already in use on this Mac appears there until it is saved.
   **Save** keeps it as an account named after its email.
3. **Add Claude Code account** or **Add Codex account** opens a sign-in tab.
   Complete the browser login; the account takes its email's name when the
   login lands (rename it any time).
4. Switch from the account menu (one line per account; the checkmark marks the
   one in use), with **Switch** in Settings, or from the command palette. Diri
   stops the Agent's conversations open in its tabs/split panes, installs the
   login, and resumes their existing conversations. Sleeping processes restart
   and return to sleep; stopped tabs stay stopped. The login becomes the
   default for new tabs.

When the account in use runs low, Settings marks the account with clearly more
room (at least 20 points) **Most room**; an account whose full window has reset
since it was last seen reads **Ready**, and one Diri could not ask yet reads
**Not checked**. The command palette offers **Switch Claude Code to …** /
**Switch Codex to …** for every account not in use, with the roomiest marked.
The account menu itself stays one line per account.

Signing in does not change the active login until you choose the account.

### Who an account is, and its limits

`account.overview` returns the catalog plus, for every local profile, the
identity its saved login names (email, organization, plan), whether a login is
saved and whether its sign-in tab is still open, and which login each Agent
uses now -- including one no profile holds (`profileId` absent). Identities
come from the logins themselves: Codex's ID token; for Claude, the account
record saved beside the store, else the one its sign-in tab left in the slot's
own `.claude.json`, completed by `claude auth status` (asked once per sign-in,
the answer kept as `identity.json` in the slot). Each is keyed by account
(`claude:<account>:<organization>`, `codex:<user>:<workspace>`), so a refreshed
login still matches its profile and another workspace never does.

The app reads plan limits for every saved local account, not only the one in
use. An account is asked only while its short-lived access token is valid:
Diri never refreshes a token (that would rotate it behind the CLI's back). An
idle account shows its last answer, kept in `account-limits.json` beside the
Engine's state (owner-only; percentages and reset times only), dated in
Settings. A login the provider rejects with a valid token (HTTP 401) reads
**Sign in again** and is never suggested.

A switch is never refused because of one tab. A Codex tab whose conversation
Diri has not learned yet (it never finished a turn, or predates id binding) is
identified from the rollout Codex wrote at launch, matched by directory and
launch time. A tab that still cannot be identified keeps running on the
previous login and is reported; it switches when restarted.

### How each Agent switches

- **Codex** keeps one `~/.codex`. Diri stores each profile's `auth.json` in a
  private slot and swaps only that file. Codex reads it once at launch, so
  open tabs relaunch with `codex resume <thread>`.
- **Claude Code** keeps one `~/.claude`. Claude derives its credential store
  from `CLAUDE_SECURESTORAGE_CONFIG_DIR` (a macOS Keychain item named after the
  path, `.credentials.json` elsewhere), so each profile owns a private store
  directory and a switch between profiles copies no tokens (the one exception,
  leaving Claude's default store, is below). Open tabs relaunch with
  `claude --resume <conversation>`; a tab that has not saved a transcript yet
  relaunches fresh with the same conversation id. The account identity that
  `/status` displays is kept per profile and swapped alongside the login; a
  record describing another account never lends the incoming one its fields.
  Sign-in tabs set `CLAUDE_CONFIG_DIR` to the slot as well (the same path as
  its store, so the credential lands in the same place): the account record a
  sign-in writes stays in the slot instead of renaming the account in use.
  Before the first switch, tabs run on Claude's default store; switching away
  from it first copies its latest login into the profile saved from it, so
  switching back is never stale.

Closed and archived sessions are excluded from restart. Since the login is
shared, their next resume also uses the current login. CLI processes outside
Diri are not restarted and may retain cached credentials; do not switch while
independently managed processes are writing the same authentication file.
Running tools are interrupted, not replayed. Profiles created by earlier
builds with their own config directory are left in that directory and counted
as unchanged; their history is never copied. A legacy Codex profile's login
alone can be imported on first switch.

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
beside that catalog; local Claude stores live under `claude-logins/<profile-id>`
(the Keychain item name is derived from that path on macOS, and the profile
records the path as `loginStore`). All are owner-only directories and files.
These are credentials: protect this directory like the Agent's own auth file. They are never returned in
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

`account.codex.login`, `account.codex.capture`, `account.claude.login` and
`account.claude.capture` take `{id}` and return a login SessionRecord or the
catalog. `account.switch_all` takes `{accountProfileId}` for either Agent and
returns updated sessions, unchanged IDs, deferred tabs (kept on the previous
login), failures and the default-save outcome. `account.overview` takes no
parameters. `account.adopt` takes `{agent}` and saves the login that Agent
uses now into the profile already holding that account, or a new one;
`account.add` takes `{agent}`, creates a profile and returns its sign-in
SessionRecord. No credential material is part of these responses.

## Other profiles

Remote profiles and legacy isolated-home profiles retain their directory-based
launch behavior. The Engine binds `CLAUDE_CONFIG_DIR` or `CODEX_HOME`, clears
ambient provider authentication/routing overrides and reasserts the binding
after shell startup. They can use **Open Agent** for sign-in. Existing explicit
Claude single-conversation continuation remains available for isolated-home
profiles; it does not transfer MCP grants. Remote profiles stay scoped to one
Agent and host, and use the existing structured Remote PTY Holder launch path.

## Verification

The `account_overview` tests cover identity keys, generated names, the unsaved
login in use being saved once and matched to its profile, a profile added by
signing in taking its email's name (and keeping a name chosen meanwhile), and
Claude being asked about a new sign-in only once. The app's
`usage::limits` tests cover reading every account from its own login, never
asking an idle account, remembering answers owner-only, and when another
account is pointed out as having the most room.

The `claude_accounts` tests exercise the shared Claude switch with a fake
Claude (resume in place, fresh relaunch keeping the id, no `CLAUDE_CONFIG_DIR`,
display identity swapped under Claude's config lock) and the Keychain name
derivation. The `codex_accounts` tests exercise credential permissions, malformed files,
symlinks, unsupported backends, refreshed-login switch-back, running/sleeping/
stopped open tabs and excluded closed records. A fixture with an 800 MiB invalid
history file verifies switching never parses or copies history and leaves tool
configuration and credentials unchanged. Tests use synthetic auth and fake Agents.
A real hosted-connector connection must be checked separately for each account.

`toml_edit` (already present in the workspace lockfile) parses only the credential
backend setting. It never rewrites MCP configuration.
