import CoreText
import UIKit

/// Font metrics for one size. Cells are a fixed grid: every glyph is placed at
/// its column, never where the previous glyph's advance happened to end, so a
/// CJK or emoji fallback font cannot drift the rest of the row.
final class TerminalFont {
    let size: CGFloat
    let regular: CTFont
    let bold: CTFont
    let italic: CTFont
    let boldItalic: CTFont
    let cellWidth: CGFloat
    let lineHeight: CGFloat
    let ascent: CGFloat
    let descent: CGFloat
    let scale: CGFloat
    /// Whether the regular face has glyphs for a scalar.
    private var covered: [UInt32: Bool] = [:]

    init(size: CGFloat, scale: CGFloat) {
        self.size = size
        self.scale = scale
        let base = UIFont.monospacedSystemFont(ofSize: size, weight: .regular)
        let heavy = UIFont.monospacedSystemFont(ofSize: size, weight: .semibold)
        regular = base as CTFont
        bold = heavy as CTFont
        italic = CTFontCreateCopyWithSymbolicTraits(regular, size, nil, .traitItalic, .traitItalic) ?? regular
        boldItalic = CTFontCreateCopyWithSymbolicTraits(bold, size, nil, .traitItalic, .traitItalic) ?? bold

        var glyph = CGGlyph(0)
        var character = UniChar(UInt8(ascii: "M"))
        CTFontGetGlyphsForCharacters(regular, &character, &glyph, 1)
        var advance = CGSize.zero
        CTFontGetAdvancesForGlyphs(regular, .horizontal, &glyph, &advance, 1)
        cellWidth = advance.width
        ascent = CTFontGetAscent(regular)
        descent = CTFontGetDescent(regular)
        // Pixel-aligned rows: a fractional height smears every line edge.
        lineHeight = (ceil((ascent + descent) * 1.18 * scale)) / scale
    }

    /// True when the regular face draws `character` itself (one cell wide),
    /// so it can share a Core Text line with its neighbours.
    func covers(_ character: Character) -> Bool {
        guard character.unicodeScalars.count == 1, let scalar = character.unicodeScalars.first else { return false }
        if let known = covered[scalar.value] { return known }
        let utf16 = Array(String(character).utf16)
        var glyphs = [CGGlyph](repeating: 0, count: utf16.count)
        let found = CTFontGetGlyphsForCharacters(regular, utf16, &glyphs, utf16.count)
        covered[scalar.value] = found
        return found
    }

    func font(for attributes: TermAttributes) -> CTFont {
        switch (attributes.contains(.bold), attributes.contains(.italic)) {
        case (true, true): boldItalic
        case (true, false): bold
        case (false, true): italic
        case (false, false): regular
        }
    }
}

/// Resolved colours for one palette, cached as CGColor so a repaint allocates
/// nothing per cell.
final class TerminalColors {
    let palette: TermPalette
    let foreground: CGColor
    let background: CGColor
    private var table: [CGColor]

    init(palette: TermPalette) {
        self.palette = palette
        foreground = Self.cg(palette.foreground)
        background = Self.cg(palette.background)
        table = (0 ... 255).map { Self.cg(palette.color(index: UInt8($0))) }
    }

    func resolve(_ color: TermColor, foreground isForeground: Bool) -> CGColor {
        switch color {
        case .standard: isForeground ? foreground : background
        case let .index(i): table[Int(i)]
        case let .rgb(value): Self.cg(value)
        }
    }

    static func cg(_ value: UInt32, alpha: CGFloat = 1) -> CGColor {
        CGColor(
            srgbRed: CGFloat((value >> 16) & 0xFF) / 255,
            green: CGFloat((value >> 8) & 0xFF) / 255,
            blue: CGFloat(value & 0xFF) / 255,
            alpha: alpha
        )
    }
}

/// Draws one terminal line with Core Text.
enum LineRenderer {
    /// Draws `line` into a context whose origin is the line's top-left, in
    /// UIKit's flipped coordinates.
    static func draw(_ line: TermLine, in context: CGContext, font: TerminalFont, colors: TerminalColors) {
        var column = 0
        let cell = font.cellWidth
        let height = font.lineHeight
        let baseline = (height - (font.ascent + font.descent)) / 2 + font.ascent

        // Backgrounds first, in one pass, so glyph overhang is never clipped
        // by the next run's fill.
        for run in line.runs {
            let width = CellWidth.of(run.text)
            defer { column += width }
            let inverse = run.attributes.contains(.inverse)
            let bgColor: TermColor = inverse ? run.fg : run.bg
            guard bgColor != .standard || inverse else { continue }
            let fill = inverse && run.fg == .standard ? colors.foreground : colors.resolve(bgColor, foreground: false)
            context.setFillColor(fill)
            context.fill(CGRect(x: CGFloat(column) * cell, y: 0, width: CGFloat(width) * cell, height: height))
        }

        context.saveGState()
        context.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
        column = 0
        for run in line.runs {
            let width = CellWidth.of(run.text)
            defer { column += width }
            let attributes = run.attributes
            if attributes.contains(.invisible) { continue }
            let inverse = attributes.contains(.inverse)
            var fg: CGColor = inverse
                ? (run.bg == .standard ? colors.background : colors.resolve(run.bg, foreground: false))
                : colors.resolve(run.fg, foreground: true)
            if attributes.contains(.dim) { fg = fg.copy(alpha: 0.55) ?? fg }
            let ctFont = font.font(for: attributes)
            let x0 = CGFloat(column) * cell

            if run.text.utf8.allSatisfy({ $0 >= 0x20 && $0 < 0x7F }) {
                // ASCII: one Core Text line for the whole run — a monospaced
                // font's advance already is the cell width.
                if run.text.contains(where: { $0 != " " }) {
                    drawText(run.text, x: x0, baseline: baseline, font: ctFont, color: fg, in: context)
                }
            } else {
                drawMixed(run.text, from: column, baseline: baseline, font: font, face: ctFont, color: fg, in: context)
            }

            if attributes.contains(.underline) || attributes.contains(.strike) {
                context.setStrokeColor(fg)
                context.setLineWidth(max(1, font.size / 14))
                let span = CGFloat(width) * cell
                if attributes.contains(.underline) {
                    let y = baseline + max(1.5, font.descent * 0.5)
                    context.strokeLineSegments(between: [CGPoint(x: x0, y: y), CGPoint(x: x0 + span, y: y)])
                }
                if attributes.contains(.strike) {
                    let y = baseline - font.ascent * 0.32
                    context.strokeLineSegments(between: [CGPoint(x: x0, y: y), CGPoint(x: x0 + span, y: y)])
                }
            }
        }
        context.restoreGState()
    }

    /// A run with non-ASCII text. Characters the terminal face draws itself
    /// (`─`, `é`, `⎿` …) are batched into one Core Text line per stretch, as
    /// ASCII is; box-drawing and block characters are drawn as geometry; only
    /// wide or fallback-font characters get a line each, centred in their
    /// cells. A border row is now a few rect fills, not one line per cell.
    private static func drawMixed(_ text: String, from start: Int, baseline: CGFloat, font: TerminalFont,
                                  face: CTFont, color: CGColor, in context: CGContext) {
        let cell = font.cellWidth
        var col = start
        var pending = ""
        var pendingStart = start
        func flush() {
            if pending.contains(where: { $0 != " " }) {
                drawText(pending, x: CGFloat(pendingStart) * cell, baseline: baseline, font: face, color: color, in: context)
            }
            pending = ""
        }
        // A stretch of one horizontal rule character, drawn as one bar.
        var rule: (scalar: Unicode.Scalar, start: Int, count: Int)?
        func flushRule() {
            guard let current = rule else { return }
            BoxDrawing.draw(current.scalar, in: CGRect(x: CGFloat(current.start) * cell, y: 0,
                                                       width: CGFloat(current.count) * cell, height: font.lineHeight),
                            scale: font.scale, color: color, context: context)
            rule = nil
        }
        for character in text {
            let w = CellWidth.of(character)
            if w == 1, let scalar = character.unicodeScalars.first, character.unicodeScalars.count == 1,
               BoxDrawing.handles(scalar) {
                flush()
                if BoxDrawing.isRule(scalar) {
                    if let current = rule, current.scalar == scalar {
                        rule?.count += 1
                    } else {
                        flushRule()
                        rule = (scalar, col, 1)
                    }
                } else {
                    flushRule()
                    BoxDrawing.draw(scalar, in: CGRect(x: CGFloat(col) * cell, y: 0, width: cell, height: font.lineHeight),
                                    scale: font.scale, color: color, context: context)
                }
                col += 1
                pendingStart = col
                continue
            }
            flushRule()
            let substituted = substitute(character)
            if w == 1, substituted == String(character), font.covers(character) {
                if pending.isEmpty { pendingStart = col }
                pending.append(character)
                col += 1
                continue
            }
            flush()
            if character != " " {
                // Core Text's own cascade from the terminal face picks the
                // font for a character the face lacks.
                drawText(substituted, x: CGFloat(col) * cell, baseline: baseline,
                         font: face, color: color, in: context, fitting: CGFloat(max(w, 1)) * cell)
            }
            col += w
            pendingStart = col
        }
        flush()
        flushRule()
    }

    /// Glyphs SF Mono lacks whose fallback font draws them as something else
    /// entirely (⏺ comes out as a capsule): drawn as their nearest text form.
    private static func substitute(_ character: Character) -> String {
        switch character {
        case "⏺": "●"
        case "⏵": "▸"
        default: String(character)
        }
    }

    private static func drawText(_ text: String, x: CGFloat, baseline: CGFloat, font: CTFont, color: CGColor,
                                 in context: CGContext, fitting: CGFloat? = nil) {
        let attributed = NSAttributedString(string: text, attributes: [
            NSAttributedString.Key(kCTFontAttributeName as String): font,
            NSAttributedString.Key(kCTForegroundColorAttributeName as String): color,
        ])
        let line = CTLineCreateWithAttributedString(attributed)
        var dx: CGFloat = 0
        if let fitting {
            // Centre a fallback glyph in its cells; a wider one is squeezed
            // rather than allowed to push into the next cell.
            let width = CGFloat(CTLineGetTypographicBounds(line, nil, nil, nil))
            if width > fitting * 1.05 {
                context.saveGState()
                context.translateBy(x: x, y: baseline)
                context.scaleBy(x: fitting / width, y: 1)
                context.textPosition = .zero
                CTLineDraw(line, context)
                context.restoreGState()
                return
            }
            dx = max(0, (fitting - width) / 2)
        }
        context.textPosition = CGPoint(x: x + dx, y: baseline)
        CTLineDraw(line, context)
    }
}

/// How many cells a character occupies: 0 for combining marks, 2 for East
/// Asian wide and emoji presentation, else 1. Matches the server's grid, which
/// is what keeps the columns of a TUI lined up.
enum CellWidth {
    static func of(_ text: String) -> Int {
        if text.utf8.allSatisfy({ $0 < 0x80 }) { return text.utf8.count }
        return text.reduce(0) { $0 + of($1) }
    }

    static func of(_ character: Character) -> Int {
        let scalars = character.unicodeScalars
        guard let first = scalars.first else { return 0 }
        if first.value < 0x80 { return 1 }
        if scalars.contains(where: { $0.value == 0xFE0F }) { return 2 }
        if scalars.count > 1, scalars.contains(where: { $0.properties.isEmojiPresentation }) { return 2 }
        return of(first)
    }

    static func of(_ scalar: Unicode.Scalar) -> Int {
        let v = scalar.value
        switch scalar.properties.generalCategory {
        case .nonspacingMark, .enclosingMark, .format: return 0
        default: break
        }
        if v == 0x200B { return 0 }
        if scalar.properties.isEmojiPresentation { return 2 }
        switch v {
        case 0x1100 ... 0x115F, 0x231A ... 0x231B, 0x2329 ... 0x232A, 0x2E80 ... 0x303E, 0x3041 ... 0x33FF,
             0x3400 ... 0x4DBF, 0x4E00 ... 0x9FFF, 0xA000 ... 0xA4CF, 0xA960 ... 0xA97F, 0xAC00 ... 0xD7A3,
             0xF900 ... 0xFAFF, 0xFE10 ... 0xFE19, 0xFE30 ... 0xFE6F, 0xFF00 ... 0xFF60, 0xFFE0 ... 0xFFE6,
             0x1F300 ... 0x1F64F, 0x1F900 ... 0x1F9FF, 0x20000 ... 0x2FFFD, 0x30000 ... 0x3FFFD:
            return 2
        default:
            return 1
        }
    }
}
