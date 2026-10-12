# diri's macOS auto-updater

Automatic in-app replacement is macOS-only. Linux installs update through a
new Debian package or AppImage from the same GitHub release; the app says so
explicitly instead of starting the macOS bundle updater. See
[`LINUX.md`](LINUX.md).

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

Checks, downloads, and verification are automatic (20 s after launch, then
every 6 h, toggleable in **Settings → General → Updates**). Installation waits
for a safe app boundary and never interrupts a live session:

1. A background check finds a release and downloads it from GitHub Releases.
2. The bundle is checksum-checked, signature-checked, and staged; the sidebar
   footer shows **Restart to update to 0.2.0**.
3. A normal quit installs it without reopening diri. The next launch is the
   new app. Clicking **Restart to update to 0.2.0** installs and relaunches it
   immediately.

diri holds live agent sessions, so it never quits or relaunches itself
uninvited.
**⌘K → Check for Updates…** and the version row in the account popover both
run a manual check, which reports "up to date" rather than staying silent.

## How the swap works

A process cannot reliably delete the bundle it is executing from, so
`install.rs` generates a small `/bin/sh` helper, spawns it detached in its own
process group, and quits. The helper first copies the staged bundle to
`diri.app.diri-next` beside the running app with `ditto`, waits (up to 60 s)
for diri's pid to disappear, then renames the old bundle to
`diri.app.diri-previous` and the new one into place. An explicit update restart
relaunches it; a normal quit does not. If anything fails it restores the old
bundle — an interrupted install leaves a working app, never a hole.

After diri exits, its path only ever changes by rename. At that moment macOS
(loginwindow, asking Background Task Management) reads the bundle at diri's
path to decide whether the processes diri started may outlive it. When the
bundle is missing or half copied the answer is an error, and macOS terminates
the Engine, every Holder and every Agent — every live session is lost. Copying
into the app's path after the exit opened that window for as long as `ditto`
ran; `scripts/install-local.sh` follows the same rule.

Staging lives in `~/Library/Caches/diri/updates/<version>/`, with the helper's
log at `install.log` there. Directories for versions at or below the running
one are swept at launch.

If the app sits somewhere the user cannot write, the writability check fails
*before* the download starts rather than after 50 MB.

## Nightly channel

Every stable release used to be whatever was on `main` that day. Bugs reached
everyone the moment a release went out. Now there are two channels:

| | Stable | Nightly |
|---|---|---|
| Built from | a nightly that soaked, promoted | `main`'s newest green commit, nightly |
| Version | `0.9.4` | `0.9.4-nightly.202610070417` (UTC stamp) |
| Feed | `releases/latest/download/appcast.json` | `releases/download/nightly/appcast.json` |
| GitHub release | `v<version>` | one rolling prerelease, tag `nightly` |
| Platforms | macOS + Linux, Homebrew cask | macOS only |
| Update mirror | yes | no |

Pick a channel in **Settings → General → Updates → Update channel**. A build
that is itself a nightly defaults to Nightly. Everything else defaults to Stable.

**Ordering.** A nightly's `X.Y.Z` is the release after the newest stable tag,
so `0.9.4-nightly.*` sorts after `0.9.3` and before `0.9.4` (semver prerelease
rules; see `crates/diri-updater/src/version.rs`). The nightly feed lists only
nightlies, so a nightly user moves from one nightly to the next. When `0.9.4`
is promoted, the next one is `0.9.5-nightly.*`, which already contains it.
Switching to Stable offers `0.9.4`, because it outranks every `0.9.4-nightly.*`.
Before a newer stable exists, you stay on your nightly unless you pick an older
version in the version picker.
The stable channel ignores nightly rows in any feed, and GitHub never treats
a prerelease as `latest`. Stable users and the cask cannot see nightlies.
Releases before the channel existed cannot see them either.

### Building the nightly

```sh
diri/scripts/nightly-macos.sh
```

It runs on the maintainer's Mac, because signing and notarization need the
Developer ID identity and the notary keychain profile, and neither leaves that
machine. A Diri schedule runs it every night with `wake_mac` on. Each run:

1. Picks the newest first-parent commit on `origin/main` with a passing CI
   push run. A red or still-running tip is never shipped; the last green
   commit is built instead.
2. Does nothing if that commit is already the newest nightly, or if a stable
   release already contains it (no commits since the release).
3. Checks the commit out in `../dirijor-nightly-build`, a persistent worktree
   with its own `target/release-pipeline` cache, and stamps diri-app's
   `Cargo.toml`/`Cargo.lock` with the nightly version for that build only.
4. Packages, signs, notarizes and staples the DMG and update zip, like
   `release.sh`. The perf gate is off for unattended runs
   (`NIGHTLY_PERF_GATE=1` turns it on).
5. Uploads both to the `nightly` prerelease and moves its tag to the commit.
   It uploads the feed last, so the feed never names a missing file. It
   rewrites the release notes (the nightlies table plus that night's commit
   list), prunes nightlies beyond the newest seven, and fails loudly if GitHub
   ever reports `nightly` as the latest release.

Rehearse without publishing: `NIGHTLY_LOCAL=1` builds this checkout's HEAD, and
`NIGHTLY_DRY_RUN=1` builds main's pick. Both stop before upload.

### Promoting a nightly to stable

```sh
diri/scripts/promote-nightly.sh 0.9.4                              # newest nightly
diri/scripts/promote-nightly.sh 0.9.4 0.9.4-nightly.202610070417  # a specific one
```

The stable release ships **exactly the commit the nightly was built from**.
Commits that landed on `main` after that nightly wait for the next one. The
script:

1. Finds the nightly's commit in the nightly feed. A nightly younger than 24 h
   is refused (`PROMOTE_MIN_SOAK_HOURS`, or `PROMOTE_FORCE=1`).
2. Creates `stable/<version>` at that commit plus one version-bump commit, in
   `../dirijor-stable-<version>`, and pushes it. The push runs CI (`ci.yml`)
   and the Nightly workflow's signed Linux package jobs on the branch.
3. Runs `release.sh <version>` there. When `origin/stable/<version>` exists,
   `release.sh` releases from it instead of `main`: the provenance lookup, the
   CI gate, the Linux packages, and the pinned Sigstore identity
   (`nightly.yml@refs/heads/stable/<version>`) all follow the branch.
4. Opens a PR that bumps `main`'s diri-app version to match.

`release.sh` waits on GitHub Actions, so run the promotion detached if your
terminal or tool has a time limit:
`nohup diri/scripts/promote-nightly.sh 0.9.4 > /tmp/promote.log 2>&1 & disown`.

**Hotfixing a promoted release.** Branch `stable/0.9.5` from
`stable/0.9.4`, cherry-pick the fix from `main`, bump diri-app to `0.9.5`,
push, then run `release.sh 0.9.5` from that checkout. Releasing straight from
`main` with a bump PR (below) still works when you mean to ship `main` as it is.

## Cutting a release

One-time setup is the Developer ID cert and notary profile described in
[PACKAGING.md](PACKAGING.md), plus `brew install cosign` to verify the Linux
signatures. No Sparkle keys, and no Linux signing key: CI signs the Linux
packages keylessly (see [PACKAGING.md](PACKAGING.md#linux-signatures)).

A release takes about five minutes from the bump to a published macOS build:

1. Open a pull request that only bumps the `diri-app` version and lockfile,
   based on the current `main`.
2. Check out that branch in a clean worktree and run

   ```sh
   diri/scripts/release.sh 0.4.1 --wait-for-merge
   ```

3. While it builds, signs and notarizes, merge the pull request (squash).

The script identifies the release by its source tree. A squash merge of a bump
PR that is up to date with `main` has exactly the branch's tree, so the bundle
built before the merge is the bundle the merge commit describes (the remote
Helper's Build ID is the tree hash, so nothing in it names a commit). After
the merge it finds that commit on `main`, passes the gate on a successful CI
run for the same tree (the PR's own run counts, so it does not wait for
`main`'s macOS queue), and publishes the DMG, update zip, feed and cask.

The Linux packages are built and signed by the Nightly workflow, which starts
its two Linux package jobs when the bump merges (about 15 minutes). If they are
ready when the macOS side is, they ship together; otherwise the macOS release
goes out first and the script attaches the Linux files to the same release
when they arrive, after the same digest and Sigstore checks. Attaching only
adds missing assets; it never replaces one.

Without `--wait-for-merge`, run from a checkout whose tree is already on
`main`. Builds use `diri/target/release-pipeline`, a cache nothing else writes
to, so a release recompiles only what changed.

The script refuses to release a version that does not match the manifest, a
dirty checkout, or a tree that never reaches `origin/main`. It requires a
passing CI run on that tree, builds the universal Rust executables, signs them,
builds the DMG and **notarizes it once** (Apple tickets the DMG and the app
inside it), staples both the DMG and the app, produces the update zip from the
stapled app, rebuilds `appcast.json` from the currently published feed,
generates `SHA256SUMS` and a reviewed dependency license inventory, verifies
the Linux manifest and artifact digests against the source commit, verifies
the Linux Sigstore signatures against the pinned
`nightly.yml@refs/heads/main` identity, and publishes the GitHub Release. It
then updates, commits, **pushes, and reads back** the Homebrew cask; the
release does not report success until the remote cask checksum matches the
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

Keep the public title to `diri <version>`. Lead the notes with one sentence
about the most useful change, then a short list of user-visible improvements
and fixes. Include required upgrade steps and known limitations when relevant.
Link PRs for implementation detail; avoid repeating the title, feature pitches,
or the full install guide in every release.

When using GitHub's **Generate release notes**, the
[release configuration](../.github/release.yml) groups PRs by the existing
labels. Review and edit that output before publishing; it does not replace
the release script's authored notes file.

### The bundled Engine updates safely with the app

`diri.app` carries `dirijord-rs` + `diri-holder` in `Contents/Resources/bin`,
and the update zip carries those exact binaries. On every launch, the app
verifies the running Engine's identity and compares its executable hash with
the bundled Engine. A mismatch asks the old process to persist and shut down,
waits for it to exit, then launches the new binary. Holder processes retain
their PTYs across that handoff, so updating the Engine does not terminate live
agent sessions.

This is content-based rather than version-string-based: rebuilding the same app
version with different Engine bytes still refreshes the Engine, while an exact
match avoids needless churn.

The canonical release tag is `v<version>`. `gh release create` creates that tag
at the verified `origin/main` commit; do not create a second `diri-v<version>`
tag. Source is merged before release rather than pushed after binary publication.

### Why the .app is notarized before the DMG

Stapling the DMG alone leaves the extracted bundle without its own ticket, so
Gatekeeper would need an online check to assess it — and the updater assesses
offline. Notarizing a zip of the app first lets the ticket be stapled to the
bundle itself, which then goes into both the DMG and the update zip.

### Switching to a specific version

Settings → General → Software updates only ever offers something newer. To
move to an exact release, including an older one, **Option-click** the update
button ("Check Now" / "Download" / "Restart"). A "Switch version" list appears
with every release the feed still carries (the release script keeps the newest
five) and an **Install** button next to each one that is not running.

Installing from that list:

- turns **Update automatically** off and marks the build you are leaving as
  skipped, so the chosen version is not replaced on the next check and an older
  diri's own manual check does not nag about the exact build you left;
- goes through the same download, checksum, code-signature and
  promised-version checks as a normal update, then swaps and relaunches
  immediately;
- is the only path that installs a version older than the running one. The
  automatic feed selection keeps its strictly-newer rule, so a tampered feed
  still cannot walk anyone backwards.

Turn **Update automatically** back on (or press Check Now) to return to the
latest release.

### Env overrides

- `DIRI_SIGN_IDENTITY` — Developer ID identity (default: auto-detected).
- `NOTARY_PROFILE` — notarytool profile (default `dirijor-notary`).
- `GH_REPO` — repository to publish to (default `cristicretu/diri`).
- `TAP_DIR` — clean Homebrew tap checkout (default `../../homebrew-diri`).
- `SKIP_CASK=1` — explicitly publish without offering the release via Homebrew.
- `SKIP_GATES=1` — skip the CI gate when re-running a failed publish.
- `DIRI_LOCAL_GATES=1` — run clippy/tests locally instead of waiting on CI.
- `DIRI_LINUX_DIST` — use an already-downloaded Linux artifact directory. It
  must include CI's `.sigstore.json` bundles; a Nightly run that predates
  signing, or a pull-request run, has none and is refused.
- `DIRI_MERGE_TIMEOUT_SECONDS` — how long `--wait-for-merge` waits for the
  merge (default 3600).
- `DIRI_RELEASE_TARGET_DIR` — release build cache (default `diri/target/release-pipeline`).

## Verifying a release

The acceptance test is that an old build updates itself:

1. Keep a copy of the previous `diri.app` (or install the previous DMG).
2. Launch it and run **⌘K → Check for Updates…**.
3. Wait for the pill to reach **Restart to update**, then click it.
4. Confirm the relaunched app reports the new version in the account popover,
   its Hello response reports the new bundled Engine hash, and every session
   that was live before the restart is still present and interactive.

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
- **"Couldn't reach the releases host."** Confirm GitHub is reachable and that
  `releases/latest/download/appcast.json` resolves for the public repository.
- **The pill never appears.** Confirm the feed lists a strictly-newer version
  than `CARGO_PKG_VERSION` and that its `minimum_system_version` is not above
  this machine's `sw_vers -productVersion`.
- **An install went wrong.** `~/Library/Caches/diri/updates/<version>/install.log`
  holds the helper's output, and `diri.app.diri-previous` next to the app is
  the pre-update bundle if the restore path also failed.
- **Every session was lost across an update.** Check the unified log for
  `log show --predicate 'process == "loginwindow" AND category == "quitsupport"'`
  around the quit: `askBTM: Error -98` followed by "scheduling its
  subordinates' termination" means the bundle was unreadable when diri exited.
