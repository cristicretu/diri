import XCTest
@testable import DiriPhone

/// The daemon's enums are single-key objects, not strings. Every one of these
/// decoded wrong at least once while the web frontend was being built, and each
/// failure looked like "the list is empty" rather than like a decoding bug.
final class ProtocolTests: XCTestCase {
    private func decode<T: Decodable>(_ type: T.Type, _ json: String) throws -> T {
        try JSONDecoder().decode(type, from: Data(json.utf8))
    }

    // MARK: - AgentKind

    func testLegacyAgentKindsDecodeToManifestIDs() throws {
        XCTAssertEqual(try decode(AgentKind.self, #"{"claudeCode":{}}"#).id, "claude-code")
        XCTAssertEqual(try decode(AgentKind.self, #"{"codex":{}}"#).id, "codex")
        XCTAssertEqual(try decode(AgentKind.self, #"{"cursor":{}}"#).id, "cursor")
        XCTAssertEqual(try decode(AgentKind.self, #"{"gemini":{}}"#).id, "gemini")
        XCTAssertEqual(try decode(AgentKind.self, #"{"shell":{}}"#).id, "shell")
    }

    func testReadinessCatalogUsesPlainManifestIDs() throws {
        XCTAssertEqual(try decode(AgentKind.self, #""claude-code""#).id, "claude-code")
        XCTAssertEqual(try decode(AgentKind.self, #""amp""#).id, "amp")
    }

    func testManifestAgentsDecodeFromTheOpenCase() throws {
        let kind = try decode(AgentKind.self, #"{"agent":{"id":"amp"}}"#)
        XCTAssertEqual(kind.id, "amp")
    }

    func testGenericCarriesItsCommand() throws {
        let kind = try decode(AgentKind.self, #"{"generic":{"command":"htop"}}"#)
        XCTAssertEqual(kind.id, "generic")
        XCTAssertEqual(kind.command, "htop")
    }

    /// The id is what `/api/spawn` echoes back, so a kind that cannot survive a
    /// round trip would start the wrong agent.
    func testAgentKindRoundTrips() throws {
        for kind in [AgentKind.claudeCode, .codex, .cursor, .gemini, .shell,
                     AgentKind(id: "amp"), AgentKind(id: "generic", command: "htop")]
        {
            let data = try JSONEncoder().encode(kind)
            let restored = try JSONDecoder().decode(AgentKind.self, from: data)
            XCTAssertEqual(restored.id, kind.id, "\(kind.id) lost its id")
            XCTAssertEqual(restored.command, kind.command, "\(kind.id) lost its command")
        }
    }

    func testBrandMarksMapToTheRightArtwork() {
        XCTAssertEqual(AgentKind.claudeCode.brandMark, .claude)
        XCTAssertEqual(AgentKind.codex.brandMark, .openAI)
        XCTAssertEqual(AgentKind.cursor.brandMark, .cursor)
        XCTAssertEqual(AgentKind.gemini.brandMark, .gemini)
        XCTAssertNil(AgentKind.shell.brandMark, "shell draws a caret, not a logo")
    }

    // MARK: - Status

    func testStatusCasesDecode() throws {
        XCTAssertEqual(try decode(SessionStatus.self, #"{"idle":{}}"#), .idle)
        XCTAssertEqual(try decode(SessionStatus.self, #"{"working":{}}"#), .working)
        XCTAssertEqual(try decode(SessionStatus.self, #"{"starting":{}}"#), .starting)
        XCTAssertEqual(
            try decode(SessionStatus.self, #"{"needsInput":{"_0":"permission"}}"#),
            .needsInput(.permission)
        )
    }

    func testAnUnknownStatusDoesNotThrow() throws {
        // A newer daemon may add a case; the row should still draw.
        XCTAssertEqual(try decode(SessionStatus.self, #"{"quiescing":{}}"#), .unknown)
    }

    // MARK: - Session

    func testSessionRecordDecodesFromDaemonJSON() throws {
        let json = """
        {
          "id": "s_1", "kind": {"claudeCode":{}}, "cwd": "/repo", "projectID": "/repo",
          "title": "Fix the thing", "titleSource": 0, "status": {"needsInput":{"_0":"question"}},
          "needsInput": {"kind":"question","summary":"Claude is waiting","riskHint":"neutral"},
          "resumability": "live", "pinned": false, "remoteActive": false,
          "createdAt": 100.0, "updatedAt": 200.0, "gitBranch": "main"
        }
        """
        let session = try decode(SessionRecord.self, json)
        XCTAssertEqual(session.id, "s_1")
        XCTAssertEqual(session.kind.id, "claude-code")
        XCTAssertEqual(session.gitBranch, "main")
        XCTAssertEqual(session.attention, .needsInput)
        XCTAssertEqual(session.statusState, .needsInput(destructive: false))
    }

    func testDestructiveRiskEscalatesTheGlyph() throws {
        let json = """
        {
          "id": "s_2", "kind": {"codex":{}}, "cwd": "/repo", "projectID": "/repo",
          "title": "rm", "status": {"needsInput":{"_0":"permission"}},
          "needsInput": {"kind":"permission","summary":"delete","riskHint":"destructive"},
          "pinned": false, "createdAt": 1.0, "updatedAt": 2.0
        }
        """
        let session = try decode(SessionRecord.self, json)
        XCTAssertEqual(session.statusState, .needsInput(destructive: true))
    }

    /// Idle splits on whether it has been seen since it last moved — that is
    /// what separates "done, look at me" from "done, you already looked".
    func testIdleSplitsOnWhetherItHasBeenSeen() throws {
        let unseen = """
        {"id":"a","kind":{"shell":{}},"cwd":"/r","projectID":"/r","title":"t",
         "status":{"idle":{}},"pinned":false,"createdAt":1.0,"updatedAt":200.0}
        """
        XCTAssertEqual(try decode(SessionRecord.self, unseen).attention, .doneUnseen)

        let seen = """
        {"id":"a","kind":{"shell":{}},"cwd":"/r","projectID":"/r","title":"t",
         "status":{"idle":{}},"pinned":false,"createdAt":1.0,"updatedAt":200.0,"lastSeenAt":300.0}
        """
        XCTAssertEqual(try decode(SessionRecord.self, seen).attention, .idleSeen)
    }

    // MARK: - Endpoint

    func testEnrolmentURLIsSplitIntoBaseAndToken() throws {
        let endpoint = try XCTUnwrap(
            DiriClient.Endpoint(enrolmentURL: "http://forge:7380/?token=deadbeef")
        )
        XCTAssertEqual(endpoint.baseURL.absoluteString, "http://forge:7380")
        XCTAssertEqual(endpoint.token, "deadbeef")
    }

    func testALinkWithoutATokenIsRejected() {
        XCTAssertNil(DiriClient.Endpoint(enrolmentURL: "http://forge:7380/"))
        XCTAssertNil(DiriClient.Endpoint(enrolmentURL: "nonsense"))
    }

    func testEnrolmentURLToleratesSurroundingWhitespace() throws {
        // Pasting from a terminal brings a newline along more often than not.
        let endpoint = DiriClient.Endpoint(enrolmentURL: "  http://forge:7380/?token=abc\n")
        XCTAssertEqual(endpoint?.token, "abc")
    }

    // MARK: - Keys

    /// `diri-web` refuses key names it does not know, so every name the row
    /// offers has to be one the server maps.
    func testEveryKeyHasAStableWireName() {
        let expected: Set<String> = [
            "enter", "escape", "tab", "shift-tab", "backspace",
            "up", "down", "left", "right", "page-up", "page-down",
            "ctrl-c", "ctrl-d", "ctrl-u", "ctrl-r", "alt-enter",
            "yes", "no", "digit-1", "digit-2", "digit-3",
        ]
        XCTAssertEqual(Set(TerminalKey.allCases.map(\.wireName)), expected)
    }
}
