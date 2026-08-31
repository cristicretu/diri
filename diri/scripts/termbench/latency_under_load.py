#!/usr/bin/env python3
"""Terminal response latency while output is streaming.

An idle terminal answering a query in microseconds proves little: what a user
feels is typing while a build log scrolls. This measures the same cursor-position
round trip as `dsr_latency.py`, but with a large payload streaming into the
terminal at the same time — which is when queues are full, the parser is busy,
and (for a client/daemon split) the IPC path is under pressure.

Reported as median / p95 / worst, in milliseconds, alongside the idle baseline
measured immediately before, so the two are directly comparable.

Usage: latency_under_load.py <payload.bin> <out.csv> [samples]
"""
import os
import select
import statistics
import subprocess
import sys
import termios
import time
import tty

payload, out = sys.argv[1], sys.argv[2]
samples_wanted = int(sys.argv[3]) if len(sys.argv) > 3 else 150

fd = os.open("/dev/tty", os.O_RDWR)
old = termios.tcgetattr(fd)


def probe(deadline_secs=1.0):
    """One cursor-position round trip; None if the terminal does not answer."""
    termios.tcflush(fd, termios.TCIFLUSH)
    start = time.perf_counter()
    os.write(fd, b"\x1b[6n")
    buf = b""
    while not buf.endswith(b"R"):
        left = deadline_secs - (time.perf_counter() - start)
        if left <= 0 or not select.select([fd], [], [], left)[0]:
            return None
        buf += os.read(fd, 64)
    return (time.perf_counter() - start) * 1000.0


def summarize(values):
    if not values:
        return (float("nan"),) * 3 + (0,)
    ordered = sorted(values)
    p95 = ordered[min(len(ordered) - 1, int(len(ordered) * 0.95))]
    return statistics.median(ordered), p95, ordered[-1], len(ordered)


try:
    tty.setraw(fd)

    # Baseline: nothing else is happening.
    idle = [probe() for _ in range(samples_wanted)]
    idle = [value for value in idle if value is not None]

    # Under load: the payload streams for the whole measurement. `cat` is
    # restarted if it finishes early so the terminal is never idle mid-run.
    loaded = []
    writer = None
    try:
        while len(loaded) < samples_wanted:
            if writer is None or writer.poll() is not None:
                writer = subprocess.Popen(["cat", payload], stdout=fd, stderr=subprocess.DEVNULL)
            value = probe(deadline_secs=5.0)
            loaded.append(value if value is not None else float("inf"))
    finally:
        if writer is not None and writer.poll() is None:
            writer.kill()
            writer.wait()
        # Drain whatever the terminal still owes us before restoring the tty.
        deadline = time.perf_counter() + 1.0
        while time.perf_counter() < deadline and select.select([fd], [], [], 0.05)[0]:
            os.read(fd, 65536)
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
    os.close(fd)

answered = [value for value in loaded if value != float("inf")]
with open(out, "w") as f:
    f.write("state,median_ms,p95_ms,worst_ms,samples\n")
    f.write("idle,%.3f,%.3f,%.3f,%d\n" % summarize(idle))
    f.write("loaded,%.3f,%.3f,%.3f,%d\n" % summarize(answered))
    f.write("unanswered,%d\n" % (len(loaded) - len(answered)))
