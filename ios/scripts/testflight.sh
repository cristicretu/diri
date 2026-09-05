#!/bin/bash
# Local preparation only: never uploads a build or invites testers.
set -euo pipefail
cd "$(dirname "$0")/.."

fail() { echo "error: $*" >&2; exit 1; }
mode="${1:-check}"
case "$mode" in check|archive|export) ;; *) fail "Usage: bash scripts/testflight.sh [check|archive|export]" ;; esac
command -v xcodegen >/dev/null || fail "Install XcodeGen first (see README.md)."
sdk_version="$(xcrun --sdk iphoneos --show-sdk-version)"
[[ "${sdk_version%%.*}" -ge 26 ]] || fail "Select Xcode 26 or later. App Store Connect requires iOS SDK 26+."

plutil -lint DiriPhone/PrivacyInfo.xcprivacy ExportOptions.plist
icon="DiriPhone/Assets.xcassets/AppIcon.appiconset/AppIcon.png"
[[ -f "$icon" ]] || fail "Missing app icon. Run: xcrun swift scripts/render-app-icon.swift"
icon_info="$(sips -g pixelWidth -g pixelHeight -g hasAlpha "$icon")"
[[ "$icon_info" == *"pixelWidth: 1024"* && "$icon_info" == *"pixelHeight: 1024"* && "$icon_info" == *"hasAlpha: no"* ]] || fail "The app icon must be 1024×1024 with no alpha channel."
xcodegen generate
plutil -lint DiriPhone/Info.plist

if [[ "$mode" == check ]]; then
    echo "Release inputs OK (iOS SDK $sdk_version). Signing, export compliance and real-device checks are still required; see TESTFLIGHT.md."
    exit 0
fi

team="${DIRI_APPLE_TEAM_ID:-}"
build="${DIRI_BUILD_NUMBER:-}"
[[ "$team" =~ ^[A-Z0-9]{10}$ ]] || fail "Set DIRI_APPLE_TEAM_ID to your 10-character Apple Developer Team ID."
[[ "$build" =~ ^[1-9][0-9]{0,3}$ ]] || fail "Set DIRI_BUILD_NUMBER to a new sequential build number (1–9999)."
archive="build/testflight/DiriPhone-${build}.xcarchive"
export_dir="build/testflight/export-${build}"
set --
if [[ "${DIRI_ALLOW_PROVISIONING_UPDATES:-0}" == 1 ]]; then
    set -- -allowProvisioningUpdates
fi

if [[ "$mode" == archive ]]; then
    [[ ! -e "$archive" ]] || fail "$archive already exists. Choose a new build number; existing archives are never overwritten."
    xcodebuild -project DiriPhone.xcodeproj -scheme DiriPhone -configuration Release \
        -destination 'generic/platform=iOS' -archivePath "$archive" \
        -derivedDataPath build/testflight/DerivedData \
        CODE_SIGNING_ALLOWED=YES DEVELOPMENT_TEAM="$team" CURRENT_PROJECT_VERSION="$build" \
        "$@" archive
    echo "Archive ready: $archive. Open it in Xcode Organizer to validate and upload when approved."
else
    [[ -d "$archive" ]] || fail "Archive missing. Run archive first with the same build number."
    [[ ! -e "$export_dir" ]] || fail "$export_dir already exists; preserve it and use a new build number."
    archive_team="$(/usr/libexec/PlistBuddy -c 'Print :ApplicationProperties:Team' "$archive/Info.plist")"
    [[ "$archive_team" == "$team" ]] || fail "Archive belongs to a different Apple Developer team."
    xcodebuild -exportArchive -archivePath "$archive" -exportOptionsPlist ExportOptions.plist \
        -exportPath "$export_dir" "$@"
    echo "IPA exported to $export_dir. Nothing was uploaded."
fi
