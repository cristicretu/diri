#!/bin/bash
# Runs the identical suite through all three terminals, one at a time.
#
# Sequential on purpose: two terminals benchmarking at once would measure the
# contention between them, not the terminals.
set -u
B="$(cd "$(dirname "$0")" && pwd)"
RUNS="${RUNS:-3}"
CAP="${CAP:-30}"
rm -f /tmp/suite-driver.done

run_env="RUNS=$RUNS IO_BENCH_TIMEOUT=$CAP"

echo "=== dirijor ==="
RUNBENCH_TIMEOUT_SECS=2400 env $run_env \
  DIRI_HOLDER_BIN=${DIRI_HOLDER_BIN:-/tmp/diri-shared-target/release/diri-holder} ${RUNBENCH:-/tmp/diri-shared-target/release/examples/runbench} 153 39 "$run_env bash $B/suite.sh dirijor" \
  >/tmp/suite-dirijor.log 2>&1

echo "=== ghostty ==="
/Applications/Ghostty.app/Contents/MacOS/ghostty --window-width=153 --window-height=39 \
  -e bash -c "$run_env bash $B/suite.sh ghostty" >/tmp/suite-ghostty.log 2>&1

echo "=== kitty ==="
/Applications/kitty.app/Contents/MacOS/kitty -o initial_window_width=153c \
  -o initial_window_height=39c -o confirm_os_window_close=0 \
  bash -c "$run_env bash $B/suite.sh kitty" >/tmp/suite-kitty.log 2>&1

echo DONE >/tmp/suite-driver.done
