import Foundation
import Testing

@testable import DirijorDaemonKit

@Test func loginPathCaptureDoesNotWaitForAChildThatInheritedStdout() {
    let started = ContinuousClock.now
    let path = LoginEnvironment.capturePath(
        shell: "/bin/sh",
        arguments: ["-c", "/bin/sleep 2 & printf '/fixture:/usr/bin\\n'"],
        timeout: 0.2)
    let elapsed = started.duration(to: .now)

    #expect(path == "/fixture:/usr/bin")
    #expect(elapsed < .seconds(1))
}

@Test func loginPathCaptureKillsAShellThatExceedsTheDeadline() {
    let started = ContinuousClock.now
    let path = LoginEnvironment.capturePath(
        shell: "/bin/sh",
        arguments: ["-c", "/bin/sleep 2; printf '/too-late:/usr/bin\\n'"],
        timeout: 0.2)
    let elapsed = started.duration(to: .now)

    #expect(path != "/too-late:/usr/bin")
    #expect(elapsed < .seconds(1))
}
