#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_dir="$(cd "${script_dir}/.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-${workspace_dir}/target}"
output_dir="${1:-${target_dir}/remote-helpers}"
protocol_major=1

default_targets="x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin"
read -r -a targets <<< "${DIRI_REMOTE_TARGETS:-${default_targets}}"

git_revision="$(git -C "${workspace_dir}" rev-parse HEAD 2>/dev/null || printf 'source')"
build_id="${DIRI_REMOTE_BUILD_ID:-${git_revision}}"
if [[ -n "$(git -C "${workspace_dir}" status --porcelain --untracked-files=normal 2>/dev/null || true)" && -z "${DIRI_REMOTE_BUILD_ID:-}" ]]; then
    build_id="${build_id}-dirty-$(date -u +%Y%m%d%H%M%S)"
fi
if [[ ! "${build_id}" =~ ^[A-Za-z0-9._-]{1,128}$ ]]; then
    echo "error: DIRI_REMOTE_BUILD_ID must be 1..128 ASCII letters, digits, '.', '_' or '-'" >&2
    exit 1
fi

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

file_length() {
    if stat -f '%z' "$1" >/dev/null 2>&1; then
        stat -f '%z' "$1"
    else
        stat -c '%s' "$1"
    fi
}

build_target() {
    local target="$1"
    case "${target}" in
        x86_64-unknown-linux-musl|aarch64-unknown-linux-musl)
            if [[ "$(uname -s)" == "Linux" ]]; then
                cargo build --locked --release --package diri-remote --target "${target}"
            elif command -v cargo-zigbuild >/dev/null 2>&1; then
                cargo zigbuild --locked --release --package diri-remote --target "${target}"
            else
                echo "error: cross-building ${target} requires cargo-zigbuild on this host" >&2
                exit 1
            fi
            ;;
        aarch64-apple-darwin)
            if [[ "$(uname -s)" != "Darwin" ]]; then
                echo "error: Apple Helper target ${target} requires a macOS release builder" >&2
                exit 1
            fi
            cargo build --locked --release --package diri-remote --target "${target}"
            ;;
        *)
            echo "error: unsupported Helper target ${target}" >&2
            exit 1
            ;;
    esac
}

cd "${workspace_dir}"
mkdir -p "${output_dir}/artifacts"
manifest_tmp="${output_dir}/.manifest.$$.tmp"
trap 'rm -f "${manifest_tmp}"' EXIT

printf '{\n  "protocolMajor": %s,\n  "buildId": "%s",\n  "artifacts": [\n' \
    "${protocol_major}" "${build_id}" > "${manifest_tmp}"

first=1
for target in "${targets[@]}"; do
    echo "==> Building diri-remote ${target} (${build_id})"
    DIRI_REMOTE_BUILD_ID="${build_id}" build_target "${target}"
    source_binary="${target_dir}/${target}/release/diri-remote"
    if [[ ! -x "${source_binary}" ]]; then
        echo "error: Helper build did not produce ${source_binary}" >&2
        exit 1
    fi
    artifact_dir="${output_dir}/artifacts/${target}"
    mkdir -p "${artifact_dir}"
    artifact="${artifact_dir}/diri-remote"
    cp "${source_binary}" "${artifact}"
    chmod 700 "${artifact}"
    # Sign BEFORE measuring. A signature is written into the Mach-O, so signing
    # an artifact after its length and digest are recorded invalidates the very
    # manifest that vouches for it, and the Engine then rejects the whole
    # catalog — every remote host, not just this one — with
    # "artifact length is N; expected M". Only Apple targets are signable;
    # the musl binaries are plain ELF and are measured as built.
    case "${target}" in
        *-apple-darwin)
            # A secure timestamp is required for notarization: without it Apple
            # rejects the whole archive with "The signature does not include a
            # secure timestamp." Ad-hoc signatures cannot carry one, so they
            # opt out explicitly — those builds are not notarized anyway.
            if [[ -n "${DIRI_SIGN_IDENTITY:-}" && "${DIRI_SIGN_IDENTITY}" != "-" ]]; then
                codesign --force --options runtime --timestamp \
                    --sign "${DIRI_SIGN_IDENTITY}" "${artifact}"
            else
                codesign --force --timestamp=none --sign - "${artifact}"
            fi
            ;;
    esac
    length="$(file_length "${artifact}")"
    sha256="$(sha256_file "${artifact}")"
    if [[ "${first}" == 0 ]]; then
        printf ',\n' >> "${manifest_tmp}"
    fi
    first=0
    printf '    {"target":"%s","path":"artifacts/%s/diri-remote","length":%s,"sha256":"%s"}' \
        "${target}" "${target}" "${length}" "${sha256}" >> "${manifest_tmp}"
done
printf '\n  ]\n}\n' >> "${manifest_tmp}"
mv "${manifest_tmp}" "${output_dir}/manifest.json"
trap - EXIT

echo "Built ${#targets[@]} versioned remote Helper artifacts in ${output_dir}"
