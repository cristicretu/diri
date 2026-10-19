#!/usr/bin/env bash
# Screenshots the onboarding the computer-use clip's agents test, one PNG per
# step, on real iPhone Simulators: an SE, a 17 Pro Max in dark mode and a 17
# Pro. The Simulators boot headless (Simulator.app never opens) and are
# deleted afterwards. site-clip-simulator.mjs lays the PNGs into Xcode's own
# device bezels.
#
#   scripts/site-clip-phones.sh <out dir>
set -euo pipefail

out=$1
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$out"
work=$(mktemp -d)
app=$work/Acme.app
mkdir -p "$app"
xcrun -sdk iphonesimulator swiftc -target arm64-apple-ios18.0-simulator \
  -parse-as-library -O "$here/site-clip-phones/Acme.swift" -o "$app/Acme"
cat > "$app/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>dev.acme.onboarding</string>
  <key>CFBundleExecutable</key><string>Acme</string>
  <key>CFBundleName</key><string>Acme</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>MinimumOSVersion</key><string>18.0</string>
  <key>UIDeviceFamily</key><array><integer>1</integer></array>
  <key>UILaunchScreen</key><dict/>
  <key>UISupportedInterfaceOrientations</key><array><string>UIInterfaceOrientationPortrait</string></array>
</dict></plist>
PLIST
codesign -s - "$app" >/dev/null 2>&1

devices=()
cleanup() {
  for udid in "${devices[@]}"; do xcrun simctl delete "$udid" >/dev/null 2>&1 || true; done
  rm -rf "$work"
}
trap cleanup EXIT

# shoot <name> <device type> <appearance> <step>...
shoot() {
  local name=$1 type=$2 appearance=$3; shift 3
  local udid
  udid=$(xcrun simctl create "diri-clip-$name" "com.apple.CoreSimulator.SimDeviceType.$type")
  devices+=("$udid")
  xcrun simctl boot "$udid"
  xcrun simctl bootstatus "$udid" >/dev/null
  xcrun simctl ui "$udid" appearance "$appearance"
  xcrun simctl status_bar "$udid" override --time 9:41 --dataNetwork wifi --wifiBars 3 \
    --cellularMode active --cellularBars 4 --batteryState charged --batteryLevel 100
  xcrun simctl install "$udid" "$app"
  local step
  for step in "$@"; do
    xcrun simctl terminate "$udid" dev.acme.onboarding >/dev/null 2>&1 || true
    xcrun simctl launch "$udid" dev.acme.onboarding -state "$step" >/dev/null
    sleep 1.5
    xcrun simctl io "$udid" screenshot --mask=alpha "$out/$name-${step//:/-}.png" >/dev/null 2>&1
  done
  xcrun simctl shutdown "$udid"
}

# signin:N has the first N letters of sam@acme.dev typed.
typed=()
for ((i = 0; i <= 12; i++)); do typed+=("signin:$i"); done
shoot se iPhone-SE-3rd-generation light "${typed[@]}" welcome:bug welcome:fixed
shoot max iPhone-17-Pro-Max dark tour:0 tour:1 tour:2
shoot pro iPhone-17-Pro light notify notify:alert notify:on
