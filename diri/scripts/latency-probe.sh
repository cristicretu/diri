#!/usr/bin/env bash
# Interactive-latency gate: proves a packaged daemon delivers input echo to
# the readable screen fast enough to feel instant.
#
# Two regressions shipped in one week that idle gates could not see: a 50ms
# paced grid flush, and a held pump that polled its log at 100ms instead of
# waking on write. Both would fail this probe; neither moved memory or idle
# CPU at all.
#
# The probe boots the PACKAGED dirijord-rs against a private App Support in a
# temp dir (never the real daemon or fleet), spawns one held shell, then
# measures send_text → marker-visible-on-screen round trips over the control
# socket. Budgets are on the median and p90 across samples, so one slow
# machine hiccup does not fail a release and one lucky sample does not pass a
# regression.
#
# Usage: latency-probe.sh --app <diri.app>
# Env: DIRI_LATENCY_MEDIAN_MS (default 25), DIRI_LATENCY_P90_MS (default 50),
#      DIRI_LATENCY_SAMPLES (default 10)

set -euo pipefail

app=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --app)
            app="$2"
            shift 2
            ;;
        *)
            echo "usage: latency-probe.sh --app <diri.app>" >&2
            exit 2
            ;;
    esac
done
[[ -d "$app" ]] || {
    echo "error: --app <diri.app> is required" >&2
    exit 2
}

daemon="$app/Contents/Resources/bin/dirijord-rs"
[[ -x "$daemon" ]] || {
    echo "error: $daemon missing; package first" >&2
    exit 2
}

support="$(mktemp -d /tmp/diri-latency.XXXXXX)"
daemon_pid=""
cleanup() {
    if [[ -n "$daemon_pid" ]]; then
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf "$support"
}
trap cleanup EXIT

echo "==> Probing interactive latency: $daemon"
DIRIJOR_APP_SUPPORT="$support" "$daemon" >"$support/boot.log" 2>&1 &
daemon_pid=$!

for _ in $(seq 1 50); do
    [[ -S "$support/daemon.sock" ]] && break
    sleep 0.1
done
[[ -S "$support/daemon.sock" ]] || {
    echo "FAIL: daemon socket never appeared" >&2
    tail -5 "$support/boot.log" >&2
    exit 1
}

DIRI_LATENCY_SOCK="$support/daemon.sock" \
DIRI_LATENCY_MEDIAN_MS="${DIRI_LATENCY_MEDIAN_MS:-25}" \
DIRI_LATENCY_P90_MS="${DIRI_LATENCY_P90_MS:-50}" \
DIRI_LATENCY_SAMPLES="${DIRI_LATENCY_SAMPLES:-10}" \
python3 - <<'PROBE'
import json
import os
import socket
import statistics
import sys
import time

sock = socket.socket(socket.AF_UNIX)
sock.connect(os.environ["DIRI_LATENCY_SOCK"])
sock.settimeout(10)
reader = sock.makefile("r")
next_id = 0


def request(method, params=None):
    global next_id
    next_id += 1
    message = {"id": next_id, "method": method}
    if params is not None:
        message["params"] = params
    sock.sendall((json.dumps(message) + "\n").encode())
    # Skip event frames interleaved on the same connection.
    while True:
        line = reader.readline()
        if not line:
            raise RuntimeError("daemon closed the connection")
        reply = json.loads(line)
        if reply.get("id") == next_id:
            if "err" in reply:
                raise RuntimeError(f"{method}: {reply['err']}")
            return reply.get("ok")


def screen_text(session_id):
    result = request("session.read_screen", {"sessionID": session_id})
    return result.get("text", "")


request("hello", {"proto": 1, "build": "latency-probe"})
spawned = request(
    "session.spawn",
    {"kind": {"shell": {}}, "cwd": "/tmp", "title": "latency-probe"},
)
session = spawned["id"]
request("session.resize", {"sessionID": session, "cols": 120, "rows": 32})

# Wait for the shell prompt (a non-empty screen; prompts with clocks never
# go byte-stable, so marker counting below carries the correctness).
deadline = time.monotonic() + 15
while time.monotonic() < deadline:
    if screen_text(session).strip():
        break
    time.sleep(0.2)
else:
    print("FAIL: shell never painted a prompt", file=sys.stderr)
    sys.exit(1)
time.sleep(0.5)

samples = []
for index in range(int(os.environ["DIRI_LATENCY_SAMPLES"])):
    marker = f"probe-{index}-{int(time.time() * 1000)}"
    started = time.monotonic()
    # Raw keystrokes, Enter included, the way a typing user reaches the PTY.
    # `submit: True` would frame a prompt paste and then wait PASTE_SUBMIT_GAP
    # (80 ms, for Gemini's paste protection) before its Enter, so every sample
    # measured that deliberate pause instead of the pipeline.
    request(
        "session.send_text",
        {"sessionID": session, "text": f"echo {marker}\r", "submit": False},
    )
    while True:
        # The echoed command AND its output line both carry the marker.
        if screen_text(session).count(marker) >= 2:
            samples.append((time.monotonic() - started) * 1000)
            break
        if time.monotonic() - started > 5:
            print(f"FAIL: sample {index} never echoed", file=sys.stderr)
            sys.exit(1)
        time.sleep(0.002)

request("session.kill", {"sessionID": session})

samples.sort()
median = statistics.median(samples)
p90 = samples[max(0, int(len(samples) * 0.9) - 1)]
budget_median = float(os.environ["DIRI_LATENCY_MEDIAN_MS"])
budget_p90 = float(os.environ["DIRI_LATENCY_P90_MS"])
print(
    f"    send→screen over {len(samples)} samples: "
    f"median {median:.0f}ms (budget {budget_median:.0f}), "
    f"p90 {p90:.0f}ms (budget {budget_p90:.0f})"
)
if median > budget_median or p90 > budget_p90:
    print("FAIL: interactive latency over budget", file=sys.stderr)
    sys.exit(1)
print("PASS: interactive latency within budget")
PROBE
