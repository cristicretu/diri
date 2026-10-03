#!/usr/bin/env bash
# Sign or verify the Linux release artifacts with Sigstore (cosign).
#
# Usage:
#   diri/scripts/linux-signatures.sh sign   <dist-directory>
#   diri/scripts/linux-signatures.sh verify <dist-directory>
#
# The signed set is every AppImage and Debian package linux-release.json
# declares (one of each per architecture, see write-linux-release-manifest.py),
# SHA256SUMS, and linux-release.json in <dist-directory>. Each <file> gets a detached
# <file>.sigstore.json bundle beside it: the signature, the short-lived signing
# certificate, and the Rekor transparency-log inclusion proof, so a bundle
# verifies without any other download.
#
# Release signing is keyless. On GitHub Actions with `id-token: write`, cosign
# exchanges the job's OIDC token for a certificate naming the exact workflow
# file and ref that built the packages. There is no private key to store,
# rotate, or leak; trust is "built by cristicretu/diri's nightly.yml on main".
#
# Verification pins that identity. Environment:
#   GH_REPO                   default cristicretu/diri
#   DIRI_SIGNING_IDENTITY     default
#       https://github.com/$GH_REPO/.github/workflows/nightly.yml@refs/heads/main
#   DIRI_SIGNING_OIDC_ISSUER  default https://token.actions.githubusercontent.com
#
# Local rehearsal only (never for releases): set DIRI_COSIGN_KEY (sign) or
# DIRI_COSIGN_PUBLIC_KEY (verify) to a throwaway `cosign generate-key-pair`
# key. Key mode is offline: nothing is uploaded to the transparency log.
set -euo pipefail

usage() {
    echo "usage: linux-signatures.sh sign|verify <dist-directory>" >&2
    exit 2
}

[ $# -eq 2 ] || usage
mode="$1"
dist_dir="$2"
case "${mode}" in
    sign | verify) ;;
    *) usage ;;
esac
if [ ! -d "${dist_dir}" ]; then
    echo "error: not a directory: ${dist_dir}" >&2
    exit 1
fi
if ! command -v cosign >/dev/null 2>&1; then
    echo "error: cosign is required (https://docs.sigstore.dev/cosign/system_config/installation/)" >&2
    exit 1
fi

gh_repo="${GH_REPO:-cristicretu/diri}"
identity="${DIRI_SIGNING_IDENTITY:-https://github.com/${gh_repo}/.github/workflows/nightly.yml@refs/heads/main}"
issuer="${DIRI_SIGNING_OIDC_ISSUER:-https://token.actions.githubusercontent.com}"

# Exactly the packages the manifest declares: a stray second AppImage would
# otherwise ship unsigned, or be signed without anyone meaning to publish it.
if ! command -v python3 >/dev/null 2>&1; then
    echo "error: python3 is required to read linux-release.json" >&2
    exit 1
fi
if [ ! -f "${dist_dir}/linux-release.json" ]; then
    echo "error: missing ${dist_dir}/linux-release.json" >&2
    exit 1
fi
# Read into a variable first: bash 3.2 (macOS, where release.sh verifies)
# misparses a here-document inside $(...).
list_packages=""
IFS= read -r -d '' list_packages <<'PY' || true
import json
import pathlib
import sys

directory = pathlib.Path(sys.argv[1])
manifest = json.loads((directory / "linux-release.json").read_text())
builds = manifest.get("builds") or [{"artifacts": manifest.get("artifacts", [])}]
declared = [record["file"] for build in builds for record in build.get("artifacts", [])]
present = sorted(
    path.name
    for path in directory.iterdir()
    if path.is_file() and path.suffix in {".deb", ".AppImage"}
)
if not declared or len(set(declared)) != len(declared) or sorted(declared) != present:
    sys.exit(
        f"error: expected exactly the packages linux-release.json declares in {directory}; "
        f"declared {sorted(declared)}, found {present}"
    )
for name in declared:
    if "/" in name or name.startswith("."):
        sys.exit(f"error: unsafe package name in linux-release.json: {name!r}")
    print(name)
PY
declared="$(python3 -c "${list_packages}" "${dist_dir}")" || exit 1
packages=()
while IFS= read -r package; do
    packages+=("${dist_dir}/${package}")
done <<< "${declared}"
files=(
    "${packages[@]}"
    "${dist_dir}/SHA256SUMS"
    "${dist_dir}/linux-release.json"
)
for file in "${files[@]}"; do
    if [ ! -f "${file}" ]; then
        echo "error: missing ${file}" >&2
        exit 1
    fi
done

sign_one() {
    local file="$1" bundle="$1.sigstore.json"
    rm -f "${bundle}"
    if [ -n "${DIRI_COSIGN_KEY:-}" ]; then
        # cosign 3 routes signing through Sigstore's signing config, which
        # always includes the public log; opt out of it to stay offline.
        cosign sign-blob --yes --key "${DIRI_COSIGN_KEY}" \
            --use-signing-config=false --tlog-upload=false \
            --bundle "${bundle}" "${file}" >/dev/null
    else
        cosign sign-blob --yes --bundle "${bundle}" "${file}" >/dev/null
    fi
    echo "    signed $(basename "${file}")"
}

verify_one() {
    local file="$1" bundle="$1.sigstore.json" output
    if [ ! -f "${bundle}" ]; then
        echo "    missing $(basename "${bundle}")" >&2
        return 1
    fi
    if [ -n "${DIRI_COSIGN_PUBLIC_KEY:-}" ]; then
        output="$(cosign verify-blob --key "${DIRI_COSIGN_PUBLIC_KEY}" \
            --insecure-ignore-tlog=true \
            --bundle "${bundle}" "${file}" 2>&1)" && return 0
    else
        output="$(cosign verify-blob \
            --certificate-identity "${identity}" \
            --certificate-oidc-issuer "${issuer}" \
            --bundle "${bundle}" "${file}" 2>&1)" && return 0
    fi
    printf '%s\n' "${output}" | sed 's/^/    /' >&2
    return 1
}

if [ "${mode}" = sign ]; then
    if [ -z "${DIRI_COSIGN_KEY:-}" ] && [ -z "${ACTIONS_ID_TOKEN_REQUEST_URL:-}" ]; then
        cat >&2 <<EOF
error: keyless signing needs a GitHub Actions OIDC token (permissions: id-token: write).

Release signatures are only made in CI, so the certificate names the workflow
that built the packages. Set DIRI_COSIGN_KEY to a throwaway key to rehearse.
EOF
        exit 1
    fi
    echo "==> Signing Linux release artifacts"
    for file in "${files[@]}"; do
        sign_one "${file}"
    done
    # A signature nobody can verify is worse than none; prove it before upload.
    if [ -n "${DIRI_COSIGN_KEY:-}" ] && [ -z "${DIRI_COSIGN_PUBLIC_KEY:-}" ]; then
        rehearsal_public_key="$(mktemp "${TMPDIR:-/tmp}/diri-cosign-pub.XXXXXX")"
        trap 'rm -f "${rehearsal_public_key}"' EXIT
        cosign public-key --key "${DIRI_COSIGN_KEY}" > "${rehearsal_public_key}"
        DIRI_COSIGN_PUBLIC_KEY="${rehearsal_public_key}"
    fi
fi

if [ -n "${DIRI_COSIGN_PUBLIC_KEY:-}" ]; then
    echo "==> Verifying Linux signatures against a local rehearsal key (not a release identity)"
else
    echo "==> Verifying Linux signatures for ${identity}"
fi
failed=0
for file in "${files[@]}"; do
    if verify_one "${file}"; then
        echo "    verified $(basename "${file}")"
    else
        echo "error: signature verification failed for $(basename "${file}")" >&2
        failed=1
    fi
done
exit "${failed}"
