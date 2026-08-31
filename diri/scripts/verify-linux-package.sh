#!/usr/bin/env bash

set -euo pipefail

dist_dir="${1:?usage: verify-linux-package.sh <dist-directory>}"
dist_dir="$(cd "${dist_dir}" && pwd)"
deb="$(find "${dist_dir}" -maxdepth 1 -type f -name '*.deb' -print -quit)"
appimage="$(find "${dist_dir}" -maxdepth 1 -type f -name '*.AppImage' -print -quit)"

if [[ -z "${deb}" || -z "${appimage}" ]]; then
    echo "error: expected a DEB and AppImage in ${dist_dir}" >&2
    exit 1
fi

stage="$(mktemp -d "${TMPDIR:-/tmp}/diri-linux-package.XXXXXX")"
cleanup() {
    rm -rf "${stage}"
}
trap cleanup EXIT

deb_root="${stage}/deb"
mkdir -p "${deb_root}"
dpkg-deb --extract "${deb}" "${deb_root}"

for path in \
    usr/bin/diri \
    usr/bin/dirijor \
    usr/bin/dirijor-mcp \
    usr/bin/dirijord-rs \
    usr/bin/diri-holder \
    usr/bin/diri-ssh-askpass \
    usr/bin/diri-remote \
    usr/lib/diri/manifests/codex.json \
    usr/lib/diri/sidecar/server.js \
    usr/lib/diri/licenses/THIRD-PARTY-LICENSES.json \
    usr/lib/diri/licenses/Apache-2.0.txt \
    usr/share/applications/diri.desktop; do
    test -e "${deb_root}/${path}"
done

chmod +x "${appimage}"
(
    cd "${stage}"
    "${appimage}" --appimage-extract >/dev/null
)
app_root="${stage}/squashfs-root"
for path in \
    usr/bin/diri \
    usr/bin/dirijor \
    usr/bin/dirijor-mcp \
    usr/bin/dirijord-rs \
    usr/bin/diri-holder \
    usr/bin/diri-ssh-askpass \
    usr/bin/diri-remote \
    usr/lib/diri/manifests/codex.json \
    usr/lib/diri/sidecar/server.js \
    usr/lib/diri/licenses/THIRD-PARTY-LICENSES.json \
    usr/lib/diri/licenses/Apache-2.0.txt; do
    test -e "${app_root}/${path}"
done

DIRI_PROBE_SYMBOLS=1 "${deb_root}/usr/bin/diri" >/dev/null
DIRI_PROBE_SYMBOLS=1 "${app_root}/usr/bin/diri" >/dev/null

echo "Linux package layouts and executable probes passed"
