# diri's auto-updater

diri updates itself. It does **not** use Sparkle — that framework is Swift-side
and the Swift app's appcast carries EdDSA signatures tied to a keypair a Rust
binary has no way to use. diri ships its own updater in `crates/diri-updater`,
reading a JSON feed published with each GitHub Release.

| | Dirijor (Swift) | diri (Rust) |
|---|---|---|
| Client | Sparkle | `diri-updater` |
| Feed | `/appcast.xml` | `appcast.json`, a release asset |
| Update artifact | DMG | zip of the stapled `.app` |
| Trust anchor | project EdDSA keypair | Developer ID + notarization |
| Host | password-gated worker (retired) | GitHub Releases, public |

## Trust model

There is **no updater signing key to manage**. A downloaded bundle is accepted
only if all of the following hold:

1. `codesign --verify --deep --strict` passes.
2. Its **Team ID** and **bundle identifier** match the *running* app's. Pinning
   to ourselves rather than a hardcoded constant means an ad-hoc-signed dev
   build (no Team ID) simply refuses to update, and no rotation can silently
   start accepting a stranger's code.
3. `spctl --assess --type execute` accepts it — for a stapled bundle that
   proves notarization without a network round trip.
4. Its `CFBundleShortVersionString` equals the version the feed promised.

Plus, at the feed layer: only strictly-newer versions are ever offered
(no downgrades), and downloads are pinned to the releases host over HTTPS.

The retired Swift app used Sparkle, whose cost was a private EdDSA key — lose it
and every install is stranded until people redownload by hand. This design has
no such key. The Developer ID certificate is already load-bearing for shipping
at all, so it is the one secret that cannot be lost without noticing.

There are no credentials. Releases are public GitHub assets, so nothing has
to ship a password inside the app to fetch them.

The feed lives at `releases/latest/download/appcast.json`. GitHub's `latest`
alias always resolves to the newest release, and every release carries the
feed as an asset, so one stable URL works with no server to run.

## What the user sees

Checks are automatic (20 s after launch, then every 6 h, toggleable in
**Settings → General → Updates**). Downloading and restarting are not:

1. A background check finds a release → the sidebar footer lights an
   **Update to 0.2.0** pill. Nothing else happens.
2. Click it → **Downloading… 43%** → the bundle is verified and staged.
3. Click **Restart to update to 0.2.0** → diri hands off to a helper and quits.

diri holds live agent sessions, so it never relaunches itself uninvited.
**⌘K → Check for Updates…** and the version row in the account popover both
run a manual check, which reports "up to date" rather than staying silent.

## How the swap works

A process cannot reliably delete the bundle it is executing from, so
`install.rs` generates a small `/bin/sh` helper, spawns it detached in its own
process group, and quits. The helper waits (up to 60 s) for diri's pid to
disappear, renames the old bundle to `diri.app.diri-previous`, unpacks the
staged one with `ditto`, and relaunches. If the unpack fails it restores the
old bundle and relaunches that instead — an interrupted install leaves a
working app, never a hole.

Staging lives in `~/Library/Caches/diri/updates/<version>/`, with the helper's
log at `install.log` there. Directories for versions at or below the running
one are swept at launch.

If the app sits somewhere the user cannot write, the writability check fails
*before* the download starts rather than after 50 MB.

## Cutting a release

One-time setup is the Developer ID cert and notary profile described in
[PACKAGING.md](PACKAGING.md). No Sparkle keys.

First open and merge a normal pull request that updates the `diri-app` version
and lockfile. Then check out the clean, current `main` branch and run:

```sh
diri/scripts/release.sh 0.4.1
```

The script refuses to release a version that does not match the manifest, a
dirty checkout, or a commit other than the current `origin/main`. It runs
clippy + tests, builds the universal Rust executables, signs them,
**notarizes and staples the .app first**, then builds and notarizes the DMG
from that stapled bundle, produces the update zip, rebuilds `appcast.json` from
the currently published feed, generates `SHA256SUMS` and a reviewed dependency
license inventory, and creates the GitHub Release at that exact source commit.
It then updates, commits, **pushes, and reads back** the Homebrew cask;
the release does not report success until the remote cask checksum matches the
published DMG.

Published asset bytes are immutable. If a rebuilt artifact differs from an
asset already attached to that version, cut a new patch version instead; the
script refuses to replace it under the old tag. If only the cask publish failed,
recover without rebuilding the release:

```sh
diri/scripts/publish-homebrew-cask.sh \
  0.4.1 diri/dist/diri-0.4.1-universal.dmg ../homebrew-diri
```

That recovery command accepts the DMG only if its checksum matches GitHub,
pushes the tap branch, and reads the remote cask back before succeeding.

Release notes come from `dist/notes-<version>.md`. The script writes a default
one if it is missing, so writing that file first — and re-running — is how you
customize them.

### The bundled Engine updates safely with the app

`diri.app` carries `dirijord-rs` + `diri-holder` in `Contents/Resources/bin`,
and the update zip carries them too. At launch, the app verifies the running
Engine's identity and executable hash. When the bundled Engine changed, it asks
the old process to persist and shut down, then launches the new binary. Holder
processes retain their PTYs across that handoff, so updating the Engine does not
terminate live agent sessions.

The canonical release tag is `v<version>`. `gh release create` creates that tag
at the verified `origin/main` commit; do not create a second `diri-v<version>`
tag. Source is merged before release rather than pushed after binary publication.

### Why the .app is notarized before the DMG

Stapling the DMG alone leaves the extracted bundle without its own ticket, so
Gatekeeper would need an online check to assess it — and the updater assesses
offline. Notarizing a zip of the app first lets the ticket be stapled to the
bundle itself, which then goes into both the DMG and the update zip.

### Env overrides

- `DIRI_SIGN_IDENTITY` — Developer ID identity (default: auto-detected).
- `NOTARY_PROFILE` — notarytool profile (default `dirijor-notary`).
- `GH_REPO` — repository to publish to (default `cristicretu/diri`).
- `TAP_DIR` — clean Homebrew tap checkout (default `../../homebrew-diri`).
- `SKIP_CASK=1` — explicitly publish without offering the release via Homebrew.
- `SKIP_GATES=1` — skip clippy/tests when re-running a failed publish.

## Verifying a release

The acceptance test is that an old build updates itself:

1. Keep a copy of the previous `diri.app` (or install the previous DMG).
2. Launch it and run **⌘K → Check for Updates…**.
3. The pill should offer the new version; click through download and restart.
4. Confirm the relaunched app reports the new version in the account popover.

To rehearse against a staging feed before publishing, point the app at one:

```sh
DIRI_UPDATE_FEED=https://example.test/appcast-staging.json /Applications/diri.app/Contents/MacOS/diri
```

`DIRI_UPDATER_ALLOW_UNSIGNED=1` lets an ad-hoc-signed local build run the flow.
The signature check on the *download* still applies, so the artifact must still
be a real notarized bundle.

## Troubleshooting

- **"Updates off for this build."** The running app is not in a `.app`, or is
  ad-hoc signed. Expected for `cargo run` and for `package.sh` output built
  without `DIRI_SIGN_IDENTITY`. Settings → General shows the exact reason.
- **"The download failed its signature check."** Usually the app was notarized
  but not stapled, or the release was built with a different Developer ID.
  Check with `xcrun stapler validate` and `codesign -dv --verbose=4` on the
  published zip's contents.
- **"diri can't write to its own folder."** The app is in `/Applications` on a
  machine where this user is not an admin. Download the DMG by hand.
- **"Couldn't reach the releases host."** Usually the Basic-auth password was
  rotated without shipping a new build; old installs must redownload.
- **The pill never appears.** Confirm the feed lists a strictly-newer version
  than `CARGO_PKG_VERSION` and that its `minimum_system_version` is not above
  this machine's `sw_vers -productVersion`.
- **An install went wrong.** `~/Library/Caches/diri/updates/<version>/install.log`
  holds the helper's output, and `diri.app.diri-previous` next to the app is
  the pre-update bundle if the restore path also failed.
