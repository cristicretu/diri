#!/usr/bin/env bash
# Release-mode terminal hot-path gates. These are deterministic, headless, and
# operate only on private temporary PTYs/Unix sockets.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_dir="$(cd "${script_dir}/.." && pwd)"

cd "${workspace_dir}"
cargo bench --locked -p diri-terminal-state --bench terminal_throughput
cargo bench --locked -p diri-term --bench terminal_renderer -- \
    --warm-up-time 1 --measurement-time 2 --sample-size 10
cargo test --locked --release -p diri-engine --test holder \
    holder_input_latency_is_reported -- --ignored --exact --nocapture
cargo test --locked --release -p diri-engine --test attach -- --nocapture
