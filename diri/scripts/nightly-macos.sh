#!/bin/bash
# Build, sign, notarize, and publish tonight's macOS nightly of main.
#
# Usage: diri/scripts/nightly-macos.sh
#
# Runs on the maintainer's Mac (a Diri scheduled task fires it nightly), because
# signing and notarization need the Developer ID identity and the notary
# keychain profile, and those never leave that machine. See diri/UPDATING.md,
# "Nightly channel".
#
# What it does:
#   1. Picks the newest commit on origin/main whose CI push run passed. A red or
#      still-running tip is skipped in favour of the last green commit, never
#      shipped.
#   2. Stops if that commit is already the newest nightly, or a stable release
#      already contains it (nothing has landed since the release, so a nightly
#      would only repackage it). NIGHTLY_FORCE=1 builds anyway.
#   3. Checks it out in a dedicated worktree (DIRI_NIGHTLY_WORKTREE, default
#      ../dirijor-nightly-build next to the main checkout) with its own build
#      cache, and stamps diri-app as X.Y.Z-nightly.YYYYMMDDHHMM. X.Y.Z is the
#      release after the latest stable tag, so every nightly sorts below the
#      stable version it may later be promoted to.
#   4. Packages, signs, and notarizes a universal DMG and update zip.
#   5. Publishes both to the rolling `nightly` GitHub prerelease (the tag moves
#      to the commit), rewrites its appcast.json, and prunes nightlies beyond
#      the newest KEEP_NIGHTLIES. A prerelease never becomes `latest`, so the
#      stable feed and the Homebrew cask cannot see any of this.
#
# No Linux packages, Homebrew cask, or update mirror: the nightly is a macOS
# dogfood channel. To ship a nightly to everyone, see promote-nightly.sh.
#
# Env overrides:
#   DIRI_SIGN_IDENTITY     "Developer ID Application: ..." (default: auto-detected)
#   NOTARY_PROFILE         notarytool keychain profile (default: dirijor-notary)
#   GH_REPO                default cristicretu/diri
#   DIRI_NIGHTLY_WORKTREE  build worktree (default ../dirijor-nightly-build)
#   NIGHTLY_COMMIT         build this main commit instead of the newest green one
#   NIGHTLY_FORCE=1        build even if the commit already has a nightly or a
#                          stable release
#   NIGHTLY_PERF_GATE=1    also run the packaged memory/idle-CPU gate (opens
#                          windows, so it is off for unattended runs)
#   KEEP_NIGHTLIES         nightlies kept in the feed and on the release (default 7)
#   NIGHTLY_DRY_RUN=1      build, notarize and write the feed, but publish nothing
#   NIGHTLY_LOCAL=1        rehearse on this checkout's HEAD (must be clean) instead
#                          of main; implies NIGHTLY_DRY_RUN=1
set -euo pipefail

# The whole script is one compound command, so bash has parsed all of it before
# stage 1 checks a different commit out under the copy that is running.
{
GH_REPO="${GH_REPO:-cristicretu/diri}"
NOTARY_PROFILE="${NOTARY_PROFILE:-dirijor-notary}"
KEEP_NIGHTLIES="${KEEP_NIGHTLIES:-7}"
NIGHTLY_TAG=nightly
FEED_URL="https://github.com/$GH_REPO/releases/download/$NIGHTLY_TAG/appcast.json"
MINIMUM_SYSTEM="15.0"

SCRIPT_WORKSPACE="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT_ROOT="$(cd "$SCRIPT_WORKSPACE/.." && pwd)"
MAIN_REPO="$(dirname "$(git -C "$SCRIPT_ROOT" rev-parse --path-format=absolute --git-common-dir)")"
BUILD_ROOT="${DIRI_NIGHTLY_WORKTREE:-$(dirname "$MAIN_REPO")/dirijor-nightly-build}"

# See package.sh: prefer the persistent home toolchain over the /tmp one.
if [ -x "$HOME/.cargo/bin/cargo" ]; then
    export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
    export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
fi
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

log() {
    echo "[$(date +%H:%M:%S)] $*"
}

for tool in gh git python3 curl shasum xcrun; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "error: $tool is required" >&2
        exit 1
    fi
done

# ----------------------------------------------------------------------------
# Stage 1 (any checkout): pick the commit, prepare the build worktree, re-exec
# ----------------------------------------------------------------------------
if [ "${NIGHTLY_LOCAL:-0}" = 1 ]; then
    NIGHTLY_STAGE=build
    NIGHTLY_DRY_RUN=1
    NIGHTLY_COMMIT="$(git -C "$SCRIPT_ROOT" rev-parse HEAD)"
fi
if [ "${NIGHTLY_STAGE:-}" != build ]; then
    LOCK="$HOME/Library/Caches/diri/nightly-macos.lock"
    mkdir -p "$(dirname "$LOCK")"
    if ! mkdir "$LOCK" 2>/dev/null; then
        echo "error: another nightly is running (remove $LOCK if it is stale)" >&2
        exit 1
    fi
    trap 'rmdir "$LOCK" 2>/dev/null || true' EXIT

    git -C "$MAIN_REPO" fetch --quiet origin main --tags --force
    if [ -n "${NIGHTLY_COMMIT:-}" ]; then
        COMMIT="$(git -C "$MAIN_REPO" rev-parse --verify "$NIGHTLY_COMMIT^{commit}")"
        if ! git -C "$MAIN_REPO" merge-base --is-ancestor "$COMMIT" origin/main; then
            echo "error: NIGHTLY_COMMIT $COMMIT is not on origin/main" >&2
            exit 1
        fi
    else
        # Newest first-parent commit on main with a passing CI push run.
        GREEN="$(gh run list -R "$GH_REPO" --workflow ci.yml --branch main --event push \
            --status success -L 100 --json headSha --jq '.[].headSha')"
        COMMIT=""
        for candidate in $(git -C "$MAIN_REPO" log --first-parent -40 --format=%H origin/main); do
            if grep -qx "$candidate" <<<"$GREEN"; then
                COMMIT="$candidate"
                break
            fi
        done
        if [ -z "$COMMIT" ]; then
            echo "error: none of main's last 40 commits has a passing CI push run" >&2
            exit 1
        fi
        TIP="$(git -C "$MAIN_REPO" rev-parse origin/main)"
        if [ "$COMMIT" != "$TIP" ]; then
            log "main's tip $TIP is not green yet; building the newest green commit"
        fi
    fi
    log "Nightly source: $COMMIT $(git -C "$MAIN_REPO" log -1 --format=%s "$COMMIT")"

    # Right after a release nothing on main is newer than the stable build.
    LATEST_TAG="$(git -C "$MAIN_REPO" tag -l "v*" | grep -E "^v[0-9]+\.[0-9]+\.[0-9]+$" | sort -V | tail -1 || true)"
    if [ -n "$LATEST_TAG" ] && [ "${NIGHTLY_FORCE:-0}" != 1 ] \
        && git -C "$MAIN_REPO" merge-base --is-ancestor "$COMMIT" "$LATEST_TAG"; then
        log "Stable $LATEST_TAG already contains $COMMIT; nothing to do (NIGHTLY_FORCE=1 rebuilds)"
        exit 0
    fi

    PREVIOUS_COMMIT="$(curl -fsSL --connect-timeout 15 --max-time 60 "$FEED_URL" 2>/dev/null \
        | python3 -c 'import json, sys; print(json.load(sys.stdin)["releases"][0].get("commit", ""))' \
        2>/dev/null || true)"
    if [ "$PREVIOUS_COMMIT" = "$COMMIT" ] && [ "${NIGHTLY_FORCE:-0}" != 1 ]; then
        log "The newest nightly already ships $COMMIT; nothing to do (NIGHTLY_FORCE=1 rebuilds)"
        exit 0
    fi

    if [ -d "$BUILD_ROOT" ]; then
        # --force discards the previous run's version stamp; the untracked
        # target/ cache survives, which is the point of a persistent worktree.
        git -C "$BUILD_ROOT" checkout --quiet --detach --force "$COMMIT"
    else
        log "Creating build worktree $BUILD_ROOT"
        git -C "$MAIN_REPO" worktree add --quiet --detach "$BUILD_ROOT" "$COMMIT"
    fi
    if [ ! -x "$BUILD_ROOT/diri/scripts/nightly-macos.sh" ]; then
        echo "error: $COMMIT predates nightly-macos.sh; nothing to build with" >&2
        exit 1
    fi
    NIGHTLY_STAGE=build NIGHTLY_COMMIT="$COMMIT" NIGHTLY_PREVIOUS_COMMIT="$PREVIOUS_COMMIT" \
        "$BUILD_ROOT/diri/scripts/nightly-macos.sh"
    exit $?
fi

# ----------------------------------------------------------------------------
# Stage 2 (the build worktree, at the chosen commit)
# ----------------------------------------------------------------------------
WORKSPACE="$SCRIPT_WORKSPACE"
ROOT="$SCRIPT_ROOT"
cd "$WORKSPACE"
COMMIT="$NIGHTLY_COMMIT"
PREVIOUS_COMMIT="${NIGHTLY_PREVIOUS_COMMIT:-}"
MANIFEST="$WORKSPACE/crates/diri-app/Cargo.toml"
LOCKFILE="$WORKSPACE/Cargo.lock"

if [ "$(git -C "$ROOT" rev-parse HEAD)" != "$COMMIT" ]; then
    echo "error: build worktree is not at $COMMIT" >&2
    exit 1
fi
if [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no)" ]; then
    echo "error: build worktree $ROOT has tracked changes" >&2
    exit 1
fi
# Whatever happens below, leave the worktree at the pristine commit.
trap 'git -C "$ROOT" checkout --quiet -- diri/crates/diri-app/Cargo.toml diri/Cargo.lock 2>/dev/null || true' EXIT

if [ -z "${DIRI_SIGN_IDENTITY:-}" ]; then
    DIRI_SIGN_IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
        | grep "Developer ID Application" | head -1 \
        | sed -E 's/.*"(.*)".*/\1/' || true)"
fi
if [ -z "${DIRI_SIGN_IDENTITY:-}" ]; then
    echo "error: no \"Developer ID Application\" signing identity found (see diri/UPDATING.md)" >&2
    exit 1
fi

# X.Y.Z: the release after the newest stable tag, or main's own version if a
# bump already moved it further.
LATEST_TAG="$(git -C "$ROOT" tag -l 'v*' | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -1)"
CARGO_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$MANIFEST" | head -1)"
BASE="$(python3 - "${LATEST_TAG#v}" "$CARGO_VERSION" <<'PY'
import sys
def parse(text):
    return tuple(int(part) for part in text.split("-")[0].split("."))
tag, cargo = parse(sys.argv[1] or "0.0.0"), parse(sys.argv[2])
after_tag = (tag[0], tag[1], tag[2] + 1)
print(".".join(map(str, max(after_tag, cargo))))
PY
)"
STAMP="$(date -u +%Y%m%d%H%M)"
VERSION="$BASE-nightly.$STAMP"
SOURCE_TREE="$(git -C "$ROOT" rev-parse 'HEAD^{tree}')"
log "Building diri $VERSION from $COMMIT (latest stable ${LATEST_TAG:-none})"

# cargo-packager stamps CFBundleShortVersionString from the manifest, and the
# updater compares CARGO_PKG_VERSION, so the version lives in Cargo.toml for
# this build only (restored by the EXIT trap).
python3 - "$MANIFEST" "$LOCKFILE" "$CARGO_VERSION" "$VERSION" <<'PY'
import pathlib, sys
manifest, lockfile, old, new = sys.argv[1:]
path = pathlib.Path(manifest)
text = path.read_text()
needle = f'version = "{old}"'
if needle not in text:
    raise SystemExit(f"error: {manifest} has no {needle}")
path.write_text(text.replace(needle, f'version = "{new}"', 1))
path = pathlib.Path(lockfile)
text = path.read_text()
needle = f'name = "diri-app"\nversion = "{old}"'
if needle not in text:
    raise SystemExit(f"error: Cargo.lock has no diri-app {old} entry")
path.write_text(text.replace(needle, f'name = "diri-app"\nversion = "{new}"', 1))
PY

# A cache only this worktree's nightly builds write to; see release.sh for why
# release builds never share a target directory with other checkouts.
export CARGO_TARGET_DIR="$WORKSPACE/target/release-pipeline"
# Remote Helpers are named by the source tree, as release.sh names them, so a
# nightly and a rebuild of the same commit share one Helper on remote hosts.
export DIRI_REMOTE_BUILD_ID="$SOURCE_TREE"
DIST="$WORKSPACE/dist"
rm -rf "$DIST"
APP="$DIST/diri.app"
DMG="$DIST/diri-$VERSION-universal.dmg"
ZIP="$DIST/diri-$VERSION-universal.zip"
FEED="$DIST/appcast.json"

log "Packaging and notarizing (a few minutes)"
DIRI_DIST_DIR="$DIST" \
DIRI_VERSION="$VERSION" \
DIRI_SIGN_IDENTITY="$DIRI_SIGN_IDENTITY" \
DIRI_CREATE_DMG=1 \
DIRI_CREATE_ZIP=1 \
APPLE_NOTARIZATION_KEYCHAIN_PROFILE="$NOTARY_PROFILE" \
    "$WORKSPACE/scripts/package.sh"

for artifact in "$APP" "$DMG" "$ZIP"; do
    if [ ! -e "$artifact" ]; then
        echo "error: packaging did not produce $artifact" >&2
        exit 1
    fi
done
log "Verifying the stapled bundle the updater will install"
xcrun stapler validate "$APP"
spctl --assess --type execute -vv "$APP"
BUNDLE_VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")"
if [ "$BUNDLE_VERSION" != "$VERSION" ]; then
    echo "error: bundle says $BUNDLE_VERSION, expected $VERSION; the updater would reject it" >&2
    exit 1
fi
if [ "${NIGHTLY_PERF_GATE:-0}" = 1 ]; then
    log "Running packaged memory/idle-CPU gate"
    "$WORKSPACE/scripts/perf-gate.sh" --app "$APP" --scenario all
fi

# ----------------------------------------------------------------------------
# Feed: the published nightly feed plus tonight's row, newest KEEP_NIGHTLIES
# ----------------------------------------------------------------------------
log "Writing $FEED"
FEED_STATUS="$(curl -sSL --connect-timeout 15 --max-time 60 -o "$FEED" -w '%{http_code}' "$FEED_URL" || true)"
case "$FEED_STATUS" in
    200) ;;
    404) rm -f "$FEED"; log "(no nightly feed yet; starting one)" ;;
    *)
        echo "error: could not fetch $FEED_URL (HTTP ${FEED_STATUS:-none}); refusing to drop older nightlies" >&2
        exit 1
        ;;
esac
BASE_URL="https://github.com/$GH_REPO/releases/download/$NIGHTLY_TAG"
VERSION="$VERSION" COMMIT="$COMMIT" PREVIOUS_COMMIT="$PREVIOUS_COMMIT" \
URL="$BASE_URL/$(basename "$ZIP")" \
SIZE="$(stat -f%z "$ZIP")" SHA256="$(shasum -a 256 "$ZIP" | awk '{print $1}')" \
PUBLISHED="$(date -u +%Y-%m-%d)" MINIMUM_SYSTEM="$MINIMUM_SYSTEM" \
FEED="$FEED" KEEP="$KEEP_NIGHTLIES" GH_REPO="$GH_REPO" \
python3 - <<'PYFEED'
import json, os, pathlib, re

feed_path = pathlib.Path(os.environ["FEED"])
feed = {"feed_version": 1, "releases": []}
if feed_path.exists():
    try:
        feed = json.loads(feed_path.read_text())
    except json.JSONDecodeError as error:
        raise SystemExit(f"error: the published nightly feed did not parse ({error})")
    if not isinstance(feed.get("releases"), list):
        raise SystemExit("error: the published nightly feed has no releases list")

repo, commit, previous = os.environ["GH_REPO"], os.environ["COMMIT"], os.environ["PREVIOUS_COMMIT"]
entry = {
    "version": os.environ["VERSION"],
    "url": os.environ["URL"],
    "size": int(os.environ["SIZE"]),
    "sha256": os.environ["SHA256"],
    "minimum_system_version": os.environ["MINIMUM_SYSTEM"],
    "published": os.environ["PUBLISHED"],
    "commit": commit,
    "notes_url": (f"https://github.com/{repo}/compare/{previous}...{commit}" if previous
                  else f"https://github.com/{repo}/commit/{commit}"),
}
pattern = re.compile(r"^\d+\.\d+\.\d+-nightly\.\d{12}$")
releases = [r for r in feed["releases"] if pattern.match(r.get("version", "")) and r["version"] != entry["version"]]
releases.append(entry)

def key(release):
    core, stamp = release["version"].split("-nightly.")
    return (tuple(int(part) for part in core.split(".")), int(stamp))

releases.sort(key=key, reverse=True)
feed["feed_version"] = feed.get("feed_version", 1)
feed["releases"] = releases[: int(os.environ["KEEP"])]
feed_path.write_text(json.dumps(feed, indent=2) + "\n")
print(f"    {len(feed['releases'])} nightl{'y' if len(feed['releases']) == 1 else 'ies'} in the feed, newest {feed['releases'][0]['version']}")
PYFEED

# ----------------------------------------------------------------------------
# Publish to the rolling `nightly` prerelease
# ----------------------------------------------------------------------------
NOTES="$DIST/nightly-notes.md"
{
    echo "Rolling macOS build of \`main\`, rebuilt every night from the newest commit whose CI passed. It may be broken: that is what it is for."
    echo
    echo "**Get it:** Settings → General → Updates → Update channel → Nightly (or download the DMG below). Switch back to Stable whenever you like; you move to the next stable release when it ships."
    echo
    echo "Signed and notarized, macOS only. Stable releases are promoted from a nightly that has soaked (\`diri/scripts/promote-nightly.sh\`)."
    echo
    echo "| Nightly | Commit | Published |"
    echo "|---|---|---|"
    python3 - "$FEED" "$GH_REPO" <<'PY'
import json, sys
for release in json.load(open(sys.argv[1]))["releases"]:
    commit = release.get("commit", "")
    link = f"[{commit[:10]}](https://github.com/{sys.argv[2]}/commit/{commit})"
    print(f"| {release['version']} | {link} | {release.get('published', '')} |")
PY
    echo
    echo "### Changes in $VERSION"
    echo
    if [ -n "$PREVIOUS_COMMIT" ] && git -C "$ROOT" cat-file -e "$PREVIOUS_COMMIT^{commit}" 2>/dev/null; then
        git -C "$ROOT" log --first-parent --format='- %s' "$PREVIOUS_COMMIT..$COMMIT" | head -60
    else
        git -C "$ROOT" log --first-parent --format='- %s' -20 "$COMMIT"
    fi
} > "$NOTES"

if [ "${NIGHTLY_DRY_RUN:-0}" = 1 ]; then
    log "Dry run: built $VERSION; nothing published. Feed and notes are in $DIST"
    exit 0
fi

if ! gh release view "$NIGHTLY_TAG" -R "$GH_REPO" >/dev/null 2>&1; then
    log "Creating the $NIGHTLY_TAG prerelease"
    gh release create "$NIGHTLY_TAG" -R "$GH_REPO" --prerelease --latest=false \
        --target "$COMMIT" --title "diri nightly" --notes-file "$NOTES"
else
    # Lightweight tag; move it to the commit tonight's build came from.
    gh api -X PATCH "repos/$GH_REPO/git/refs/tags/$NIGHTLY_TAG" \
        -f sha="$COMMIT" -F force=true --silent
fi

# Archives first, feed last: the published feed never names a missing file.
log "Uploading $(basename "$ZIP") and $(basename "$DMG")"
gh release upload "$NIGHTLY_TAG" -R "$GH_REPO" --clobber "$ZIP" "$DMG"
log "Publishing the nightly feed"
gh release upload "$NIGHTLY_TAG" -R "$GH_REPO" --clobber "$FEED"
gh release edit "$NIGHTLY_TAG" -R "$GH_REPO" --prerelease --latest=false \
    --title "diri nightly $VERSION" --notes-file "$NOTES" >/dev/null

# Drop archives the feed no longer lists.
KEEP_NAMES="$(python3 - "$FEED" <<'PY'
import json, sys
for release in json.load(open(sys.argv[1]))["releases"]:
    for kind in ("zip", "dmg"):
        print(f"diri-{release['version']}-universal.{kind}")
PY
)"
gh release view "$NIGHTLY_TAG" -R "$GH_REPO" --json assets --jq '.assets[].name' \
    | grep -E '^diri-.*-nightly\.[0-9]+-universal\.(zip|dmg)$' \
    | while read -r asset; do
        if ! grep -qx "$asset" <<<"$KEEP_NAMES"; then
            log "Pruning $asset"
            gh release delete-asset "$NIGHTLY_TAG" "$asset" -R "$GH_REPO" --yes
        fi
    done

# The stable channel reads releases/latest; a nightly must never become it.
LATEST="$(gh api "repos/$GH_REPO/releases/latest" --jq .tag_name)"
if [ "$LATEST" = "$NIGHTLY_TAG" ]; then
    echo "error: GitHub's latest release is now $NIGHTLY_TAG; stable users would be offered nightlies" >&2
    exit 1
fi

# Read the feed back the way the app does and check it names these bytes.
ZIP_SHA="$(shasum -a 256 "$ZIP" | awk '{print $1}')"
for attempt in 1 2 3 4 5 6; do
    if curl -fsSL --connect-timeout 15 --max-time 60 "$FEED_URL" \
        | python3 -c 'import json, sys; r = json.load(sys.stdin)["releases"][0]; sys.exit(0 if (r["version"], r["sha256"]) == (sys.argv[1], sys.argv[2]) else 1)' \
            "$VERSION" "$ZIP_SHA"; then
        break
    fi
    if [ "$attempt" = 6 ]; then
        echo "warning: $FEED_URL does not list $VERSION yet (CDN lag?)" >&2
    fi
    sleep 10
done

cat <<EOF

============================================================
  diri nightly $VERSION published
============================================================
  Source  : $COMMIT
  Release : https://github.com/$GH_REPO/releases/tag/$NIGHTLY_TAG
  Feed    : $FEED_URL
  Promote : diri/scripts/promote-nightly.sh $BASE $VERSION
============================================================
EOF
exit 0
}
