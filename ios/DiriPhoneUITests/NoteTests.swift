import XCTest

/// Opt-in: edits a note through the phone against a real gateway and
/// captures the sidebar and editor for PR media. Skipped unless
/// `DIRI_SHOTS_URL` (a pairing link), `DIRI_SHOTS_DIR` (where PNGs are
/// written) and `DIRI_NOTE_SESSION` (a note session's id) reach the runner:
///
///     TEST_RUNNER_DIRI_SHOTS_URL='http://127.0.0.1:7391/?token=…' \
///     TEST_RUNNER_DIRI_SHOTS_DIR=/tmp/shots \
///     TEST_RUNNER_DIRI_NOTE_SESSION=s_… \
///     xcodebuild … -only-testing:DiriPhoneUITests/NoteTests test
///
/// It appends a to-do to the note, which it leaves in place for inspection.
@MainActor
final class NoteTests: XCTestCase {
    func testEditANoteFromThePhone() throws {
        let env = ProcessInfo.processInfo.environment
        guard let link = env["DIRI_SHOTS_URL"], let dir = env["DIRI_SHOTS_DIR"],
              let session = env["DIRI_NOTE_SESSION"]
        else {
            throw XCTSkip("Set TEST_RUNNER_DIRI_SHOTS_URL, TEST_RUNNER_DIRI_SHOTS_DIR and TEST_RUNNER_DIRI_NOTE_SESSION.")
        }
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)

        let app = XCUIApplication()
        app.launchArguments = ["-endpoint", link, "-session", session]
        app.launch()

        let editor = app.textViews["note-editor"]
        XCTAssertTrue(editor.waitForExistence(timeout: 20))
        let state = app.staticTexts["note-save-state"]
        XCTAssertTrue(state.waitForExistence(timeout: 10))
        settle(1)
        save(dir, "note")

        // Type at the end of the note, through the Markdown key bar.
        editor.tap()
        if app.buttons["Continue"].waitForExistence(timeout: 1.5) { app.buttons["Continue"].tap() }
        editor.coordinate(withNormalizedOffset: CGVector(dx: 0.9, dy: 0.98)).tap()
        settle(0.5)
        editor.typeText("\nfrom the phone")
        app.buttons["md-todo"].tap()
        settle(0.5)
        save(dir, "note-keyboard")
        app.buttons["md-hide"].tap()

        let saved = NSPredicate(format: "label == %@", "Note · saved")
        expectation(for: saved, evaluatedWith: state)
        waitForExpectations(timeout: 10)
        XCTAssertTrue((editor.value as? String ?? "").contains("- [ ] from the phone"))

        app.buttons["show-sessions"].tap()
        settle(1.5)
        save(dir, "sidebar")
    }

    private func save(_ dir: String, _ name: String) {
        let shot = XCUIScreen.main.screenshot()
        try? shot.pngRepresentation.write(to: URL(fileURLWithPath: dir).appendingPathComponent("\(name).png"))
        let attachment = XCTAttachment(screenshot: shot)
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    private func settle(_ seconds: TimeInterval) {
        RunLoop.current.run(until: Date().addingTimeInterval(seconds))
    }
}
