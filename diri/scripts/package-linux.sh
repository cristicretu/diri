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

echo "==> Building Linux release binaries"
cargo build --locked --release --package diri-app --bin diri
cargo build --locked --release --package dirijor-mcp --bin dirijor --bin dirijor-mcp
cargo build --locked --release --package diri-engine \
    --bin dirijord-rs --bin diri-holder --bin diri-ssh-askpass
cargo build --locked --release --package diri-remote --bin diri-remote

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
        --commit "${source_commit}"
fi

echo "==> Linux artifacts are in ${dist_dir}"
