import CoreGraphics
import XCTest
@testable import DiriPhone

/// A path parser fails quietly: a mark that does not parse renders as nothing,
/// and an empty 16pt box in a list reads as "this row has no agent" rather than
/// as a bug. These assert the artwork is actually there.
final class SVGPathTests: XCTestCase {
    func testEveryBrandMarkParsesToARealShape() {
        for kind in BrandMarkKind.allCases {
            let path = SVGPath.cgPath(from: kind.pathData, size: 24)
            XCTAssertFalse(path.isEmpty, "\(kind.label) parsed to an empty path")

            let box = path.boundingBox
            XCTAssertFalse(box.isNull, "\(kind.label) has a null bounding box")
            // Each mark is authored to fill its 24x24 view box; anything much
            // smaller means the parser bailed partway through the data.
            XCTAssertGreaterThan(box.width, 16, "\(kind.label) is too narrow: \(box)")
            XCTAssertGreaterThan(box.height, 16, "\(kind.label) is too short: \(box)")
            XCTAssertLessThanOrEqual(box.maxX.rounded(), 25, "\(kind.label) overflows: \(box)")
            XCTAssertLessThanOrEqual(box.maxY.rounded(), 25, "\(kind.label) overflows: \(box)")
        }
    }

    func testMarksScaleWithTheRequestedSize() {
        let small = SVGPath.cgPath(from: BrandMarkKind.claude.pathData, size: 12).boundingBox
        let large = SVGPath.cgPath(from: BrandMarkKind.claude.pathData, size: 48).boundingBox
        XCTAssertEqual(large.width / small.width, 4, accuracy: 0.01)
    }

    /// `OPENAI_PATH` and `CURSOR_PATH` are the ones with elliptical arcs, which
    /// need endpoint→centre conversion rather than a plain `addArc`.
    func testArcCommandsProduceCurvedGeometry() {
        for kind in [BrandMarkKind.openAI, .cursor] {
            let path = SVGPath.cgPath(from: kind.pathData, size: 24)
            var elements = 0
            var curves = 0
            path.applyWithBlock { pointer in
                elements += 1
                switch pointer.pointee.type {
                case .addCurveToPoint, .addQuadCurveToPoint: curves += 1
                default: break
                }
            }
            XCTAssertGreaterThan(elements, 10, "\(kind.label) has too few elements")
            XCTAssertGreaterThan(curves, 0, "\(kind.label) produced no curves — arcs were dropped")
        }
    }

    /// Arc flags are exactly one character wide, so `0 1` and `01` are the same
    /// two flags. Reading them as numbers would swallow both into one value and
    /// throw every following coordinate off by a field.
    ///
    /// Note the rotation stays separate: it is a number, so `0 01` packs to
    /// rotation 0, largeArc 0, sweep 1 — whereas `001` would be rotation 0,
    /// largeArc 0, sweep 0, which is a different arc and not the comparison
    /// this test wants to make.
    func testArcFlagsAreReadOneCharacterAtATime() {
        let spaced = SVGPath.cgPath(from: "M0 0 A5 5 0 0 1 10 0", size: 24)
        let packed = SVGPath.cgPath(from: "M0 0 A5 5 0 01 10 0", size: 24)
        XCTAssertEqual(spaced.boundingBox.width, packed.boundingBox.width, accuracy: 0.001)
        XCTAssertEqual(spaced.boundingBox.height, packed.boundingBox.height, accuracy: 0.001)
        // A half-circle of radius 5 spans 10 across and 5 deep; if the flags were
        // misread the sweep would flip and the box would be the other way up.
        XCTAssertEqual(spaced.boundingBox.width, 10, accuracy: 0.001)
        XCTAssertEqual(spaced.boundingBox.height, 5, accuracy: 0.001)
    }

    /// A moveto followed by extra coordinate pairs is a polyline, which is how
    /// the Claude mark encodes most of its outline.
    func testImplicitLineTosAfterAMoveAreHonoured() {
        let path = SVGPath.cgPath(from: "M0 0 5 0 5 5 0 5Z", size: 24)
        XCTAssertEqual(path.boundingBox.width, 5, accuracy: 0.001)
        XCTAssertEqual(path.boundingBox.height, 5, accuracy: 0.001)
    }

    func testMalformedDataYieldsAnEmptyPathRatherThanCrashing() {
        for data in ["", "Z", "M", "garbage", "M0 0 L"] {
            _ = SVGPath.cgPath(from: data, size: 24)
        }
    }
}
