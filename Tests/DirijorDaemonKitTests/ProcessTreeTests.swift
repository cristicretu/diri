import Darwin
import DirijorCore
import Foundation
import Testing

@testable import DirijorDaemonKit

/// Spawns a small process tree of its own (never touches foreign pids) via the
/// PTY helper — its `setsid()` gives the root a fresh session + process group,
/// so the pgrp net can't sweep up the test runner.
@Test func processTreeStopContinueKill() async throws {
    let spawned = try PTY.spawn(
        argv: ["/bin/sh", "-c", "sleep 30 & sleep 30 & wait"],
        cwd: "/tmp",
        environment: ["PATH": "/usr/bin:/bin"],
        cols: 80, rows: 24
    )
    let root = spawned.pid
    defer {
        // Belt and braces: whatever happened above, tear our tree down.
        ProcessTree.killAll(ProcessTree.enumerate(root: root))
        Darwin.kill(-root, SIGKILL)
        Darwin.kill(-root, SIGCONT)
        close(spawned.masterFD)
        var status: Int32 = 0
        waitpid(root, &status, 0)
    }

    // Enumerate: sh + 2 background sleeps.
    try await waitUntil(timeout: .seconds(5)) {
        ProcessTree.enumerate(root: root).count >= 3
    }
    let tree = ProcessTree.enumerate(root: root)
    #expect(tree.contains { $0.pid == root })
    #expect(tree.allSatisfy { $0.startSec > 0 })

    // Footprint of a live tree is nonzero.
    #expect(ProcessTree.footprint(tree.map(\.pid)) > 0)

    // Stop: every pid verified SSTOP.
    let stopped = ProcessTree.stopAll(root: root)
    #expect(stopped.count >= 3)
    for sample in stopped {
        #expect(ProcessTree.status(sample.pid) == UInt32(SSTOP))
    }

    // Continue: nothing left stopped.
    ProcessTree.continueAll(stopped)
    try await waitUntil(timeout: .seconds(5)) {
        stopped.allSatisfy { ProcessTree.status($0.pid) != UInt32(SSTOP) }
    }

    // Kill: root reaps; the tree is gone.
    ProcessTree.killAll(stopped)
    var status: Int32 = 0
    waitpid(root, &status, 0)
    try await waitUntil(timeout: .seconds(5)) {
        let survivors = stopped.filter { sample in
            guard let current = ProcessTree.startTime(sample.pid) else { return false }
            // Same start time = still our process (not a recycled pid).
            return current == sample.startSec && ProcessTree.status(sample.pid) != UInt32(SZOMB)
        }
        return survivors.isEmpty
    }
}

/// End-to-end hibernation on a live PTY session: freeze verifies SSTOP,
/// input while frozen auto-wakes and still delivers.
@Test func sessionHibernateWakeRoundTrip() async throws {
    let dir = FileManager.default.temporaryDirectory
        .appendingPathComponent("dirijor-hib-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: dir) }

    let session = AgentSession(
        id: SessionID(rawValue: "s_hib"), kind: .shell, logDirectory: dir)
    try await session.start(argv: ["/bin/cat"], cwd: "/tmp", extraEnv: [:]) { _, _ in }
    defer { Task { await session.terminate(graceSeconds: 0.2) } }

    // Deferred launch: wait for the child to exist.
    try await waitUntil(timeout: .seconds(5)) { await session.pid > 0 }
    let pid = await session.pid

    // Freeze: recorded info + the child actually SSTOPped.
    let info = await session.hibernate(reason: .manual)
    #expect(info != nil)
    #expect(info?.reason == .manual)
    #expect(info?.treePids.contains(pid) == true)
    #expect(ProcessTree.status(pid) == UInt32(SSTOP))
    let hibernated = await session.isHibernated
    #expect(hibernated)

    // Input while frozen: queued, auto-wake, flushed after CONT.
    await session.write(Data("thaw me\r".utf8))
    let awake = await session.isHibernated
    #expect(!awake)
    try await waitUntil(timeout: .seconds(5)) {
        ProcessTree.status(pid) != UInt32(SSTOP)
    }
    // cat echoes the queued bytes → they reached the PTY after the wake.
    try await waitUntil(timeout: .seconds(5)) {
        await session.screenText().contains("thaw me")
    }

    await session.terminate(graceSeconds: 0.2)
    try await waitUntil(timeout: .seconds(5)) { await session.isRunning == false }
}

/// A holder can outlive the daemon that stopped it while the persisted
/// hibernation marker is stale or missing. Attaching must reconcile the real
/// process state; otherwise the UI looks live and accepts bytes into a PTY
/// whose child can never read them.
@Test func sessionAttachRecoversStoppedTreeWithStaleMetadata() async throws {
    let dir = FileManager.default.temporaryDirectory
        .appendingPathComponent("dirijor-stale-stop-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: dir) }

    let session = AgentSession(
        id: SessionID(rawValue: "s_stale_stop"), kind: .shell, logDirectory: dir)
    try await session.start(argv: ["/bin/cat"], cwd: "/tmp", extraEnv: [:]) { _, _ in }
    defer { Task { await session.terminate(graceSeconds: 0.2) } }

    try await waitUntil(timeout: .seconds(5)) { await session.pid > 0 }
    let pid = await session.pid
    let stopped = ProcessTree.stopAll(root: pid)
    #expect(!stopped.isEmpty)
    #expect(ProcessTree.status(pid) == UInt32(SSTOP))
    #expect(await session.isHibernated == false)

    await session.attach(CollectingSink(), fromOffset: nil)
    await session.write(Data("typed into stale stop\r".utf8))

    try await waitUntil(timeout: .seconds(5)) {
        ProcessTree.status(pid) != UInt32(SSTOP)
    }
    try await waitUntil(timeout: .seconds(5)) {
        await session.screenText().contains("typed into stale stop")
    }

    await session.terminate(graceSeconds: 0.2)
    try await waitUntil(timeout: .seconds(5)) { await session.isRunning == false }
}

@Test func processTreeStartTimeGuardsAgainstPidReuse() {
    // A sample whose start time can't match (future) must never be signalled;
    // killAll on it is a no-op rather than a kill of an innocent pid.
    let bogus = ProcSample(pid: getpid(), startSec: Int64.max)
    ProcessTree.killAll([bogus])  // would kill the test runner if unguarded
    #expect(ProcessTree.status(getpid()) != nil)
}

@Test func processTreeEnumerateOfDeadPidIsEmpty() {
    // pid 1 is filtered; a certainly-invalid pid yields nothing.
    #expect(ProcessTree.enumerate(root: 1).isEmpty)
    #expect(ProcessTree.footprint([]) == 0)
}
