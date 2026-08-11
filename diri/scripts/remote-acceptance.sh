#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_dir="$(cd "${script_dir}/.." && pwd)"

cd "${workspace_dir}"
"${script_dir}/remote-perf-gate.sh"
cargo test --locked --release --package diri-remote --test holder_e2e \
    slow_attach_never_blocks_pty_and_reconnects_from_full_snapshot -- --ignored --exact --nocapture
cargo test --locked --release --package diri-remote --test holder_e2e \
    existing_user_supervisor_can_own_one_holder_without_persistent_configuration -- --ignored --exact --nocapture
