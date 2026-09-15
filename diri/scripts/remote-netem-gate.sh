#!/usr/bin/env bash
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${script_dir}/.."
if [[ "$(uname -s)" != Linux ]]; then
    echo "This gate requires Linux unprivileged user/network namespaces and kernel netem." >&2
    exit 64
fi
cargo test --locked --release --package diri-remote --test holder_e2e \
    impaired_tcp_preserves_input_history_and_reconnect -- --ignored --exact --nocapture
