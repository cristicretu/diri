# Dirijor Node

`diri-node` makes a VPS (or another workstation) a first-party Dirijor execution host. It is a per-user service: provider credentials remain on the machine where Claude Code or Codex runs, while Dirijor gets one versioned management interface for accounts, usage, provider sessions, and handoff.

The [handoff design](HANDOFF.md) explains exactly which settings, MCP state,
authentication, conversation data, and working-tree data move between nodes,
including why Jujutsu is an optional code adapter rather than a credential
synchronizer.

SSH is still configured. It is the install/recovery path and the compatibility terminal path; it is no longer the source of truth for identity, usage, or movement.

## Install on the VPS

The VPS needs Claude Code and/or Codex on `PATH`, a private Tailscale address, and systemd user services.

```sh
cargo build --release -p diri-node
./scripts/install-node.sh
systemctl --user edit diri-node
```

Set the listener to the VPS's Tailscale address in the override:

```ini
[Service]
Environment=DIRIJOR_NODE_LISTEN=100.64.0.2:7337
```

Then start it and print the one-time enrollment values:

```sh
systemctl --user enable --now diri-node
diri-node init
```

The node config is owner-only and contains its app-layer capability token. Tailscale encrypts the transport; the token authenticates Dirijor at the application layer. Do not bind the listener to a public interface and do not copy `node.json` wholesale.

On the local Mac, put only the printed token in an owner-only file:

```sh
mkdir -p ~/.config/dirijor
chmod 700 ~/.config/dirijor
printf '%s\n' 'PASTE_THE_TOKEN' > ~/.config/dirijor/forge.token
chmod 600 ~/.config/dirijor/forge.token
```

In diri, open Settings → Remote and add both the SSH fallback and first-party node fields. The resulting `hosts.json` entry is:

```json
{
  "id": "forge",
  "name": "Forge",
  "ssh": "you@forge",
  "defaultCwd": "~/code",
  "node": {
    "endpoint": "tcp://100.64.0.2:7337",
    "tokenFile": "~/.config/dirijor/forge.token",
    "nodeId": "node-a1b2c3d4"
  }
}
```

`nodeId` pins the stable identity. It may be omitted for first enrollment, but should be saved after the first verified hello.

## Individual accounts on each machine

Profiles are labels and identity metadata. An installation is that profile's login on one node. Provider secrets never appear in the profile registry or over the Dirijor protocol.

```sh
diri-node account add --provider codex --id personal --label Personal
diri-node account add --provider codex --id work --label Work
diri-node account add --provider claude --id personal --label Personal

diri-node account login --id personal
diri-node account status --id personal
diri-node account default --provider codex --id work
diri-node account list
```

You can run those commands directly from the local Mac against the VPS—no SSH
shell is required. Add the enrolled connection flags to any `status` or
`account` command:

```sh
diri-node account login --id personal \
  --endpoint tcp://100.64.0.2:7337 \
  --token-file ~/.config/dirijor/forge.token \
  --node-id node-a1b2c3d4
```

Codex uses the official app-server device-code/browser flow. The URL and one-time code are printed where the command runs, so a VPS login completes in the local browser without copying `auth.json`. Claude's supported interactive auth command is streamed through the node; open the emitted URL locally. On Linux, each Claude installation uses its own `CLAUDE_CONFIG_DIR`. Claude Code's macOS Keychain credential is host-wide, so multiple simultaneous Claude subscription identities on one Mac are intentionally not claimed as isolated.

Codex installations use a separate `CODEX_HOME`. Sessions bind to an explicit profile; changing a node default affects new sessions, not a running session. This is identity selection for legitimate personal/work contexts—not automatic rate-limit failover.

Provider settings can follow the profile without its credentials:

```sh
diri-node account sync-config --id work \
  --target-endpoint tcp://100.64.0.2:7337 \
  --target-token-file ~/.config/dirijor/forge.token \
  --target-node-id node-a1b2c3d4
```

The handoff command runs the same sync automatically when both nodes advertise
`portable-config.v1`. Target conflicts are kept and reported; known inline MCP
credential fields are omitted, and provider login state never crosses the node
boundary. See [Local ↔ VPS handoff design](HANDOFF.md) for the allowlist and
security model.

## Instant move and fork

The client handoff coordinator performs:

1. target identity/account/capability preflight;
2. source checkpoint at a turn boundary;
3. content-addressed transfer of only missing blobs;
4. target restore into quarantine;
5. provider-native resume/fork (`thread/resume` or `thread/fork` for Codex; background-agent resume/fork for Claude);
6. the same location lease committed on target and source.

The live workspace is never overwritten during staging. `.git`, provider homes, SSH material, `.env*`, known credential files, dependency/build directories, symlinks, oversized files, and special files are excluded and recorded in the manifest. A failure before commit aborts both sides; a committed move is reversed with a new move rather than destructive rollback.

The existing `session.migrate` SSH implementation remains available for old hosts and recovery.

The same coordinator is available from the management CLI for a node-managed
session. This example moves a Codex thread from the local node to Forge; use
`--mode fork` to leave the source lease running:

```sh
diri-node handoff \
  --provider codex --profile work \
  --session dirijor-session-id --provider-session codex-thread-id \
  --workspace "$PWD" --mode move \
  --target-endpoint tcp://100.64.0.2:7337 \
  --target-token-file ~/.config/dirijor/forge.token \
  --target-node-id node-a1b2c3d4
```

Add `--endpoint`, `--token-file`, and optional `--node-id` when the source is
another enrolled node rather than the local node service. The source and target
may use different provider logins under the same profile label, which makes a
local `work` installation and a VPS `work` installation independently tunable.

## Fleet usage

Each node keeps `usage.sqlite3` in WAL mode. Events are idempotent and dimensioned by node, profile, session, provider, model, and source. The schema distinguishes:

- subscription/rate-limit quota;
- estimated API-equivalent cost;
- authoritative billed spend.

Codex account/rate-limit/usage snapshots come from app-server. Node-managed transcripts are scanned every 30 seconds as the durable fallback. Claude OpenTelemetry collectors can normalize request events into `usage.record`; transcript import remains available when telemetry is not enabled. The desktop queries every enrolled node and merges today/month provider totals with its local usage projection. An unreachable node never blocks local numbers and is retried on reconciliation.

## Operations and security

- Run one node per Unix user. Never run it as root.
- Bind only loopback or a Tailscale address. Public TCP is unsupported.
- Keep `node.json`, `accounts.json`, provider homes, and enrollment-token files mode `0600`/directories `0700`.
- Provider credentials are node-local. Checkpoints and usage rows contain no auth material.
- Rotate enrollment by stopping the service, replacing `authToken` in `node.json`, and updating enrolled token files.
- Back up `usage.sqlite3`, `accounts.json`, and provider homes according to provider policy. Checkpoint blobs are disposable caches.
- Audit by checkpoint/move JSON records and the SQLite ledger; raw provider auth responses are not logged.

Protocol changes are additive within `NODE_PROTOCOL_VERSION = 1`. Clients authenticate with `node.hello`, verify the pinned identity, and negotiate explicit capability strings before using a feature.
