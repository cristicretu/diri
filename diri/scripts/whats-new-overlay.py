#!/usr/bin/env python3
"""Draws the pointer and held keys onto a recorded What's New clip.

A headless recording has no OS cursor or keyboard. A clip that is about a
gesture writes overlay.txt beside frames.txt, one `<x> <y> <held>` line per
frame in window points, and this paints a macOS-style arrow at (x, y) and
the ⌘T keycaps while `held` is 1. Frames are edited in place.

    scripts/whats-new-overlay.py <clip frame dir>
"""
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ARROW = [(0, 0), (0, 17), (4.2, 13.2), (7.2, 20), (10, 18.8), (7.1, 12.2), (12.6, 12.2)]
FONT = "/System/Library/Fonts/SFNS.ttf"


def arrow(draw, x, y, s):
    points = [(x + px * s, y + py * s) for px, py in ARROW]
    shadow = [(px + 1.0 * s, py + 1.5 * s) for px, py in points]
    draw.polygon(shadow, fill=(0, 0, 0, 70))
    draw.polygon(points, fill=(0, 0, 0, 255), outline=(255, 255, 255, 255), width=round(1.4 * s))


def keycaps(draw, cx, bottom, s, light):
    font = ImageFont.truetype(FONT, round(17 * s))
    face = (255, 255, 255, 235) if light else (58, 58, 62, 240)
    edge = (0, 0, 0, 40) if light else (255, 255, 255, 30)
    ink = (30, 30, 32, 255) if light else (240, 240, 242, 255)
    size, gap = 40 * s, 8 * s
    total = size * 2 + gap
    left = cx - total / 2
    top = bottom - size
    for i, label in enumerate(["⌘", "T"]):
        x = left + i * (size + gap)
        draw.rounded_rectangle(
            (x, top + 2 * s, x + size, top + size + 2 * s), radius=9 * s, fill=(0, 0, 0, 60)
        )
        draw.rounded_rectangle(
            (x, top, x + size, top + size), radius=9 * s, fill=face, outline=edge, width=round(s)
        )
        draw.text((x + size / 2, top + size / 2), label, font=font, fill=ink, anchor="mm")


def main(directory):
    directory = Path(directory)
    frames = [line.split()[0] for line in (directory / "frames.txt").read_text().splitlines()]
    overlay = [line.split() for line in (directory / "overlay.txt").read_text().splitlines()]
    light = directory.name.endswith("-light")
    for frame, (x, y, held) in zip(frames, overlay):
        path = directory / frame
        image = Image.open(path).convert("RGBA")
        layer = Image.new("RGBA", image.size, (0, 0, 0, 0))
        draw = ImageDraw.Draw(layer)
        # Frames are captured at 2x; overlay.txt is in window points.
        s = 2.0
        if held == "1":
            # Centred under the terminal card, clear of the sidebar.
            keycaps(draw, image.width * 0.61, image.height - 34 * s, s, light)
        arrow(draw, float(x) * s, float(y) * s, s)
        Image.alpha_composite(image, layer).convert("RGB").save(path)


if __name__ == "__main__":
    main(sys.argv[1])
