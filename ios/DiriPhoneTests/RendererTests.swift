import CoreText
import UIKit
import XCTest
@testable import DiriPhone

/// The line renderer on the cases a coding agent's screen is made of:
/// box-drawn borders, CJK and emoji, and its cost per row.
final class RendererTests: XCTestCase {
    private let font = TerminalFont(size: 11, scale: 3)
    private let colors = TerminalColors(palette: .standard)

    private func render(_ line: TermLine, cols: Int) -> (CGContext, Int, Int) {
        let width = Int(ceil(CGFloat(cols) * font.cellWidth * 3)), height = Int(ceil(font.lineHeight * 3))
        let context = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: width * 4,
                                space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        context.translateBy(x: 0, y: CGFloat(height))
        context.scaleBy(x: 3, y: -3)
        context.setFillColor(colors.background)
        context.fill(CGRect(x: 0, y: 0, width: CGFloat(width), height: CGFloat(height)))
        LineRenderer.draw(line, in: context, font: font, colors: colors)
        return (context, width, height)
    }

    /// Red channel at a device pixel, top-left origin.
    private func red(_ context: CGContext, _ x: Int, _ y: Int) -> UInt8 {
        let data = context.data!.assumingMemoryBound(to: UInt8.self)
        let row = context.height - 1 - y
        return data[row * context.bytesPerRow + x * 4]
    }

    func testBordersRunEdgeToEdgeSoRowsJoin() {
        // A red vertical bar: lit at the very top and bottom pixel of its cell,
        // which is what makes stacked rows join into one line.
        let (bar, _, height) = render(TermLine(runs: [TermRun("│", fg: .rgb(0xFF0000))]), cols: 1)
        let mid = Int(font.cellWidth * 3 / 2)
        XCTAssertGreaterThan(red(bar, mid, 0), 200, "top pixel")
        XCTAssertGreaterThan(red(bar, mid, height - 1), 200, "bottom pixel")
        // And a horizontal rule reaches both edges of every cell.
        let (rule, width, h) = render(TermLine(runs: [TermRun("────", fg: .rgb(0xFF0000))]), cols: 4)
        let y = (0 ..< h).first { red(rule, width / 2, $0) > 200 }
        XCTAssertNotNil(y)
        XCTAssertGreaterThan(red(rule, 0, y!), 200)
        XCTAssertGreaterThan(red(rule, width - 2, y!), 200)
    }

    func testTheFaceCoverageDecidesBatching() {
        XCTAssertTrue(font.covers("─"))
        XCTAssertTrue(font.covers("é"))
        XCTAssertFalse(font.covers("测"))
    }

    func testEveryBoxAndBlockCharacterDraws() {
        for value in UInt32(0x2500) ... 0x259F {
            let scalar = Unicode.Scalar(value)!
            guard BoxDrawing.handles(scalar) else { continue }
            let (context, width, height) = render(TermLine(runs: [TermRun(String(Character(scalar)), fg: .rgb(0xFF0000))]), cols: 1)
            let lit = (0 ..< height).contains { y in (0 ..< width).contains { red(context, $0, y) > 60 } }
            XCTAssertTrue(lit, String(format: "U+%04X draws nothing", value))
        }
    }

    /// Cost of drawing the rows of a Claude Code screen. Logged, not
    /// asserted: run on main and on this branch to compare.
    func testRowCost() {
        let bar = String(repeating: "─", count: 62)
        let rows: [(String, TermLine)] = [
            ("border", TermLine(runs: [TermRun("╭" + bar + "╮", fg: .rgb(0xD97757))])),
            ("cjk+emoji", TermLine(runs: [TermRun("⏺ ", fg: .index(2)), TermRun("测试通过 — tests pass ✅ 🎉 "), TermRun("(76 passed)", fg: .index(2))])),
            ("diff", TermLine(runs: [TermRun("      412 ", fg: .index(8)), TermRun("+        if from.cols != self.cols", fg: .index(2), bg: .rgb(0x123A1E))])),
            ("ascii", TermLine(runs: [TermRun(String(repeating: "compiling crate-17 v0.11.5 ", count: 2))])),
        ]
        var report: [String] = []
        for (name, line) in rows {
            _ = render(line, cols: 64) // warm caches
            let start = CACurrentMediaTime()
            let n = 300
            for _ in 0 ..< n { _ = render(line, cols: 64) }
            report.append(String(format: "%@ %.0f µs", name, (CACurrentMediaTime() - start) / Double(n) * 1e6))
        }
        print("DIRI_ROW_COST " + report.joined(separator: "; "))
    }

    func testWheelReportsAreSGR() {
        XCTAssertEqual(String(decoding: TerminalView.wheelReport(up: true, col: 3, row: 7, count: 2), as: UTF8.self),
                       "\u{1B}[<64;3;7M\u{1B}[<64;3;7M")
        XCTAssertEqual(String(decoding: TerminalView.wheelReport(up: false, col: 1, row: 1, count: 1), as: UTF8.self),
                       "\u{1B}[<65;1;1M")
    }
}
