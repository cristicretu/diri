import XCTest

/// Opt-in: the terminal against `diri-web`'s phone fixture (a fake Engine
/// behind a real gateway; `phone_fixture` in `diri-web/src/lib.rs`). Skipped
/// unless the fixture's directory reaches the runner:
///
///     DIRI_PHONE_FIXTURE_DIR=/tmp/phone-fixture \
///       cargo test -p diri-web phone_fixture -- --ignored &
///     TEST_RUNNER_DIRI_FIXTURE_DIR=/tmp/phone-fixture \
///       xcodebuild … -only-testing:DiriPhoneUITests/TerminalFixtureTests test
///
/// `DIRI_SHOTS_DIR` additionally saves screenshots for PR media.
@MainActor
final class TerminalFixtureTests: XCTestCase {
    private var dir = ""
    private var shots: String?

    override func setUpWithError() throws {
        let env = ProcessInfo.processInfo.environment
        guard let dir = env["DIRI_FIXTURE_DIR"] else { throw XCTSkip("Set TEST_RUNNER_DIRI_FIXTURE_DIR.") }
        self.dir = dir
        shots = env["DIRI_SHOTS_DIR"]
        if let shots { try FileManager.default.createDirectory(atPath: shots, withIntermediateDirectories: true) }
    }

    private func launch() throws -> XCUIApplication {
        let url = try String(contentsOfFile: dir + "/url", encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines)
        let app = XCUIApplication()
        app.launchArguments = ["-endpoint", url, "-session", "s_claude"]
        app.launch()
        XCTAssertTrue(terminal(app).waitForExistence(timeout: 20))
        return app
    }

    private func terminal(_ app: XCUIApplication) -> XCUIElement { app.descendants(matching: .any)["terminal-text"] }

    private func value(_ app: XCUIApplication) -> String { terminal(app).value as? String ?? "" }

    private func waitFor(_ app: XCUIApplication, _ text: String, timeout: TimeInterval = 15) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if value(app).contains(text) { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.25))
        }
        return false
    }

    private func control(_ command: String) throws {
        let handle = try XCTUnwrap(FileHandle(forWritingAtPath: dir + "/control"))
        handle.seekToEndOfFile()
        handle.write(Data((command + "\n").utf8))
        try handle.close()
    }

    private func typed() -> String { (try? String(contentsOfFile: dir + "/typed", encoding: .utf8)) ?? "" }

    private func save(_ name: String) {
        guard let shots else { return }
        let png = XCUIScreen.main.screenshot().pngRepresentation
        FileManager.default.createFile(atPath: "\(shots)/\(name).png", contents: png)
    }

    private func settle(_ seconds: TimeInterval) { RunLoop.current.run(until: Date().addingTimeInterval(seconds)) }

    /// The stream drops (as on a gateway restart or a dead socket) and the
    /// phone comes back with a `resume` that carries only the changed row,
    /// never a second snapshot.
    func testADroppedStreamResumesWithOnlyTheChangedRow() throws {
        let app = try launch()
        XCTAssertTrue(waitFor(app, "snapshots 1; resumes 0"), value(app))
        settle(1)
        save("resume-before")
        let shown = terminal(app).label
        try control("drop")
        XCTAssertTrue(waitFor(app, "snapshots 1; resumes 1; resumed rows 1"), value(app))
        // The fixture reseeds with its stream counter in the last row.
        XCTAssertNotEqual(terminal(app).label, shown, "the changed row is on screen")
        settle(0.5)
        save("resume-after")
    }

    /// Every key on the row reaches the PTY as the bytes a terminal expects.
    func testTheKeyRowTypesWhatATerminalExpects() throws {
        let app = try launch()
        XCTAssertTrue(waitFor(app, "snapshots 1"))
        app.buttons["terminal-keyboard"].tap()
        if app.buttons["Continue"].waitForExistence(timeout: 1.5) { app.buttons["Continue"].tap() }
        XCTAssertTrue(app.buttons["key-esc"].waitForExistence(timeout: 5))
        settle(0.5)
        save("keyrow")
        let before = typed()
        for id in ["key-esc", "key-ctrl"] { app.buttons[id].tap() }
        app.keys["c"].tap()
        for id in ["key-tab", "key-left", "key-up", "key-down", "key-right", "key-enter", "key-backtab", "key-ctrl-c"] {
            app.buttons[id].tap()
        }
        let row = app.scrollViews["key-row"]
        row.swipeLeft()
        settle(0.5)
        save("keyrow-scrolled")
        for id in ["key-pipe", "key-slash", "key-tilde", "key-dash"] { app.buttons[id].tap() }
        let expected = #"\u{1b}\u{3}\t\u{1b}[D\u{1b}[A\u{1b}[B\u{1b}[C\r\u{1b}[Z\u{3}|/~-"#
        let deadline = Date().addingTimeInterval(10)
        var sent = ""
        while Date() < deadline {
            sent = String(typed().dropFirst(before.count)).replacingOccurrences(of: "\n", with: "")
            if sent == expected { break }
            settle(0.25)
        }
        XCTAssertEqual(sent, expected)
    }

    /// The emoji keyboard is reachable (the ASCII-only keyboard type hid it,
    /// and with it every non-Latin keyboard), and an emoji reaches the
    /// program as typed.
    func testTheEmojiKeyboardTypesIntoTheTerminal() throws {
        let app = try launch()
        XCTAssertTrue(waitFor(app, "snapshots 1"))
        app.buttons["terminal-keyboard"].tap()
        if app.buttons["Continue"].waitForExistence(timeout: 1.5) { app.buttons["Continue"].tap() }
        XCTAssertTrue(app.buttons["key-esc"].waitForExistence(timeout: 5))
        let emojiKey = app.keyboards.buttons["Emoji"].firstMatch
        XCTAssertTrue(emojiKey.waitForExistence(timeout: 3), "the keyboard offers emoji")
        save("emoji-key")
        let before = typed()
        emojiKey.tap()
        settle(1.5)
        save("emoji-keyboard")
        // The first "frequently used" emoji. The emoji grid exposes no
        // elements to UI tests, so it is tapped by position: 40 pt above the
        // bottom of the emoji keyboard's top section (iPhone 17 Pro layout).
        let keyboard = app.keyboards.firstMatch.frame
        let cell = CGPoint(x: keyboard.minX + 27, y: keyboard.minY + 84)
        app.coordinate(withNormalizedOffset: .zero).withOffset(CGVector(dx: cell.x, dy: cell.y)).tap()
        let deadline = Date().addingTimeInterval(8)
        while Date() < deadline, typed().count == before.count { settle(0.25) }
        let sent = String(typed().dropFirst(before.count))
        // An emoji is one non-ASCII scalar, escaped by the fixture as \u{1f…}.
        XCTAssertTrue(sent.contains("\\u{1f") || sent.contains("\\u{2"), sent)
    }
}
