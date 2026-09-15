#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_dir="$(cd "${script_dir}/.." && pwd)"

cd "${workspace_dir}"
cargo test --locked --release --package diri-remote --test holder_e2e \
    performance_gate_meets_remote_holder_latency_budget -- --ignored --exact --nocapture

cargo test --locked --release --package diri-remote --test holder_e2e \
    continuous_output_keeps_controller_responsive -- --ignored --exact --nocapture

cargo test --locked --release --package diri-remote --lib \
    output_log::tests::append_latency_gate -- --ignored --exact --nocapture
