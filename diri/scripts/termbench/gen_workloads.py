#!/usr/bin/env python3
"""Generate the byte streams the terminal suite replays.

Each file is a recording of what a program would write to a terminal. Feeding
identical bytes to every terminal is what makes the comparison fair: no
terminal gets to be fast because it received less work.

Sizes are chosen so a fast terminal spends a few hundred ms on each.
"""
import os
import random
import sys

OUT = sys.argv[1] if len(sys.argv) > 1 else "/tmp/termbench"
COLS, ROWS = 153, 39
TARGET = 40 << 20  # ~40 MB per workload: small runs are dominated by system noise

os.makedirs(OUT, exist_ok=True)
rng = random.Random(0x5EED)


def write(name, chunks):
    path = os.path.join(OUT, name + ".bin")
    total = 0
    with open(path, "wb") as f:
        for chunk in chunks:
            f.write(chunk)
            total += len(chunk)
    print(f"{name:<16} {total/(1<<20):6.1f} MB")


def until_target(make):
    """Yield from make() until TARGET bytes have been produced."""
    total = 0
    while total < TARGET:
        chunk = make()
        total += len(chunk)
        yield chunk


def plain_ascii():
    """Baseline scroll: the classic `cat a big text file`."""
    src = open(
        "/private/tmp/claude-501/-Users-giga-fun/dc68f650-1b85-4b6d-88b8-8f981781413d"
        "/scratchpad/terminal-benchmark/test/shakespeare.txt", "rb"
    ).read()
    # Repeated to match the other workloads' size: a run short enough to be
    # measured in tens of milliseconds is mostly measuring system noise.
    return [src] * max(1, TARGET // len(src))


def dense_sgr():
    """Every cell its own truecolor: the worst case for SGR parsing."""
    def row():
        out = bytearray()
        for col in range(COLS):
            r, g, b = (col * 5) % 256, (col * 11) % 256, (col * 17) % 256
            out += b"\x1b[38;2;%d;%d;%dm\x1b[48;2;%d;%d;%dm#" % (r, g, b, b, r, g)
        out += b"\x1b[0m\n"
        return bytes(out)
    line = row()
    return until_target(lambda: line)


def cursor_motion():
    """Absolute positioning with tiny writes — a TUI redrawing scattered cells."""
    def burst():
        out = bytearray()
        for _ in range(256):
            y, x = rng.randint(1, ROWS), rng.randint(1, COLS)
            out += b"\x1b[%d;%dH" % (y, x)
            out += bytes([rng.randint(65, 90)])
        return bytes(out)
    return until_target(burst)


def long_lines():
    """No newlines at all: pure wrapping, which forces a scroll per screenful."""
    line = bytes([rng.randint(33, 126) for _ in range(8192)])
    return until_target(lambda: line)


def scroll_region():
    """A pager's shape: a scrolling region with inserts and deletes."""
    def burst():
        out = bytearray(b"\x1b[2;%dr" % (ROWS - 1))
        for i in range(64):
            out += b"\x1b[%d;1H" % (rng.randint(2, ROWS - 1))
            out += b"\x1b[L" if i % 2 else b"\x1b[M"
            out += b"a line of replacement text in a scrolling region\n"
        return bytes(out)
    return until_target(burst)


def unicode_cjk():
    """Wide characters: two cells per glyph, and a different width path."""
    line = ("汉字宽度测试 " * (COLS // 14) + "\n").encode()
    return until_target(lambda: line)


def combining():
    """Base characters plus combining marks — the zero-width path."""
    line = ("éàôü " * (COLS // 12) + "\n").encode()
    return until_target(lambda: line)


def erase_churn():
    """Clear and redraw the screen over and over: erase-heavy, like a spinner."""
    body = b"".join(b"\x1b[%d;1H" % y + b"x" * (COLS - 1) for y in range(1, ROWS + 1))
    frame = b"\x1b[2J\x1b[H" + body
    return until_target(lambda: frame)


def alt_screen():
    """Enter and leave the alternate screen repeatedly, painting each time."""
    body = b"".join(b"\x1b[%d;1H" % y + b"alt screen content" for y in range(1, ROWS + 1))
    frame = b"\x1b[?1049h" + body + b"\x1b[?1049l" + b"back on the primary screen\n"
    return until_target(lambda: frame)


def sync_update():
    """Frames wrapped in DECSET 2026, the flicker-free repaint protocol."""
    body = b"".join(
        b"\x1b[%d;1H" % y + b"\x1b[38;5;%dm" % (y % 256) + b"synchronized frame row" * 3
        for y in range(1, ROWS + 1)
    )
    frame = b"\x1b[?2026h\x1b[H" + body + b"\x1b[?2026l"
    return until_target(lambda: frame)


def mixed_log():
    """What a build log actually looks like: color, timestamps, plain text."""
    def burst():
        out = bytearray()
        for i in range(64):
            out += b"\x1b[2m12:34:5%d\x1b[0m " % (i % 10)
            out += b"\x1b[32mINFO\x1b[0m " if i % 3 else b"\x1b[33mWARN\x1b[0m "
            out += b"compiling crate number %d with several flags set\n" % i
        return bytes(out)
    return until_target(burst)


WORKLOADS = {
    "plain_ascii": plain_ascii,
    "dense_sgr": dense_sgr,
    "cursor_motion": cursor_motion,
    "long_lines": long_lines,
    "scroll_region": scroll_region,
    "unicode_cjk": unicode_cjk,
    "combining": combining,
    "erase_churn": erase_churn,
    "alt_screen": alt_screen,
    "sync_update": sync_update,
    "mixed_log": mixed_log,
}

if __name__ == "__main__":
    for name, make in WORKLOADS.items():
        write(name, make())
    print(f"\nwrote {len(WORKLOADS)} workloads to {OUT}")
