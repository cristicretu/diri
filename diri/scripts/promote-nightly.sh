#!/bin/bash
# Promote a macOS nightly to a stable release.
#
# Usage: diri/scripts/promote-nightly.sh <X.Y.Z> [<nightly-version>]
#
#   diri/scripts/promote-nightly.sh 0.9.4                       newest nightly
#   diri/scripts/promote-nightly.sh 0.9.4 0.9.4-nightly.202610070417
#
# A stable release ships exactly the commit a nightly was built from, so the
# code everyone gets is the code that soaked on the nightly channel. Commits
# that landed on main after that nightly do not ride along. Steps:
#
#   1. Looks the nightly up in the published nightly feed and finds its commit.
#      A nightly younger than PROMOTE_MIN_SOAK_HOURS (default 24) is refused
#      unless PROMOTE_FORCE=1.
#   2. Creates stable/<X.Y.Z> at that commit, plus one commit that bumps
#      diri-app to X.Y.Z and syncs model prices, in a worktree next to the main checkout
#      (../dirijor-stable-<X.Y.Z>), and pushes the branch. The push runs CI and
#      the Nightly workflow's signed Linux package jobs on the branch.
#   3. Runs release.sh <X.Y.Z> from that worktree. release.sh sees
#      origin/stable/<X.Y.Z> and releases from it instead of main.
#   4. Opens a pull request that bumps main's diri-app to X.Y.Z too, so the next
#      nightly and the next bump start from the promoted version.
#
# A hotfix to a promoted release is a cherry-pick: branch stable/<X.Y.Z+1>
# from stable/<X.Y.Z>, cherry-pick the fix from main, bump the version, push,
# and run release.sh <X.Y.Z+1> from it. See diri/UPDATING.md, "Nightly channel".
#
# release.sh waits on GitHub Actions, which can take longer than a tool's
# background limit. Run it detached when needed:
#   nohup diri/scripts/promote-nightly.sh 0.9.4 > /tmp/promote.log 2>&1 & disown
#
# Env:
#   GH_REPO                  default cristicretu/diri
#   PROMOTE_MIN_SOAK_HOURS   default 24
#   PROMOTE_FORCE=1          promote a nightly younger than that
#   PROMOTE_NO_RELEASE=1     stop after pushing the release branch
#   plus everything release.sh reads (NOTARY_PROFILE, TAP_DIR, ...)
set -euo pipefail

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
    echo "usage: diri/scripts/promote-nightly.sh <X.Y.Z> [<nightly-version>]" >&2
    exit 2
fi
VERSION="$1"
WANTED="${2:-}"
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "error: version '$VERSION' is not X.Y.Z" >&2
    exit 2
fi

GH_REPO="${GH_REPO:-cristicretu/diri}"
FEED_URL="https://github.com/$GH_REPO/releases/download/nightly/appcast.json"
MIN_SOAK_HOURS="${PROMOTE_MIN_SOAK_HOURS:-24}"
BRANCH="stable/$VERSION"
SCRIPT_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
MAIN_REPO="$(dirname "$(git -C "$SCRIPT_ROOT" rev-parse --path-format=absolute --git-common-dir)")"
RELEASE_ROOT="$(dirname "$MAIN_REPO")/dirijor-stable-$VERSION"

log() {
    echo "[$(date +%H:%M:%S)] $*"
}

# Rewrites diri-app's version in <workspace>'s Cargo.toml and Cargo.lock.
set_app_version() {
    python3 - "$1/crates/diri-app/Cargo.toml" "$1/Cargo.lock" "$2" <<'PY'
import pathlib, re, sys
manifest, lockfile, new = sys.argv[1:]
path = pathlib.Path(manifest)
text, count = re.subn(r'(?m)^version = "[^"]+"', f'version = "{new}"', path.read_text(), count=1)
if count != 1:
    raise SystemExit(f"error: {manifest} has no version line")
path.write_text(text)
path = pathlib.Path(lockfile)
text, count = re.subn(r'(name = "diri-app"\nversion = )"[^"]+"', rf'\1"{new}"', path.read_text(), count=1)
if count != 1:
    raise SystemExit("error: Cargo.lock has no diri-app entry")
path.write_text(text)
PY
}

# Refreshes <workspace>'s bundled model prices from LiteLLM. Prices only feed
# cost estimates, so an unreachable source keeps the old table, not the release.
sync_prices() {
    if [ -f "$1/scripts/sync-model-prices.py" ]; then
        python3 "$1/scripts/sync-model-prices.py" \
            || log "warning: model price sync failed; keeping the bundled prices"
    fi
}

# ----------------------------------------------------------------------------
# 1. Which nightly, which commit
# ----------------------------------------------------------------------------
git -C "$MAIN_REPO" fetch --quiet origin main --tags
FEED="$(curl -fsSL --connect-timeout 15 --max-time 60 "$FEED_URL")" || {
    echo "error: could not fetch the nightly feed $FEED_URL" >&2
    exit 1
}
NIGHTLY_INFO="$(python3 -c '
import datetime, json, sys
feed = json.loads(sys.stdin.read())
wanted = sys.argv[1]
releases = feed.get("releases", [])
release = next((r for r in releases if r["version"] == wanted), None) if wanted else (releases[0] if releases else None)
if release is None:
    sys.exit("error: nightly " + (wanted or "(newest)") + " is not in the feed; it lists: " + ", ".join(r["version"] for r in releases))
stamp = release["version"].split("-nightly.")[1]
built = datetime.datetime.strptime(stamp, "%Y%m%d%H%M").replace(tzinfo=datetime.timezone.utc)
age = (datetime.datetime.now(datetime.timezone.utc) - built).total_seconds() / 3600
print(release["version"], release.get("commit", ""), int(age))
' "$WANTED" <<<"$FEED")"
read -r NIGHTLY COMMIT AGE_HOURS <<<"$NIGHTLY_INFO"
if ! [[ "$COMMIT" =~ ^[0-9a-f]{40}$ ]]; then
    echo "error: nightly $NIGHTLY does not record its source commit" >&2
    exit 1
fi
if ! git -C "$MAIN_REPO" merge-base --is-ancestor "$COMMIT" origin/main; then
    echo "error: nightly $NIGHTLY's commit $COMMIT is not on origin/main" >&2
    exit 1
fi
NIGHTLY_BASE="${NIGHTLY%%-nightly.*}"
LATEST_TAG="$(git -C "$MAIN_REPO" tag -l 'v*' | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -1)"
if ! python3 - "$VERSION" "$NIGHTLY_BASE" "${LATEST_TAG#v}" <<'PY'
import sys
version, base, latest = (tuple(int(p) for p in (v or "0.0.0").split(".")) for v in sys.argv[1:])
if version < base:
    sys.exit(f"error: {sys.argv[1]} is older than the nightly's own version {sys.argv[2]}")
if version <= latest:
    sys.exit(f"error: {sys.argv[1]} is not newer than the latest stable release {sys.argv[3]}")
PY
then
    exit 1
fi
if git -C "$MAIN_REPO" rev-parse --verify --quiet "refs/tags/v$VERSION" >/dev/null; then
    echo "error: v$VERSION already exists" >&2
    exit 1
fi

SINCE_STABLE="$(git -C "$MAIN_REPO" rev-list --first-parent --count "${LATEST_TAG:-$COMMIT}..$COMMIT" 2>/dev/null || echo "?")"
BEHIND_MAIN="$(git -C "$MAIN_REPO" rev-list --first-parent --count "$COMMIT..origin/main")"
cat <<EOF
==> Promoting nightly $NIGHTLY to diri $VERSION
    Commit        : $COMMIT $(git -C "$MAIN_REPO" log -1 --format=%s "$COMMIT")
    Soaked        : ${AGE_HOURS}h (minimum ${MIN_SOAK_HOURS}h)
    Since ${LATEST_TAG:-start}  : $SINCE_STABLE commits on main
    Left out      : $BEHIND_MAIN newer commits on main stay for the next nightly
EOF
if [ "$AGE_HOURS" -lt "$MIN_SOAK_HOURS" ] && [ "${PROMOTE_FORCE:-0}" != 1 ]; then
    echo "error: $NIGHTLY has soaked ${AGE_HOURS}h of ${MIN_SOAK_HOURS}h; PROMOTE_FORCE=1 to promote it anyway" >&2
    exit 1
fi

# ----------------------------------------------------------------------------
# 2. stable/<X.Y.Z> = nightly commit + version bump
# ----------------------------------------------------------------------------
if git -C "$MAIN_REPO" ls-remote --exit-code --heads origin "$BRANCH" >/dev/null 2>&1; then
    git -C "$MAIN_REPO" fetch --quiet origin "$BRANCH"
    PARENT="$(git -C "$MAIN_REPO" rev-parse "origin/$BRANCH^")"
    if [ "$PARENT" != "$COMMIT" ]; then
        echo "error: origin/$BRANCH already exists on top of $PARENT, not $COMMIT" >&2
        echo "  (it was made from another nightly or by hand; delete it or release it as is)" >&2
        exit 1
    fi
    log "origin/$BRANCH already exists for this nightly; reusing it"
    if [ ! -d "$RELEASE_ROOT" ]; then
        git -C "$MAIN_REPO" worktree add --quiet "$RELEASE_ROOT" -B "$BRANCH" "origin/$BRANCH"
    fi
else
    if [ -e "$RELEASE_ROOT" ]; then
        echo "error: $RELEASE_ROOT exists but origin/$BRANCH does not; remove that worktree first" >&2
        exit 1
    fi
    log "Creating $BRANCH at $COMMIT in $RELEASE_ROOT"
    git -C "$MAIN_REPO" worktree add --quiet -b "$BRANCH" "$RELEASE_ROOT" "$COMMIT"
    set_app_version "$RELEASE_ROOT/diri" "$VERSION"
    sync_prices "$RELEASE_ROOT/diri"
    git -C "$RELEASE_ROOT" commit --quiet -am "Release $VERSION from nightly $NIGHTLY"
    git -C "$RELEASE_ROOT" push --quiet -u origin "$BRANCH"
    log "Pushed $BRANCH; CI and the Linux package jobs are starting on it"
fi

if [ "${PROMOTE_NO_RELEASE:-0}" = 1 ]; then
    log "PROMOTE_NO_RELEASE=1: stopping before release.sh"
    echo "    finish with: (cd $RELEASE_ROOT && diri/scripts/release.sh $VERSION)"
    exit 0
fi

# ----------------------------------------------------------------------------
# 3. Release from the branch
# ----------------------------------------------------------------------------
(cd "$RELEASE_ROOT" && diri/scripts/release.sh "$VERSION")

# ----------------------------------------------------------------------------
# 4. Bring main's version up to the release
# ----------------------------------------------------------------------------
MAIN_VERSION="$(git -C "$MAIN_REPO" show origin/main:diri/crates/diri-app/Cargo.toml \
    | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)"
if python3 -c 'import sys; a, b = (tuple(int(p) for p in v.split("-")[0].split(".")) for v in sys.argv[1:]); sys.exit(0 if a < b else 1)' \
    "$MAIN_VERSION" "$VERSION"; then
    BUMP_BRANCH="chore/version-$VERSION"
    BUMP_ROOT="$(dirname "$MAIN_REPO")/dirijor-version-$VERSION"
    log "Opening a PR that bumps main from $MAIN_VERSION to $VERSION"
    git -C "$MAIN_REPO" worktree add --quiet -b "$BUMP_BRANCH" "$BUMP_ROOT" origin/main
    set_app_version "$BUMP_ROOT/diri" "$VERSION"
    sync_prices "$BUMP_ROOT/diri"
    git -C "$BUMP_ROOT" commit --quiet -am "Bump diri-app to $VERSION after promoting $NIGHTLY"
    git -C "$BUMP_ROOT" push --quiet -u origin "$BUMP_BRANCH"
    gh pr create -R "$GH_REPO" --base main --head "$BUMP_BRANCH" \
        --title "Bump diri-app to $VERSION" \
        --body "diri $VERSION was promoted from nightly \`$NIGHTLY\` ($COMMIT) and released from \`$BRANCH\`. This moves main's version up to match, so the next nightly is \`${VERSION%.*}.$((${VERSION##*.} + 1))-nightly.*\`."
    git -C "$MAIN_REPO" worktree remove "$BUMP_ROOT"
fi

log "Promoted $NIGHTLY to $VERSION: https://github.com/$GH_REPO/releases/tag/v$VERSION"
