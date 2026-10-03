#!/bin/bash
# Waits on GitHub Actions for a release source commit, so release.sh reuses
# work CI already does instead of redoing it on the release machine.
#
# Usage:
#   diri/scripts/await-ci.sh gates <sha>
#       Succeeds once a CI run has passed on <sha>'s exact source tree: the
#       push run on <sha>, or the pull-request run on the head of the PR that
#       <sha> merged when that head has the same tree. A squash merge of an
#       up-to-date bump PR is that case, so a release does not wait for main's
#       macOS queue to re-test code CI already passed.
#   diri/scripts/await-ci.sh linux <sha> <out-dir>
#       Downloads the linux-packages-<sha> artifact from a Nightly run on <sha>,
#       dispatching one against main if none exists. The Linux package jobs
#       must have passed; unrelated Nightly jobs do not gate the download. The
#       artifact is fetched in parallel byte ranges, since GitHub's artifact
#       store can throttle one connection to tens of KB/s.
#
# Env overrides:
#   GH_REPO                  default cristicretu/diri
#   DIRI_CI_POLL_SECONDS     default 20
#   DIRI_CI_TIMEOUT_SECONDS  default 5400 (a fresh Nightly takes ~40 minutes)
#   DIRI_DOWNLOAD_STREAMS    parallel ranges for artifact downloads (default 16)
set -euo pipefail

GH_REPO="${GH_REPO:-cristicretu/diri}"
POLL="${DIRI_CI_POLL_SECONDS:-20}"
DEADLINE=$((SECONDS + ${DIRI_CI_TIMEOUT_SECONDS:-5400}))
# Display-name prefix shared by the Nightly Linux jobs: the per-architecture
# package builds (x86_64, aarch64), the signed release set that merges them,
# and the per-architecture Ubuntu 24.04 smokes. Renaming those jobs, or
# changing the architecture matrix, must update these.
LINUX_JOB_PREFIX="Linux package"
LINUX_JOB_COUNT=5

usage() {
    echo "usage: await-ci.sh gates <sha> | await-ci.sh linux <sha> <out-dir>" >&2
    exit 2
}

log() {
    echo "[$(date +%H:%M:%S)] $*"
}

sleep_or_timeout() {
    if [ "$SECONDS" -ge "$DEADLINE" ]; then
        echo "error: timed out waiting on GitHub Actions ($1)" >&2
        exit 1
    fi
    sleep "$POLL"
}

# Newest run of <workflow> on <sha> whose event passes <jq filter>, or empty.
newest_run() {
    gh run list -R "$GH_REPO" --workflow "$1" --commit "$2" -L 20 \
        --json databaseId,event,createdAt \
        --jq "[.[] | select($3)] | sort_by(.createdAt) | last | .databaseId // empty"
}

tree_of() {
    gh api "repos/$GH_REPO/commits/$1" --jq .commit.tree.sha
}

# CI runs that tested <sha>'s tree: its push run, plus pull-request runs on the
# head of any PR it merged whose tree is identical. Prints "<id> <event>" lines.
gate_candidates() {
    local sha="$1" tree="$2" head
    gh run list -R "$GH_REPO" --workflow ci.yml --commit "$sha" -L 20 \
        --json databaseId,event --jq '.[] | select(.event == "push") | "\(.databaseId) push"'
    for head in $(gh api "repos/$GH_REPO/commits/$sha/pulls" --jq '.[].head.sha' 2>/dev/null); do
        [ "$head" != "$sha" ] || continue
        [ "$(tree_of "$head")" = "$tree" ] || continue
        gh run list -R "$GH_REPO" --workflow ci.yml --commit "$head" -L 20 \
            --json databaseId,event --jq '.[] | select(.event == "pull_request") | "\(.databaseId) pull_request"'
    done
}

await_gates() {
    local sha="$1" tree reran="" id event status conclusion pending
    tree="$(tree_of "$sha")"
    log "Gate: a passing CI run on tree $tree (commit $sha)"
    while :; do
        pending=0
        while read -r id event; do
            [ -n "$id" ] || continue
            read -r status conclusion < <(gh run view "$id" -R "$GH_REPO" \
                --json status,conclusion --jq '"\(.status) \(.conclusion)"')
            if [ "$status" = "completed" ] && [ "$conclusion" = "success" ]; then
                log "CI passed: run $id ($event) tested the same tree"
                return 0
            fi
            if [ "$status" != "completed" ]; then
                pending=$((pending + 1))
            elif [ "$conclusion" = "cancelled" ] && [ "$event" = "push" ] && [[ " $reran " != *" $id "* ]]; then
                # A run superseded by a later push never reached a verdict.
                # Ask for one, once; a second cancellation counts as a failure.
                log "CI run $id was cancelled before finishing; re-running it"
                gh run rerun "$id" -R "$GH_REPO"
                reran="$reran $id"
                pending=$((pending + 1))
            fi
        done < <(gate_candidates "$sha" "$tree")
        if [ "$pending" -eq 0 ] && [ -n "$(gate_candidates "$sha" "$tree")" ]; then
            echo "error: every CI run on tree $tree finished without passing" >&2
            echo "  https://github.com/$GH_REPO/commit/$sha" >&2
            exit 1
        fi
        sleep_or_timeout "a CI verdict on $sha ($pending run(s) in progress)"
    done
}

# Prints "<run status> <linux job count> <linux jobs not yet completed> <linux jobs failed>".
linux_job_state() {
    gh run view "$1" -R "$GH_REPO" --json status,jobs --jq "
        [.jobs[] | select(.name | startswith(\"$LINUX_JOB_PREFIX\"))] as \$linux
        | \"\(.status) \(\$linux | length) \
\([\$linux[] | select(.status != \"completed\")] | length) \
\([\$linux[] | select(.status == \"completed\" and .conclusion != \"success\")] | length)\""
}

await_linux() {
    local sha="$1" out="$2" run status count pending failed
    # Pull-request runs name their artifact after the PR head, never a main
    # commit, so only scheduled and dispatched runs can carry this one.
    run="$(newest_run nightly.yml "$sha" '.event != "pull_request"')"
    if [ -z "$run" ]; then
        log "no Nightly run on $sha; dispatching one against main"
        gh workflow run nightly.yml -R "$GH_REPO" --ref main
        # The dispatched run builds main as of now. release.sh has already
        # proven main == $sha; if main moved since, no run on $sha appears and
        # this times out rather than shipping a different commit's packages.
        while [ -z "$run" ]; do
            sleep_or_timeout "the dispatched Nightly run never appeared for $sha (did main move?)"
            run="$(newest_run nightly.yml "$sha" '.event == "workflow_dispatch"')"
        done
    fi
    log "Nightly run $run for $sha: https://github.com/$GH_REPO/actions/runs/$run"

    # Dependent jobs are only listed once they are queued, so require every
    # Linux job to be present before trusting "none pending".
    while :; do
        read -r status count pending failed < <(linux_job_state "$run")
        if [ "$failed" -gt 0 ]; then
            echo "error: a Linux package job failed in Nightly run $run" >&2
            echo "  https://github.com/$GH_REPO/actions/runs/$run" >&2
            exit 1
        fi
        if [ "$count" -ge "$LINUX_JOB_COUNT" ] && [ "$pending" -eq 0 ]; then
            break
        fi
        if [ "$status" = "completed" ]; then
            echo "error: Nightly run $run finished without passing Linux package jobs" \
                "(it may have been cancelled by a newer Nightly on main)" >&2
            exit 1
        fi
        sleep_or_timeout "Linux package jobs in run $run ($pending of $count pending)"
    done

    until download_artifact "$run" "linux-packages-$sha" "$out"; do
        sleep_or_timeout "downloading linux-packages-$sha from run $run"
    done
    log "Linux packages for $sha in $out"
}

# Fetches a run's artifact into <out-dir> in parallel byte ranges. The artifact
# is downloadable as soon as its job uploads it, while the rest of the run is
# still going. GitHub's artifact store has served one connection at 40 KB/s
# while sixteen ranges reached 1 MB/s.
download_artifact() {
    local run="$1" name="$2" out="$3" id size url parts streams chunk start end i
    read -r id size < <(gh api "repos/$GH_REPO/actions/runs/$run/artifacts" \
        --jq ".artifacts[] | select(.name == \"$name\" and (.expired | not)) | \"\(.id) \(.size_in_bytes)\"")
    [ -n "${id:-}" ] || return 1
    url="$(curl -fsS -o /dev/null -w '%{redirect_url}' \
        -H "Authorization: Bearer $(gh auth token)" \
        "https://api.github.com/repos/$GH_REPO/actions/artifacts/$id/zip")" || return 1
    [ -n "$url" ] || return 1
    parts="$(mktemp -d "${TMPDIR:-/tmp}/diri-artifact.XXXXXX")"
    streams="${DIRI_DOWNLOAD_STREAMS:-16}"
    chunk=$(( (size + streams - 1) / streams ))
    local pids=()
    for ((i = 0; i < streams; i++)); do
        start=$((i * chunk))
        [ "$start" -lt "$size" ] || break
        end=$((start + chunk - 1))
        [ "$end" -lt "$size" ] || end=$((size - 1))
        curl -fsS --retry 5 --retry-all-errors -r "$start-$end" \
            -o "$parts/$(printf %03d "$i")" "$url" &
        pids+=("$!")
    done
    local failed=0 pid
    for pid in "${pids[@]}"; do
        wait "$pid" || failed=1
    done
    if [ "$failed" = 1 ]; then
        rm -rf "$parts"
        return 1
    fi
    cat "$parts"/[0-9][0-9][0-9] > "$parts/artifact.zip"
    if [ "$(wc -c < "$parts/artifact.zip" | tr -d ' ')" != "$size" ]; then
        rm -rf "$parts"
        return 1
    fi
    rm -rf "$out"
    mkdir -p "$out"
    unzip -q "$parts/artifact.zip" -d "$out" || { rm -rf "$parts"; return 1; }
    rm -rf "$parts"
}

case "${1:-}" in
    gates)
        [ $# -eq 2 ] || usage
        await_gates "$2"
        ;;
    linux)
        [ $# -eq 3 ] || usage
        await_linux "$2" "$3"
        ;;
    *)
        usage
        ;;
esac
