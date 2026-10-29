#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_dir="$(cd "${script_dir}/.." && pwd)"
dist_dir="${DIRI_DIST_DIR:-${workspace_dir}/dist}"
app_path="${dist_dir}/diri.app"
# The updater compares against CARGO_PKG_VERSION, so artifact names have to come
# from the same place rather than a hand-passed number that can drift from it.
cargo_version="$(sed -n 's/^version = "\(.*\)"/\1/p' "${workspace_dir}/crates/diri-app/Cargo.toml" | head -1)"
version="${DIRI_VERSION:-${cargo_version:-0.1.0}}"
dmg_path="${dist_dir}/diri-${version}-universal.dmg"
# Update artifact: a zip of the stapled bundle, which is what diri's updater
# downloads. See crates/diri-updater/src/install.rs.
zip_path="${dist_dir}/diri-${version}-universal.zip"
entitlements="${workspace_dir}/assets/diri.entitlements"
# NEVER default to /tmp/diri-shared-target: that cache is shared with agent
# worktrees and cross-workspace fingerprint collisions produce Franken-builds
# (stale crates from other checkouts linked into the shipped app).
target_dir="${CARGO_TARGET_DIR:-${workspace_dir}/target}"
universal_dir="${target_dir}/universal-apple-darwin/release"
universal_binary="${universal_dir}/diri"
universal_cli_binary="${universal_dir}/dirijor"
universal_mcp_binary="${universal_dir}/dirijor-mcp"
universal_engine_binary="${universal_dir}/dirijord-rs"
universal_holder_binary="${universal_dir}/diri-holder"
universal_askpass_binary="${universal_dir}/diri-ssh-askpass"
universal_wake_helper_binary="${universal_dir}/diri-wake-helper"

# Toolchain location. The migration-era toolchain lived in /tmp, which macOS
# sweeps -- a reboot deleted it mid-project and releases could not be built at
# all until it was reinstalled. Prefer the persistent home install and fall back
# to /tmp only if that is where this machine still keeps it.
if [[ -x "${HOME}/.cargo/bin/cargo" ]]; then
    export CARGO_HOME="${CARGO_HOME:-${HOME}/.cargo}"
    export RUSTUP_HOME="${RUSTUP_HOME:-${HOME}/.rustup}"
else
    export CARGO_HOME="${CARGO_HOME:-/tmp/diri-cargo-home}"
    export RUSTUP_HOME="${RUSTUP_HOME:-/tmp/diri-rustup-home}"
fi
export PATH="${CARGO_HOME}/bin:${PATH}"
if ! command -v cargo >/dev/null 2>&1; then
    echo "error: no cargo on PATH (looked in ${CARGO_HOME}/bin)" >&2
    exit 1
fi
export CARGO_TARGET_DIR="${target_dir}"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-15.0}"
export CLANG_MODULE_CACHE_PATH="${CLANG_MODULE_CACHE_PATH:-${target_dir}/clang-module-cache}"

if ! command -v cargo-packager >/dev/null 2>&1; then
    echo "error: cargo-packager is missing; install it with 'cargo install cargo-packager --locked'" >&2
    exit 1
fi


# Xcode 27's lipo rejects `-verify_arch arm64 x86_64` ("requires exactly one
# input file"); one architecture per invocation works on old and new lipo.
verify_universal() {
    lipo "$1" -verify_arch arm64
    lipo "$1" -verify_arch x86_64
}

cd "${workspace_dir}"

# The remote Helper catalog shares nothing with the macOS products, so it builds
# alongside them in its own target dir (one target dir admits one cargo at a
# time) and is copied into the bundle once cargo-packager has made it.
remote_helpers_stage="${target_dir}/remote-helpers-stage"
remote_helpers_log="${target_dir}/remote-helpers.log"
rm -rf "${remote_helpers_stage}"
mkdir -p "${target_dir}"
echo "==> Building three-platform Rust remote Helper catalog (in the background)"
CARGO_TARGET_DIR="${target_dir}/remote-helpers-build" \
DIRI_SIGN_IDENTITY="${DIRI_SIGN_IDENTITY:-}" \
    "${script_dir}/build-remote-helpers.sh" "${remote_helpers_stage}" \
    > "${remote_helpers_log}" 2>&1 &
remote_helpers_pid=$!
trap 'kill "${remote_helpers_pid}" 2>/dev/null || true' EXIT

# One cargo call per package, each building both slices at once so cargo can
# schedule them in parallel and share host build scripts and proc macros. The
# packages stay in separate calls on purpose: a joint build unifies dependency
# features across them, which would change the shipped Engine and CLI (serde_json
# preserve_order, for one) relative to building each alone.
mac_targets=(--target aarch64-apple-darwin --target x86_64-apple-darwin)
# DIRI_TERMINAL_CORE=v2 builds the app and Engine on terminal core v2
# (docs/core-v2.md); the default, v1, is the current core. Only the packages
# that depend on diri-terminal-state take the feature.
core_features=()
case "${DIRI_TERMINAL_CORE:-v1}" in
    v1) ;;
    v2) core_features=(--features diri-terminal-state/core-v2-screen) ;;
    *) echo "error: DIRI_TERMINAL_CORE must be v1 or v2" >&2; exit 1 ;;
esac
echo "==> Building diri (Apple silicon + Intel)"
cargo build --release --package diri-app --bin diri ${core_features[@]+"${core_features[@]}"} "${mac_targets[@]}"
cargo build --release --package dirijor-mcp --bin dirijor --bin dirijor-mcp "${mac_targets[@]}"

echo "==> Creating universal executable"
mkdir -p "${universal_dir}" "${dist_dir}"
lipo -create \
    "${target_dir}/aarch64-apple-darwin/release/diri" \
    "${target_dir}/x86_64-apple-darwin/release/diri" \
    -output "${universal_binary}"
verify_universal "${universal_binary}"
lipo -create \
    "${target_dir}/aarch64-apple-darwin/release/dirijor" \
    "${target_dir}/x86_64-apple-darwin/release/dirijor" \
    -output "${universal_cli_binary}"
verify_universal "${universal_cli_binary}"
lipo -create \
    "${target_dir}/aarch64-apple-darwin/release/dirijor-mcp" \
    "${target_dir}/x86_64-apple-darwin/release/dirijor-mcp" \
    -output "${universal_mcp_binary}"
verify_universal "${universal_mcp_binary}"

echo "==> Assembling ${app_path} with cargo-packager"
cargo packager \
    --release \
    --packages diri-app \
    --formats app \
    --target universal-apple-darwin \
    --binaries-dir "${universal_dir}" \
    --out-dir "${dist_dir}"

# cargo-packager only knows the legacy .icns. The compiled Icon Composer asset
# catalog is what macOS 26+ draws; without it the system re-skins icon.icns
# into a generic glass tile. Info.plist names it via CFBundleIconName.
cp "${workspace_dir}/assets/Assets.car" "${app_path}/Contents/Resources/Assets.car"

# Ship the same reviewed dependency disclosure that CI validates. The JSON is
# also attached to GitHub Releases so users can inspect it without mounting the
# app bundle.
third_party_inventory="${dist_dir}/THIRD-PARTY-LICENSES.json"
echo "==> Generating third-party license inventory"
python3 "${workspace_dir}/../scripts/check-licenses.py" --output "${third_party_inventory}"
license_dir="${app_path}/Contents/Resources/licenses"
mkdir -p "${license_dir}"
cp "${workspace_dir}/../LICENSE" "${license_dir}/AGPL-3.0.txt"
cp "${workspace_dir}/../NOTICE" "${license_dir}/NOTICE.txt"
cp "${workspace_dir}/../scripts/license-policy.json" "${license_dir}/license-policy.json"
cp "${workspace_dir}/../docs/third-party/"*.txt "${license_dir}/"
cp "${third_party_inventory}" "${license_dir}/THIRD-PARTY-LICENSES.json"

app_bin_dir="${app_path}/Contents/Resources/bin"
echo "==> Bundling Rust CLI and MCP frontend into Resources/bin"
mkdir -p "${app_bin_dir}"
cp "${universal_cli_binary}" "${app_bin_dir}/dirijor"
cp "${universal_mcp_binary}" "${app_bin_dir}/dirijor-mcp"
# The Rust Engine is the authoritative daemon launched by diri. The remote
# Helper catalog below is consumed by this executable directly.
echo "==> Building the authoritative Rust Engine (universal)"
cargo build --release --package diri-engine --bin dirijord-rs --bin diri-holder \
    --bin diri-ssh-askpass --bin diri-wake-helper ${core_features[@]+"${core_features[@]}"} "${mac_targets[@]}"
lipo -create \
    "${target_dir}/aarch64-apple-darwin/release/dirijord-rs" \
    "${target_dir}/x86_64-apple-darwin/release/dirijord-rs" \
    -output "${universal_engine_binary}"
lipo -create \
    "${target_dir}/aarch64-apple-darwin/release/diri-holder" \
    "${target_dir}/x86_64-apple-darwin/release/diri-holder" \
    -output "${universal_holder_binary}"
lipo -create \
    "${target_dir}/aarch64-apple-darwin/release/diri-ssh-askpass" \
    "${target_dir}/x86_64-apple-darwin/release/diri-ssh-askpass" \
    -output "${universal_askpass_binary}"
verify_universal "${universal_engine_binary}"
verify_universal "${universal_holder_binary}"
lipo -create \
    "${target_dir}/aarch64-apple-darwin/release/diri-wake-helper" \
    "${target_dir}/x86_64-apple-darwin/release/diri-wake-helper" \
    -output "${universal_wake_helper_binary}"
verify_universal "${universal_askpass_binary}"
verify_universal "${universal_wake_helper_binary}"
cp "${universal_engine_binary}" "${app_bin_dir}/dirijord-rs"
cp "${universal_holder_binary}" "${app_bin_dir}/diri-holder"
cp "${universal_askpass_binary}" "${app_bin_dir}/diri-ssh-askpass"
cp "${universal_wake_helper_binary}" "${app_bin_dir}/diri-wake-helper"
# "Talk with your agent": on-device digest and speech. Built against Xcode 26's
# SDK, weak-linked so it still launches on macOS 15.
echo "==> Building the voice helper (universal)"
"${workspace_dir}/scripts/build-voice-helper.sh" "${app_bin_dir}/diri-voice"
verify_universal "${app_bin_dir}/diri-voice"
# Kokoro's runtime is downloaded with its model, not bundled
# (scripts/publish-onnxruntime.sh); its notices still ship with the app.
if [[ -e "${app_bin_dir}/libonnxruntime.1.dylib" ]]; then
    echo "package: ONNX Runtime must not be bundled" >&2
    exit 1
fi
ort_dir="${CARGO_TARGET_DIR:-${workspace_dir}/target}/onnxruntime-osx-arm64-1.31.0"
cp "${ort_dir}/LICENSE" "${license_dir}/ONNXRuntime-MIT.txt"
cp "${ort_dir}/ThirdPartyNotices.txt" "${license_dir}/ONNXRuntime-ThirdPartyNotices.txt"
# The wake helper's launchd plist, registered at runtime through
# SMAppService.daemonServiceWithPlistName and approved once by an admin.
launch_daemons_dir="${app_path}/Contents/Library/LaunchDaemons"
mkdir -p "${launch_daemons_dir}"
cp "${workspace_dir}/assets/com.dirijor.diri.wake.plist" "${launch_daemons_dir}/"
plutil -lint "${launch_daemons_dir}/com.dirijor.diri.wake.plist" >/dev/null

# The default SSH transport bootstraps one exact Rust Helper artifact selected
# by remote OS/architecture. This build is independent of all daemon products
# above and emits a versioned manifest verified again before upload.
echo "==> Waiting for the remote Helper catalog"
if ! wait "${remote_helpers_pid}"; then
    echo "error: remote Helper build failed:" >&2
    sed 's/^/    /' "${remote_helpers_log}" >&2
    exit 1
fi
trap - EXIT
tail -n 1 "${remote_helpers_log}"
remote_helpers_dir="${app_bin_dir}/remote-helpers"
rm -rf "${remote_helpers_dir}"
# -p keeps the owner-only 0700 modes the manifest was measured with.
cp -Rp "${remote_helpers_stage}" "${remote_helpers_dir}"

# Rust-owned Agent catalog used by local and remote session orchestration.
rm -rf "${app_bin_dir}/manifests"
cp -R "${workspace_dir}/crates/diri-engine/manifests" "${app_bin_dir}/manifests"
# Count, not just presence. A catalog that is merely SMALLER never errors at
# runtime: each missing manifest silently downgrades that agent to a bare login
# shell, which looks like a working session. That shipped once. The source
# directory is the reference, so any shrink between it and the bundle is a
# packaging bug worth failing the release over.
source_manifests="$(find "${workspace_dir}/crates/diri-engine/manifests" -name '*.json' -type f | wc -l | tr -d ' ')"
bundled_manifests="$(find "${app_bin_dir}/manifests" -name '*.json' -type f | wc -l | tr -d ' ')"
if [[ ! -f "${app_bin_dir}/manifests/codex.json" || "${bundled_manifests}" != "${source_manifests}" || "${bundled_manifests}" -lt 20 ]]; then
    echo "error: bundled Agent catalog is incomplete: ${bundled_manifests} manifest(s) bundled, ${source_manifests} in source (expected at least 20)" >&2
    exit 1
fi
echo "==> Bundled ${bundled_manifests} Agent manifests"

# Inside-out signing: sign the nested daemon binaries FIRST (their own hardened
# runtime + timestamp), then the app LAST WITHOUT --deep. A --deep sign would
# re-stamp the nested executables with the app's identifier and can fail
# notarization; nested Mach-O must be signed independently.
# Auto-detect a Developer ID when none is given, exactly as release.sh does.
# TCC keys privacy grants (Documents/Desktop access prompts) to the signing
# identity: ad-hoc changes every rebuild, so each dev install used to re-ask —
# a stable Developer ID makes one "Allow" stick across every future install.
if [[ -z "${DIRI_SIGN_IDENTITY:-}" ]]; then
    DIRI_SIGN_IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
        | grep "Developer ID Application" | head -1 \
        | sed -E 's/.*"(.*)".*/\1/' || true)"
fi
sign_id="${DIRI_SIGN_IDENTITY:--}"
ts_flag=(--timestamp)
[[ "${sign_id}" == "-" ]] && ts_flag=(--timestamp=none) && echo "==> No signing identity found; ad-hoc signature"
[[ "${sign_id}" != "-" ]] && echo "==> Signing with: ${sign_id}"
echo "==> Signing nested executables"
codesign --force --options runtime "${ts_flag[@]}" --sign "${sign_id}" "${app_bin_dir}/dirijor"
codesign --force --options runtime "${ts_flag[@]}" --sign "${sign_id}" "${app_bin_dir}/dirijor-mcp"
codesign --force --options runtime "${ts_flag[@]}" --sign "${sign_id}" "${app_bin_dir}/dirijord-rs"
codesign --force --options runtime "${ts_flag[@]}" --sign "${sign_id}" "${app_bin_dir}/diri-holder"
codesign --force --options runtime "${ts_flag[@]}" --sign "${sign_id}" "${app_bin_dir}/diri-ssh-askpass"
# The hardened runtime blocks the microphone without this entitlement.
codesign --force --options runtime "${ts_flag[@]}" \
    --entitlements "${workspace_dir}/assets/diri-voice.entitlements" \
    --sign "${sign_id}" "${app_bin_dir}/diri-voice"
codesign --force --options runtime "${ts_flag[@]}" --identifier com.dirijor.diri.wake \
    --sign "${sign_id}" "${app_bin_dir}/diri-wake-helper"
# The Apple remote Helper is deliberately NOT signed here. Signing rewrites the
# Mach-O, and its length and digest are already recorded in the catalog manifest
# that the Engine verifies before upload; signing after the fact invalidates the
# manifest and the Engine rejects the entire catalog, disabling every remote
# host. build-remote-helpers.sh signs it before measuring instead.
echo "==> Signing ${app_path}"
codesign --force --options runtime "${ts_flag[@]}" \
    --entitlements "${entitlements}" \
    --identifier com.dirijor.diri \
    --sign "${sign_id}" \
    "${app_path}"

codesign --verify --deep --strict "${app_path}"

notary_profile="${APPLE_NOTARIZATION_KEYCHAIN_PROFILE:-${APPLE_KEYCHAIN_PROFILE:-${NOTARY_PROFILE:-}}}"
notary_apple_id="${APPLE_NOTARIZATION_APPLE_ID:-${APPLE_ID:-}}"
notary_password="${APPLE_NOTARIZATION_PASSWORD:-${APPLE_PASSWORD:-}}"
notary_team_id="${APPLE_NOTARIZATION_TEAM_ID:-${APPLE_TEAM_ID:-}}"
notary_requested=0
if [[ -n "${notary_profile}" || -n "${notary_apple_id}" || -n "${notary_password}" || -n "${notary_team_id}" ]]; then
    notary_requested=1
fi

# One place that knows how to talk to notarytool, called for the app zip and
# again for the DMG.
submit_for_notarization() {
    local artifact="$1"
    if [[ -n "${notary_profile}" ]]; then
        xcrun notarytool submit "${artifact}" --keychain-profile "${notary_profile}" --wait
    elif [[ -n "${notary_apple_id}" && -n "${notary_password}" && -n "${notary_team_id}" ]]; then
        xcrun notarytool submit "${artifact}" \
            --apple-id "${notary_apple_id}" \
            --password "${notary_password}" \
            --team-id "${notary_team_id}" \
            --wait
    else
        echo "error: set a keychain profile or all APPLE_NOTARIZATION_{APPLE_ID,PASSWORD,TEAM_ID} values" >&2
        exit 1
    fi
}

if [[ "${notary_requested}" == "1" ]]; then
    if [[ -z "${DIRI_SIGN_IDENTITY:-}" ]]; then
        echo "error: notarization requires DIRI_SIGN_IDENTITY" >&2
        exit 1
    fi

    # With a DMG, only the DMG is submitted (below): Apple tickets every
    # nested item, so one round-trip covers both and the app is stapled from
    # that ticket. The update zip is made from the stapled app, because the
    # updater verifies downloads offline. The DMG's own copy of the app is
    # not stapled; Gatekeeper checks it online on first launch, while the DMG
    # itself carries a stapled ticket. Without a DMG the app goes alone.
    if [[ "${DIRI_CREATE_DMG:-0}" != "1" ]]; then
        echo "==> Notarizing ${app_path}"
        notarization_zip="$(mktemp -d "${TMPDIR:-/tmp}/diri-notarize.XXXXXX")/diri.zip"
        ditto -c -k --keepParent "${app_path}" "${notarization_zip}"
        submit_for_notarization "${notarization_zip}"
        rm -rf "$(dirname "${notarization_zip}")"
        xcrun stapler staple "${app_path}"
        xcrun stapler validate "${app_path}"
    fi
fi

if [[ "${DIRI_CREATE_DMG:-0}" == "1" ]]; then
    echo "==> Creating ${dmg_path}"
    dmg_stage="$(mktemp -d "${TMPDIR:-/tmp}/diri-dmg.XXXXXX")"
    cleanup_dmg_stage() {
        rm -rf "${dmg_stage}"
    }
    trap cleanup_dmg_stage EXIT
    ditto "${app_path}" "${dmg_stage}/diri.app"
    ln -s /Applications "${dmg_stage}/Applications"
    rm -f "${dmg_path}"
    hdiutil create -quiet -volname "diri" -srcfolder "${dmg_stage}" -ov -format UDZO "${dmg_path}"
    if [[ -n "${DIRI_SIGN_IDENTITY:-}" ]]; then
        codesign --force --timestamp --sign "${DIRI_SIGN_IDENTITY}" "${dmg_path}"
    fi
fi

if [[ "${DIRI_CREATE_DMG:-0}" == "1" && "${notary_requested}" == "1" ]]; then
    echo "==> Notarizing ${dmg_path} (covers the app inside it)"
    submit_for_notarization "${dmg_path}"
    xcrun stapler staple "${dmg_path}"
    xcrun stapler validate "${dmg_path}"
    xcrun stapler staple "${app_path}"
    xcrun stapler validate "${app_path}"
fi

if [[ "${DIRI_CREATE_ZIP:-0}" == "1" || "${notary_requested}" == "1" ]]; then
    echo "==> Creating ${zip_path}"
    rm -f "${zip_path}"
    # --keepParent puts diri.app at the archive root, which is the layout the
    # updater's unpack step requires.
    ditto -c -k --keepParent "${app_path}" "${zip_path}"
fi

bundle_size="$(du -sh "${app_path}" | awk '{print $1}')"
echo "Built ${app_path} (${bundle_size})"
if [[ "${DIRI_CREATE_DMG:-0}" == "1" ]]; then
    echo "Built ${dmg_path}"
fi
if [[ -f "${zip_path}" ]]; then
    echo "Built ${zip_path}"
fi
