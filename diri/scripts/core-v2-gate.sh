#!/usr/bin/env bash
# Nightly gates for terminal core v2 (docs/core-v2.md), run against the
# current core in the same job:
#
# 1. Golden: vt_workloads, both cores, each checked against the committed
#    digests (one byte per read) before it reports a number. A mismatch fails.
# 2. Screenshots: diri-term's render tests write PNGs with each core; every
#    file must be byte-identical.
# 3. Holder->Engine vtbench, both cores, rounds interleaved. Every run checks
#    its bytes and its terminal state against a reference parse and panics on
#    a difference, so a failure is a correctness failure. The MB/s are
#    reported, not asserted: a shared runner is the wrong place to hold a
#    build on a timing, and two of the six workloads are bound by the PTY.
#
# DIRI_CORE_V2_GATE_DIR keeps the screenshots, payloads and tables (default: a
# temporary directory). DIRI_CORE_V2_GATE_ROUNDS sets the vtbench rounds per
# core (default 5).
set -euo pipefail

workspace_dir="$(cd "$(dirname "$0")/.." && pwd)"
cd "${workspace_dir}"
out="${DIRI_CORE_V2_GATE_DIR:-$(mktemp -d)}"
rounds="${DIRI_CORE_V2_GATE_ROUNDS:-5}"
target_dir="${CARGO_TARGET_DIR:-${workspace_dir}/target}"
v2=(--features diri-terminal-state/core-v2-screen)
mkdir -p "${out}"

echo "==> Golden: vt_workloads on the current core"
cargo bench --locked -p diri-terminal-state --bench vt_workloads --features bench-verify \
    | tee "${out}/vt_workloads-v1.txt"
echo "==> Golden: vt_workloads on core v2"
cargo bench --locked -p diri-terminal-state --bench vt_workloads \
    --features bench-verify,core-v2-screen | tee "${out}/vt_workloads-v2.txt"

screenshots=(copy_selection_render grapheme_clusters inline_images)
for core in v1 v2; do
    features=()
    [[ "${core}" == v2 ]] && features=("${v2[@]}")
    for test in "${screenshots[@]}"; do
        echo "==> Screenshots: ${test} on ${core}"
        DIRI_VISUAL_OUTPUT="${out}/screens-${core}/${test}" \
            cargo test --locked -p diri-term ${features[@]+"${features[@]}"} \
            --test "${test}" -- --ignored
    done
done
echo "==> Screenshots: comparing every PNG"
diff -r "${out}/screens-v1" "${out}/screens-v2"
echo "identical: $(find "${out}/screens-v1" -name '*.png' | wc -l | tr -d ' ') PNGs"

echo "==> vtbench payloads"
DIRI_VT_BENCH_WRITE="${out}/payloads" \
    cargo bench --locked -p diri-terminal-state --bench vt_workloads --features bench-verify
for core in v1 v2; do
    features=()
    [[ "${core}" == v2 ]] && features=("${v2[@]}")
    echo "==> vtbench build on ${core}"
    cargo build --locked --release -p diri-engine ${features[@]+"${features[@]}"} \
        --bin diri-holder --example vtbench
    mkdir -p "${out}/bin-${core}"
    cp "${target_dir}/release/diri-holder" "${target_dir}/release/examples/vtbench" "${out}/bin-${core}/"
done
for round in $(seq 1 "${rounds}"); do
    for core in v1 v2; do
        echo "==> vtbench round ${round} on ${core}"
        DIRI_VT_BENCH_RUNS=1 DIRI_HOLDER_BIN="${out}/bin-${core}/diri-holder" \
            "${out}/bin-${core}/vtbench" "${out}/payloads" | tee -a "${out}/vtbench-${core}.txt"
    done
done
echo "==> Results in ${out}"
