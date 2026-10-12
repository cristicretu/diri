#!/usr/bin/env python3
"""Regenerates the Agent marks in this directory from `source/` and Inter.

Every mark is written as `<id>.svg`: one even-odd path in a 24x24 view box,
centred, its larger side scaled to `24 * size`. `size` is the optical
correction that makes a solid block, a ring and a starburst read at the same
weight next to Diri's four brand marks; it is chosen by eye against the grid
the `render_agent_marks_screenshot` test draws (see README.md).

A monogram is a rounded tile with the letters knocked out of it, set in Inter
Bold, for an Agent whose official artwork is not under a licence that allows
redistributing it.

    python3 -I generate.py /path/to/Inter-4.1/extras/ttf/Inter-Bold.ttf

Needs fontTools (`pip install fonttools`). Writes nothing outside this
directory and touches no network.
"""

import re
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

from fontTools.misc.transform import Transform
from fontTools.pens.boundsPen import BoundsPen
from fontTools.pens.recordingPen import RecordingPen
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.svgLib.path import parse_path
from fontTools.ttLib import TTFont

HERE = Path(__file__).resolve().parent

# id: (source file, optical size)
OFFICIAL = {
    "aider": ("aider-safari-pinned-tab.svg", 0.76),
    "antigravity": ("antigravity.svg", 0.98),
    "cline": ("cline-logo-filled.svg", 0.98),
    "codebuddy": ("codebuddy.svg", 0.94),
    "command-code": ("commandcode.svg", 0.84),
    "copilot": ("copilot-24.svg", 1.0),
    "goose": ("goose.svg", 0.94),
    "grok": ("grok.svg", 0.92),
    "junie": ("junie.svg", 0.82),
    "kilo": ("kilo.svg", 0.76),
    "kimi": ("kimi.svg", 0.88),
    "kiro": ("kiro.svg", 0.92),
    "muse": ("meta.svg", 1.0),
    "openclaw": ("openclaw.svg", 0.94),
    "opencode": ("opencode.svg", 0.84),
    "openhands": ("openhands.svg", 1.0),
    "qoder": ("qoder.svg", 0.92),
    "qwen": ("qwen.svg", 0.94),
    "traecli": ("trae.svg", 0.88),
    "vibe": ("mistral-vibe.svg", 0.88),
}

# id: letters. One letter when no other monogram starts with it.
MONOGRAMS = {
    "abacusai": "Ab",
    "amp": "A",
    "auggie": "Au",
    "codebuff": "Cb",
    "coderabbit": "CR",
    "crush": "C",
    "devin": "D",
    "droid": "Dr",
    "hermes": "H",
    "maki": "M",
    "mcode": "Mc",
    "pi": "π",
    "warp": "W",
    "whipcode": "Wh",
}

# The monogram tile, in the 24 box: same optical size as a solid mark.
TILE = 0.76
TILE_RADIUS = 0.28  # of the tile's side
# Letter heights, as a fraction of the tile's side, by letter count.
LETTER_HEIGHT = {1: 0.52, 2: 0.44}
LETTER_WIDTH = 0.72  # of the tile's side, the most the letters may span
LETTER_GAP = 0.04  # of the tile's side, extra tracking between two letters

NUMBER = re.compile(r"-?(?:\d+\.?\d*|\.\d+)(?:e-?\d+)?")


def parse_transform(text):
    transform = Transform()
    for name, args in re.findall(r"(\w+)\s*\(([^)]*)\)", text or ""):
        values = [float(value) for value in NUMBER.findall(args)]
        if name == "translate":
            transform = transform.translate(values[0], values[1] if len(values) > 1 else 0)
        elif name == "scale":
            transform = transform.scale(values[0], values[1] if len(values) > 1 else values[0])
        elif name == "matrix":
            transform = transform.transform(values)
        else:
            raise ValueError(f"unsupported transform {name}")
    return transform


def rect_path(element):
    x = float(element.get("x", 0))
    y = float(element.get("y", 0))
    w = float(element.get("width"))
    h = float(element.get("height"))
    r = min(float(element.get("rx", element.get("ry", 0))), w / 2, h / 2)
    if r == 0:
        return f"M{x} {y}h{w}v{h}h{-w}z"
    return (
        f"M{x + r} {y}h{w - 2 * r}a{r} {r} 0 0 1 {r} {r}v{h - 2 * r}"
        f"a{r} {r} 0 0 1 {-r} {r}h{-(w - 2 * r)}a{r} {r} 0 0 1 {-r} {-r}"
        f"v{-(h - 2 * r)}a{r} {r} 0 0 1 {r} {-r}z"
    )


def source_outline(path):
    """Every filled shape in an SVG, as one recording in its user space."""
    recording = RecordingPen()
    root = ET.parse(path).getroot()

    def walk(element, transform):
        tag = element.tag.split("}")[-1]
        if tag in ("defs", "clipPath", "title", "metadata", "mask"):
            return
        transform = transform.transform(parse_transform(element.get("transform")))
        if element.get("fill") == "none" and tag != "svg":
            return
        if tag == "path":
            parse_path(element.get("d"), TransformPen(recording, transform))
        elif tag == "rect":
            parse_path(rect_path(element), TransformPen(recording, transform))
        for child in element:
            walk(child, transform)

    walk(root, Transform())
    return recording


def bounds(recording):
    pen = BoundsPen(None)
    recording.replay(pen)
    return pen.bounds


def fit(recording, size):
    """Centres `recording` in the 24 box, its larger side `24 * size`."""
    x0, y0, x1, y1 = bounds(recording)
    scale = 24 * size / max(x1 - x0, y1 - y0)
    transform = (
        Transform()
        .translate(12, 12)
        .scale(scale)
        .translate(-(x0 + x1) / 2, -(y0 + y1) / 2)
    )
    fitted = RecordingPen()
    recording.replay(TransformPen(fitted, transform))
    return fitted


def monogram(font, letters):
    glyphs = font.getGlyphSet()
    cmap = font.getBestCmap()
    side = 24 * TILE
    origin = (24 - side) / 2
    out = RecordingPen()
    tile = {"x": origin, "y": origin, "width": side, "height": side, "rx": side * TILE_RADIUS}
    parse_path(rect_path(tile), out)

    # Set the letters in font units, Y flipped to SVG's downward Y.
    text = RecordingPen()
    advance = 0.0
    gap = LETTER_GAP * side
    cap = font["OS/2"].sCapHeight
    height = LETTER_HEIGHT[len(letters)] * side
    for index, letter in enumerate(letters):
        name = cmap[ord(letter)]
        if index:
            advance += gap * cap / height
        glyphs[name].draw(TransformPen(text, Transform().translate(advance, 0).scale(1, -1)))
        advance += glyphs[name].width
    x0, y0, x1, y1 = bounds(text)
    # Wide pairs (Wh, Mc) shrink to keep a margin inside the tile.
    scale = min(height / cap, LETTER_WIDTH * side / (x1 - x0))
    # Centre on the ink horizontally and on the cap height vertically, so a
    # lowercase pair sits where a capital would.
    transform = (
        Transform()
        .translate(12, 12)
        .scale(scale)
        .translate(-(x0 + x1) / 2, cap / 2)
    )
    if letters == "π":
        # π has no cap height: centre its ink.
        transform = (
            Transform().translate(12, 12).scale(scale).translate(-(x0 + x1) / 2, -(y0 + y1) / 2)
        )
    text.replay(TransformPen(out, transform))
    return out


def write(mark_id, recording, note):
    pen = SVGPathPen(None, ntos=lambda value: f"{round(value, 3):g}")
    recording.replay(pen)
    d = pen.getCommands()
    (HERE / f"{mark_id}.svg").write_text(
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">'
        f"<!-- {note} -->"
        f'<path fill-rule="evenodd" d="{d}"/></svg>\n'
    )


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    font = TTFont(sys.argv[1])
    for mark_id, (source, size) in OFFICIAL.items():
        write(mark_id, fit(source_outline(HERE / "source" / source), size), f"from source/{source}")
    for mark_id, letters in MONOGRAMS.items():
        write(mark_id, monogram(font, letters), f"monogram {letters}, Inter Bold")


if __name__ == "__main__":
    main()
