import CoreGraphics
import Foundation

/// A minimal SVG path-data parser, mirroring `diri-ui`'s `SvgPath::parse`.
///
/// The agent marks are authored once, as 24×24 path data, and every platform
/// parses that same string. Rasterising them into image assets instead would
/// mean a second source of truth that silently goes stale the day someone
/// edits the Rust constant.
///
/// Supports the full command set the bundled marks use — `M L H V C S Q T A Z`
/// in both cases — because `OPENAI_PATH` and `CURSOR_PATH` contain elliptical
/// arcs and `GEMINI_PATH` contains quadratics.
enum SVGPath {
    /// Builds a `CGPath` in the source view box, scaled to fit `size` and
    /// centred. Returns an empty path if the data cannot be parsed, which
    /// renders as nothing rather than crashing a session list.
    static func cgPath(from data: String, viewBox: CGFloat = 24, size: CGFloat) -> CGPath {
        let path = CGMutablePath()
        var scanner = Scanner(data: data)
        var current = CGPoint.zero
        var subpathStart = CGPoint.zero
        // The reflection anchors for smooth curves (`S`/`T`). Nil unless the
        // previous command was of the matching family, per the SVG spec.
        var lastCubicControl: CGPoint?
        var lastQuadControl: CGPoint?

        while let command = scanner.nextCommand() {
            let absolute = command.isUppercase
            repeat {
                switch Character(command.lowercased()) {
                case "m":
                    guard let point = scanner.point(relativeTo: current, absolute: absolute) else { break }
                    current = point
                    subpathStart = point
                    path.move(to: point)
                    lastCubicControl = nil
                    lastQuadControl = nil
                    // Per spec, extra coordinate pairs after a moveto are
                    // implicit linetos — which is how these marks encode runs.
                    while scanner.hasMoreArguments {
                        guard let next = scanner.point(relativeTo: current, absolute: absolute) else { break }
                        path.addLine(to: next)
                        current = next
                    }
                case "l":
                    guard let point = scanner.point(relativeTo: current, absolute: absolute) else { break }
                    path.addLine(to: point)
                    current = point
                    lastCubicControl = nil
                    lastQuadControl = nil
                case "h":
                    guard let value = scanner.number() else { break }
                    current = CGPoint(x: absolute ? value : current.x + value, y: current.y)
                    path.addLine(to: current)
                    lastCubicControl = nil
                    lastQuadControl = nil
                case "v":
                    guard let value = scanner.number() else { break }
                    current = CGPoint(x: current.x, y: absolute ? value : current.y + value)
                    path.addLine(to: current)
                    lastCubicControl = nil
                    lastQuadControl = nil
                case "c":
                    guard let one = scanner.point(relativeTo: current, absolute: absolute),
                          let two = scanner.point(relativeTo: current, absolute: absolute),
                          let end = scanner.point(relativeTo: current, absolute: absolute)
                    else { break }
                    path.addCurve(to: end, control1: one, control2: two)
                    current = end
                    lastCubicControl = two
                    lastQuadControl = nil
                case "s":
                    let reflected = reflect(lastCubicControl, around: current)
                    guard let two = scanner.point(relativeTo: current, absolute: absolute),
                          let end = scanner.point(relativeTo: current, absolute: absolute)
                    else { break }
                    path.addCurve(to: end, control1: reflected, control2: two)
                    current = end
                    lastCubicControl = two
                    lastQuadControl = nil
                case "q":
                    guard let control = scanner.point(relativeTo: current, absolute: absolute),
                          let end = scanner.point(relativeTo: current, absolute: absolute)
                    else { break }
                    path.addQuadCurve(to: end, control: control)
                    current = end
                    lastQuadControl = control
                    lastCubicControl = nil
                case "t":
                    let reflected = reflect(lastQuadControl, around: current)
                    guard let end = scanner.point(relativeTo: current, absolute: absolute) else { break }
                    path.addQuadCurve(to: end, control: reflected)
                    current = end
                    lastQuadControl = reflected
                    lastCubicControl = nil
                case "a":
                    guard let rx = scanner.number(), let ry = scanner.number(),
                          let rotation = scanner.number(),
                          let largeArc = scanner.flag(), let sweep = scanner.flag(),
                          let end = scanner.point(relativeTo: current, absolute: absolute)
                    else { break }
                    addArc(
                        to: path, from: current, to: end,
                        rx: rx, ry: ry, rotationDegrees: rotation,
                        largeArc: largeArc, sweep: sweep
                    )
                    current = end
                    lastCubicControl = nil
                    lastQuadControl = nil
                case "z":
                    path.closeSubpath()
                    current = subpathStart
                    lastCubicControl = nil
                    lastQuadControl = nil
                default:
                    return scaled(path, viewBox: viewBox, size: size)
                }
                // A command repeats while bare arguments keep arriving, which
                // is how `m4.7 15.9 4.7-2.6 …` encodes a polyline.
            } while scanner.hasMoreArguments && Character(command.lowercased()) != "z"
        }

        return scaled(path, viewBox: viewBox, size: size)
    }

    private static func reflect(_ control: CGPoint?, around point: CGPoint) -> CGPoint {
        guard let control else { return point }
        return CGPoint(x: 2 * point.x - control.x, y: 2 * point.y - control.y)
    }

    private static func scaled(_ path: CGMutablePath, viewBox: CGFloat, size: CGFloat) -> CGPath {
        let factor = size / viewBox
        var transform = CGAffineTransform(scaleX: factor, y: factor)
        return path.copy(using: &transform) ?? path
    }

    /// Endpoint-parameterisation → centre-parameterisation, per SVG 1.1
    /// appendix F.6. `CGPath` has no elliptical-arc primitive, so the arc is
    /// converted to a centre, angles and radii and then drawn as a unit arc
    /// under the ellipse's own transform.
    private static func addArc(
        to path: CGMutablePath,
        from start: CGPoint,
        to end: CGPoint,
        rx: CGFloat,
        ry: CGFloat,
        rotationDegrees: CGFloat,
        largeArc: Bool,
        sweep: Bool
    ) {
        // Degenerate radii mean a straight line, and equal endpoints mean the
        // arc is omitted entirely — both are spec-mandated, not shortcuts.
        guard rx != 0, ry != 0 else {
            path.addLine(to: end)
            return
        }
        if start == end { return }

        var rx = abs(rx)
        var ry = abs(ry)
        let phi = rotationDegrees * .pi / 180
        let cosPhi = cos(phi)
        let sinPhi = sin(phi)

        let dx2 = (start.x - end.x) / 2
        let dy2 = (start.y - end.y) / 2
        let x1p = cosPhi * dx2 + sinPhi * dy2
        let y1p = -sinPhi * dx2 + cosPhi * dy2

        // Scale the radii up if they are too small to span the endpoints.
        let lambda = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry)
        if lambda > 1 {
            let scale = sqrt(lambda)
            rx *= scale
            ry *= scale
        }

        let sign: CGFloat = largeArc == sweep ? -1 : 1
        let numerator = max(0, rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p)
        let denominator = rx * rx * y1p * y1p + ry * ry * x1p * x1p
        let coefficient = denominator == 0 ? 0 : sign * sqrt(numerator / denominator)

        let cxp = coefficient * rx * y1p / ry
        let cyp = -coefficient * ry * x1p / rx
        let center = CGPoint(
            x: cosPhi * cxp - sinPhi * cyp + (start.x + end.x) / 2,
            y: sinPhi * cxp + cosPhi * cyp + (start.y + end.y) / 2
        )

        let startAngle = angle(
            ux: (x1p - cxp) / rx, uy: (y1p - cyp) / ry,
            vx: 1, vy: 0
        )
        var delta = angle(
            ux: (x1p - cxp) / rx, uy: (y1p - cyp) / ry,
            vx: (-x1p - cxp) / rx, vy: (-y1p - cyp) / ry
        )
        if !sweep, delta > 0 { delta -= 2 * .pi }
        if sweep, delta < 0 { delta += 2 * .pi }

        // Draw a unit arc and let the ellipse's own transform give it radii and
        // rotation — `CGPath` has no elliptical-arc primitive of its own.
        let transform = CGAffineTransform(translationX: center.x, y: center.y)
            .rotated(by: phi)
            .scaledBy(x: rx, y: ry)
        path.addArc(
            center: .zero,
            radius: 1,
            startAngle: startAngle,
            endAngle: startAngle + delta,
            clockwise: delta < 0,
            transform: transform
        )
    }

    /// Signed angle between two vectors, the form F.6.5 needs.
    private static func angle(ux: CGFloat, uy: CGFloat, vx: CGFloat, vy: CGFloat) -> CGFloat {
        let dot = ux * vx + uy * vy
        let length = sqrt(ux * ux + uy * uy) * sqrt(vx * vx + vy * vy)
        guard length != 0 else { return 0 }
        let clamped = min(1, max(-1, dot / length))
        let result = acos(clamped)
        return (ux * vy - uy * vx) < 0 ? -result : result
    }
}

// MARK: - Tokenizer

/// SVG path data is not whitespace-delimited: `1.5.3` is two numbers, `-2` may
/// follow a digit with no separator, and arc flags are single characters that
/// may run straight into the next number. A hand-rolled scanner is the honest
/// way to read it.
private struct Scanner {
    private let characters: [Character]
    private var index: Int = 0

    init(data: String) {
        characters = Array(data)
    }

    private mutating func skipSeparators() {
        while index < characters.count, characters[index] == " " || characters[index] == ","
            || characters[index] == "\n" || characters[index] == "\t" || characters[index] == "\r"
        {
            index += 1
        }
    }

    mutating func nextCommand() -> Character? {
        skipSeparators()
        guard index < characters.count else { return nil }
        let character = characters[index]
        guard character.isLetter else { return nil }
        index += 1
        return character
    }

    /// True when the next token is a number, meaning the current command
    /// repeats with another set of arguments.
    var hasMoreArguments: Bool {
        var probe = index
        while probe < characters.count,
              characters[probe] == " " || characters[probe] == "," || characters[probe] == "\n"
              || characters[probe] == "\t" || characters[probe] == "\r"
        {
            probe += 1
        }
        guard probe < characters.count else { return false }
        let character = characters[probe]
        return character.isNumber || character == "-" || character == "+" || character == "."
    }

    mutating func number() -> CGFloat? {
        skipSeparators()
        guard index < characters.count else { return nil }
        var text = ""
        if characters[index] == "-" || characters[index] == "+" {
            text.append(characters[index])
            index += 1
        }
        var seenDot = false
        var seenExponent = false
        while index < characters.count {
            let character = characters[index]
            if character.isNumber {
                text.append(character)
                index += 1
            } else if character == ".", !seenDot, !seenExponent {
                seenDot = true
                text.append(character)
                index += 1
            } else if character == "e" || character == "E", !seenExponent, !text.isEmpty {
                seenExponent = true
                text.append(character)
                index += 1
                if index < characters.count, characters[index] == "-" || characters[index] == "+" {
                    text.append(characters[index])
                    index += 1
                }
            } else {
                break
            }
        }
        guard let value = Double(text) else { return nil }
        return CGFloat(value)
    }

    /// Arc flags are exactly one character wide, so `0 0 1` and `001` are the
    /// same three flags — reading them as numbers would swallow all three.
    mutating func flag() -> Bool? {
        skipSeparators()
        guard index < characters.count else { return nil }
        let character = characters[index]
        guard character == "0" || character == "1" else { return nil }
        index += 1
        return character == "1"
    }

    mutating func point(relativeTo current: CGPoint, absolute: Bool) -> CGPoint? {
        guard let x = number(), let y = number() else { return nil }
        return absolute ? CGPoint(x: x, y: y) : CGPoint(x: current.x + x, y: current.y + y)
    }
}
