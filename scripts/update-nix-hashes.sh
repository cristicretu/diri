#!/usr/bin/env bash
# Regenerates nix/cargo-git-hashes.json: the hash of each Git source in
# diri/Cargo.lock. With a source's hash the flake fetches only the pinned
# commit; without one it clones the whole repository with every branch, tag
# and pull request, over a gigabyte for Zed. The Nix hashes workflow runs
# `--check` on every pull request that changes Cargo.lock or the flake.
#
# Each key ends in the pinned commit, so an existing entry stays correct and is
# reused; only new sources are fetched. `--refresh` fetches every source again.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
hashes_file="${root}/nix/cargo-git-hashes.json"
usage="usage: ./scripts/update-nix-hashes.sh [--check] [--refresh]"

check=false
refresh=false
for arg in "$@"; do
    case "${arg}" in
        --check) check=true ;;
        --refresh) refresh=true ;;
        -h|--help)
            echo "${usage}"
            exit 0
            ;;
        *)
            echo "error: unknown option: ${arg}" >&2
            echo "${usage}" >&2
            exit 2
            ;;
    esac
done

if ! command -v nix >/dev/null 2>&1; then
    echo "error: nix is required" >&2
    exit 1
fi

nix_cmd=(nix --extra-experimental-features "nix-command flakes")

# One line per Git source, sorted bytewise and deduplicated by Nix itself:
# SOURCE<TAB>URL<TAB>COMMIT<TAB>EXISTING-HASH. URL and commit are split with
# the same pattern as importCargoLock's parseGit, so both agree on every source.
# shellcheck disable=SC2016 # Nix interpolation, not shell.
sources_expr='
{ root, refresh }:
let
  lock = builtins.fromTOML (builtins.readFile (root + "/diri/Cargo.lock"));
  hashesFile = root + "/nix/cargo-git-hashes.json";
  existing =
    if refresh == "false" && builtins.pathExists hashesFile then
      builtins.fromJSON (builtins.readFile hashesFile)
    else
      { };
  parse = source: builtins.match "git\\+([^?]+)(\\?(rev|tag|branch)=(.*))?#(.*)" source;
  sources = builtins.listToAttrs (
    map (p: { name = p.source; value = parse p.source; }) (
      builtins.filter (p: p ? source && parse p.source != null) lock.package
    )
  );
in
builtins.concatStringsSep "" (
  map (
    source:
    let parts = sources.${source}; in
    "${source}\t${builtins.elemAt parts 0}\t${builtins.elemAt parts 4}\t${existing.${source} or ""}\n"
  ) (builtins.attrNames sources)
)'

# The fetchgit call importCargoLock makes for a hashed Git source, from the
# nixpkgs the flake's root input resolves to, so the hash matches what the
# flake will check. Building it with a placeholder hash fails and reports the
# real one.
# shellcheck disable=SC2016 # Nix interpolation, not shell.
fetch_expr='
{ root, url, rev }:
let
  lock = builtins.fromJSON (builtins.readFile (root + "/flake.lock"));
  pkgs = import (builtins.fetchTree lock.nodes.${lock.nodes.root.inputs.nixpkgs}.locked) {
    config = { };
    overlays = [ ];
  };
in
pkgs.fetchgit {
  inherit url rev;
  hash = pkgs.lib.fakeHash;
}'

# `nix eval` and `nix build` do not apply --argstr to an --expr function, so
# each expression is called here with values read from the environment.
call() {
    printf '(%s) { %s }' "$1" "$2"
}
sources="$(DIRI_NIX_ROOT="${root}" DIRI_NIX_REFRESH="${refresh}" "${nix_cmd[@]}" eval --raw --impure \
    --expr "$(call "${sources_expr}" 'root = builtins.getEnv "DIRI_NIX_ROOT"; refresh = builtins.getEnv "DIRI_NIX_REFRESH";')")"

entries=""
while IFS=$'\t' read -r source url rev hash; do
    [[ -n "${source}" ]] || continue
    if [[ -z "${hash}" ]]; then
        echo "==> ${url} ${rev}" >&2
        log="$(DIRI_NIX_ROOT="${root}" DIRI_NIX_URL="${url}" DIRI_NIX_REV="${rev}" \
            "${nix_cmd[@]}" build --no-link --impure \
            --expr "$(call "${fetch_expr}" 'root = builtins.getEnv "DIRI_NIX_ROOT"; url = builtins.getEnv "DIRI_NIX_URL"; rev = builtins.getEnv "DIRI_NIX_REV";')" \
            </dev/null 2>&1)" || true
        hash="$(sed -n 's/^ *got: *\(sha256-[^ ]*\).*/\1/p' <<<"${log}")"
        if [[ -z "${hash}" ]]; then
            printf '%s\n' "${log}" >&2
            echo "error: could not determine the hash of ${url} at ${rev}" >&2
            exit 1
        fi
    fi
    entries+="${entries:+,
}  \"${source}\": \"${hash}\""
done <<<"${sources}"

contents="$(printf '{\n%s\n}\n' "${entries}")"

if [[ "${check}" == true ]]; then
    if [[ -f "${hashes_file}" ]] && diff -u "${hashes_file}" - <<<"${contents}" >&2; then
        echo "nix/cargo-git-hashes.json is up to date." >&2
        exit 0
    fi
    echo "error: nix/cargo-git-hashes.json is stale; run ./scripts/update-nix-hashes.sh and commit the result." >&2
    exit 1
fi

mkdir -p "$(dirname "${hashes_file}")"
printf '%s\n' "${contents}" >"${hashes_file}"
echo "wrote ${hashes_file#"${root}/"}" >&2
