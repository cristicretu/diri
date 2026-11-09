import XCTest

/// Opt-in: swipes over a full-screen program and leaves the result on its
/// screen for the caller to check. Skipped unless `DIRI_SHOTS_URL` (a pairing
/// link), `DIRI_SHOTS_DIR` (where PNGs are written) and `DIRI_WHEEL_SESSION`
/// (a session running a full-screen program) reach the runner:
///
///     TEST_RUNNER_DIRI_SHOTS_URL='http://127.0.0.1:7391/?token=…' \
///     TEST_RUNNER_DIRI_SHOTS_DIR=/tmp/shots \
///     TEST_RUNNER_DIRI_WHEEL_SESSION=s_… \
///     xcodebuild … -only-testing:DiriPhoneUITests/WheelTests test
///
/// It swipes down twice (towards older output) and up once.
@MainActor
final class WheelTests: XCTestCase {
    func testSwipesReachAFullScreenProgram() throws {
        let env = ProcessInfo.processInfo.environment
        guard let link = env["DIRI_SHOTS_URL"], let dir = env["DIRI_SHOTS_DIR"],
              let session = env["DIRI_WHEEL_SESSION"]
        else {
            throw XCTSkip("Set TEST_RUNNER_DIRI_SHOTS_URL, TEST_RUNNER_DIRI_SHOTS_DIR and TEST_RUNNER_DIRI_WHEEL_SESSION.")
        }
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)

        let app = XCUIApplication()
        app.launchArguments = ["-endpoint", link, "-session", session]
        app.launch()
        let terminal = app.descendants(matching: .any)["terminal-text"]
        XCTAssertTrue(terminal.waitForExistence(timeout: 20))
        settle(2)

        terminal.swipeDown(velocity: .slow)
        settle(0.5)
        terminal.swipeDown(velocity: .slow)
        settle(0.5)
        terminal.swipeUp(velocity: .slow)
        settle(2)
        let shot = XCUIScreen.main.screenshot()
        try? shot.pngRepresentation.write(to: URL(fileURLWithPath: dir).appendingPathComponent("wheel.png"))
    }

    private func settle(_ seconds: TimeInterval) {
        RunLoop.current.run(until: Date().addingTimeInterval(seconds))
    }
}
