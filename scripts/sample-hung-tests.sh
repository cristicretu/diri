#!/usr/bin/env bash

# Print where a stuck `swift test` run actually is.
#
# The engine suite spawns real PTYs, child agents and git repositories, so a
# hang can sit either in the test binary or in something it spawned. A runner
# gives you neither: SwiftPM block-buffers when stdout is a pipe, the job dies
# on its timeout, and the log's last line is wherever the buffer happened to
# flush. Guessing from that has already cost more time than it should.
#
# So: run this in the background next to the test, and after `delay` seconds it
# starts printing, on a fixed cadence until the run ends, a `ps` snapshot of the
# test binary and every descendant plus a `sample` call graph for each of them.
# A blocked call shows up by name, once, instead of being inferred.
#
# usage: scripts/sample-hung-tests.sh [delay-seconds] [passes] [interval-seconds]
#
# It exits 0 as soon as no process matches, so the normal case — the suite
# finishes long before `delay` — costs nothing and prints nothing.

set -uo pipefail

delay="${1:-300}"
passes="${2:-3}"
interval="${3:-120}"
pattern="${DIRIJOR_WATCHDOG_PATTERN:-dirijorPackageTests}"

# `sample` needs the same uid as the target, or root. Both hold on a GitHub
# runner; elsewhere it may not, and a permission error is worth seeing rather
# than swallowing.
sample_cmd=(sample)
if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
    sample_cmd=(sudo sample)
fi

descendants() {
    # Breadth-first walk of the process tree under $1, parent before child.
    # Plain string queues, not arrays: /bin/bash on macOS is 3.2, where an
    # empty array under `set -u` is an unbound variable rather than nothing.
    local queue="$1" next pid kid
    while [ -n "${queue}" ]; do
        next=""
        for pid in ${queue}; do
            echo "${pid}"
            for kid in $(pgrep -P "${pid}" 2>/dev/null); do
                next="${next} ${kid}"
            done
        done
        queue="${next}"
    done
}

sleep "${delay}"

for pass in $(seq 1 "${passes}"); do
    roots="$(pgrep -f "${pattern}" 2>/dev/null)"
    if [ -z "${roots}" ]; then
        echo "watchdog: no process matches ${pattern}; the run is not stuck here"
        exit 0
    fi

    echo "::group::watchdog pass ${pass}/${passes} (after $((delay + (pass - 1) * interval))s)"
    for root in ${roots}; do
        pids="$(descendants "${root}")"
        echo "--- process tree under ${root} ---"
        # shellcheck disable=SC2086
        ps -o pid,ppid,stat,etime,wchan,command -p ${pids//$'\n'/ } 2>/dev/null

        for pid in ${pids}; do
            echo "--- sample ${pid} ---"
            # 2s at 10ms: enough to tell a blocked thread from a busy one, and
            # short enough that three passes stay inside any sane job timeout.
            "${sample_cmd[@]}" "${pid}" 2 10 -f /dev/stdout 2>&1 ||
                echo "watchdog: sample ${pid} failed (exit $?)"
        done
    done
    echo "::endgroup::"

    if [ "${pass}" -lt "${passes}" ]; then
        sleep "${interval}"
    fi
done

# The watchdog reports; it never decides the run failed. Leaving the loop's
# last exit status to stand would fail the step on the one pass that matters.
exit 0
