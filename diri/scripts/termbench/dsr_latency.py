#!/usr/bin/env python3
"""Terminal response latency: how long the emulator takes to answer a query.

This is not keystroke-to-photon latency — that needs a high-speed camera. It is
the software half of it: write a cursor-position request, wait for the reply.
Every millisecond here is a millisecond the same terminal would take to notice
and act on input, so it is the part of input latency that can be measured
without hardware.

Usage: dsr_latency.py <out.csv> [iterations]
"""
import os
import select
import statistics
import sys
import termios
import time
import tty

out = sys.argv[1]
iterations = int(sys.argv[2]) if len(sys.argv) > 2 else 200

fd = os.open("/dev/tty", os.O_RDWR)
old = termios.tcgetattr(fd)
samples = []
try:
    tty.setraw(fd)
    for _ in range(iterations):
        termios.tcflush(fd, termios.TCIFLUSH)
        start = time.perf_counter()
        os.write(fd, b"\x1b[6n")
        buf = b""
        while not buf.endswith(b"R"):
            if not select.select([fd], [], [], 1.0)[0]:
                break
            buf += os.read(fd, 32)
        if buf.endswith(b"R"):
            samples.append((time.perf_counter() - start) * 1000.0)
        time.sleep(0.002)  # don't let one query's work overlap the next
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
    os.close(fd)

with open(out, "w") as f:
    if not samples:
        f.write("nan,nan,nan,0\n")
    else:
        f.write(
            "%.3f,%.3f,%.3f,%d\n"
            % (
                statistics.median(samples),
                min(samples),
                statistics.quantiles(samples, n=100)[94] if len(samples) > 20 else max(samples),
                len(samples),
            )
        )
