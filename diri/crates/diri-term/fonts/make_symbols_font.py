"""Builds DiriSymbolsMono-Regular.ttf from the official Symbols Nerd Font Mono.

    curl -LO https://github.com/ryanoasis/nerd-fonts/releases/download/v3.5.1/NerdFontsSymbolsOnly.zip
    unzip NerdFontsSymbolsOnly.zip SymbolsNerdFontMono-Regular.ttf
    python3 make_symbols_font.py SymbolsNerdFontMono-Regular.ttf DiriSymbolsMono-Regular.ttf

Requires fontTools (`pip install fonttools`). The output is checked in, so a
build never runs this.

Three changes, all so the font is a fallback that only fills gaps:

* Only Private Use Area code points are kept. The upstream font also maps 14
  ordinary symbols (U+2665, U+26A1, U+276F, ...) that system fonts already
  draw, and keeping them would change how those look.
* Every glyph is scaled into one terminal cell. Upstream draws icons in a
  1 em square; a monospaced cell is 0.6 em wide, so an unscaled icon spills
  into the next column. Icons keep their aspect ratio and are centred where
  text sits. The powerline extras (U+E0C0-U+E0D7) are stretched to the full
  line box instead, as patched Nerd Font Mono families do.
* The family is renamed so it can never be confused with (or shadow) a copy
  of the upstream font the user installed.
"""

import sys

from fontTools.pens.transformPen import TransformPen
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.subset import Options, Subsetter
from fontTools.ttLib import TTFont

FAMILY = "Diri Symbols Mono"
POSTSCRIPT = "DiriSymbolsMono-Regular"

UPEM = 2048
CELL = round(0.6 * UPEM)  # advance of SF Mono, Menlo, JetBrains Mono, Fira Code
ICON_SCALE = 0.58  # leaves a hairline between neighbouring icons
TEXT_CENTER = round(0.35 * UPEM)  # midline of a monospaced line box
SOURCE_CENTER = (1024, 614)  # centre of upstream's -410..1638 em square
LINE_TOP = round(0.94 * UPEM)
LINE_BOTTOM = round(-0.22 * UPEM)


def is_private_use(cp):
    return 0xE000 <= cp <= 0xF8FF or 0xF0000 <= cp <= 0x10FFFD


def is_stretched(cp):
    return 0xE0C0 <= cp <= 0xE0D7


def main(source, target):
    font = TTFont(source)
    keep = [cp for cp in font.getBestCmap() if is_private_use(cp)]
    options = Options()
    options.hinting = False
    options.layout_features = []
    options.name_IDs = ["*"]
    options.notdef_outline = True
    subsetter = Subsetter(options)
    subsetter.populate(unicodes=keep)
    subsetter.subset(font)

    assert font["head"].unitsPerEm == UPEM
    glyf = font["glyf"]
    hmtx = font["hmtx"]
    cmap = font.getBestCmap()
    stretched = {name for cp, name in cmap.items() if is_stretched(cp)}
    glyph_set = font.getGlyphSet()
    for name in font.getGlyphOrder():
        glyph = glyf[name]
        if glyph.numberOfContours == 0:
            hmtx[name] = (CELL, 0)
            continue
        if name in stretched:
            glyph.recalcBounds(glyf)
            sx = CELL / max(glyph.xMax - glyph.xMin, 1)
            sy = (LINE_TOP - LINE_BOTTOM) / max(glyph.yMax - glyph.yMin, 1)
            matrix = (sx, 0, 0, sy, -glyph.xMin * sx, LINE_BOTTOM - glyph.yMin * sy)
        else:
            s = ICON_SCALE
            cx, cy = SOURCE_CENTER
            matrix = (s, 0, 0, s, CELL / 2 - cx * s, TEXT_CENTER - cy * s)
        pen = TTGlyphPen(glyph_set)
        glyph_set[name].draw(TransformPen(pen, matrix))
        glyf[name] = pen.glyph()
        glyf[name].recalcBounds(glyf)
        hmtx[name] = (CELL, glyf[name].xMin)

    hhea = font["hhea"]
    hhea.advanceWidthMax = CELL
    os2 = font["OS/2"]
    os2.xAvgCharWidth = CELL
    font["post"].isFixedPitch = 1
    os2.panose.bProportion = 9  # monospaced

    names = font["name"]
    for record in list(names.names):
        if record.nameID in (1, 3, 4, 6, 16, 17, 18, 21, 22):
            names.removeNames(nameID=record.nameID)
    for name_id, value in (
        (1, FAMILY),
        (3, f"{POSTSCRIPT};derived from Symbols Nerd Font Mono 3.5.1"),
        (4, FAMILY),
        (6, POSTSCRIPT),
    ):
        names.setName(value, name_id, 3, 1, 0x409)
        names.setName(value, name_id, 1, 0, 0)
    font.save(target)


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
