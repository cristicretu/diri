#!/usr/bin/env bash
# Regenerates every committed app-icon artifact from the Icon Composer sources.
#
#   assets/diri.icon      -> assets/Assets.car, assets/icon.icns, assets/icon.png
#   assets/diri-dev.icon  -> assets/dev-Assets.car, assets/dev-icon.icns
#
# Assets.car carries the layered icon macOS 26+ renders natively (light, dark,
# tinted, clear). Without it the system re-skins the legacy .icns into a
# generic glass tile. The .icns is the pre-26 fallback and icon.png feeds the
# Linux packages.
#
# The outputs are committed so package.sh, dev.sh and CI need no Xcode 26+;
# rerun this (with Xcode 26 or newer selected) after editing either .icon.
# Icon Composer opens them directly.
set -euo pipefail

workspace_dir="$(cd "$(dirname "$0")/.." && pwd)"
assets="${workspace_dir}/assets"
profiles=/System/Library/ColorSync/Profiles

ictool="$(xcode-select -p)/../Applications/Icon Composer.app/Contents/Executables/ictool"
if [[ ! -x "${ictool}" ]]; then
    echo "error: Icon Composer's ictool not found; select Xcode 26 or newer with xcode-select" >&2
    exit 1
fi

scratch="$(mktemp -d)"
trap 'rm -rf "${scratch}"' EXIT

# build <name> <car destination> <icns destination>
build() {
    local name="$1" source="${assets}/$1.icon" out="${scratch}/$1"
    mkdir -p "${out}/icon.iconset"

    # actool names the app icon after the .icon file, so CFBundleIconName is
    # "diri" for the release bundle and "diri-dev" for dev.sh bundles.
    xcrun actool "${source}" \
        --compile "${out}" \
        --platform macosx \
        --minimum-deployment-target 15.0 \
        --app-icon "${name}" \
        --output-partial-info-plist "${out}/partial.plist" \
        --errors --warnings >/dev/null
    cp "${out}/Assets.car" "$2"

    # actool's own .icns stops at 256px, too small for a Retina Dock on
    # macOS 15, so the fallback is assembled from Icon Composer renders. They
    # come out 16-bit; matching to Display P3 makes them 8-bit and keeps the
    # glow's gamut.
    local size scale suffix png
    for size in 16 32 128 256 512; do
        for scale in 1 2; do
            suffix=""
            [[ ${scale} == 2 ]] && suffix="@2x"
            png="${out}/icon.iconset/icon_${size}x${size}${suffix}.png"
            "${ictool}" "${source}" --export-image --output-file "${png}" --platform macOS \
                --rendition Default --width "${size}" --height "${size}" --scale "${scale}" >/dev/null
            sips --matchTo "${profiles}/Display P3.icc" "${png}" --out "${png}" >/dev/null
        done
    done
    iconutil -c icns "${out}/icon.iconset" -o "$3"
}

build diri "${assets}/Assets.car" "${assets}/icon.icns"
build diri-dev "${assets}/dev-Assets.car" "${assets}/dev-icon.icns"

# Linux desktops assume sRGB.
sips --matchTo "${profiles}/sRGB Profile.icc" "${scratch}/diri/icon.iconset/icon_512x512@2x.png" \
    --out "${assets}/icon.png" >/dev/null

echo "Regenerated icon artifacts in ${assets}"
