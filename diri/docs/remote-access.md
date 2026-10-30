# Remote access: driving Diri from other devices

Status: design, October 2026. Phase 1, step 1 (tailnet admission) is
implemented in `crates/diri-web`. Everything else here is planned.

## Goal

Reach and drive your Diri sessions from another Mac, a browser and a phone at
least as well as a dedicated multiplexer that has its own network server.
The owner already codes from a phone over Tailscale, so that path comes first
and has to be excellent.

## Decision

**The network is Tailscale, using the host's own `tailscaled`. Identity comes
from Tailscale WhoIs. The gateway is `diri-web`.** No bundled `tsnet`, no
passwords, no public ports, no new transport to Holders.

```text
  phone / browser / other Mac / CLI
        │  HTTP + SSE inside WireGuard (the tailnet)
        ▼
  diri-web gateway  ── WhoIs(peer) ──▶ host tailscaled LocalAPI
        │  admits: owner's untagged device AND paired here
        │  Unix socket
        ▼
  Engine (dirijord) ──ssh -T──▶ Remote Holders   (unchanged)
```

Every remote client speaks to an Engine through its `diri-web` gateway. The
gateway is a stateless frontend: it holds no sessions and killing it loses
nothing. Remote Holders and SSH stay exactly as `REMOTE_PORT.md` describes.
The gateway never attaches to a Holder and never takes the controller lease.
It reads the Engine-owned grid through the same receive-only preview the
desktop's tab previews use, and it types through `session.send_text`.

### Why the host's tailscaled and not tsnet

| | Host `tailscaled` LocalAPI (chosen) | Embedded `tsnet` node |
|---|---|---|
| Binary size | +0 (a ~300-line HTTP client) | +30 MB or more: `tailscale.com`, `wireguard-go`, gVisor netstack |
| Data path | Kernel or system extension WireGuard | Userspace netstack: more CPU per byte, MTU stalls |
| Setup | The user already runs Tailscale | A second device in the admin console, its own login, its own key expiry |
| Idle cost | Nothing until the gateway is on | A WireGuard peer and DERP connection held open |
| Works without Tailscale installed | No | Yes |

The one thing `tsnet` buys is reachability without the Tailscale app, and
the relay in phase 2 covers that for people who don't use Tailscale. If we
later need HTTPS with a `*.ts.net` certificate (for example for a Web Push
service worker), `tailscale serve` on the host does it with no new
dependency.

### Why WhoIs and pairing, not a bearer token

Today the gateway admits anyone with tailnet reach and a single 256-bit
token. The token is shared by every paired phone, so one phone can't be
revoked without re-pairing them all, and a leaked token works from any
device on the tailnet, including a teammate's on a shared tailnet.

With **tailnet admission** each TCP connection's peer address is resolved by
the host's `tailscaled` (`/localapi/v0/whois`). A request gets in only when
all of these hold:

1. The peer is a tailnet address. Loopback, LAN and public peers are refused
   without a lookup.
2. The node carries **no tags**. Tagged nodes act for the tailnet, not for a
   person.
3. The node belongs to the **owner**: by default the user this host's node is
   logged in as. A tagged host, such as a shared VPS, has no such user and
   admits nobody until it is given `--tailnet-owner LOGIN`.
4. The node is **paired** here: its stable node id is in the device list.
5. The request is **first-party**: no `Sec-Fetch-Site: cross-site`, and the
   `Host` is an IP literal, a MagicDNS short name, or a `*.ts.net` name.

Pairing is explicit. The Mac shows a QR code holding a **single-use 128-bit
code that expires after 10 minutes**. The device opens the link and presents
the code, and the gateway records that device's Tailscale node id. After that
the device carries no secret at all: identity comes from WireGuard. A leaked
code is worth little. Only the owner's own untagged device can spend it,
because rules 1 to 3 are checked before the code is even looked at.

Revoking a device deletes its record. Its next request gets
`device_not_paired`, and its open event and terminal streams close at once
(within 15 s when the revoke comes from another process).

## Phase 1: Tailscale-native access

### 1. Tailnet admission (implemented)

- `crates/diri-web/src/tailnet.rs`: LocalAPI discovery and client, the
  admission policy (`decide`), a WhoIs cache, and the revocation wait.
  - Discovery order: the `tailscaled` Unix socket (Linux, or the open-source
    daemon on macOS); the standalone macOS app's loopback port and password
    (`/Library/Tailscale/ipnport`, `sameuserproof-<port>`); then the
    `tailscale` CLI (`whois --json`, `status --json --peers=false`). The CLI
    path covers the App Store app, whose LocalAPI is sandboxed.
  - WhoIs answers are cached per peer IP for 30 s, with at most 256 entries.
    Pairing is checked on every request, so the cache never delays a
    revocation. An outage is never cached.
- `crates/diri-web/src/devices.rs`: the paired-device list
  (`{version, devices:[{id, name, os, login, pairedAt}]}`, owner-only file in
  an owner-only directory, atomic rename) and the pairing code (its own
  owner-only file until it is spent, replaced or expired). A running gateway
  notices another process's edit by the file's modification time and size.
- **Off by default.** Settings > Phone access > *Identify devices with
  Tailscale*. It lists the paired devices, each with **Remove**. Turning it on
  or off stops the running gateway so the next Enable uses the new rule. With
  it on, *New code* replaces the one-time code without dropping paired
  devices.
- Standalone (`forge`): `diri-web --admit tailnet [--tailnet-owner LOGIN]`,
  plus `diri-web pair`, `diri-web devices` and `diri-web revoke NODE_ID`.
- **Fails closed.** If the LocalAPI does not answer, every request gets
  `503 tailnet_identity_unavailable`. Under tailnet admission the bearer
  token, cookie and `?token=` are never accepted as credentials; a presented
  value is only ever tried as a pairing code.
- Structured refusals: `tailnet_identity_unavailable`, `not_a_tailnet_peer`,
  `tagged_node_refused`, `tailnet_owner_unknown`, `not_the_owner`,
  `device_not_paired`, `foreign_origin`.
- Compatibility: the iOS app and the browser already send whatever `token`
  the QR held. Under tailnet admission that value is the pairing code, so
  neither client needs a change to pair.

### 2. Browser terminal (implemented)

`diri-web` already serves everything a terminal needs. The phone page just
doesn't use it yet:

- `GET /api/session/:id/stream` (SSE): a `snapshot`, then `diff` frames of
  styled grid rows (`GridRowCodec`), 16 ms coalescing, a bounded queue of 4,
  and a snapshot reseed when the client falls behind. It is the same grid the
  Mac renders.
- `POST /api/session/:id/input` (idempotent batches, `input_ledger.rs`),
  `/key`, `/resize`, `GET /scrollback` and `/history` pages.
- `POST /api/session/:id/answer` `{answer: approve|deny, id, occurredAt}`:
  answers a permission prompt with the agent's manifest keys (what the Mac's
  attention inbox sends). The gateway re-checks that the session is still on
  the prompt raised at `occurredAt` and refuses with `409 moved_on`
  otherwise; questions and secret prompts get `422`. `id` makes it
  exactly-once, like `/input`. Phone list cards and notification actions use
  it, so a stale tap can never type into whatever replaced the prompt.

The work is a small JS renderer in `ui/` that paints rows into a monospace
DOM grid (one `<div>` per row, spans per style run; no canvas, so text
selection and the iOS text system still work). It forwards keys and IME
composition to `/input` and pages scrollback on demand. The same page serves
the phone browser and desktop browsers. No framework and no build step: it
ships inside the binary like the current page.

### 3. Resume after a blip

The goal is **state-exact**, not byte-exact. Diri sends grid state, not raw
PTY bytes, so the right resume converges on the current grid with the fewest
bytes. Nothing is ever replayed twice and nothing is ever lost.

- **Output:** the client reconnects the stream with
  `?resume=COLSxROWS.<hex digest per row>`, where each digest is FNV-1a over
  the row's JSON as the stream sent it. The gateway's fresh preview mirror
  compares digests, the same idea as #831's `grid-resume-v1` between Engine
  and Holder, and answers with a `resume` frame holding only the rows that
  differ (plus cursor, modes and palette), or a `snapshot` if the size
  changed. An unchanged screen resumes with zero rows. No state is kept
  between connections, so a mismatch only costs the rows it covers.
- **Input:** already exactly-once. Every batch has a client id. The ledger
  drops resends, so a POST that timed out on a cell handoff can be resent
  blindly.
- **Events:** `/api/events` re-seeds and emits `resync` on lag. The client
  re-reads the session list rather than applying deltas.
- **Client policy:** reconnect immediately on `online` or `visibilitychange`,
  then back off with jitter (0.25 s, then up to 5 s). Mark the view stale
  after 2 s without a frame, but keep showing it.

### 4. Phone

Both clients use the same gateway API and the same admission:

- **Browser / home-screen web app:** the browser terminal above. It is the
  zero-install path, and it is what a new device gets the moment it scans
  the QR code.
- **iOS app (`ios/`):** native SwiftUI over the same API. It already keeps
  several computers in the Keychain, probes them and merges sessions. Under
  tailnet admission it stores no credential. Keychain entries become
  `{url, label}` and the stored token is ignored. Native rendering, haptics,
  voice and (in phase 2) push stay app-only.

### 5. Mac to Mac

One Diri drives another Diri's sessions as a client of that Mac's gateway.

- Settings > Computers lists other Diri gateways. Each is paired like a
  phone, so the target Mac shows a QR code or link and this Mac opens it.
- The sidebar gains a section per computer. Its sessions render in `diri-term`
  from the same `GridUpdate` rows the stream already carries, so there is no
  second terminal parser. Input goes through `/input`.
- This Mac's Engine is **not** involved. The remote Mac's Engine stays the
  only owner of those sessions. Nothing new talks to a Holder, and the
  single-controller lease is untouched. Closing the section or quitting
  drops the streams, and the sessions keep running.
- Discovery without a hosts server: the local `tailscaled` already knows the
  owner's online devices (`status`). A computer list can offer "Diri found on
  `studio`" by probing those peers' `:7380/api/health`. Pairing is still
  explicit.

### 6. The CLI

The CLI already uses `--host` to mean "the configured SSH host this Engine
should launch on" (`dirijor session run --host development …`). Reusing the
same flag for "which Diri to talk to" would make `--host forge` ambiguous.
So:

- `--host ID` keeps its meaning: the host in the **target Engine's**
  catalog.
- `--computer NAME` (env `DIRI_COMPUTER`) picks **which Engine**: a paired
  gateway by label, MagicDNS name or URL. Without it the CLI uses the local
  Engine socket, as today. With it, the CLI calls that gateway's HTTP API and
  is admitted like any other device: the machine running the CLI must be the
  owner's paired device. `dirijor computers pair <link>` pairs it.

### 7. A hosts list on a "home" computer

The reference keeps hosts on a home server and queues offline writes. Diri
needs less. The tailnet is already the directory of the owner's devices, and
each device keeps its own computer list, as iOS does today. A synced list
gets designed together with the phase 2 relay, as an end-to-end-encrypted
blob, so a server can't read it. Until then there is no home server to be
offline.

## Security

### Threat model

| Adversary | Can they drive sessions? | Why not / mitigation |
|---|---|---|
| Internet | No | No public bind. `diri-web` refuses addresses that aren't loopback or tailnet. Under tailnet admission non-tailnet peers are refused before any lookup. |
| LAN / café Wi-Fi | No | Same. WireGuard encrypts and authenticates every packet. |
| Another user on a shared tailnet | No | WhoIs owner check (`not_the_owner`). A leaked pairing code doesn't help them. |
| A tagged server on the tailnet (CI, a compromised VPS) | No | `tagged_node_refused`. A tagged *host* admits nobody without an explicit owner login. |
| A web page open on the owner's paired phone | No | Network identity is ambient, like a cookie. Cross-site requests (`Sec-Fetch-Site`), DNS-rebound `Host` names, and cross-origin mutations (existing `Origin` check) are refused. No CORS headers are sent, so cross-origin reads are opaque. |
| A lost or stolen paired phone | Until revoked | Settings > Remove (immediate, closes live streams), or remove the device in the Tailscale admin console (WhoIs then fails). |
| Malware on a paired device | Yes | Same trust as that device's Tailscale identity. Out of scope; revoke it. |
| Local processes on the gateway host | Only if the host itself is paired | The host's own tailnet IP resolves to itself. It isn't paired unless the owner paired it. |
| A spoofed LocalAPI | No | Fixed root-owned socket paths, a fixed `/Library/Tailscale` port file, absolute CLI paths. Nothing is taken from `PATH` or the environment. |
| Compromised `tailscaled` or Tailscale control plane | Yes | The trust root. Same as SSH trusting `known_hosts` and its keys. |

### Invariants

- Off by default. No listener, LocalAPI call, file or task exists until
  phone access is enabled, and tailnet lookups happen only with *Identify
  devices* on.
- Fail closed. An unavailable LocalAPI never falls back to the token.
- No secrets in logs. Pairing codes, terminal bytes and input are never
  logged. Devices are logged, if at all, by node id.
- Files are owner-only: directories `0700`, the device list and pairing code
  `0600`.
- Remote callers can't manage access. There is no HTTP route that lists,
  pairs or revokes devices or changes admission. Those are local-only
  (Settings, or the `diri-web` subcommands on the host).
- No host-wide configuration. Diri never installs, configures or approves
  anything in Tailscale.

## Footprint

| State | Cost |
|---|---|
| Phone access off (default) | Zero: no listener, no task, no file reads. |
| On, nobody connected | One accept task. No polling, no timers. |
| On, phone connected | One task per connection. One LocalAPI round trip per new peer per 30 s (Unix socket ≈ 1 ms; CLI fallback ≈ one short process). One `stat` of the device list per request. SSE heartbeat every 20 s. |
| Terminal stream | One Engine preview per open view, coalesced to 16 ms, bounded to 4 frames. A slow phone costs one grid of memory. |

## Phase 2: the relay, for people without Tailscale

Deferred by the owner. The shape is recorded here so phase 1 doesn't paint
it into a corner.

- A dumb **Cloudflare Workers + Durable Objects** rendezvous: one DO per host
  route. The gateway and its clients both connect *outbound* over WSS, so no
  inbound port is opened anywhere and idle routes hibernate for free.
- **Noise end to end.** `XXpsk3` for QR pairing, then `KK` per channel.
  Cloudflare sees route ids, sizes and timing, never terminal bytes, titles
  or paths. Per-device static keys replace WhoIs as the identity. The device
  list and per-device revoke in Settings stay the same, so the user sees one
  concept.
- The relay carries the unchanged `diri-web` API inside each channel and
  calls the same `Api::route`. Holders and SSH are untouched.
- Costs: new dependencies (`snow`, a WSS + TLS client) and a hosted service
  to operate. It is about $0 up to tens of users on the Workers free plan. It
  adds roughly 20–50 ms over a direct tailnet path in the same region.
- It unlocks push: a contentless, end-to-end-encrypted "needs you" notice to
  APNs or Web Push while the phone sleeps. Phase 1 can't do that without a
  server.

## Order of work

1. Tailnet admission, device pairing and revocation, Settings toggle. **Done.**
2. Browser terminal on the existing stream, input and scrollback routes. **Done.**
3. Row-digest resume on `/stream`. **Done.** The iOS app drops its stored
   token under tailnet admission.
4. Mac-to-Mac computers in the sidebar, rendered by `diri-term`.
5. `--computer` for the CLI.
6. Phase 2 relay, push, and an encrypted computer list.
