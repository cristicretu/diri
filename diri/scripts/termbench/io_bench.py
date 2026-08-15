#!/usr/bin/env python3
"""Replay a recorded byte stream into the terminal and time it, twice over:

  t_cat  - wall time until the writer exits (what a plain `time cat` measures;
           it only proves the terminal drained the pty, not that it parsed it)
  t_sync - wall time until the terminal answers a cursor-position report queued
           *after* the payload, i.e. until everything has actually been parsed

A terminal that never answers the query reports t_sync as `nan`; one that is
too slow to finish the payload at all reports `timeout`.

Usage: io_bench.py <file> <out.csv>
"""
import os
import select
import subprocess
import sys
import termios
import time
import tty

path, out = sys.argv[1], sys.argv[2]
# A terminal pathologically slow at one workload must not stall the whole
# suite, so every run is capped.
CAP = float(os.environ.get("IO_BENCH_TIMEOUT", "60"))
# Terminals that answer DSR do so in well under a millisecond; one that does
# not implement it never will, so don't sit on a long timeout.
DSR_WAIT = 2.0

tty_fd = os.open("/dev/tty", os.O_RDWR)
old = termios.tcgetattr(tty_fd)
try:
    t0 = time.perf_counter()
    child = subprocess.Popen(["cat", path], stdout=tty_fd)
    try:
        child.wait(timeout=CAP)
        timed_out = False
    except subprocess.TimeoutExpired:
        child.kill()
        child.wait()
        timed_out = True
    t_cat = time.perf_counter() - t0

    if timed_out:
        result = f"{t_cat * 1000:.1f},timeout"
    else:
        tty.setraw(tty_fd)
        termios.tcflush(tty_fd, termios.TCIFLUSH)
        os.write(tty_fd, b"\x1b[6n")
        buf = b""
        deadline = time.perf_counter() + DSR_WAIT
        while not buf.endswith(b"R"):
            left = deadline - time.perf_counter()
            if left <= 0 or not select.select([tty_fd], [], [], left)[0]:
                buf = b""  # terminal never answered
                break
            chunk = os.read(tty_fd, 32)
            if not chunk:
                break
            buf += chunk
        t_sync = (time.perf_counter() - t0) if buf else float("nan")
        result = f"{t_cat * 1000:.1f},{t_sync * 1000:.1f}"
finally:
    termios.tcsetattr(tty_fd, termios.TCSADRAIN, old)
    os.close(tty_fd)

with open(out, "a") as f:
    f.write(result + "\n")
