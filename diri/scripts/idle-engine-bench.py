"""Measure an idle macOS Engine (empty Home, no app window or sessions)."""
import argparse
import ctypes
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time


def counters(pid):
    usage = (ctypes.c_uint64 * 64)()
    if ctypes.CDLL("/usr/lib/libproc.dylib").proc_pid_rusage(pid, 2, usage):
        raise RuntimeError("proc_pid_rusage failed")
    # rusage_info_v2 starts with uuid_t then user/system Mach time units.
    return usage[2] + usage[3], usage[4], usage[5]


def measure(binary, seconds):
    scale = (ctypes.c_uint32 * 2)()
    ctypes.CDLL(None).mach_timebase_info(scale)
    with tempfile.TemporaryDirectory(prefix="diri-idle-", dir="/tmp") as root:
        env = dict(HOME=root, PATH=os.defpath, DIRIJOR_APP_SUPPORT=root + "/state",
                   DIRI_TELEMETRY="off", ZDOTDIR=root)
        # Workspace targets can contain an arbitrary set of sibling helpers.
        # Give both revisions the same standalone layout.
        isolated = shutil.copy2(binary, Path(root, "dirijord-rs"))
        proc = subprocess.Popen([isolated], cwd=root, env=env)
        try:
            for _ in range(200):
                if proc.poll() is not None:
                    raise RuntimeError("Engine exited during startup")
                if any(p.is_socket() for p in Path(root, "state").glob("*")):
                    break
                time.sleep(.05)
            else:
                raise RuntimeError("Engine socket missing")
            time.sleep(2)
            start = counters(proc.pid)
            t0 = time.monotonic()
            time.sleep(seconds)
            end = counters(proc.pid)
            duration = time.monotonic() - t0
            cpu, idle, irq = (b-a for a, b in zip(start, end))
            cpu_ns = cpu * scale[0] / scale[1]
            print(f"{duration:.2f}s idle, CPU {100*cpu_ns/1e9/duration:.4f}%, "
                  f"package idle wakeups {idle}, interrupt wakeups {irq}")
        finally:
            proc.terminate()
            proc.wait(timeout=5)


if __name__ == "__main__":
    args = argparse.ArgumentParser(description=__doc__)
    args.add_argument("binary", type=Path)
    args.add_argument("--seconds", default=60, type=float)
    opts = args.parse_args()
    measure(opts.binary.resolve(), opts.seconds)
