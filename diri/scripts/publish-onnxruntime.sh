#!/usr/bin/env bash
# Publishes the ONNX Runtime library Kokoro downloads on demand, once per
# ONNX Runtime version.
#
#   DIRI_SIGN_IDENTITY="Developer ID Application: …" scripts/publish-onnxruntime.sh [--upload]
#
# diri-voice runs with the hardened runtime, whose library validation only
# loads libraries signed by Diri's own team, so Microsoft's release cannot be
# downloaded as is. This signs its unmodified dylib with Diri's Developer ID
# and prints the SHA-256 and size to pin in KokoroFiles.runtime
# (native/diri-voice/Kokoro.swift). With --upload it also attaches the file
# to the `onnxruntime-<version>` GitHub release, which is never marked latest.
# A new signature changes the hash, and shipped builds pin the published
# one, so a version is uploaded once and never replaced.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ort_version="1.31.0"
ort_dir="${CARGO_TARGET_DIR:-${script_dir}/../target}/onnxruntime-osx-arm64-${ort_version}"
asset="libonnxruntime.${ort_version}.dylib"
tag="onnxruntime-${ort_version}"
repo="cristicretu/diri"
sign_id="${DIRI_SIGN_IDENTITY:?set DIRI_SIGN_IDENTITY to the Developer ID Application identity}"

if [[ ! -f "${ort_dir}/lib/${asset}" ]]; then
    # build-voice-helper.sh downloads and verifies the pinned release.
    "${script_dir}/build-voice-helper.sh" "$(mktemp -d)/diri-voice" >/dev/null
fi

out="${ort_dir}/signed"
mkdir -p "${out}"
cp "${ort_dir}/lib/${asset}" "${out}/${asset}"
lipo "${out}/${asset}" -verify_arch arm64
codesign --force --options runtime --timestamp --sign "${sign_id}" "${out}/${asset}"
codesign --verify --strict "${out}/${asset}"

sha256="$(shasum -a 256 "${out}/${asset}" | cut -d' ' -f1)"
size="$(stat -f %z "${out}/${asset}")"
echo "file:   ${out}/${asset}"
echo "sha256: ${sha256}"
echo "size:   ${size}"

if [[ "${1:-}" == "--upload" ]]; then
    if ! gh release view "${tag}" -R "${repo}" >/dev/null 2>&1; then
        gh release create "${tag}" -R "${repo}" --prerelease --latest=false \
            --title "ONNX Runtime ${ort_version} for Kokoro" \
            --notes "Microsoft's unmodified ONNX Runtime ${ort_version} (MIT) for Apple silicon, signed with Diri's Developer ID. Diri downloads it on demand for the Kokoro voice; not an app release."
    fi
    # Never replace it: shipped builds pin the published file's hash.
    gh release upload "${tag}" -R "${repo}" "${out}/${asset}"
fi
