#!/usr/bin/env bash
# Exercises linux-signatures.sh end to end with a throwaway cosign key on dummy
# artifacts: sign, verify, and reject tampered files, a foreign key, a missing
# bundle, a stray second package, and the release identity against a bundle
# that was not signed by CI. Nothing here touches the transparency log.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
signer="${script_dir}/linux-signatures.sh"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/diri-linux-signatures-test.XXXXXX")"

cleanup() {
    rm -rf "${fixture_root}"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

if ! command -v cosign >/dev/null 2>&1; then
    fail "cosign is required for this test"
fi

# The release set Nightly signs: one AppImage and one DEB per architecture,
# merged from the per-architecture build directories.
dist="${fixture_root}/dist"
commit=0000000000000000000000000000000000000000
for arch in x86_64:amd64 aarch64:arm64; do
    build="${fixture_root}/build-${arch%%:*}"
    mkdir -p "${build}"
    head -c 262144 /dev/urandom > "${build}/diri_0.0.0_${arch%%:*}.AppImage"
    head -c 4096 /dev/urandom > "${build}/diri_0.0.0_${arch##*:}.deb"
    python3 "${script_dir}/write-linux-release-manifest.py" --directory "${build}" \
        --version 0.0.0 --architecture "${arch%%:*}" --commit "${commit}"
done
python3 "${script_dir}/write-linux-release-manifest.py" --directory "${dist}" \
    --merge "${fixture_root}/build-x86_64" "${fixture_root}/build-aarch64"

export COSIGN_PASSWORD=""
cosign generate-key-pair --output-key-prefix "${fixture_root}/rehearsal" >/dev/null 2>&1
cosign generate-key-pair --output-key-prefix "${fixture_root}/foreign" >/dev/null 2>&1
key="${fixture_root}/rehearsal.key"
public_key="${fixture_root}/rehearsal.pub"
log="${fixture_root}/log"

# Each case runs in a clean environment so an exported override from the
# caller cannot change what is being tested.
run() {
    env -u DIRI_COSIGN_KEY -u DIRI_COSIGN_PUBLIC_KEY -u DIRI_SIGNING_IDENTITY \
        -u DIRI_SIGNING_OIDC_ISSUER -u ACTIONS_ID_TOKEN_REQUEST_URL "$@" >"${log}" 2>&1
}

expect_ok() {
    local label="$1"
    shift
    run "$@" || { cat "${log}" >&2; fail "${label}: expected success"; }
    echo "ok: ${label}"
}

expect_fail() {
    local label="$1" pattern="$2"
    shift 2
    if run "$@"; then
        cat "${log}" >&2
        fail "${label}: expected failure"
    fi
    grep -q -- "${pattern}" "${log}" || { cat "${log}" >&2; fail "${label}: missing '${pattern}'"; }
    echo "ok: ${label}"
}

expect_fail "keyless signing outside GitHub Actions is refused" \
    "needs a GitHub Actions OIDC token" "${signer}" sign "${dist}"
expect_ok "sign every Linux release file and self-verify" \
    DIRI_COSIGN_KEY="${key}" "${signer}" sign "${dist}"
for name in diri_0.0.0_x86_64.AppImage diri_0.0.0_amd64.deb diri_0.0.0_aarch64.AppImage \
    diri_0.0.0_arm64.deb SHA256SUMS linux-release.json; do
    test -s "${dist}/${name}.sigstore.json" || fail "no bundle for ${name}"
done
expect_ok "verify untouched files" DIRI_COSIGN_PUBLIC_KEY="${public_key}" "${signer}" verify "${dist}"

for name in diri_0.0.0_x86_64.AppImage diri_0.0.0_aarch64.AppImage diri_0.0.0_arm64.deb; do
    cp "${dist}/${name}" "${fixture_root}/original"
    printf 'X' | dd of="${dist}/${name}" bs=1 seek=100 conv=notrunc 2>/dev/null
    expect_fail "reject a one-byte change to ${name}" \
        "verification failed for ${name}" DIRI_COSIGN_PUBLIC_KEY="${public_key}" "${signer}" verify "${dist}"
    cp "${fixture_root}/original" "${dist}/${name}"
done

cp "${dist}/SHA256SUMS" "${fixture_root}/sums"
printf '%064d  evil.AppImage\n' 0 >> "${dist}/SHA256SUMS"
expect_fail "reject an edited checksum list" \
    "verification failed for SHA256SUMS" DIRI_COSIGN_PUBLIC_KEY="${public_key}" "${signer}" verify "${dist}"
cp "${fixture_root}/sums" "${dist}/SHA256SUMS"

expect_fail "reject a signature from another key" \
    "verification failed" DIRI_COSIGN_PUBLIC_KEY="${fixture_root}/foreign.pub" "${signer}" verify "${dist}"

mv "${dist}/diri_0.0.0_arm64.deb.sigstore.json" "${fixture_root}/"
expect_fail "reject a missing bundle" \
    "missing diri_0.0.0_arm64.deb.sigstore.json" DIRI_COSIGN_PUBLIC_KEY="${public_key}" "${signer}" verify "${dist}"
mv "${fixture_root}/diri_0.0.0_arm64.deb.sigstore.json" "${dist}/"

cp "${dist}/diri_0.0.0_x86_64.AppImage" "${dist}/stray.AppImage"
expect_fail "refuse an undeclared extra AppImage" \
    "expected exactly the packages" DIRI_COSIGN_PUBLIC_KEY="${public_key}" "${signer}" verify "${dist}"
rm "${dist}/stray.AppImage"

mv "${dist}/diri_0.0.0_aarch64.AppImage" "${fixture_root}/"
expect_fail "refuse a release set missing a declared package" \
    "expected exactly the packages" DIRI_COSIGN_PUBLIC_KEY="${public_key}" "${signer}" verify "${dist}"
mv "${fixture_root}/diri_0.0.0_aarch64.AppImage" "${dist}/"

# release.sh's path: no key override, so the pinned CI identity decides, and a
# locally signed bundle has neither that certificate nor a log entry.
expect_fail "release identity rejects a non-CI signature" \
    "verification failed" "${signer}" verify "${dist}"

expect_ok "verify restored files" DIRI_COSIGN_PUBLIC_KEY="${public_key}" "${signer}" verify "${dist}"
echo "linux-signatures tests passed"
