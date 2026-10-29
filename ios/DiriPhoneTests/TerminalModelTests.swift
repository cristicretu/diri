import XCTest
@testable import DiriPhone

final class TerminalModelTests: XCTestCase {
    private func line(_ text: String) -> TermLine { TermLine(runs: [TermRun(text)]) }

    private func snapshot(rows: [String], history: Int = 0, seq: UInt64 = 1, cols: Int = 20) -> GridSnapshot {
        GridSnapshot(cols: cols, rows: rows.count, seq: seq, historySize: history, lines: rows.map(line))
    }

    // MARK: Wire decoding

    func testDecodesTheGridContract() throws {
        let json = """
        {"cols":10,"rows":2,"seq":7,"historySize":3,
         "cursor":{"x":2,"y":1,"visible":true,"shape":"block"},
         "modes":{"altScreen":false,"appCursor":true,"bracketedPaste":true,"mouse":false},
         "palette":{"fg":"#d4d7dd","bg":"#121318","ansi":["#000000","#ff0000","#00ff00","#ffff00","#0000ff","#ff00ff","#00ffff","#ffffff","#000000","#ff0000","#00ff00","#ffff00","#0000ff","#ff00ff","#00ffff","#ffffff"]},
         "lines":[{"runs":[{"t":"ok","fg":2,"bg":null,"a":1},{"t":" x","fg":"#112233","bg":"#445566","a":24}],"wrapped":true},
                  {"runs":[],"wrapped":false}]}
        """
        let grid = try JSONDecoder().decode(GridSnapshot.self, from: Data(json.utf8))
        XCTAssertEqual(grid.seq, 7)
        XCTAssertEqual(grid.historySize, 3)
        XCTAssertTrue(grid.modes.appCursor)
        XCTAssertTrue(grid.modes.bracketedPaste)
        XCTAssertEqual(grid.palette.ansi[1], 0xFF0000)
        XCTAssertEqual(grid.lines[0].runs[0].fg, .index(2))
        XCTAssertEqual(grid.lines[0].runs[0].bg, .standard)
        XCTAssertEqual(grid.lines[0].runs[0].attributes, .bold)
        XCTAssertEqual(grid.lines[0].runs[1].fg, .rgb(0x112233))
        XCTAssertEqual(grid.lines[0].runs[1].attributes, [.underline, .inverse])
        XCTAssertTrue(grid.lines[0].wrapped)
        XCTAssertEqual(grid.lines[1], .blank)
        XCTAssertEqual(grid.cursor.x, 2)
    }

    func testDecodesADiffKeyedByRow() throws {
        let json = #"{"seq":9,"lines":{"0":{"runs":[{"t":"a"}]},"5":{"runs":[]}},"historySize":12}"#
        let diff = try JSONDecoder().decode(GridDiff.self, from: Data(json.utf8))
        XCTAssertEqual(diff.seq, 9)
        XCTAssertEqual(Set(diff.lines.keys), [0, 5])
        XCTAssertEqual(diff.historySize, 12)
        XCTAssertNil(diff.cursor)
    }

    func testPaletteResolvesTheCubeAndGreys() {
        let palette = TermPalette.standard
        XCTAssertEqual(palette.color(index: 16), 0x000000)
        XCTAssertEqual(palette.color(index: 196), 0xFF0000)
        XCTAssertEqual(palette.color(index: 231), 0xFFFFFF)
        XCTAssertEqual(palette.color(index: 232), 0x080808)
        XCTAssertEqual(palette.color(index: 255), 0xEEEEEE)
    }

    // MARK: Diffs

    func testDiffReplacesOnlyItsRows() {
        var buffer = TerminalBuffer()
        buffer.apply(snapshot(rows: ["a", "b", "c"], history: 10))
        let result = buffer.apply(GridDiff(seq: 2, lines: [1: line("B")]))
        guard case let .applied(change) = result else { return XCTFail("not applied") }
        XCTAssertEqual(change.lines, IndexSet(integer: 11))
        XCTAssertEqual(buffer.screen.map(\.text), ["a", "B", "c"])
        XCTAssertEqual(buffer.line(at: 11)?.text, "B")
    }

    func testStaleAndEarlyDiffsAreRejected() {
        var buffer = TerminalBuffer()
        XCTAssertEqual(buffer.apply(GridDiff(seq: 1, lines: [:])), .needsSnapshot)
        buffer.apply(snapshot(rows: ["a"], seq: 5))
        XCTAssertEqual(buffer.apply(GridDiff(seq: 5, lines: [0: line("x")])), .stale)
        XCTAssertEqual(buffer.apply(GridDiff(seq: 4, lines: [0: line("x")])), .stale)
        XCTAssertEqual(buffer.screen[0].text, "a")
    }

    func testRowsOutsideTheScreenAreIgnored() {
        var buffer = TerminalBuffer()
        buffer.apply(snapshot(rows: ["a", "b"]))
        _ = buffer.apply(GridDiff(seq: 2, lines: [7: line("x"), -1: line("y")]))
        XCTAssertEqual(buffer.screen.map(\.text), ["a", "b"])
    }

    /// The server may send the scrolled rows and the larger history in two
    /// diffs. Neither may duplicate or shift what the first already placed.
    func testHistoryGrowthInASeparateDiffDoesNotShiftTheScreen() {
        var buffer = TerminalBuffer()
        buffer.apply(snapshot(rows: ["1", "2", "3"], history: 4))
        _ = buffer.apply(GridDiff(seq: 2, lines: [0: line("2"), 1: line("3"), 2: line("4")]))
        _ = buffer.apply(GridDiff(seq: 3, lines: [:], historySize: 5))
        XCTAssertEqual(buffer.screen.map(\.text), ["2", "3", "4"])
        XCTAssertEqual(buffer.totalLines, 8)
        XCTAssertEqual(buffer.line(at: 5)?.text, "2")
        XCTAssertNil(buffer.line(at: 4), "the scrolled-off row is fetched, never guessed")
    }

    func testAWidthChangeDropsTheReflowedCache() {
        var buffer = TerminalBuffer()
        buffer.apply(snapshot(rows: ["a"], history: 300))
        _ = buffer.apply(HistoryPage(lines: (0 ..< 256).map { line("h\($0)") }, start: 0, historySize: 300))
        XCTAssertEqual(buffer.line(at: 3)?.text, "h3")
        _ = buffer.apply(GridDiff(seq: 2, lines: [:], cols: 40))
        XCTAssertNil(buffer.line(at: 3))
        XCTAssertEqual(buffer.cols, 40)
    }

    func testSnapshotResetsSeqAndScreen() {
        var buffer = TerminalBuffer()
        buffer.apply(snapshot(rows: ["a", "b"], seq: 10))
        buffer.apply(snapshot(rows: ["c"], seq: 3))
        XCTAssertEqual(buffer.seq, 3)
        XCTAssertEqual(buffer.screenText, "c")
        if case .applied = buffer.apply(GridDiff(seq: 4, lines: [:])) {} else { XCTFail("a new snapshot restarts the sequence") }
    }

    // MARK: History paging

    func testHistoryPagesFillTheCacheAndReportTheirLines() {
        var buffer = TerminalBuffer()
        buffer.apply(snapshot(rows: ["live"], history: 600))
        XCTAssertNil(buffer.line(at: 599))
        let change = buffer.apply(HistoryPage(lines: (512 ..< 600).map { line("h\($0)") }, start: 512, historySize: 600))
        XCTAssertEqual(change.lines, IndexSet(integersIn: 512 ..< 600))
        XCTAssertEqual(buffer.line(at: 599)?.text, "h599")
        XCTAssertEqual(buffer.line(at: 600)?.text, "live")
    }

    func testMissingPagesAreNearestFirstAndSkipInFlight() {
        var cache = LineCache()
        let pages = cache.missingPages(in: 0 ..< 2048, historySize: 2048, focus: 1100)
        XCTAssertEqual(pages.first, 4)
        XCTAssertEqual(Set(pages), Set(0 ..< 8))
        cache.markRequested(4)
        XCTAssertFalse(cache.missingPages(in: 0 ..< 2048, historySize: 2048, focus: 1100).contains(4))
        cache.insert((0 ..< 256).map { _ in .blank }, at: 512)
        XCTAssertFalse(cache.missingPages(in: 0 ..< 2048, historySize: 2048, focus: 0).contains(2))
    }

    func testAPartialLastPageIsRefetchedWhenHistoryGrows() {
        var cache = LineCache()
        cache.insert((0 ..< 100).map { _ in .blank }, at: 0)
        XCTAssertTrue(cache.isComplete(0, historySize: 100))
        XCTAssertFalse(cache.isComplete(0, historySize: 103))
    }

    func testTrimmedScrollbackIsNotRequestedForever() {
        var buffer = TerminalBuffer()
        buffer.apply(snapshot(rows: ["x"], history: 1000))
        // The server kept only rows from 300 on.
        _ = buffer.apply(HistoryPage(lines: [], start: 300, historySize: 1000), requestedStart: 0)
        _ = buffer.apply(HistoryPage(lines: (300 ..< 512).map { _ in .blank }, start: 300, historySize: 1000), requestedStart: 256)
        let missing = buffer.cache.missingPages(in: 0 ..< 1000, historySize: 1000, focus: 0)
        XCTAssertFalse(missing.contains(0))
        XCTAssertFalse(missing.contains(1))
        XCTAssertTrue(missing.contains(2))
    }

    func testDuplicateInsertDoesNotDoubleCount() {
        var cache = LineCache()
        cache.insert([line("a"), line("b")], at: 10)
        cache.insert([line("a"), line("c")], at: 10)
        XCTAssertEqual(cache.lineCount, 2)
        XCTAssertEqual(cache.peek(11)?.text, "c")
    }

    // MARK: Eviction

    func testEvictsLeastRecentlyUsedPagesBeyondCapacity() {
        var cache = LineCache(capacity: 3 * LineCache.pageSize)
        for page in 0 ..< 3 { cache.insert([line("p\(page)")], at: page * LineCache.pageSize) }
        _ = cache.line(at: 0) // page 0 is now the most recent
        cache.insert([line("p3")], at: 3 * LineCache.pageSize)
        XCTAssertEqual(cache.pageCount, 3)
        XCTAssertNotNil(cache.peek(0))
        XCTAssertNil(cache.peek(LineCache.pageSize), "page 1 was least recently used")
        XCTAssertNotNil(cache.peek(3 * LineCache.pageSize))
    }

    func testEvictionSparesTheProtectedPages() {
        var cache = LineCache(capacity: 2 * LineCache.pageSize)
        cache.insert([line("a")], at: 0)
        cache.insert([line("b")], at: LineCache.pageSize)
        cache.insert([line("c")], at: 2 * LineCache.pageSize, protecting: 0 ... 0)
        XCTAssertNotNil(cache.peek(0))
        XCTAssertNil(cache.peek(LineCache.pageSize))
    }

    func testTenThousandLinesStayWithinCapacity() {
        var cache = LineCache(capacity: 4096)
        for start in stride(from: 0, to: 10_000, by: 256) {
            cache.insert((0 ..< 256).map { _ in .blank }, at: start)
        }
        XCTAssertLessThanOrEqual(cache.lineCount, 4096)
        XCTAssertNotNil(cache.peek(9_999), "the newest pages survive")
    }

    // MARK: Key encoding

    func testControlCombos() {
        XCTAssertEqual(KeyEncoder.encodeText("c", modifiers: .ctrl), [0x03])
        XCTAssertEqual(KeyEncoder.encodeText("C", modifiers: .ctrl), [0x03])
        XCTAssertEqual(KeyEncoder.encodeText("[", modifiers: .ctrl), [0x1B])
        XCTAssertEqual(KeyEncoder.encodeText(" ", modifiers: .ctrl), [0x00])
        XCTAssertEqual(KeyEncoder.encodeText("\\", modifiers: .ctrl), [0x1C])
        XCTAssertEqual(KeyEncoder.encodeText("?", modifiers: .ctrl), [0x7F])
        XCTAssertEqual(KeyEncoder.encodeText("b", modifiers: [.ctrl, .alt]), [0x1B, 0x02])
        XCTAssertEqual(KeyEncoder.encodeText("f", modifiers: .alt), [0x1B, UInt8(ascii: "f")])
    }

    func testReturnIsCarriageReturn() {
        XCTAssertEqual(KeyEncoder.encodeText("\n"), [0x0D])
        XCTAssertEqual(KeyEncoder.encode(.enter), [0x0D])
        XCTAssertEqual(KeyEncoder.encode(.backspace), [0x7F])
        XCTAssertEqual(KeyEncoder.encode(.tab, modifiers: .shift), Array("\u{1B}[Z".utf8))
    }

    func testArrowsFollowApplicationCursorMode() {
        XCTAssertEqual(KeyEncoder.encode(.up), Array("\u{1B}[A".utf8))
        XCTAssertEqual(KeyEncoder.encode(.up, appCursor: true), Array("\u{1B}OA".utf8))
        XCTAssertEqual(KeyEncoder.encode(.left, appCursor: true), Array("\u{1B}OD".utf8))
        XCTAssertEqual(KeyEncoder.encode(.right, modifiers: .ctrl, appCursor: true), Array("\u{1B}[1;5C".utf8))
        XCTAssertEqual(KeyEncoder.encode(.down, modifiers: .shift), Array("\u{1B}[1;2B".utf8))
        XCTAssertEqual(KeyEncoder.encode(.pageUp), Array("\u{1B}[5~".utf8))
    }

    func testBracketedPaste() {
        XCTAssertEqual(KeyEncoder.paste("a\nb", bracketed: false), Array("a\rb".utf8))
        XCTAssertEqual(KeyEncoder.paste("hi", bracketed: true), Array("\u{1B}[200~hi\u{1B}[201~".utf8))
        // A pasted end marker cannot close the paste early.
        XCTAssertEqual(KeyEncoder.paste("x\u{1B}[201~rm", bracketed: true), Array("\u{1B}[200~xrm\u{1B}[201~".utf8))
    }

    func testEveryEncodedKeyIsValidUTF8() {
        let keys: [TermKey] = [.enter, .tab, .escape, .backspace, .delete, .up, .down, .left, .right, .home, .end, .pageUp, .pageDown, .text("é"), .text("x")]
        for key in keys {
            for modifiers in [KeyModifiers(), .ctrl, .alt, .shift, [.ctrl, .alt]] {
                let bytes = KeyEncoder.encode(key, modifiers: modifiers, appCursor: true)
                XCTAssertNotNil(String(bytes: bytes, encoding: .utf8), "\(key) \(modifiers)")
            }
        }
    }

    // MARK: Cell width

    func testCellWidths() {
        XCTAssertEqual(CellWidth.of("abc"), 3)
        XCTAssertEqual(CellWidth.of("中文"), 4)
        XCTAssertEqual(CellWidth.of("✅"), 2)
        XCTAssertEqual(CellWidth.of("e\u{301}"), 1)
        XCTAssertEqual(CellWidth.of("─│╭"), 3)
    }

    // MARK: Resume

    /// The values `diri-web`'s `line_digest` gives for the same lines (FNV-1a
    /// over serde_json's bytes, keys sorted): see its test
    /// `line_digests_match_the_pages_algorithm`.
    func testLineDigestsMatchTheGatewaysBytes() throws {
        XCTAssertEqual(TermLine.blank.wireDigest, 218_253_595)
        let escaped = try JSONDecoder().decode(TermLine.self, from: Data(#"{"runs":[{"a":1,"bg":3,"fg":null,"t":"a\u0001\"é"}],"wrapped":true}"#.utf8))
        XCTAssertEqual(escaped.wireDigest, 2_123_155_227)
        let rgb = TermLine(runs: [TermRun("x", fg: .rgb(0xD97757))])
        XCTAssertEqual(rgb.wireDigest, 469_929_067)
    }

    func testResumeTokenDescribesTheVisibleRows() {
        var buffer = TerminalBuffer()
        XCTAssertNil(buffer.resumeToken, "nothing to resume before a snapshot")
        buffer.apply(GridSnapshot(cols: 64, rows: 2, seq: 1, historySize: 0, lines: [.blank, .blank]))
        XCTAssertEqual(buffer.resumeToken, "64x2.d02491b.d02491b")
    }

    func testAResumeAppliesOnlyItsRowsAndRestartsTheSequence() throws {
        var buffer = TerminalBuffer()
        buffer.apply(snapshot(rows: ["one", "two", "three"], history: 5, seq: 900))
        // A restarted gateway counts from one again; the resume still applies.
        let frame = try JSONDecoder().decode(GridDiff.self, from: Data(#"""
        {"cols":20,"rows":3,"seq":2,"historySize":7,"cursor":{"x":1,"y":2,"visible":true,"shape":"block"},
         "modes":{"altScreen":false,"appCursor":true,"bracketedPaste":false,"mouse":false},
         "palette":{"fg":"#ffffff","bg":"#000000","ansi":[]},
         "lines":{"1":{"runs":[{"a":0,"bg":null,"fg":null,"t":"TWO"}],"wrapped":false}}}
        """#.utf8))
        let change = try XCTUnwrap(buffer.resume(frame))
        XCTAssertTrue(change.geometry, "history grew while away")
        XCTAssertEqual(buffer.screen.map(\.text), ["one", "TWO", "three"])
        XCTAssertEqual(buffer.historySize, 7)
        XCTAssertTrue(buffer.modes.appCursor)
        XCTAssertEqual(buffer.palette.background, 0x000000)
        // Diffs after it follow the new sequence.
        guard case .applied = buffer.apply(GridDiff(seq: 3, lines: [0: line("ONE")])) else {
            return XCTFail("the diff after a resume applies")
        }
        XCTAssertEqual(buffer.screen[0].text, "ONE")
    }

    func testAResumeOfAnotherSizeIsRefused() {
        var buffer = TerminalBuffer()
        buffer.apply(snapshot(rows: ["a", "b"]))
        XCTAssertNil(buffer.resume(GridDiff(seq: 5, lines: [:], cols: 30, rows: 2)))
        XCTAssertNil(buffer.resume(GridDiff(seq: 5, lines: [:], cols: 20, rows: 3)))
        XCTAssertNotNil(buffer.resume(GridDiff(seq: 5, lines: [:], cols: 20, rows: 2)))
    }

    @MainActor
    func testTheKeyRowMatchesTheBrowsers() {
        XCTAssertEqual(KeyAccessoryView.keys.map(\.id), [
            "key-esc", "key-ctrl", "key-tab", "key-left", "key-up", "key-down", "key-right",
            "key-enter", "key-backtab", "key-ctrl-c", "key-pipe", "key-slash", "key-tilde", "key-dash",
        ])
    }

    // MARK: Cells from the gateway

    func testRunsArePlacedOnTheGatewaysCells() throws {
        // What diri-web sends for `a测试🎉é─` (see its test
        // `non_ascii_runs_carry_their_cells_so_widths_are_never_guessed`).
        let json = #"{"runs":[{"a":0,"bg":null,"fg":null,"g":[[1,1,1],[2,1,2],[1,2,2],[2,1,1]],"t":"a测试🎉é─"}],"wrapped":false}"#
        let line = try JSONDecoder().decode(TermLine.self, from: Data(json.utf8))
        let run = line.runs[0]
        XCTAssertEqual(run.columns, 1 + 2 + 2 + 2 + 1 + 1)
        XCTAssertEqual(run.cellTexts.map { "\($0.text):\($0.width)" }, ["a:1", "测:2", "试:2", "🎉:2", "é:1", "─:1"])
        // The digest covers the cells exactly as the gateway serialised them.
        XCTAssertEqual(line.wireDigest, FNVTest.digest(json))
    }

    /// The Engine's width wins over the phone's guess: an Indic spacing mark
    /// the phone would fold into its consonant still gets its own column.
    func testTheGatewaysWidthWinsOverTheLocalGuess() throws {
        let json = #"{"runs":[{"a":0,"bg":null,"fg":null,"g":[[3,1,1]],"t":"কা!"}],"wrapped":false}"#
        let run = try JSONDecoder().decode(TermLine.self, from: Data(json.utf8)).runs[0]
        XCTAssertEqual(run.columns, 3)
        XCTAssertEqual(CellWidth.of(run.text), 2, "the local tables would have drifted the row by one")
    }

    func testCursorBlinkDecodes() throws {
        let cursor = try JSONDecoder().decode(TermCursor.self, from: Data(#"{"x":1,"y":2,"visible":true,"shape":"bar","blink":true}"#.utf8))
        XCTAssertEqual(cursor.shape, .bar)
        XCTAssertTrue(cursor.blink)
    }
}

enum FNVTest {
    static func digest(_ text: String) -> UInt32 {
        var hash = FNV1a()
        hash.add(text)
        return hash.value
    }
}
