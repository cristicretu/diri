#!/usr/bin/env bash

set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "error: Linux packages must be built natively on Linux" >&2
    exit 64
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_dir="$(cd "${script_dir}/.." && pwd)"
repository_dir="$(cd "${workspace_dir}/.." && pwd)"
dist_dir="${DIRI_DIST_DIR:-${workspace_dir}/dist/linux}"
target_dir="${CARGO_TARGET_DIR:-${workspace_dir}/target}"
cargo_version="$(sed -n 's/^version = "\(.*\)"/\1/p' "${workspace_dir}/crates/diri-app/Cargo.toml" | head -1)"
version="${DIRI_VERSION:-${cargo_version}}"
formats="${DIRI_LINUX_FORMATS:-appimage,deb}"
source_commit="${SOURCE_COMMIT:-$(git -C "${repository_dir}" rev-parse HEAD)}"

for tool in cargo cargo-packager npm python3; do
    if ! command -v "${tool}" >/dev/null 2>&1; then
        echo "error: ${tool} is required to package diri for Linux" >&2
        exit 1
    fi
done

mkdir -p "${dist_dir}"
cd "${workspace_dir}"

# Releases build the app, the Engine and the bundled Helper on terminal core
# v2 (docs/core-v2.md, step 8), as scripts/package.sh does on macOS;
# DIRI_TERMINAL_CORE=v1 rebuilds them on the previous core. Only the packages
# that depend on diri-terminal-state take the feature.
core_features=()
case "${DIRI_TERMINAL_CORE:-v2}" in
    v1) ;;
    v2) core_features=(--features diri-terminal-state/core-v2-screen) ;;
    *) echo "error: DIRI_TERMINAL_CORE must be v1 or v2" >&2; exit 1 ;;
esac
# A v2 Helper never shares a Build ID (and so an install path on a remote
# host) with a previous-core Helper of the same source: it is named after the
# source with a -core-v2 suffix, as scripts/build-remote-helpers.sh names it.
helper_build_id="${DIRI_REMOTE_BUILD_ID:-}"
if [[ "${#core_features[@]}" -gt 0 ]]; then
    if [[ -z "${helper_build_id}" ]]; then
        helper_build_id="${source_commit}"
        if [[ -n "$(git -C "${repository_dir}" status --porcelain --untracked-files=normal 2>/dev/null || true)" ]]; then
            helper_build_id="${helper_build_id}-dirty-$(date -u +%Y%m%d%H%M%S)"
        fi
    fi
    helper_build_id="${helper_build_id}-core-v2"
fi

echo "==> Building Linux release binaries"
cargo build --locked --release --package diri-app --bin diri ${core_features[@]+"${core_features[@]}"}
cargo build --locked --release --package dirijor-mcp --bin dirijor --bin dirijor-mcp
cargo build --locked --release --package diri-engine \
    --bin dirijord-rs --bin diri-holder --bin diri-ssh-askpass ${core_features[@]+"${core_features[@]}"}
if [[ -n "${helper_build_id}" ]]; then
    DIRI_REMOTE_BUILD_ID="${helper_build_id}" \
        cargo build --locked --release --package diri-remote --bin diri-remote ${core_features[@]+"${core_features[@]}"}
else
    cargo build --locked --release --package diri-remote --bin diri-remote
fi

echo "==> Installing reviewed browser-sidecar dependencies"
npm ci --omit=dev --prefix "${repository_dir}/sidecar"

license_inventory="${dist_dir}/THIRD-PARTY-LICENSES.json"
echo "==> Generating third-party license inventory"
python3 "${repository_dir}/scripts/check-licenses.py" --output "${license_inventory}"

packager_config="$(python3 "${script_dir}/linux-packager-config.py" \
    --workspace "${workspace_dir}" \
    --binaries "${target_dir}/release" \
    --output "${dist_dir}" \
    --version "${version}" \
    --license-inventory "${license_inventory}")"

echo "==> Creating ${formats} packages"
cargo-packager --config "${packager_config}" --formats "${formats}"

if [[ "${formats}" == *appimage* && "${formats}" == *deb* ]]; then
    python3 "${script_dir}/write-linux-release-manifest.py" \
        --directory "${dist_dir}" \
        --version "${version}" \
        --architecture "$(uname -m)" \
        --commit "${source_commit}"
fi

echo "==> Linux artifacts are in ${dist_dir}"
