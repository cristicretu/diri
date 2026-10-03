#!/bin/bash
# Build, sign, notarize, and publish a diri release.
#
# Usage: diri/scripts/release.sh <version> [--wait-for-merge]
#
# The fast path (about five minutes, see diri/UPDATING.md): check out the
# version-bump PR's branch and run with --wait-for-merge. The macOS app is
# built, signed and notarized while the PR's CI runs; merge the PR meanwhile.
# The script then finds the merge on main by its tree (a squash merge of an
# up-to-date PR has the branch's exact tree), passes the gate on the PR's own
# CI run, and publishes. Without the flag the checkout's tree must already be
# on main.
#
# Env overrides:
#   DIRI_SIGN_IDENTITY  "Developer ID Application: ..." (default: auto-detected)
#   NOTARY_PROFILE      notarytool keychain profile (default: dirijor-notary)
#   GH_REPO             GitHub repo to publish to (default: cristicretu/diri)
#   TAP_DIR             Homebrew tap checkout (default: ../../homebrew-diri)
#   SKIP_CASK=1         explicitly publish without updating the Homebrew cask
#   SKIP_GATES=1        skip the CI gate (for re-running a failed publish)
#   DIRI_LOCAL_GATES=1  run cargo clippy/test here instead of trusting CI's run
#   SKIP_PERF_GATE=1   skip packaged app memory/idle-CPU probe
#   DIRI_LINUX_DIST     use this Linux CI artifact directory instead of fetching
#                       (it must carry CI's .sigstore.json signature bundles)
#   DIRI_MERGE_TIMEOUT_SECONDS  how long --wait-for-merge waits (default 3600)
#   DIRI_RELEASE_TARGET_DIR  build cache (default: diri/target/release-pipeline)
#
# Speed: the macOS build runs locally while GitHub Actions work is awaited in
# the background: the gate (a passing CI run on the release's exact tree) and
# the signed Linux packages, which the Nightly workflow starts building when
# the version bump merges. The macOS release, feed and cask go out as soon as
# the gate passes; the Linux files are attached to the same release when they
# arrive, verified the same way. See scripts/await-ci.sh.
#
# This publishes two notarized macOS artifacts plus the CI-built Linux
# AppImage and Debian package for each of x86_64 and aarch64:
#   diri-<version>-universal.dmg  what people download by hand
#   diri-<version>-universal.zip  what the in-app updater fetches
# plus appcast.json, SHA256SUMS, and the reviewed dependency-license inventory.
# The Linux files carry the Sigstore bundles CI signed them with
# (<file>.sigstore.json); they are verified here before anything is published.
# All are attached to a GitHub Release, so the updater feed has a stable URL.
# See diri/UPDATING.md for the trust model and one-time setup.
set -euo pipefail

if [ $# -lt 1 ] || [ $# -gt 2 ] || { [ $# -eq 2 ] && [ "$2" != "--wait-for-merge" ]; }; then
    echo "usage: diri/scripts/release.sh <version> [--wait-for-merge]   (e.g. 0.2.0)" >&2
    exit 2
fi
VERSION="$1"
WAIT_FOR_MERGE=0
[ "${2:-}" = "--wait-for-merge" ] && WAIT_FOR_MERGE=1
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "error: version '$VERSION' is not X.Y.Z" >&2
    exit 2
fi

WORKSPACE="$(cd "$(dirname "$0")/.." && pwd)"
ROOT="$(cd "$WORKSPACE/.." && pwd)"
cd "$WORKSPACE"

DIST="$WORKSPACE/dist"
APP="$DIST/diri.app"
DMG="$DIST/diri-$VERSION-universal.dmg"
ZIP="$DIST/diri-$VERSION-universal.zip"
MANIFEST="$WORKSPACE/crates/diri-app/Cargo.toml"

NOTARY_PROFILE="${NOTARY_PROFILE:-dirijor-notary}"
GH_REPO="${GH_REPO:-cristicretu/diri}"
# Homebrew tap checkout, bumped in lockstep with each release (step 6).
TAP_DIR="${TAP_DIR:-$ROOT/../homebrew-diri}"
TAG="v$VERSION"
FEED="$DIST/appcast.json"
CHECKSUMS="$DIST/SHA256SUMS"
INVENTORY="$DIST/THIRD-PARTY-LICENSES.json"
MINIMUM_SYSTEM="15.0"
# Old builds stay downloadable but the repo should not grow without bound.
KEEP_RELEASES=5

# See package.sh: prefer the persistent home toolchain over the /tmp one, which
# macOS sweeps out from under us.
if [ -x "$HOME/.cargo/bin/cargo" ]; then
    export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
    export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
else
    export CARGO_HOME="${CARGO_HOME:-/tmp/diri-cargo-home}"
    export RUSTUP_HOME="${RUSTUP_HOME:-/tmp/diri-rustup-home}"
fi
export PATH="$CARGO_HOME/bin:$PATH"
# A build cache only release builds write to, deliberately ignoring an inherited
# CARGO_TARGET_DIR. Agent worktrees are told to share $WORKSPACE/target, and a
# worktree whose sources differ writes artifacts cargo then considers fresh there
# -- a cross-workspace fingerprint collision that once linked an x86_64 diri-term
# missing a method its source had. Releases used to defend against that by
# cleaning every first-party crate, recompiling all of diri for both slices each
# time. A cache nothing else touches makes the clean unnecessary, so a release
# rebuilds only what changed since the last one.
export CARGO_TARGET_DIR="${DIRI_RELEASE_TARGET_DIR:-$WORKSPACE/target/release-pipeline}"

if [ -z "${DIRI_SIGN_IDENTITY:-}" ]; then
    # `|| true`: grep exits 1 with no match, which pipefail would turn into an
    # abort before the friendly error below.
    DIRI_SIGN_IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
        | grep "Developer ID Application" | head -1 \
        | sed -E 's/.*"(.*)".*/\1/' || true)"
fi
if [ -z "${DIRI_SIGN_IDENTITY:-}" ]; then
    cat >&2 <<EOF
error: no "Developer ID Application" signing identity found.

  Create one in Xcode → Settings → Accounts → Manage Certificates → +
  → "Developer ID Application", then re-run. Or set DIRI_SIGN_IDENTITY.
  See diri/UPDATING.md.
EOF
    exit 1
fi

echo "==> Releasing diri $VERSION"
echo "    Sign identity : $DIRI_SIGN_IDENTITY"
echo "    Notary profile: $NOTARY_PROFILE"
echo "    Publishing to : $GH_REPO"

if ! command -v gh >/dev/null 2>&1; then
    echo "error: the GitHub CLI (gh) is required to publish" >&2
    exit 1
fi
# Checked now rather than after a 40-minute build: the Linux signatures are
# verified before publishing, and an unverifiable release must not ship.
if ! command -v cosign >/dev/null 2>&1; then
    echo "error: cosign is required to verify the Linux signatures (brew install cosign)" >&2
    exit 1
fi
if [ "${SKIP_CASK:-0}" != "1" ] && [ ! -f "$TAP_DIR/Casks/diri.rb" ]; then
    cat >&2 <<EOF
error: Homebrew tap checkout is missing Casks/diri.rb: $TAP_DIR

Set TAP_DIR to a clean cristicretu/homebrew-diri checkout. The release cannot
claim success unless its cask is pushed and verified. Use SKIP_CASK=1 only when
you intentionally do not want this release offered through Homebrew.
EOF
    exit 1
fi

# ----------------------------------------------------------------------------
# 1. Source provenance
# ----------------------------------------------------------------------------
# The updater compares against CARGO_PKG_VERSION, so the manifest is the single
# source of truth for what version this build claims to be. Version bumps go
# through a normal pull request; a release must build the exact remote main
# commit rather than creating an unpushed release-only commit.
CURRENT="$(sed -n 's/^version = "\(.*\)"/\1/p' "$MANIFEST" | head -1)"
if [ "$CURRENT" != "$VERSION" ]; then
    cat >&2 <<EOF
error: diri-app is $CURRENT, not $VERSION

Open and merge a version-bump pull request first, then run this script from the
clean main checkout. Release artifacts must map to a reviewed source commit.
EOF
    exit 1
fi
if [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no)" ]; then
    echo "error: tracked files are dirty; release from a clean checkout" >&2
    exit 1
fi
# A release ships a reviewed commit on main. It is identified by its source
# tree, so the bump PR's branch (whose squash merge has the same tree) can be
# built and notarized before the merge, and a merge landing after it does not
# force a rebuild. SOURCE_COMMIT is the newest main commit with this tree.
SOURCE_TREE="$(git -C "$ROOT" rev-parse 'HEAD^{tree}')"
# The remote Helper's Build ID is the tree too, so a bundle built from the
# branch is byte-for-byte the bundle the merge commit would produce.
export DIRI_REMOTE_BUILD_ID="$SOURCE_TREE"
find_source_commit() {
    git -C "$ROOT" fetch --quiet origin main --tags
    # awk reads to the end rather than exiting at the match: an early exit
    # sends git log SIGPIPE, which pipefail turns into a failed lookup, so a
    # found merge looked missing and --wait-for-merge polled forever.
    git -C "$ROOT" log --first-parent -50 --format='%H %T' origin/main \
        | awk -v tree="$SOURCE_TREE" '$2 == tree && !found { print $1; found = 1 }'
}
SOURCE_COMMIT="$(find_source_commit)"
if [ -z "$SOURCE_COMMIT" ] && [ "$WAIT_FOR_MERGE" != 1 ]; then
    cat >&2 <<EOF
error: this checkout's tree ($SOURCE_TREE) is not on origin/main

Merge the version bump first, or run from the bump PR's branch with
--wait-for-merge to build while it merges.
EOF
    exit 1
fi
check_tag() {
    if git -C "$ROOT" rev-parse --verify --quiet "refs/tags/$TAG" >/dev/null; then
        local tag_commit
        tag_commit="$(git -C "$ROOT" rev-list -n 1 "$TAG")"
        if [ "$tag_commit" != "$SOURCE_COMMIT" ]; then
            echo "error: $TAG points to $tag_commit, not release source $SOURCE_COMMIT" >&2
            exit 1
        fi
    fi
}
if [ -n "$SOURCE_COMMIT" ]; then
    check_tag
    echo "    source commit : $SOURCE_COMMIT"
else
    echo "    source tree   : $SOURCE_TREE (waiting for it to merge to main)"
fi

# ----------------------------------------------------------------------------
# Background CI work, joined before publishing
# ----------------------------------------------------------------------------
CI_LOG_DIR="$CARGO_TARGET_DIR/release-logs"
mkdir -p "$CI_LOG_DIR"
BACKGROUND_PIDS=()
# `|| true` on each kill: this runs under set -e, and a job that already
# finished makes kill fail, which would abort the trap before the remaining
# jobs are stopped and turn a successful release into exit 1.
trap 'for pid in "${BACKGROUND_PIDS[@]:-}"; do if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; fi; done' EXIT

# Waits for a background step; on failure prints its log and aborts.
join_background() {
    local pid="$1" label="$2" log="$3"
    if ! wait "$pid"; then
        echo "error: $label failed:" >&2
        sed 's/^/    /' "$log" >&2
        exit 1
    fi
    echo "==> $label: done ($(tail -n 1 "$log"))"
}

# Aborts early if a background step has already failed; otherwise returns.
check_background() {
    local pid="$1" label="$2" log="$3"
    if ! kill -0 "$pid" 2>/dev/null; then
        join_background "$pid" "$label" "$log"
    fi
}

GATES_PID=""
GATES_LOG="$CI_LOG_DIR/gates.log"
LINUX_PID=""
LINUX_LOG="$CI_LOG_DIR/linux.log"
LINUX_FROM_ENV=0
if [ -n "${DIRI_LINUX_DIST:-}" ]; then
    if [ ! -d "$DIRI_LINUX_DIST" ]; then
        echo "error: DIRI_LINUX_DIST is not a directory: $DIRI_LINUX_DIST" >&2
        exit 1
    fi
    LINUX_FROM_ENV=1
fi
start_ci_work() {
    if [ "${SKIP_GATES:-0}" = "1" ]; then
        echo "==> Skipping the CI gate (SKIP_GATES=1)"
    elif [ "${DIRI_LOCAL_GATES:-0}" = "1" ]; then
        echo "==> Running release gates locally (DIRI_LOCAL_GATES=1)"
        cargo clippy --workspace --all-targets -- -D warnings
        cargo test --workspace
    else
        echo "==> Waiting on a passing CI run for $SOURCE_COMMIT's tree (background, log: $GATES_LOG)"
        GH_REPO="$GH_REPO" "$WORKSPACE/scripts/await-ci.sh" gates "$SOURCE_COMMIT" \
            > "$GATES_LOG" 2>&1 &
        GATES_PID=$!
        BACKGROUND_PIDS+=("$GATES_PID")
    fi
    if [ "$LINUX_FROM_ENV" = 1 ]; then
        echo "==> Using Linux packages from $DIRI_LINUX_DIST"
    else
        DIRI_LINUX_DIST="$CARGO_TARGET_DIR/linux-packages-$SOURCE_COMMIT"
        echo "==> Fetching Linux packages for $SOURCE_COMMIT (background, log: $LINUX_LOG)"
        GH_REPO="$GH_REPO" "$WORKSPACE/scripts/await-ci.sh" linux "$SOURCE_COMMIT" \
            "$DIRI_LINUX_DIST" > "$LINUX_LOG" 2>&1 &
        LINUX_PID=$!
        BACKGROUND_PIDS+=("$LINUX_PID")
    fi
}
if [ -n "$SOURCE_COMMIT" ]; then
    start_ci_work
fi

# ----------------------------------------------------------------------------
# 2. Build, sign, notarize, staple (app first, then DMG — see package.sh)
# ----------------------------------------------------------------------------
if [ -n "$GATES_PID" ]; then
    check_background "$GATES_PID" "CI gate" "$GATES_LOG"
fi
echo "==> Packaging (notarization can take a few minutes)"
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

# The updater refuses a download whose ticket does not validate offline, so
# check that here rather than discovering it from a user's failed update.
echo "==> Verifying the stapled bundle the updater will install"
xcrun stapler validate "$APP"
spctl --assess --type execute -vv "$APP"

# The regression probe must run against this exact signed/notarized bundle.
# It owns and terminates only the two Diri processes it launches.
if [ "${SKIP_PERF_GATE:-0}" != "1" ]; then
    echo "==> Running packaged memory/idle-CPU gate"
    "$WORKSPACE/scripts/perf-gate.sh" --app "$APP" --scenario all
fi

# ----------------------------------------------------------------------------
# 3. Wait for the merge (--wait-for-merge), then the gate
# ----------------------------------------------------------------------------
if [ -z "$SOURCE_COMMIT" ]; then
    echo "==> Built and notarized. Waiting for tree $SOURCE_TREE to merge to main"
    merge_deadline=$((SECONDS + ${DIRI_MERGE_TIMEOUT_SECONDS:-3600}))
    until SOURCE_COMMIT="$(find_source_commit)" && [ -n "$SOURCE_COMMIT" ]; do
        if [ "$SECONDS" -ge "$merge_deadline" ]; then
            echo "error: the bump never reached main with this tree; nothing was published" >&2
            echo "  (a squash merge of a PR that is behind main has a different tree)" >&2
            exit 1
        fi
        sleep 10
    done
    check_tag
    echo "    source commit : $SOURCE_COMMIT"
    start_ci_work
fi
if [ -n "$GATES_PID" ]; then
    echo "==> Waiting for the CI gate"
    join_background "$GATES_PID" "CI gate" "$GATES_LOG"
fi

# Validates the Linux CI artifact, verifies its Sigstore signatures against
# main's Nightly identity, and stages it in $DIST. Fills LINUX_PACKAGES and
# LINUX_ASSETS.
#
# The artifact is Nightly's merged release set: an AppImage and a Debian
# package for every architecture in LINUX_ARCHITECTURES, one SHA256SUMS over
# all of them, and one linux-release.json. Its top-level "architecture" and
# "artifacts" stay the x86_64 build's, as every release before aarch64 had
# them; "builds" lists each architecture (see write-linux-release-manifest.py).
LINUX_ARCHITECTURES="x86_64 aarch64"
LINUX_PACKAGES=()
LINUX_ASSETS=()
stage_linux() {
    LINUX_MANIFEST_SOURCE="$DIRI_LINUX_DIST/linux-release.json"
    if [ ! -f "$LINUX_MANIFEST_SOURCE" ]; then
        echo "error: Linux CI artifact must contain linux-release.json" >&2
        exit 1
    fi

    local listing="$CI_LOG_DIR/linux-packages.txt"
    python3 - "$LINUX_MANIFEST_SOURCE" "$VERSION" "$SOURCE_COMMIT" \
        "$LINUX_ARCHITECTURES" > "$listing" <<'PYLINUX'
import hashlib
import json
import pathlib
import sys

manifest_path, version, commit, architectures = sys.argv[1:]
manifest_path = pathlib.Path(manifest_path)
manifest = json.loads(manifest_path.read_text())
if manifest.get("version") != version or manifest.get("commit") != commit:
    raise SystemExit("Linux artifact version/source commit does not match this release")
# Readers that predate "builds" take the top-level fields as the x86_64 build.
if manifest.get("platform") != "linux" or manifest.get("architecture") != "x86_64":
    raise SystemExit("Linux artifact has the wrong platform or architecture")
builds = {build.get("architecture"): build for build in manifest.get("builds", [])}
if sorted(builds) != sorted(architectures.split()) or len(builds) != len(manifest["builds"]):
    raise SystemExit(f"Linux artifact builds {sorted(builds)}, expected {architectures.split()}")
if manifest.get("artifacts") != builds["x86_64"].get("artifacts"):
    raise SystemExit("Linux manifest's top-level artifacts are not the x86_64 build")
for architecture, build in builds.items():
    formats = sorted(record.get("format") for record in build.get("artifacts", []))
    if formats != ["appimage", "deb"]:
        raise SystemExit(f"Linux {architecture} build must have one AppImage and one DEB")
    for record in build["artifacts"]:
        name = record["file"]
        if "/" in name or name.startswith("."):
            raise SystemExit(f"unsafe Linux artifact name {name!r}")
        path = manifest_path.parent / name
        if not path.is_file():
            raise SystemExit(f"Linux CI artifact is missing {name}")
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        if digest != record.get("sha256"):
            raise SystemExit(f"Linux artifact digest mismatch: {name}")
        print(name)
PYLINUX

    # The packages must carry signatures from main's Nightly workflow. Overrides a
    # maintainer may have exported for a rehearsal are dropped so the pinned
    # identity, not the environment, decides what is accepted. This also
    # refuses any package in the directory that the manifest does not declare.
    echo "==> Verifying Linux Sigstore signatures"
    env -u DIRI_COSIGN_PUBLIC_KEY -u DIRI_SIGNING_IDENTITY -u DIRI_SIGNING_OIDC_ISSUER \
        GH_REPO="$GH_REPO" "$WORKSPACE/scripts/linux-signatures.sh" verify "$DIRI_LINUX_DIST"

    LINUX_MANIFEST="$DIST/linux-release.json"
    # The release-wide SHA256SUMS below covers every platform and is written here,
    # so it cannot carry CI's signature. CI's signed Linux-only list ships beside it
    # under its own name; the bundle signs bytes, not a filename.
    LINUX_CHECKSUMS="$DIST/SHA256SUMS-linux"
    mkdir -p "$DIST"
    copy_linux_asset() {
        if [ "$1" != "$2" ]; then
            cp "$1" "$2"
        fi
    }
    LINUX_PACKAGES=()
    local package_signatures=() name
    while IFS= read -r name; do
        [ -n "$name" ] || continue
        copy_linux_asset "$DIRI_LINUX_DIST/$name" "$DIST/$name"
        copy_linux_asset "$DIRI_LINUX_DIST/$name.sigstore.json" "$DIST/$name.sigstore.json"
        LINUX_PACKAGES+=("$DIST/$name")
        package_signatures+=("$DIST/$name.sigstore.json")
    done < "$listing"
    copy_linux_asset "$LINUX_MANIFEST_SOURCE" "$LINUX_MANIFEST"
    copy_linux_asset "$DIRI_LINUX_DIST/SHA256SUMS" "$LINUX_CHECKSUMS"
    copy_linux_asset "$LINUX_MANIFEST_SOURCE.sigstore.json" "$LINUX_MANIFEST.sigstore.json"
    copy_linux_asset "$DIRI_LINUX_DIST/SHA256SUMS.sigstore.json" "$LINUX_CHECKSUMS.sigstore.json"
    LINUX_SIGNATURES=(
        "$LINUX_CHECKSUMS"
        "${package_signatures[@]}"
        "$LINUX_MANIFEST.sigstore.json"
        "$LINUX_CHECKSUMS.sigstore.json"
    )
    LINUX_ASSETS=(
        "${LINUX_PACKAGES[@]}" "$LINUX_MANIFEST" "${LINUX_SIGNATURES[@]}"
    )
}

# Linux packages that are already here ship with the release; otherwise the
# macOS release goes out now and they are attached when the Nightly run that
# the bump's merge started has built and signed them (~15 minutes).
LINUX_DEFERRED=0
if [ "$LINUX_FROM_ENV" = 1 ]; then
    stage_linux
elif [ -n "$LINUX_PID" ] && ! kill -0 "$LINUX_PID" 2>/dev/null; then
    join_background "$LINUX_PID" "Linux packages" "$LINUX_LOG"
    stage_linux
else
    LINUX_DEFERRED=1
    echo "==> Linux packages are still building; publishing macOS first"
fi

# ----------------------------------------------------------------------------
# 4. Build the update feed
# ----------------------------------------------------------------------------
# Download URLs are the release's own assets. The feed is attached to every
# release, so the `latest` alias always resolves to the newest one — that is
# what gives a stable feed URL with no server to run.
BASE_URL="https://github.com/$GH_REPO/releases/download/$TAG"
SIZE="$(stat -f%z "$ZIP")"
SHA256="$(shasum -a 256 "$ZIP" | awk '{print $1}')"
DMG_SHA256="$(shasum -a 256 "$DMG" | awk '{print $1}')"
PUBLISHED="$(date -u +%Y-%m-%d)"

# Start from the published feed so releases people skipped stay offerable.
echo "==> Fetching the current feed"
if ! curl -fsSL "https://github.com/$GH_REPO/releases/latest/download/appcast.json" -o "$FEED" 2>/dev/null; then
    echo "    (no published feed yet — starting a new one)"
    rm -f "$FEED"
fi

echo "==> Writing $FEED"
VERSION="$VERSION" \
URL="$BASE_URL/diri-$VERSION-universal.zip" \
SIZE="$SIZE" SHA256="$SHA256" PUBLISHED="$PUBLISHED" \
MINIMUM_SYSTEM="$MINIMUM_SYSTEM" \
FEED="$FEED" KEEP_RELEASES="$KEEP_RELEASES" \
python3 - <<'PYFEED'
import json, os, pathlib

feed_path = pathlib.Path(os.environ["FEED"])
feed = {"feed_version": 1, "releases": []}
if feed_path.exists():
    try:
        feed = json.loads(feed_path.read_text())
    except json.JSONDecodeError:
        print("    (published feed did not parse — starting a new one)")
        feed = {"feed_version": 1, "releases": []}
    feed.setdefault("feed_version", 1)
    feed.setdefault("releases", [])

version = os.environ["VERSION"]
entry = {
    "version": version,
    "url": os.environ["URL"],
    "size": int(os.environ["SIZE"]),
    "sha256": os.environ["SHA256"],
    "minimum_system_version": os.environ["MINIMUM_SYSTEM"],
    "published": os.environ["PUBLISHED"],
}

# Re-releasing a version replaces its row rather than adding a second one the
# client would have to disambiguate.
releases = [r for r in feed["releases"] if r.get("version") != version]
releases.append(entry)


def sort_key(release):
    parts = (release.get("version") or "0").split(".")
    return tuple(int(part) if part.isdigit() else 0 for part in (parts + ["0", "0", "0"])[:3])


releases.sort(key=sort_key, reverse=True)
feed["releases"] = releases[: int(os.environ["KEEP_RELEASES"])]
feed_path.write_text(json.dumps(feed, indent=2) + "\n")
print(f"    {len(feed['releases'])} release(s) in the feed, newest {feed['releases'][0]['version']}")
PYFEED

if [ ! -f "$INVENTORY" ]; then
    echo "error: packaging did not produce $INVENTORY" >&2
    exit 1
fi

# Covers the macOS files, the feed and the inventory, plus the Linux packages
# when they ship together. Linux files attached later are covered by their own
# signed SHA256SUMS-linux.
echo "==> Writing $CHECKSUMS"
CHECKSUMMED=("$(basename "$DMG")" "$(basename "$ZIP")")
if [ "$LINUX_DEFERRED" = 0 ]; then
    for package in "${LINUX_PACKAGES[@]}"; do
        CHECKSUMMED+=("$(basename "$package")")
    done
    CHECKSUMMED+=("$(basename "$LINUX_MANIFEST")")
fi
CHECKSUMMED+=("$(basename "$FEED")" "$(basename "$INVENTORY")")
(
    cd "$DIST"
    shasum -a 256 "${CHECKSUMMED[@]}" > "$(basename "$CHECKSUMS")"
    shasum -a 256 -c "$(basename "$CHECKSUMS")"
)

# ----------------------------------------------------------------------------
# 5. Publish the GitHub Release
# ----------------------------------------------------------------------------
NOTES_FILE="$DIST/notes-$VERSION.md"
if [ ! -f "$NOTES_FILE" ]; then
    cat > "$NOTES_FILE" <<NOTES
## diri $VERSION

Run coding agents in parallel with live status, persistent local sessions, git
worktrees, review tools, and direct SSH hosts.

**macOS:** download the DMG below, open it, drag diri to Applications.
Universal (Apple silicon and Intel), signed and notarized, so it opens without
a Gatekeeper prompt.

**Linux beta:** download the Debian package or AppImage for your machine:
\`amd64\`/\`x86_64\` for Intel and AMD, \`arm64\`/\`aarch64\` for 64-bit ARM.
Ubuntu 22.04 and 24.04 are supported under X11 and Wayland. Linux updates use
a newer package/download rather than the in-app macOS updater.

See \`SHA256SUMS\` and \`linux-release.json\` for artifact metadata. Each Linux
file has a Sigstore \`.sigstore.json\` signature from this repository's CI;
\`diri/LINUX.md\` shows how to verify it. The macOS
app updates itself from the \`appcast.json\` feed attached here.
NOTES
    echo "==> Wrote default notes to $NOTES_FILE (edit and re-run to customize)"
fi

echo "==> Publishing $TAG to $GH_REPO"
GH_REPO="$GH_REPO" SOURCE_COMMIT="$SOURCE_COMMIT" \
    "$WORKSPACE/scripts/publish-github-release.sh" \
    "$VERSION" "$NOTES_FILE" "$DMG" "$ZIP" \
    ${LINUX_ASSETS[@]+"${LINUX_ASSETS[@]}"} \
    "$FEED" "$CHECKSUMS" "$INVENTORY"

# ----------------------------------------------------------------------------
# 6. Bump the Homebrew cask
# ----------------------------------------------------------------------------
# The cask pins the DMG's sha256, so it has to move in lockstep. The publisher
# verifies the local DMG against GitHub first, pushes even if the correct commit
# already existed locally, then reads the remote branch back to prove the cask
# users receive matches the immutable release asset.
if [ "${SKIP_CASK:-0}" = "1" ]; then
    echo "==> Skipping the Homebrew cask (SKIP_CASK=1)"
else
    echo "==> Publishing the Homebrew cask for $VERSION"
    GH_REPO="$GH_REPO" \
        "$WORKSPACE/scripts/publish-homebrew-cask.sh" \
        "$VERSION" "$DMG" "$TAP_DIR"
fi

echo "==> macOS release, feed and cask are live: https://github.com/$GH_REPO/releases/tag/$TAG"

# ----------------------------------------------------------------------------
# 7. Attach the Linux packages if they were still building
# ----------------------------------------------------------------------------
if [ "$LINUX_DEFERRED" = 1 ]; then
    echo "==> Waiting for the Linux packages (Nightly run)"
    join_background "$LINUX_PID" "Linux packages" "$LINUX_LOG"
    stage_linux
    echo "==> Attaching the Linux packages to $TAG"
    PUBLISH_ATTACH=1 GH_REPO="$GH_REPO" \
        "$WORKSPACE/scripts/publish-github-release.sh" \
        "$VERSION" "$NOTES_FILE" "${LINUX_ASSETS[@]}"
fi

cat <<EOF

============================================================
  diri $VERSION released
============================================================
  DMG        : $DMG
  Update zip : $ZIP
  Linux      : $(for package in "${LINUX_PACKAGES[@]}"; do printf '%s ' "$(basename "$package")"; done)
  Linux meta : $LINUX_MANIFEST
  DMG sha256 : $DMG_SHA256
  ZIP sha256 : $SHA256
  Checksums   : $CHECKSUMS
  Licenses    : $INVENTORY
  Source      : $SOURCE_COMMIT
  Release    : https://github.com/$GH_REPO/releases/tag/$TAG
  Feed       : https://github.com/$GH_REPO/releases/latest/download/appcast.json

  Next steps:
    1. Confirm an old build updates itself (diri/UPDATING.md → "Verifying a release")
    2. Install one Linux artifact on native X11 and Wayland hosts
============================================================
EOF
