import UIKit
import XCTest
@testable import DiriPhone

/// The terminal as an input-method client: composition stays local until a
/// candidate is committed, then goes to the PTY exactly once.
@MainActor
final class TextInputTests: XCTestCase {
    private var sent: [String] = []
    private var view: TerminalView!

    override func setUp() async throws {
        let client = DiriClient(endpoint: .init(baseURL: URL(string: "http://127.0.0.1:9")!, token: "t"))
        view = TerminalView(session: TerminalSession(sessionID: "s_t", client: client))
        sent = []
        view.emit = { [unowned self] bytes in sent.append(String(decoding: bytes, as: UTF8.self)) }
    }

    func testPinyinCompositionSendsOnlyTheChosenCandidate() {
        view.setMarkedText("n", selectedRange: NSRange(location: 1, length: 0))
        view.setMarkedText("ni", selectedRange: NSRange(location: 2, length: 0))
        view.setMarkedText("ni h", selectedRange: NSRange(location: 4, length: 0))
        XCTAssertEqual(sent, [], "nothing reaches the program while composing")
        XCTAssertNotNil(view.markedTextRange)
        XCTAssertEqual(view.text(in: view.markedTextRange!), "ni h")
        XCTAssertFalse(view.markedLabel.isHidden, "the composition is drawn at the cursor")

        view.insertText("你好")
        XCTAssertEqual(sent, ["你好"])
        XCTAssertNil(view.markedTextRange)
        XCTAssertTrue(view.markedLabel.isHidden)
    }

    func testBackspaceEditsTheCompositionNotTheProgram() {
        view.setMarkedText("ka", selectedRange: NSRange(location: 2, length: 0))
        view.deleteBackward()
        XCTAssertEqual(view.text(in: view.markedTextRange!), "k")
        view.deleteBackward()
        XCTAssertNil(view.markedTextRange)
        XCTAssertEqual(sent, [])
        // With nothing composed, Backspace is DEL for the program.
        view.deleteBackward()
        XCTAssertEqual(sent, ["\u{7F}"])
    }

    func testUnmarkingCommitsWhatWasComposed() {
        view.setMarkedText("かな", selectedRange: NSRange(location: 2, length: 0))
        view.unmarkText()
        XCTAssertEqual(sent, ["かな"])
        view.unmarkText()
        XCTAssertEqual(sent, ["かな"], "committing twice sends nothing more")
    }

    func testEmojiAndDictationArriveAsCommittedText() {
        view.insertText("🎉")
        view.insertText("run the tests")
        view.insertText("\n")
        XCTAssertEqual(sent, ["🎉", "run the tests", "\r"])
    }

    func testReplacingInsideTheCompositionStaysLocal() {
        view.setMarkedText("nihao", selectedRange: NSRange(location: 5, length: 0))
        let range = view.textRange(from: view.position(from: view.beginningOfDocument, offset: 2)!, to: view.endOfDocument)!
        view.replace(range, withText: "HAO")
        XCTAssertEqual(view.text(in: view.markedTextRange!), "niHAO")
        XCTAssertEqual(sent, [])
    }

    func testStickyCtrlStillAppliesToTheNextLetter() {
        view.toggleCtrl()
        view.insertText("c")
        XCTAssertEqual(sent, ["\u{03}"])
    }

    func testEveryKeyboardIsAllowedButNothingRewritesInput() {
        XCTAssertEqual(view.keyboardType, .default)
        XCTAssertEqual(view.autocorrectionType, .no)
        XCTAssertEqual(view.spellCheckingType, .no)
        XCTAssertEqual(view.smartQuotesType, .no)
        XCTAssertEqual(view.autocapitalizationType, UITextAutocapitalizationType.none)
    }
}
