import CoreGraphics

/// Box-drawing (U+2500–U+257F) and block elements (U+2580–U+259F) drawn as
/// geometry on the cell grid, the way terminals do, instead of as font glyphs.
///
/// A glyph from the font (or its fallback) neither fills the cell's height nor
/// meets its neighbour exactly, so a TUI's borders broke into dashes with gaps
/// between rows. Geometry that runs edge to edge joins up, and it costs a few
/// rect fills instead of a Core Text line per character.
enum BoxDrawing {
    /// Per code point from U+2500: the weight of each arm, two bits apiece —
    /// up, right, down, left from the high bits; 1 light, 2 heavy, 3 double.
    /// Generated from the Unicode character names; 0 is a diagonal (left to
    /// the font). Dashed lines are drawn solid.
    private static let arms: [UInt8] = [
        0x11, 0x22, 0x44, 0x88, 0x11, 0x22, 0x44, 0x88, 0x11, 0x22, 0x44, 0x88, 0x14, 0x24, 0x18, 0x28,
        0x05, 0x06, 0x09, 0x0A, 0x50, 0x60, 0x90, 0xA0, 0x41, 0x42, 0x81, 0x82, 0x54, 0x64, 0x94, 0x58,
        0x98, 0xA4, 0x68, 0xA8, 0x45, 0x46, 0x85, 0x49, 0x89, 0x86, 0x4A, 0x8A, 0x15, 0x16, 0x25, 0x26,
        0x19, 0x1A, 0x29, 0x2A, 0x51, 0x52, 0x61, 0x62, 0x91, 0x92, 0xA1, 0xA2, 0x55, 0x56, 0x65, 0x66,
        0x95, 0x59, 0x99, 0x96, 0xA5, 0x5A, 0x69, 0xA6, 0x6A, 0x9A, 0xA9, 0xAA, 0x11, 0x22, 0x44, 0x88,
        0x33, 0xCC, 0x34, 0x1C, 0x3C, 0x07, 0x0D, 0x0F, 0x70, 0xD0, 0xF0, 0x43, 0xC1, 0xC3, 0x74, 0xDC,
        0xFC, 0x47, 0xCD, 0xCF, 0x37, 0x1D, 0x3F, 0x73, 0xD1, 0xF3, 0x77, 0xDD, 0xFF, 0x14, 0x05, 0x41,
        0x50, 0x00, 0x00, 0x00, 0x01, 0x40, 0x10, 0x04, 0x02, 0x80, 0x20, 0x08, 0x21, 0x48, 0x12, 0x84,
    ]

    /// Whether `scalar` is drawn here rather than by a font.
    static func handles(_ scalar: Unicode.Scalar) -> Bool {
        let v = scalar.value
        if v >= 0x2580, v <= 0x259F { return true }
        guard v >= 0x2500, v <= 0x257F else { return false }
        return arms[Int(v - 0x2500)] != 0
    }

    /// A purely horizontal line (`─ ━ ═` and their dashed forms): a run of
    /// them can be drawn as one bar across all its cells.
    static func isRule(_ scalar: Unicode.Scalar) -> Bool {
        let v = scalar.value
        guard v >= 0x2500, v <= 0x257F else { return false }
        let packed = arms[Int(v - 0x2500)]
        return packed >> 6 == 0 && packed >> 2 & 3 == 0 && packed >> 4 & 3 == packed & 3 && packed & 3 != 0
    }

    /// Draws `scalar` into `cell` (UIKit-flipped: y grows down). `scale` is
    /// the screen scale, for pixel-aligned strokes.
    static func draw(_ scalar: Unicode.Scalar, in cell: CGRect, scale: CGFloat, color: CGColor, context: CGContext) {
        let v = scalar.value
        context.setFillColor(color)
        if v >= 0x2580 {
            block(v, in: cell, color: color, context: context)
            return
        }
        let packed = arms[Int(v - 0x2500)]
        let up = Int(packed >> 6 & 3), right = Int(packed >> 4 & 3), down = Int(packed >> 2 & 3), left = Int(packed & 3)
        let pixel = 1 / max(scale, 1)
        // From the height, which a run of rules drawn as one bar shares.
        let light = max(pixel, (cell.height / 18 * scale).rounded() / scale)
        let heavy = light * 2
        let snap = { (value: CGFloat) in (value * scale).rounded() / scale }
        let cx = snap(cell.midX)
        let cy = snap(cell.midY)

        if (0x256D ... 0x2570).contains(v) {
            arc(v, in: cell, cx: cx, cy: cy, width: light, color: color, context: context)
            return
        }

        func width(_ weight: Int) -> CGFloat { weight == 2 ? heavy : light }
        // Arms reach past the centre by half the widest crossing arm, so a
        // corner or tee closes without a notch.
        let vertical = max(width(up), width(down))
        let horizontal = max(width(left), width(right))
        func arm(_ weight: Int, _ rect: (CGFloat) -> [CGRect]) {
            guard weight != 0 else { return }
            if weight == 3 {
                let gap = light
                for offset in [-gap, gap] { for r in rect(light) { context.fill(r.offsetBy(dx: offset * r.isVertical, dy: offset * r.isHorizontal)) } }
            } else {
                for r in rect(width(weight)) { context.fill(r) }
            }
        }
        arm(up) { w in [CGRect(x: cx - w / 2, y: cell.minY, width: w, height: cy - cell.minY + horizontal / 2)] }
        arm(down) { w in [CGRect(x: cx - w / 2, y: cy - horizontal / 2, width: w, height: cell.maxY - cy + horizontal / 2)] }
        arm(left) { w in [CGRect(x: cell.minX, y: cy - w / 2, width: cx - cell.minX + vertical / 2, height: w)] }
        arm(right) { w in [CGRect(x: cx - vertical / 2, y: cy - w / 2, width: cell.maxX - cx + vertical / 2, height: w)] }
    }

    private static func arc(_ v: UInt32, in cell: CGRect, cx: CGFloat, cy: CGFloat, width: CGFloat,
                            color: CGColor, context: CGContext) {
        let radius = min(cell.width, cell.height) / 2
        let path = CGMutablePath()
        switch v {
        case 0x256D: // ╭ down and right
            path.move(to: CGPoint(x: cx, y: cell.maxY))
            path.addLine(to: CGPoint(x: cx, y: cy + radius))
            path.addArc(tangent1End: CGPoint(x: cx, y: cy), tangent2End: CGPoint(x: cx + radius, y: cy), radius: radius)
            path.addLine(to: CGPoint(x: cell.maxX, y: cy))
        case 0x256E: // ╮ down and left
            path.move(to: CGPoint(x: cx, y: cell.maxY))
            path.addLine(to: CGPoint(x: cx, y: cy + radius))
            path.addArc(tangent1End: CGPoint(x: cx, y: cy), tangent2End: CGPoint(x: cx - radius, y: cy), radius: radius)
            path.addLine(to: CGPoint(x: cell.minX, y: cy))
        case 0x256F: // ╯ up and left
            path.move(to: CGPoint(x: cx, y: cell.minY))
            path.addLine(to: CGPoint(x: cx, y: cy - radius))
            path.addArc(tangent1End: CGPoint(x: cx, y: cy), tangent2End: CGPoint(x: cx - radius, y: cy), radius: radius)
            path.addLine(to: CGPoint(x: cell.minX, y: cy))
        default: // ╰ up and right
            path.move(to: CGPoint(x: cx, y: cell.minY))
            path.addLine(to: CGPoint(x: cx, y: cy - radius))
            path.addArc(tangent1End: CGPoint(x: cx, y: cy), tangent2End: CGPoint(x: cx + radius, y: cy), radius: radius)
            path.addLine(to: CGPoint(x: cell.maxX, y: cy))
        }
        context.saveGState()
        context.setStrokeColor(color)
        context.setLineWidth(width)
        context.setLineCap(.butt)
        context.addPath(path)
        context.strokePath()
        context.restoreGState()
    }

    private static func block(_ v: UInt32, in cell: CGRect, color: CGColor, context: CGContext) {
        let (x, y, w, h) = (cell.minX, cell.minY, cell.width, cell.height)
        func fill(_ fx: CGFloat, _ fy: CGFloat, _ fw: CGFloat, _ fh: CGFloat) {
            context.fill(CGRect(x: x + fx * w, y: y + fy * h, width: fw * w, height: fh * h))
        }
        switch v {
        case 0x2580: fill(0, 0, 1, 0.5)
        case 0x2581 ... 0x2587:
            let eighths = CGFloat(v - 0x2580) / 8
            fill(0, 1 - eighths, 1, eighths)
        case 0x2588: fill(0, 0, 1, 1)
        case 0x2589 ... 0x258F:
            let eighths = CGFloat(0x2590 - v) / 8
            fill(0, 0, eighths, 1)
        case 0x2590: fill(0.5, 0, 0.5, 1)
        case 0x2591 ... 0x2593:
            context.setFillColor(color.copy(alpha: color.alpha * CGFloat(v - 0x2590) / 4) ?? color)
            fill(0, 0, 1, 1)
        case 0x2594: fill(0, 0, 1, 0.125)
        case 0x2595: fill(0.875, 0, 0.125, 1)
        default:
            // Quadrants: upper-left, upper-right, lower-left, lower-right.
            let quadrants: [UInt32: [Int]] = [
                0x2596: [2], 0x2597: [3], 0x2598: [0], 0x2599: [0, 2, 3], 0x259A: [0, 3],
                0x259B: [0, 1, 2], 0x259C: [0, 1, 3], 0x259D: [1], 0x259E: [1, 2], 0x259F: [1, 2, 3],
            ]
            for q in quadrants[v] ?? [] { fill(q % 2 == 0 ? 0 : 0.5, q < 2 ? 0 : 0.5, 0.5, 0.5) }
        }
    }
}

private extension CGRect {
    /// 1 for a tall (vertical) stroke, else 0: which way a double line's two
    /// strokes are spread.
    var isVertical: CGFloat { height > width ? 1 : 0 }
    var isHorizontal: CGFloat { height > width ? 0 : 1 }
}
