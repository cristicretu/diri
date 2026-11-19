#!/usr/bin/env python3
"""Sample one private macOS Engine and Holder under continuous terminal output."""

import argparse
import csv
import ctypes
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time


def footprint(libproc, pid):
    # rusage_info_v2 starts with the UUID (16 bytes) followed by eight u64s
    # through ri_phys_footprint. Reserve space for the entire kernel struct.
    info = (ctypes.c_uint64 * 64)()
    if libproc.proc_pid_rusage(pid, 2, ctypes.byref(info)) != 0:
        raise OSError(ctypes.get_errno(), f"could not sample private PID {pid}")
    return info[9]


def measure(root, binaries, samples, duration):
    home = root / "home"
    home.mkdir()
    state = root / "state"
    child_env = {
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "HOME": str(home),
        "TMPDIR": str(root),
        "DIRIJOR_APP_SUPPORT": str(state),
        "DIRIJOR_HOLDER_PATH": str(binaries / "diri-holder"),
        "DIRI_MANIFESTS_DIR": str(Path(__file__).resolve().parents[1]
                                  / "crates/diri-engine/manifests"),
        "DIRI_TELEMETRY": "off",
        "DIRI_HOLDER_IDLE_SECONDS": "1",
    }
    libproc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    with (root / "engine.log").open("w") as log:
        engine = subprocess.Popen([binaries / "dirijord-rs"], cwd=home, env=child_env,
                                  stdout=log, stderr=log)
    try:
        with socket.socket(socket.AF_UNIX) as control:
            control.settimeout(15)
            for _ in range(150):
                if engine.poll() is not None:
                    raise RuntimeError((root / "engine.log").read_text())
                try:
                    control.connect(str(state / "daemon.sock"))
                    break
                except (FileNotFoundError, ConnectionRefusedError):
                    time.sleep(0.1)
            else:
                raise TimeoutError("private Engine did not open its socket")

            with control.makefile("rb") as replies:
                request_id = 0

                def request(method, params):
                    nonlocal request_id
                    request_id += 1
                    line = json.dumps({"id": request_id, "method": method, "params": params})
                    control.sendall((line + "\n").encode())
                    reply = json.loads(replies.readline())
                    if reply.get("id") != request_id or "ok" not in reply:
                        raise RuntimeError(f"{method} failed: {reply}")
                    return reply["ok"]

                hello = request("hello", {"proto": 1, "build": "headless-footprint"})
                if hello["engineKind"] != "diri-rust-engine":
                    raise RuntimeError("unexpected Engine identity on private socket")
                script = 'i=0; while :; do i=$((i+1)); printf "work %s\\n" "$i"; sleep 0.1; done'
                sid = request("session.spawn", {
                    "kind": {"shell": {}}, "cwd": str(home),
                    "argv": ["/bin/sh", "-c", script],
                })["id"]
                session = {"sessionID": sid}
                try:
                    pid_file = state / "holders" / (sid + ".pid")
                    for _ in range(200):
                        if pid_file.exists() and "work" in request("session.read_screen", session)["text"]:
                            break
                        time.sleep(0.05)
                    else:
                        raise TimeoutError("private child did not paint")
                    holder_pid = int(pid_file.read_text())
                    writer = csv.writer(samples)
                    writer.writerow(["seconds", "engine_bytes", "holder_bytes"])
                    start = time.monotonic()
                    for tick in range(4 * duration + 1):
                        time.sleep(max(0, start + tick / 4 - time.monotonic()))
                        if tick % 8 == 0 and "work" not in request("session.read_screen", session)["text"]:
                            raise RuntimeError("child output disappeared")
                        writer.writerow([time.monotonic() - start, footprint(libproc, engine.pid),
                                         footprint(libproc, holder_pid)])
                        if tick % 240 == 0:
                            samples.flush()
                finally:
                    request("session.remove", session)
    finally:
        engine.terminate()
        try:
            engine.wait(timeout=10)
        except subprocess.TimeoutExpired:
            engine.kill()
            engine.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path, help="worktree Cargo debug or release directory")
    parser.add_argument("csv", type=Path, help="new sample file")
    parser.add_argument("--seconds", type=int, default=1800)
    args = parser.parse_args()
    if sys.platform != "darwin" or args.seconds <= 0:
        parser.error("requires macOS and a positive duration")
    binaries = args.binaries.resolve()
    for name in ("dirijord-rs", "diri-holder"):
        if not os.access(binaries / name, os.X_OK):
            parser.error(f"missing executable {binaries / name}")
    # Short path keeps the daemon and per-session sockets inside this root.
    with tempfile.TemporaryDirectory(prefix="adm-", dir="/tmp") as root, args.csv.open("x") as output:
        measure(Path(root), binaries, output, args.seconds)


if __name__ == "__main__":
    main()
