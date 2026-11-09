import XCTest
@testable import DiriPhone

final class MarkdownTests: XCTestCase {
    private func apply(_ change: MarkdownEdit.Change, to text: String) -> String {
        (text as NSString).replacingCharacters(in: change.range, with: change.replacement)
    }

    private func kinds(_ text: String) -> [MarkdownStyle.Kind] {
        MarkdownStyle.runs(in: text).map(\.kind)
    }

    // MARK: - Styling

    func testTheFirstH1IsTheTitleAndLaterOnesAreHeadings() {
        let runs = MarkdownStyle.runs(in: "# Plan\n# Again")
        XCTAssertTrue(runs.contains(.init(range: NSRange(location: 0, length: 6), kind: .heading(0))))
        XCTAssertTrue(runs.contains(.init(range: NSRange(location: 7, length: 7), kind: .heading(1))))
    }

    func testATickedToDoIsStruckThrough() {
        let runs = MarkdownStyle.runs(in: "- [x] ship it")
        XCTAssertEqual(runs.first, .init(range: NSRange(location: 0, length: 5), kind: .todoBox(checked: true)))
        XCTAssertTrue(runs.contains(.init(range: NSRange(location: 6, length: 7), kind: .doneText)))
    }

    func testNothingInsideCodeIsMarkdown() {
        XCTAssertFalse(kinds("`**not bold**`").contains(.bold))
        XCTAssertTrue(kinds("**bold** and `code`").contains(.bold))
        // A fenced block swallows everything up to its closing fence.
        let fenced = MarkdownStyle.runs(in: "```\n## not a heading\n```")
        XCTAssertFalse(fenced.contains { if case .heading = $0.kind { true } else { false } })
    }

    func testNoteLinksAndLinksAreTinted() {
        XCTAssertTrue(kinds("see [[Launch plan]]").contains(.noteLink))
        XCTAssertTrue(kinds("see [docs](https://x.dev)").contains(.link))
        // A stored note mention reads as its title alone.
        let mention = kinds("see [@Plan](diri://note/20261109-1)")
        XCTAssertTrue(mention.contains(.noteLink))
        XCTAssertEqual(mention.filter { $0 == .folded }.count, 2)
    }

    // MARK: - Edits

    func testWrapAddsAndRemovesAPair() {
        let text = "make it bold"
        let wrapped = MarkdownEdit.toggleWrap(text, selection: NSRange(location: 8, length: 4), open: "**")
        let bold = apply(wrapped, to: text)
        XCTAssertEqual(bold, "make it **bold**")
        XCTAssertEqual(wrapped.selection, NSRange(location: 10, length: 4))
        let unwrapped = MarkdownEdit.toggleWrap(bold, selection: wrapped.selection, open: "**")
        XCTAssertEqual(apply(unwrapped, to: bold), text)
    }

    func testAnEmptyWrapPutsTheCaretInside() {
        let change = MarkdownEdit.toggleWrap("a ", selection: NSRange(location: 2, length: 0), open: "[[", close: "]]")
        XCTAssertEqual(apply(change, to: "a "), "a [[]]")
        XCTAssertEqual(change.selection, NSRange(location: 4, length: 0))
    }

    func testALinePrefixReplacesAnotherAndTogglesOff() {
        let text = "- milk"
        let todo = apply(MarkdownEdit.toggleLinePrefix(text, selection: NSRange(location: 3, length: 0), prefix: "- [ ] "), to: text)
        XCTAssertEqual(todo, "- [ ] milk")
        let plain = apply(MarkdownEdit.toggleLinePrefix(todo, selection: NSRange(location: 7, length: 0), prefix: "- [ ] "), to: todo)
        XCTAssertEqual(plain, "milk")
    }

    func testAPrefixCoversEveryLineOfTheSelection() {
        let text = "one\ntwo\nthree"
        let change = MarkdownEdit.toggleLinePrefix(text, selection: NSRange(location: 1, length: 5), prefix: "- ")
        XCTAssertEqual(apply(change, to: text), "- one\n- two\nthree")
    }

    func testTickingAToDo() {
        let text = "x\n  - [ ] nested"
        let change = try! XCTUnwrap(MarkdownEdit.toggleTodo(text, at: 6))
        XCTAssertEqual(apply(change, to: text), "x\n  - [x] nested")
        XCTAssertNil(MarkdownEdit.toggleTodo("- plain", at: 1))
    }

    func testReturnContinuesAListAndEndsItWhenEmpty() {
        let text = "- [x] done"
        let next = try! XCTUnwrap(MarkdownEdit.newline(text, selection: NSRange(location: 10, length: 0)))
        let continued = apply(next, to: text)
        XCTAssertEqual(continued, "- [x] done\n- [ ] ")
        let end = try! XCTUnwrap(MarkdownEdit.newline(continued, selection: next.selection))
        XCTAssertEqual(apply(end, to: continued), "- [x] done\n")

        let numbered = "1. first"
        XCTAssertEqual(apply(try! XCTUnwrap(MarkdownEdit.newline(numbered, selection: NSRange(location: 8, length: 0))), to: numbered),
                       "1. first\n2. ")
        XCTAssertNil(MarkdownEdit.newline("plain", selection: NSRange(location: 5, length: 0)))
    }

    func testIndentAndOutdent() {
        let text = "- a\n- b"
        let indented = apply(MarkdownEdit.indent(text, selection: NSRange(location: 0, length: 7), outdent: false), to: text)
        XCTAssertEqual(indented, "  - a\n  - b")
        XCTAssertEqual(apply(MarkdownEdit.indent(indented, selection: NSRange(location: 0, length: 11), outdent: true), to: indented), text)
    }
}
