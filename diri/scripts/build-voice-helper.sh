#!/usr/bin/env bash
# Builds native/diri-voice into a universal `diri-voice` binary.
#
#   scripts/build-voice-helper.sh <output-path>
#
# The helper needs the macOS 26 SDK (Xcode 26) for Foundation Models, but must
# still launch on macOS 15: FoundationModels is weak-linked and every use is
# behind `#available(macOS 26.0, *)`. Set DEVELOPER_DIR to pick an Xcode; when
# the selected SDK lacks FoundationModels the script exits 3 so callers can
# decide whether that is fatal (release) or fine (dev, plain-text fallback).
#
# Kokoro, the open-weights voice, runs on ONNX Runtime, which ships for Apple
# silicon only: the arm64 slice is built against its C headers and the x86_64
# slice without Kokoro (`#if KOKORO`). The runtime is not linked or bundled:
# the helper downloads Diri's signed copy with Kokoro's model and dlopens it
# (Kokoro.swift; scripts/publish-onnxruntime.sh publishes it). The pinned
# release is downloaded once into target/ for its headers and licenses.
set -euo pipefail

output="${1:?usage: build-voice-helper.sh <output-path>}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source_dir="${script_dir}/../native/diri-voice"
ort_version="1.31.0"
ort_sha256="1031b36dd3bdaa8d976a6130d9cf2a05e9be320b93ae804f8427b91dc3432afc"
ort_cache="${CARGO_TARGET_DIR:-${script_dir}/../target}"
ort_dir="${ort_cache}/onnxruntime-osx-arm64-${ort_version}"
ort_dylib="libonnxruntime.${ort_version}.dylib"
deployment="${MACOSX_DEPLOYMENT_TARGET:-15.0}"

has_foundation_models() {
    [[ -d "$(xcrun --sdk macosx --show-sdk-path 2>/dev/null)/System/Library/Frameworks/FoundationModels.framework" ]]
}
# The selected developer directory is often the Command Line Tools, whose SDK
# lags Xcode's. Without an explicit DEVELOPER_DIR, use any installed Xcode
# that has the framework.
if ! has_foundation_models && [[ -z "${DEVELOPER_DIR:-}" ]]; then
    while IFS= read -r xcode; do
        export DEVELOPER_DIR="${xcode}/Contents/Developer"
        has_foundation_models && break
        unset DEVELOPER_DIR
    done < <(mdfind "kMDItemCFBundleIdentifier == 'com.apple.dt.Xcode'" 2>/dev/null)
fi
if ! has_foundation_models; then
    echo "build-voice-helper: no macOS SDK with FoundationModels; install Xcode 26 or set DEVELOPER_DIR" >&2
    exit 3
fi
sdk="$(xcrun --sdk macosx --show-sdk-path)"

if [[ ! -f "${ort_dir}/lib/${ort_dylib}" ]]; then
    archive="${ort_cache}/onnxruntime-osx-arm64-${ort_version}.tgz"
    mkdir -p "${ort_cache}"
    curl -sSLf -o "${archive}" \
        "https://github.com/microsoft/onnxruntime/releases/download/v${ort_version}/onnxruntime-osx-arm64-${ort_version}.tgz"
    if [[ "$(shasum -a 256 "${archive}" | cut -d' ' -f1)" != "${ort_sha256}" ]]; then
        rm -f "${archive}"
        echo "build-voice-helper: ONNX Runtime archive does not match its pinned SHA-256" >&2
        exit 1
    fi
    tar -xzf "${archive}" -C "${ort_cache}"
fi

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT
for arch in arm64 x86_64; do
    kokoro=()
    if [[ "${arch}" == arm64 ]]; then
        kokoro=(
            -D KOKORO
            -import-objc-header "${source_dir}/onnxruntime-bridge.h"
            -Xcc -I"${ort_dir}/include"
        )
    fi
    xcrun --sdk macosx swiftc -O -parse-as-library -swift-version 5 \
        -target "${arch}-apple-macos${deployment}" \
        -sdk "${sdk}" \
        -Xlinker -weak_framework -Xlinker FoundationModels \
        ${kokoro[@]+"${kokoro[@]}"} \
        "${source_dir}"/*.swift -o "${work}/diri-voice-${arch}"
done
mkdir -p "$(dirname "${output}")"
lipo -create "${work}/diri-voice-arm64" "${work}/diri-voice-x86_64" -output "${output}"
# Loaded at runtime from the download, never at launch.
if otool -L "${output}" | grep -q onnxruntime; then
    echo "build-voice-helper: ONNX Runtime is linked; Kokoro must dlopen it" >&2
    exit 1
fi

# A strong link would stop the helper from loading at all on macOS 15.
if otool -l "${output}" | grep -A2 LC_LOAD_DYLIB | grep -q FoundationModels; then
    echo "build-voice-helper: FoundationModels is strongly linked" >&2
    exit 1
fi
echo "${output}"
