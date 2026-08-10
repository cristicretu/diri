#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
publisher="${script_dir}/publish-homebrew-cask.sh"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/diri-cask-publish-test.XXXXXX")"

cleanup() {
    rm -rf "${fixture_root}"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

remote="${fixture_root}/homebrew-diri.git"
seed="${fixture_root}/seed"
checkout="${fixture_root}/checkout"
dmg="${fixture_root}/diri-0.4.6-universal.dmg"
fake_gh="${fixture_root}/gh"

git init -q --bare "${remote}"
git init -q "${seed}"
git -C "${seed}" config user.name "Release Test"
git -C "${seed}" config user.email "release-test@example.test"
mkdir -p "${seed}/Casks"
printf '%s\n' \
    'cask "diri" do' \
    '  version "0.4.6"' \
    '  sha256 "132816bd668a47af945bdca39c252c0e82313a0b6114b9ddb5cabf2292815087"' \
    'end' > "${seed}/Casks/diri.rb"
git -C "${seed}" add Casks/diri.rb
git -C "${seed}" commit -q -m "diri 0.4.6"
git -C "${seed}" branch -M main
git -C "${seed}" remote add origin "${remote}"
git -C "${seed}" push -q -u origin main
git -C "${remote}" symbolic-ref HEAD refs/heads/main

git clone -q "${remote}" "${checkout}"
git -C "${checkout}" config user.name "Release Test"
git -C "${checkout}" config user.email "release-test@example.test"

printf 'replacement release bytes\n' > "${dmg}"
expected_sha="$(shasum -a 256 "${dmg}" | awk '{print $1}')"

printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -euo pipefail' \
    'printf "sha256:%s\n" "${TEST_PUBLISHED_SHA:?}"' > "${fake_gh}"
chmod +x "${fake_gh}"

# Reproduce issue #9 exactly: the cask edit is committed locally, but the tap's
# remote branch still serves the old checksum. The publisher must push even
# when there is no new diff to commit during this recovery run.
/usr/bin/sed -i '' -E \
    -e "s|^  sha256 \".*\"$|  sha256 \"${expected_sha}\"|" \
    "${checkout}/Casks/diri.rb"
git -C "${checkout}" add Casks/diri.rb
git -C "${checkout}" commit -q -m "diri 0.4.6"

TEST_PUBLISHED_SHA="${expected_sha}" \
GH_BIN="${fake_gh}" \
GH_REPO="example/diri" \
    "${publisher}" 0.4.6 "${dmg}" "${checkout}"

remote_cask="$(git -C "${remote}" show refs/heads/main:Casks/diri.rb)"
grep -q "^  version \"0.4.6\"$" <<<"${remote_cask}" \
    || fail "remote cask version was not updated"
grep -q "^  sha256 \"${expected_sha}\"$" <<<"${remote_cask}" \
    || fail "correct cask commit never reached the remote"
test "$(git -C "${checkout}" rev-parse HEAD)" = \
    "$(git -C "${checkout}" rev-parse '@{upstream}')" \
    || fail "tap checkout is still ahead of its upstream after publishing"

# Never update the tap from bytes that do not match the asset GitHub says it
# published. This makes a partial or stale upload fail closed.
printf 'different local bytes\n' > "${dmg}"
if TEST_PUBLISHED_SHA="${expected_sha}" \
    GH_BIN="${fake_gh}" \
    GH_REPO="example/diri" \
        "${publisher}" 0.4.6 "${dmg}" "${checkout}" \
        >"${fixture_root}/mismatch.log" 2>&1; then
    fail "publisher accepted a local DMG that differs from the published asset"
fi
grep -q "does not match the published GitHub asset" "${fixture_root}/mismatch.log" \
    || fail "checksum mismatch did not produce the expected diagnostic"

echo "publish-homebrew-cask regression test passed"
