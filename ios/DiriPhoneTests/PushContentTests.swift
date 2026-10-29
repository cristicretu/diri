import UserNotifications
import XCTest
@testable import DiriPhone

final class PushContentTests: XCTestCase {
    private let payload: [String: Any] = [
        "aps": ["alert": ["title": "Diri", "body": "An agent is waiting for you"], "category": "diri.needs",
                "mutable-content": 1, "thread-id": "s_a"],
        "diri": ["s": "s_a", "k": "needs", "b": "http://100.64.0.1:7380", "o": 10.0],
    ]

    func testThePayloadIsReadWithoutSessionContent() throws {
        let content = PushContent.content(fromPayload: payload)
        XCTAssertEqual(content.body, "An agent is waiting for you")
        let reference = try XCTUnwrap(PushContent.Reference(userInfo: content.userInfo))
        XCTAssertEqual(reference.session, "s_a")
        XCTAssertEqual(reference.occurredAt, 10)
        XCTAssertEqual(PushContent.attentionURL(reference)?.absoluteString, "http://100.64.0.1:7380/api/session/s%5Fa/attention", "escaped as every other request is")
        XCTAssertNil(PushContent.Reference(userInfo: ["diri": ["s": "s_a", "k": "needs", "b": "file:///etc"]]))
    }

    func testAPermissionPromptGetsActionsAndADestructiveOneNeedsUnlock() throws {
        let content = PushContent.content(fromPayload: payload)
        let reference = try XCTUnwrap(PushContent.Reference(userInfo: content.userInfo))
        var attention = PushContent.Attention(title: "Fix the flaky test", agent: "claude-code", waiting: true,
                                              question: "Bash(cargo test)", options: nil, kind: "permission",
                                              risk: "neutral", occurredAt: 11, quickAnswer: true)
        PushContent.apply(attention, to: content, reference: reference)
        XCTAssertEqual(content.title, "Fix the flaky test")
        XCTAssertEqual(content.subtitle, "Claude Code")
        XCTAssertEqual(content.body, "Bash(cargo test)")
        XCTAssertEqual(content.categoryIdentifier, PushContent.permissionCategory)
        XCTAssertEqual(PushContent.Reference(userInfo: content.userInfo)?.occurredAt, 11, "answers the prompt shown")

        attention.risk = "destructive"
        PushContent.apply(attention, to: content, reference: reference)
        XCTAssertEqual(content.categoryIdentifier, PushContent.destructiveCategory)
        let destructive = try XCTUnwrap(PushContent.categories.first { $0.identifier == PushContent.destructiveCategory })
        let approve = try XCTUnwrap(destructive.actions.first { $0.identifier == PushContent.approveAction })
        XCTAssertTrue(approve.options.contains(.authenticationRequired))

        // Moved on, or no verified approve key: no actions on the banner.
        attention.waiting = false
        PushContent.apply(attention, to: content, reference: reference)
        XCTAssertEqual(content.categoryIdentifier, PushContent.needsCategory)
    }

    func testAQuestionShowsItsOptionsButOffersNoNumberedAnswers() throws {
        let content = PushContent.content(fromPayload: payload)
        let reference = try XCTUnwrap(PushContent.Reference(userInfo: content.userInfo))
        let attention = PushContent.Attention(title: "Port the key row", agent: "codex", waiting: true,
                                              question: "Which branch?", options: ["main", "dev"], kind: "question",
                                              risk: nil, occurredAt: 12, quickAnswer: false)
        PushContent.apply(attention, to: content, reference: reference)
        XCTAssertEqual(content.body, "Which branch?\n1. main  2. dev")
        XCTAssertEqual(content.categoryIdentifier, PushContent.needsCategory)
        let needs = try XCTUnwrap(PushContent.categories.first { $0.identifier == PushContent.needsCategory })
        XCTAssertTrue(needs.actions.isEmpty)
    }
}
