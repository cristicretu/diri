#!/usr/bin/env bash

set -euo pipefail

if [[ $# -lt 3 ]]; then
    echo "usage: $0 <version> <notes-file> <artifact> [artifact ...]" >&2
    exit 2
fi

version="$1"
notes_file="$2"
shift 2
artifacts=("$@")
gh_repo="${GH_REPO:-cristicretu/diri}"
gh_bin="${GH_BIN:-gh}"
tag="v${version}"
source_commit="${SOURCE_COMMIT:-}"

if ! [[ "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "error: version '${version}' is not X.Y.Z" >&2
    exit 2
fi
if [[ ! -f "${notes_file}" ]]; then
    echo "error: release notes do not exist: ${notes_file}" >&2
    exit 1
fi
for artifact in "${artifacts[@]}"; do
    if [[ ! -f "${artifact}" ]]; then
        echo "error: release artifact does not exist: ${artifact}" >&2
        exit 1
    fi
done

published_digest_for() {
    local asset_name="$1"
    "${gh_bin}" release view "${tag}" \
        --repo "${gh_repo}" \
        --json assets \
        --jq ".assets[] | select(.name == \"${asset_name}\") | .digest"
}

verify_published_artifacts() {
    local artifact asset_name local_sha published_digest published_sha
    for artifact in "${artifacts[@]}"; do
        asset_name="$(basename "${artifact}")"
        local_sha="$(shasum -a 256 "${artifact}" | awk '{print $1}')"
        published_digest="$(published_digest_for "${asset_name}")"
        published_sha="${published_digest#sha256:}"
        if [[ "${published_sha}" != "${local_sha}" ]]; then
            cat >&2 <<EOF
error: refusing to replace immutable release asset ${tag}/${asset_name}
  local:     ${local_sha}
  published: ${published_sha:-<missing>}

Cut a new patch version. Published bytes may already be pinned by Homebrew or
an installed app, so replacing an asset under the same version is unsafe.
EOF
            return 1
        fi
    done
}

if "${gh_bin}" release view "${tag}" --repo "${gh_repo}" >/dev/null 2>&1; then
    # A rerun is only a recovery path for a later failed step. It may reuse the
    # exact artifacts already online, but it must never mutate released bytes.
    verify_published_artifacts
    echo "    ${tag} already exists with identical artifacts; leaving it immutable"
else
    create_args=(release create "${tag}" "${artifacts[@]}")
    if [[ -n "${source_commit}" ]]; then
        create_args+=(--target "${source_commit}")
    fi
    "${gh_bin}" "${create_args[@]}" \
        --repo "${gh_repo}" \
        --title "diri ${version}" \
        --notes-file "${notes_file}"
    verify_published_artifacts
    echo "    ${tag} artifacts published and verified"
fi
