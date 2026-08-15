# Local ↔ VPS handoff design

Dirijor handoff moves a running coding session without pretending that every
piece of machine state belongs in one synchronizer. The design has four lanes:

| State | Mechanism | Conflict / security rule |
| --- | --- | --- |
| Conversation | Provider-native resume/fork plus the one session transcript | Never includes provider credentials |
| Working tree | Existing content-addressed checkpoint into a target quarantine | `.git`, `.jj`, `.env*`, credential files, dependencies, and build output stay out |
| Settings + MCP topology | `portable-config.v1` profile bundle | Fixed allowlist, 1 MiB maximum, target wins every conflict |
| Authentication + MCP OAuth | A matching profile is signed in independently on each node | Identity mismatch blocks handoff; tokens never cross the node protocol |

This keeps the interface small: callers request one handoff. The node hides
configuration filtering, target conflict handling, checkpoint transfer, resume,
and lease commit behind that interface.

## What the proof of concept does

Before a handoff, the client checks that the source and target profiles use the
requested provider, are ready, and resolve to the same account identity when
both providers expose one. It then synchronizes this bounded configuration:

- Codex: `config.toml` and `AGENTS.md`. The TOML carries Codex settings and MCP
  definitions. A config containing inline MCP `env` or `http_headers` values is
  omitted; environment-variable references such as `bearer_token_env_var` and
  `env_http_headers` remain portable.
- Claude: `settings.json`, `CLAUDE.md`, `keybindings.json`, and only the
  top-level `mcpServers` map extracted from `.claude.json`. OAuth, machine IDs,
  project caches, approvals, and other app state in `.claude.json` are never
  exported. An MCP definition with a known inline credential field is omitted.
- Claude project MCP configuration already travels as the workspace's checked-in
  `.mcp.json`. Environment references are the recommended way to keep its
  secrets machine-local.

On the target, a missing file is installed with owner-only permissions, an
identical file is reported unchanged, and a differing file is reported as a
conflict without being overwritten. Claude's mixed `.claude.json` receives a
server-by-server merge, preserving its local OAuth and app state. The apply
report is returned with the handoff and printed by the CLI.

Paths are deliberately node-relative. Portable-config bundles contain only
provider-home-relative allowlisted names; the source and target nodes resolve
those names against their own profile homes. Likewise, the source workspace's
absolute path is informational metadata only. The target stages files below its
own node restore root and passes that target-local path to provider resume/fork.
This allows paths such as `/Users/alice/code/project` and
`/home/alice/.local/share/dirijor/node/restores/...` to differ safely.

Portable-config installation is a separate, additive step and is not rolled
back if a later workspace or conversation handoff step fails.

Configuration sync is also independently useful:

```sh
diri-node account sync-config --id work \
  --target-endpoint tcp://100.64.0.2:7337 \
  --target-token-file ~/.config/dirijor/forge.token \
  --target-node-id node-a1b2c3d4
```

To sync back, make the VPS the source with `--endpoint`, `--token-file`, and
`--node-id`, then point the `--target-*` flags at the local node. The operation
is additive in both directions; it is intentionally not last-writer-wins.

## Why authentication is re-created, not synchronized

Codex can cache credentials in `auth.json` or an OS credential store, and the
official headless flow supports device login as well as an explicit
`auth.json` copy fallback. Claude's `.claude.json` combines OAuth with MCP,
project, UI, and cache state, while macOS may use Keychain. Those storage
models do not form a stable, provider-neutral transfer format.

Dirijor therefore synchronizes *identity intent* (the profile and observed
account identity), not bearer credentials. Run `diri-node account login --id
work` against each node. The browser/device challenge is printed locally even
when the node is on a VPS. This avoids turning an enrollment token into a remote
credential-export capability and avoids persisting account tokens in checkpoint
blobs.

References:

- [Codex authentication and headless login](https://learn.chatgpt.com/docs/auth)
- [Claude settings scopes and global state](https://code.claude.com/docs/en/settings)
- [Claude MCP scopes and environment references](https://code.claude.com/docs/en/mcp)

## Where Jujutsu fits

Jujutsu is a strong optional adapter for the **code lane**, not a transport for
settings or credentials. A colocated Jujutsu workspace interoperates with Git
remotes, but `.jj` contains local operation/workspace state and should not be
copied between machines. Jujutsu workspaces attached to one repository are also
local working copies, not a cross-host replication primitive.

The proof of concept keeps the existing Git-compatible checkpoint path and has
no runtime dependency on `jj`. A later adapter can detect a colocated workspace,
publish its working-copy commit/bookmark to the existing Git remote, and create
a fresh target workspace. That adapter should still leave portable config and
authentication in their current lanes.

References:

- [Jujutsu Git compatibility and colocation](https://docs.jj-vcs.dev/latest/git-compatibility/)
- [Jujutsu working copies and workspaces](https://docs.jj-vcs.dev/latest/working-copy/)

## POC limits and next steps

- Only small top-level provider settings are portable. Skills, plugins, hooks
  with executable payloads, memories, arbitrary dotfiles, and shell profiles
  need their own threat model before joining the allowlist.
- A config conflict is reported, not merged. A future UI can preview a
  provider-aware three-way merge using the last successfully applied digest.
- The workspace checkpoint remains the data path for uncommitted files. A
  future Git/Jujutsu adapter can seed the target quarantine from a remote commit
  first, reducing transfer size without changing the handoff interface.
- Existing nodes that do not advertise `portable-config.v1` still hand off the
  session and workspace; configuration sync is skipped and reported.
