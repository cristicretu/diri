import XCTest
@testable import DiriPhone

final class SelectionTests: XCTestCase {
    private func line(_ text: String, wrapped: Bool = false) -> TermLine { TermLine(runs: [TermRun(text)], wrapped: wrapped) }

    func testAWordIsTheNonBlankCellsAroundThePress() {
        let row = line("  cargo test -p diri-web  ")
        XCTAssertEqual(TerminalSelection.word(in: row, at: 4), 2 ... 6)       // cargo
        XCTAssertEqual(TerminalSelection.word(in: row, at: 17), 16 ... 23)    // diri-web
        XCTAssertEqual(TerminalSelection.word(in: row, at: 0), 0 ... 0)       // blank
    }

    func testWordsStopAtBracketsAndQuotes() {
        let row = line("Read(diri-web/src/terminal.rs)")
        XCTAssertEqual(TerminalSelection.word(in: row, at: 10), 5 ... 28)
    }

    func testCopiedTextJoinsWrappedLinesAndTrimsTrailingBlanks() {
        let lines = [line("first line   "), line("https://example.com/a-very-", wrapped: true), line("long/path   "), line("last")]
        let selection = TerminalSelection(.init(line: 0, column: 6), .init(line: 3, column: 1))
        XCTAssertEqual(selection.text { lines[$0] }, "line\nhttps://example.com/a-very-long/path\nla")
    }

    func testWideCellsAreSelectedWhole() throws {
        let json = #"{"runs":[{"a":0,"bg":null,"fg":null,"g":[[2,1,2],[1,1,1]],"t":"测试!"}],"wrapped":false}"#
        let row = try JSONDecoder().decode(TermLine.self, from: Data(json.utf8))
        XCTAssertEqual(TerminalSelection.word(in: row, at: 1), 0 ... 4)
        XCTAssertEqual(TerminalSelection(.init(line: 0, column: 2), .init(line: 0, column: 3)).text { _ in row }, "试")
    }

    func testUnfetchedHistoryContributesNothing() {
        let selection = TerminalSelection(.init(line: 0, column: 0), .init(line: 2, column: 3))
        XCTAssertEqual(selection.text { $0 == 1 ? nil : self.line("abcd") }, "abcd\nabcd")
    }
}
