#!/bin/bash
# Build, sign, notarize, and publish a diri release.
#
# Usage: diri/scripts/release.sh <version>        e.g. diri/scripts/release.sh 0.2.0
#
# Env overrides:
#   DIRI_SIGN_IDENTITY  "Developer ID Application: ..." (default: auto-detected)
#   NOTARY_PROFILE      notarytool keychain profile (default: dirijor-notary)
#   GH_REPO             GitHub repo to publish to (default: cristicretu/diri)
#   TAP_DIR             Homebrew tap checkout (default: ../../homebrew-diri)
#   SKIP_CASK=1         explicitly publish without updating the Homebrew cask
#   SKIP_GATES=1        skip cargo test/clippy (for re-running a failed publish)
#   SKIP_PERF_GATE=1   skip packaged app memory/idle-CPU probe
#   DIRI_LINUX_DIST     downloaded Linux CI artifact for this source commit
#
# This publishes two notarized macOS artifacts plus the CI-built Linux
# AppImage and Debian package:
#   diri-<version>-universal.dmg  what people download by hand
#   diri-<version>-universal.zip  what the in-app updater fetches
# plus appcast.json, SHA256SUMS, and the reviewed dependency-license inventory.
# All are attached to a GitHub Release, so the updater feed has a stable URL.
# See diri/UPDATING.md for the trust model and one-time setup.
set -euo pipefail

if [ $# -lt 1 ]; then
    echo "usage: diri/scripts/release.sh <version>   (e.g. 0.2.0)" >&2
    exit 2
fi
VERSION="$1"
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
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$WORKSPACE/target}"

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
    echo "error: tracked files are dirty; release from a clean main checkout" >&2
    exit 1
fi
git -C "$ROOT" fetch --quiet origin main --tags
SOURCE_COMMIT="$(git -C "$ROOT" rev-parse HEAD)"
REMOTE_MAIN="$(git -C "$ROOT" rev-parse origin/main)"
if [ "$SOURCE_COMMIT" != "$REMOTE_MAIN" ]; then
    cat >&2 <<EOF
error: release source is not the current origin/main
  local:  $SOURCE_COMMIT
  remote: $REMOTE_MAIN

Merge the source and version bump first, then check out and pull main.
EOF
    exit 1
fi
if git -C "$ROOT" rev-parse --verify --quiet "refs/tags/$TAG" >/dev/null; then
    TAG_COMMIT="$(git -C "$ROOT" rev-list -n 1 "$TAG")"
    if [ "$TAG_COMMIT" != "$SOURCE_COMMIT" ]; then
        echo "error: $TAG points to $TAG_COMMIT, not release source $SOURCE_COMMIT" >&2
        exit 1
    fi
fi
echo "    source commit : $SOURCE_COMMIT"

if [ -z "${DIRI_LINUX_DIST:-}" ] || [ ! -d "$DIRI_LINUX_DIST" ]; then
    cat >&2 <<EOF
error: DIRI_LINUX_DIST must name the downloaded linux-packages CI artifact
for source commit $SOURCE_COMMIT.

The release is created atomically with macOS and Linux assets. Download the
Linux package artifact from the successful CI run for this commit, set
DIRI_LINUX_DIST to that directory, and re-run.
EOF
    exit 1
fi

LINUX_APPIMAGE_SOURCE="$(find "$DIRI_LINUX_DIST" -maxdepth 1 -type f -name '*.AppImage' -print -quit)"
LINUX_DEB_SOURCE="$(find "$DIRI_LINUX_DIST" -maxdepth 1 -type f -name '*.deb' -print -quit)"
LINUX_MANIFEST_SOURCE="$DIRI_LINUX_DIST/linux-release.json"
if [ -z "$LINUX_APPIMAGE_SOURCE" ] || [ -z "$LINUX_DEB_SOURCE" ] || [ ! -f "$LINUX_MANIFEST_SOURCE" ]; then
    echo "error: Linux CI artifact must contain one AppImage, one DEB, and linux-release.json" >&2
    exit 1
fi

python3 - "$LINUX_MANIFEST_SOURCE" "$VERSION" "$SOURCE_COMMIT" \
    "$LINUX_APPIMAGE_SOURCE" "$LINUX_DEB_SOURCE" <<'PYLINUX'
import hashlib
import json
import pathlib
import sys

manifest_path, version, commit, *artifact_names = sys.argv[1:]
manifest = json.loads(pathlib.Path(manifest_path).read_text())
if manifest.get("version") != version or manifest.get("commit") != commit:
    raise SystemExit("Linux artifact version/source commit does not match this release")
if manifest.get("platform") != "linux" or manifest.get("architecture") != "x86_64":
    raise SystemExit("Linux artifact has the wrong platform or architecture")
records = {record["file"]: record for record in manifest.get("artifacts", [])}
for name in artifact_names:
    path = pathlib.Path(name)
    record = records.get(path.name)
    if record is None:
        raise SystemExit(f"Linux manifest does not declare {path.name}")
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    if digest != record.get("sha256"):
        raise SystemExit(f"Linux artifact digest mismatch: {path.name}")
PYLINUX

LINUX_APPIMAGE="$DIST/$(basename "$LINUX_APPIMAGE_SOURCE")"
LINUX_DEB="$DIST/$(basename "$LINUX_DEB_SOURCE")"
LINUX_MANIFEST="$DIST/linux-release.json"
mkdir -p "$DIST"
if [ "$LINUX_APPIMAGE_SOURCE" != "$LINUX_APPIMAGE" ]; then
    cp "$LINUX_APPIMAGE_SOURCE" "$LINUX_APPIMAGE"
fi
if [ "$LINUX_DEB_SOURCE" != "$LINUX_DEB" ]; then
    cp "$LINUX_DEB_SOURCE" "$LINUX_DEB"
fi
if [ "$LINUX_MANIFEST_SOURCE" != "$LINUX_MANIFEST" ]; then
    cp "$LINUX_MANIFEST_SOURCE" "$LINUX_MANIFEST"
fi

# ----------------------------------------------------------------------------
# 2. Gates
# ----------------------------------------------------------------------------
if [ "${SKIP_GATES:-0}" != "1" ]; then
    echo "==> Running release gates"
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
fi

# ----------------------------------------------------------------------------
# 3. Build, sign, notarize, staple (app first, then DMG — see package.sh)
# ----------------------------------------------------------------------------
# Agent worktrees are told to share this target dir (see the warning in
# package.sh). A build from a worktree whose sources differ writes artifacts
# that cargo then considers fresh here -- a cross-workspace fingerprint
# collision -- so a stale crate from another checkout can be linked in silently.
# It has happened: an x86_64 diri-term with a NEWER mtime than its source but
# missing a method the source had. A compile error is the lucky outcome; the
# unlucky one is a signed, notarized build shipping someone else's code. Third-
# party deps cannot collide this way (they are immutable at a given version), so
# only first-party crates are purged and the expensive GPUI build stays cached.
echo "==> Purging first-party build artifacts (cross-worktree cache safety)"
FIRST_PARTY=(-p diri-app -p diri-client -p diri-proto -p diri-term -p diri-ui -p diri-updater)
for release_target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo clean "${FIRST_PARTY[@]}" --release --target "$release_target"
done

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

echo "==> Writing $CHECKSUMS"
(
    cd "$DIST"
    shasum -a 256 \
        "$(basename "$DMG")" \
        "$(basename "$ZIP")" \
        "$(basename "$LINUX_APPIMAGE")" \
        "$(basename "$LINUX_DEB")" \
        "$(basename "$LINUX_MANIFEST")" \
        "$(basename "$FEED")" \
        "$(basename "$INVENTORY")" > "$(basename "$CHECKSUMS")"
    shasum -a 256 -c "$(basename "$CHECKSUMS")"
)

# ----------------------------------------------------------------------------
# 5. Publish the GitHub Release
# ----------------------------------------------------------------------------
NOTES_FILE="$DIST/notes-$VERSION.md"
if [ ! -f "$NOTES_FILE" ]; then
    cat > "$NOTES_FILE" <<NOTES
## diri $VERSION

Native desktop orchestrator for coding agents on macOS and Linux.

**macOS:** download the DMG below, open it, drag diri to Applications.
Universal (Apple silicon and Intel), signed and notarized, so it opens without
a Gatekeeper prompt.

**Linux beta:** download the x86_64 Debian package or AppImage. Ubuntu 22.04
and 24.04 are supported under X11 and Wayland. Linux updates use a newer
package/download rather than the in-app macOS updater.

See \`SHA256SUMS\` and \`linux-release.json\` for artifact metadata. The macOS
app updates itself from the \`appcast.json\` feed attached here.
NOTES
    echo "==> Wrote default notes to $NOTES_FILE (edit and re-run to customize)"
fi

echo "==> Publishing $TAG to $GH_REPO"
GH_REPO="$GH_REPO" SOURCE_COMMIT="$SOURCE_COMMIT" \
    "$WORKSPACE/scripts/publish-github-release.sh" \
    "$VERSION" "$NOTES_FILE" "$DMG" "$ZIP" \
    "$LINUX_APPIMAGE" "$LINUX_DEB" "$LINUX_MANIFEST" \
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

cat <<EOF

============================================================
  diri $VERSION released
============================================================
  DMG        : $DMG
  Update zip : $ZIP
  AppImage   : $LINUX_APPIMAGE
  Debian     : $LINUX_DEB
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
