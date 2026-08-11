#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_dir="$(cd "${script_dir}/.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-${workspace_dir}/target}"
profile="debug"
settings_preview=""
cargo_args=()

usage() {
    cat <<'USAGE'
Usage: scripts/dev.sh [--release] [--settings TAB] [-- CARGO_BUILD_ARGS...]

Build and launch an unmistakable development copy of diri.

Options:
  --release       Build with Cargo's release profile.
  --settings TAB  Open Settings on general, terminal, resources, or remote.
  -h, --help      Show this help.

Arguments after -- are passed to cargo build. Options that change Cargo's
target directory, target triple, or profile are not supported; set
CARGO_TARGET_DIR or use --release instead.
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --release)
            profile="release"
            cargo_args+=("$1")
            shift
            ;;
        --settings)
            if [[ $# -lt 2 ]]; then
                echo "error: --settings requires a tab" >&2
                usage >&2
                exit 2
            fi
            settings_preview="$2"
            shift 2
            ;;
        --settings=*)
            settings_preview="${1#*=}"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            cargo_args+=("$@")
            break
            ;;
        *)
            echo "error: unknown option: $1 (put cargo build arguments after --)" >&2
            usage >&2
            exit 2
            ;;
    esac
done

case "${settings_preview}" in
    ""|general|terminal|resources|remote) ;;
    *)
        echo "error: unknown Settings tab: ${settings_preview}" >&2
        exit 2
        ;;
esac

if (( ${#cargo_args[@]} > 0 )); then
    for argument in "${cargo_args[@]}"; do
        case "${argument}" in
            --target|--target=*|--target-dir|--target-dir=*|--profile|--profile=*)
                echo "error: ${argument} changes where the app binary is written" >&2
                exit 2
                ;;
        esac
    done
fi

branch="$(git -C "${workspace_dir}" symbolic-ref --quiet --short HEAD || true)"
branch="${branch:-detached}"
short_sha="$(git -C "${workspace_dir}" rev-parse --short=8 HEAD)"
dirty=""
if ! git -C "${workspace_dir}" diff --quiet --ignore-submodules -- \
    || ! git -C "${workspace_dir}" diff --cached --quiet --ignore-submodules --; then
    dirty="+dirty"
fi
build_label="${branch}@${short_sha}${dirty}"
bundle_id="com.dirijor.diri.dev.${short_sha}"
display_name="diri dev ${short_sha}"

mkdir -p "${target_dir}"

cd "${workspace_dir}"
echo "==> Building ${display_name} (${profile})"
# diri-app does not pull dirijord-rs into target/<profile>/; build both so a
# clean checkout launches against this tree's Engine instead of a missing or
# stale binary (installed app / leftover debug build).
if (( ${#cargo_args[@]} > 0 )); then
    cargo build --package diri-app --bin diri --package diri-engine --bin dirijord-rs "${cargo_args[@]}"
else
    cargo build --package diri-app --bin diri --package diri-engine --bin dirijord-rs
fi

binary="${target_dir}/${profile}/diri"
if [[ ! -x "${binary}" ]]; then
    echo "error: cargo did not produce ${binary}" >&2
    exit 1
fi

engine_bin="${target_dir}/${profile}/dirijord-rs"
if [[ ! -x "${engine_bin}" ]]; then
    echo "error: cargo did not produce ${engine_bin}" >&2
    exit 1
fi

# Every invocation gets a fresh bundle. Replacing a bundle beneath a still-
# running process invalidates its code signature, which is especially easy to
# do when judging two builds side by side.
bundle_root="$(mktemp -d "${target_dir}/diri-dev-${short_sha}.XXXXXX")"
app_path="${bundle_root}/${display_name}.app"
contents="${app_path}/Contents"
mkdir -p "${contents}/MacOS" "${contents}/Resources"
cp "${binary}" "${contents}/MacOS/diri"
cp "${workspace_dir}/assets/dev-icon.icns" "${contents}/Resources/dev-icon.icns"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "${workspace_dir}/crates/diri-app/Cargo.toml" | head -1)"
cat > "${contents}/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleDisplayName</key><string>${display_name}</string>
    <key>CFBundleExecutable</key><string>diri</string>
    <key>CFBundleIconFile</key><string>dev-icon.icns</string>
    <key>CFBundleIdentifier</key><string>${bundle_id}</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>${display_name}</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>${version}</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>LSMinimumSystemVersion</key><string>15.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
</dict>
</plist>
PLIST

# A bundle assembled after Cargo's link step must be sealed as a unit. Ad-hoc
# signing gives it coherent bundle metadata without ever resembling a release.
codesign --force --sign - \
    --entitlements "${workspace_dir}/assets/diri.entitlements" \
    --identifier "${bundle_id}" \
    "${app_path}"
codesign --verify --deep --strict "${app_path}"

launch_environment=("DIRI_DEV=1" "DIRI_DEV_BUILD=${build_label}")
if [[ -n "${settings_preview}" ]]; then
    launch_environment+=("DIRI_SETTINGS_PREVIEW=${settings_preview}")
fi

# The Rust app fail-closes unless Hello reports engineKind=diri-rust-engine.
# Prefer a local cargo build of dirijord-rs, then the packaged Rust Engine in
# an installed bundle. Never point at the retired legacy daemon — the client
# rejects it and the UI comes up unable to spawn or list sessions.
if [[ -z "${DIRIJORD_PATH:-}" ]]; then
    for candidate in \
        "${target_dir}/${profile}/dirijord-rs" \
        "${target_dir}/debug/dirijord-rs" \
        "${target_dir}/release/dirijord-rs" \
        "${HOME}/Applications/diri.app/Contents/Resources/bin/dirijord-rs" \
        "/Applications/diri.app/Contents/Resources/bin/dirijord-rs"
    do
        if [[ -x "${candidate}" ]]; then
            launch_environment+=("DIRIJORD_PATH=${candidate}")
            break
        fi
    done
fi

echo "==> Launching ${display_name} (${build_label})"
echo "    ${app_path}"
if [[ -n "${DIRIJORD_PATH:-}" ]]; then
    echo "    engine: ${DIRIJORD_PATH}"
fi
for item in "${launch_environment[@]}"; do
    if [[ "${item}" == DIRIJORD_PATH=* ]]; then
        echo "    engine: ${item#DIRIJORD_PATH=}"
    fi
done
exec env \
    -u DIRIJOR_SOCKET \
    -u DIRIJOR_SESSION_ID \
    -u DIRIJOR_CLI \
    -u NO_COLOR \
    "${launch_environment[@]}" \
    "${contents}/MacOS/diri"
