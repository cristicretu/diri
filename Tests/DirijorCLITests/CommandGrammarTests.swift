import ArgumentParser
import DirijorCore
import DirijorProtocol
import Foundation
import Testing

@testable import dirijor_cli

/// Parses argv the way the binary does and hands back the concrete leaf command,
/// so a test asserts on the parsed values rather than on help text.
private func parse<T: ParsableCommand>(_ argv: [String], as type: T.Type) throws -> T {
    let command = try Dirijor.parseAsRoot(argv)
    return try #require(command as? T)
}

// MARK: - resource/action shape

@Test func rootExposesTheResourceGroupsAndKeepsTheLegacyEntryPoints() throws {
    #expect(throws: Never.self) { _ = try parse(["session", "list"], as: SessionList.self) }
    #expect(throws: Never.self) { _ = try parse(["worktree", "list"], as: WorktreeList.self) }
    #expect(throws: Never.self) { _ = try parse(["artifacts", "s_a"], as: Artifacts.self) }
    #expect(throws: Never.self) { _ = try parse(["events", "subscribe"], as: EventsSubscribe.self) }
    // These are invoked by other programs (agent hook config, MCP clients,
    // existing scripts), so their argv is a contract, not a style choice.
    #expect(throws: Never.self) { _ = try parse(["status"], as: Status.self) }
    #expect(throws: Never.self) { _ = try parse(["hook", "Stop"], as: Hook.self) }
    #expect(throws: Never.self) { _ = try parse(["notify", "{}"], as: Notify.self) }
    #expect(throws: Never.self) { _ = try parse(["doctor"], as: Doctor.self) }
    #expect(throws: Never.self) { _ = try parse(["mcp-stdio"], as: McpStdio.self) }
    #expect(throws: Never.self) { _ = try parse(["mcp-tools"], as: McpTools.self) }
    #expect(throws: Never.self) {
        _ = try parse(["mcp-call", "--tool", "list_agents"], as: McpCall.self)
    }
}

@Test func legacyMcpCommandFindsTheSiblingProxy() throws {
    let directory = FileManager.default.temporaryDirectory
        .appendingPathComponent("dirijor-cli-proxy-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: directory) }
    let cli = directory.appendingPathComponent("dirijor")
    let proxy = directory.appendingPathComponent("dirijor-mcp")
    FileManager.default.createFile(atPath: proxy.path, contents: Data())
    #expect(McpStdio.standaloneProxyPath(nextTo: cli.path) == nil)

    try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: proxy.path)
    #expect(McpStdio.standaloneProxyPath(nextTo: cli.path) == proxy.path)
}

@Test func bareResourceFallsBackToItsListAction() throws {
    #expect(throws: Never.self) { _ = try parse(["session"], as: SessionList.self) }
    #expect(throws: Never.self) { _ = try parse(["worktree"], as: WorktreeList.self) }
    #expect(throws: Never.self) { _ = try parse(["events"], as: EventsSubscribe.self) }
}

// MARK: - session

@Test func sessionListTakesFiltersAndJSON() throws {
    let command = try parse(["session", "list", "--json", "--status", "working"], as: SessionList.self)
    #expect(command.output.json)
    #expect(command.status == "working")
    #expect(!command.all)
}

@Test func sessionReadDefaultsToTheLiveScreen() throws {
    let plain = try parse(["session", "read", "s_ab"], as: SessionRead.self)
    #expect(plain.session == "s_ab")
    #expect(plain.source == "screen")
    #expect(plain.lines == nil)

    let tail = try parse(
        ["session", "read", "s_ab", "--source", "scrollback", "--lines", "50", "--json"],
        as: SessionRead.self)
    #expect(tail.source == "scrollback")
    #expect(tail.lines == 50)
    #expect(tail.output.json)
}

@Test func sessionSendCollectsRemainingArgvAsOneMessage() throws {
    let command = try parse(
        ["session", "send", "s_ab", "run", "the", "tests"], as: SessionSend.self)
    #expect(command.session == "s_ab")
    #expect(command.text == ["run", "the", "tests"])
    #expect(!command.noSubmit)

    let typed = try parse(["session", "send", "s_ab", "--no-submit", "2"], as: SessionSend.self)
    #expect(typed.noSubmit)
    #expect(typed.text == ["2"])
}

@Test func sessionWaitDefaultsToDoneAndAcceptsRepeatedTargets() throws {
    let byDefault = try parse(["session", "wait", "s_ab"], as: SessionWait.self)
    #expect(byDefault.until == ["done"])
    #expect(byDefault.timeout == 600)

    let multi = try parse(
        ["session", "wait", "s_ab", "--until", "done", "--until", "needs-input", "--timeout", "30"],
        as: SessionWait.self)
    #expect(multi.until == ["done", "needs-input"])
    #expect(multi.timeout == 30)
    // Every documented target has to be one the daemon actually resolves.
    for target in multi.until {
        #expect(SessionStatus.waitTargets.contains(target))
    }
}

@Test func sessionSpawnMapsAgentNamesAndFallsBackToAGenericCommand() throws {
    let command = try parse(
        ["session", "spawn", "claude", "--cwd", "/tmp/x", "--worktree", "--prompt", "go"],
        as: SessionSpawn.self)
    #expect(AgentKind.parse(command.kind) == .claudeCode)
    #expect(command.cwd == "/tmp/x")
    #expect(command.worktree)
    #expect(command.prompt == "go")

    #expect(AgentKind.parse("codex") == .codex)
    #expect(AgentKind.parse("shell") == .shell)
    #expect(AgentKind.parse("htop") == .generic(command: "htop"))
}

@Test func sessionReleaseAndArchiveHaveDistinctDestructiveness() throws {
    let release = try parse(["session", "release", "s_ab"], as: SessionRelease.self)
    #expect(!release.remove, "release must not forget the record unless asked")

    let removed = try parse(["session", "release", "s_ab", "--remove"], as: SessionRelease.self)
    #expect(removed.remove)

    let undo = try parse(["session", "archive", "s_ab", "--undo"], as: SessionArchive.self)
    #expect(undo.undo)
}

// MARK: - worktree / artifacts

@Test func worktreeCommandsParseRepoAndBranchOptions() throws {
    let create = try parse(
        ["worktree", "create", "/repo", "--branch", "feat/x", "--base", "main"],
        as: WorktreeCreate.self)
    #expect(create.repo == "/repo")
    #expect(create.branch == "feat/x")
    #expect(create.base == "main")

    let remove = try parse(
        ["worktree", "remove", "/repo", "/repo/../wt", "--force"], as: WorktreeRemove.self)
    #expect(remove.repo == "/repo")
    #expect(remove.path == "/repo/../wt")
    #expect(remove.force)

    // Repo defaults to the cwd, which is what makes the command usable from
    // inside the checkout you are already standing in.
    let list = try parse(["worktree", "list"], as: WorktreeList.self)
    #expect(list.repo == nil)
}

@Test func artifactsTakesASessionAndJSON() throws {
    let command = try parse(["artifacts", "s_ab", "--json"], as: Artifacts.self)
    #expect(command.session == "s_ab")
    #expect(command.output.json)
}

// MARK: - events

@Test func eventsSubscribeAcceptsRepeatedSessionAndKindFilters() throws {
    let command = try parse(
        [
            "events", "subscribe", "--session", "s_a", "--session", "s_b",
            "--kind", "session.status", "--kind", "session.needs_input",
            "--since-seq", "42", "--json",
        ], as: EventsSubscribe.self)
    #expect(command.session == ["s_a", "s_b"])
    #expect(command.kind == ["session.status", "session.needs_input"])
    #expect(command.sinceSeq == 42)
    #expect(command.output.json)
}

/// `parseAsRoot` runs each command's `validate()`, so a bad filter is rejected
/// before the CLI ever opens a socket — the failure a script wants is "you
/// typed a kind that does not exist", not a subscription that matches nothing.
@Test func eventsSubscribeRejectsAnUnknownKindBeforeConnecting() {
    #expect(throws: (any Error).self) {
        try Dirijor.parseAsRoot(["events", "subscribe", "--kind", "session.nope"])
    }
}

@Test func eventsWaitRequiresSomethingToWaitFor() throws {
    #expect(throws: (any Error).self) { try Dirijor.parseAsRoot(["events", "wait"]) }
    // A status is a property of one session, so `--until` alone is ambiguous.
    #expect(throws: (any Error).self) {
        try Dirijor.parseAsRoot(["events", "wait", "--until", "done"])
    }

    let scoped = try parse(
        ["events", "wait", "--session", "s_a", "--until", "done"], as: EventsWait.self)
    #expect(scoped.session == "s_a")

    // A kind wait needs no session: "tell me about the next PR anywhere".
    let anyPR = try parse(
        ["events", "wait", "--kind", "session.artifact", "--timeout", "45"], as: EventsWait.self)
    #expect(anyPR.timeout == 45)
    #expect(anyPR.session == nil)
}

@Test func everyAdvertisedEventKindIsAcceptedByTheValidator() {
    for name in EventName.all {
        #expect(throws: Never.self) {
            _ = try Dirijor.parseAsRoot(["events", "subscribe", "--kind", name])
        }
    }
}

// MARK: - exit codes

@Test func exitCodesAreDistinctAndMeaningful() {
    let codes = [CLIExit.failure, CLIExit.timeout, CLIExit.notFound, CLIExit.unreachable]
    #expect(Set(codes.map(\.rawValue)).count == codes.count)
    #expect(CLIExit.timeout.rawValue == 2, "scripts branch on 2 meaning `wait` expired")
    #expect(codes.allSatisfy { $0.rawValue != 0 })
}

@Test func daemonErrorsMapOntoTheRightExitCode() {
    func code(_ error: DaemonError) -> Int32? { (CLIClient.translate(error) as? ExitCode)?.rawValue }
    #expect(code(.timeout) == CLIExit.timeout.rawValue)
    #expect(code(.control(.notFound("s_x"))) == CLIExit.notFound.rawValue)
    #expect(code(.control(.badRequest("nope"))) == CLIExit.failure.rawValue)
    #expect(code(.io("socket gone")) == CLIExit.failure.rawValue)
}

// MARK: - target resolution

@Test func sessionTargetsResolveByIdPrefixAndTitle() throws {
    let alpha = record(id: "s_alpha1", title: "Refactor the parser")
    let beta = record(id: "s_beta22", title: "Ship the release")
    let sessions = [alpha, beta]

    #expect(try CLIClient.resolve("s_alpha1", in: sessions).id == alpha.id)
    #expect(try CLIClient.resolve("s_al", in: sessions).id == alpha.id)
    #expect(try CLIClient.resolve("release", in: sessions).id == beta.id)
    #expect(try CLIClient.resolve("RELEASE", in: sessions).id == beta.id)

    // Ambiguity and absence are different failures and get different codes.
    #expect(throws: ExitCode(1)) { _ = try CLIClient.resolve("s_", in: sessions) }
    #expect(throws: ExitCode(3)) { _ = try CLIClient.resolve("nothing", in: sessions) }
}

// MARK: - agent vocabulary

/// The CLI must resolve agent names through the manifest catalog, not a literal
/// list. This is a merge-shaped regression rather than a build-shaped one: a
/// hardcoded switch compiles fine against a new manifest and simply falls
/// through to `.generic`, so a first-class agent would spawn as a dumb terminal
/// with no detection and no status. Asserting `isFirstClass` — not just the id —
/// is what makes that silent degradation fail here.
@Test func spawnResolvesEveryCatalogAgentAndNotJustTheOriginalFour() {
    for descriptor in AgentCatalog.shared.launchable {
        let parsed = AgentKind.parse(descriptor.id)
        #expect(parsed.id == descriptor.id, "\(descriptor.id) did not round-trip")
        #expect(
            parsed.isFirstClass == descriptor.firstClass,
            "\(descriptor.id) lost its first-class status through the CLI")
    }
}

@Test func spawnAcceptsAliasesAndFallsBackToAGenericCommand() {
    #expect(AgentKind.parse("claude-code") == .claudeCode)
    #expect(AgentKind.parse("CLAUDE") == .claudeCode)
    #expect(AgentKind.parse("bash") == .shell)
    // An unknown name stays a plain command, which is what keeps
    // `dirijor session spawn htop` working.
    #expect(AgentKind.parse("htop") == .generic(command: "htop"))
}

@Test func doctorDerivesDistinctDeterministicBinaryChecksFromTheCatalog() {
    let descriptors = [
        AgentDescriptor(
            id: "zeta", displayName: "Zeta Agent", statusAuthority: .screen,
            binary: "shared-agent"),
        AgentDescriptor(
            id: "beta", displayName: "Beta", statusAuthority: .screen,
            binary: "beta"),
        AgentDescriptor(
            id: "alpha", displayName: "Alpha Agent", statusAuthority: .screen,
            binary: "shared-agent"),
        // Pseudo-agents stay excluded even if malformed input gives them a binary.
        AgentDescriptor(
            id: BuiltinAgentID.shell, displayName: "Shell", binary: "/bin/zsh"),
        AgentDescriptor(
            id: BuiltinAgentID.generic, displayName: "Command", binary: "command"),
        AgentDescriptor(id: "empty", displayName: "Empty", binary: ""),
    ]

    let checks = Doctor.agentBinaryDiagnostics(from: descriptors)
    #expect(checks == [
        AgentBinaryDiagnostic(binary: "beta", displayName: "Beta"),
        AgentBinaryDiagnostic(binary: "shared-agent", displayName: "Alpha Agent"),
    ])
    #expect(checks.map(\.label) == ["Beta", "Alpha Agent (shared-agent)"])
}

@Test func doctorMapsTheRealLaunchableCatalogWithoutPathProbing() {
    let checks = Doctor.agentBinaryDiagnostics(from: AgentCatalog.shared.launchable)
    let binaries = checks.map(\.binary)
    #expect(binaries == binaries.sorted())
    #expect(Set(binaries).count == binaries.count)
    #expect(!binaries.isEmpty)
    #expect(!checks.contains { $0.binary == "shell" || $0.binary == "generic" })
}

private func record(id: String, title: String) -> SessionRecord {
    SessionRecord(
        id: SessionID(rawValue: id),
        kind: .claudeCode,
        cwd: "/tmp",
        projectID: ProjectID(root: "/tmp"),
        title: title,
        status: .idle)
}
