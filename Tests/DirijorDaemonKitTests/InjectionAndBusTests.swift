import DirijorCore
import DirijorProtocol
import Foundation
import Testing

@testable import DirijorDaemonKit

@Test func claudeSpawnPlanPreGeneratesSessionID() {
    let plan = InjectionBuilder.plan(
        kind: .claudeCode,
        sessionID: SessionID(rawValue: "s_abc"),
        cwd: "/Users/x/proj",
        socketPath: "/tmp/d.sock",
        cliPath: "/opt/dirijor",
        injectDir: URL(fileURLWithPath: "/nonexistent")
    )
    let uuid = try! #require(plan.agentSessionID)
    #expect(UUID(uuidString: uuid) != nil)
    #expect(uuid == uuid.lowercased())
    // The agent runs *inside* an interactive login shell (`InjectionBuilder
    // .returnToLoginShell`) so exiting it drops to a prompt instead of ending
    // the session — the real command line is the shell's -c argument, not argv.
    #expect(plan.argv[0] == LoginEnvironment.loginShell)
    let dashC = try! #require(plan.argv.firstIndex(of: "-c"))
    let command = plan.argv[dashC + 1]
    // Each original argv element is shell-quoted individually before being
    // joined into the -c string.
    #expect(command.contains("'claude'"))
    #expect(command.contains("'--session-id' '\(uuid)'"))
    // No --settings since the inject file doesn't exist.
    #expect(!command.contains("--settings"))
    #expect(plan.extraEnv[DirijorEnv.sessionID] == "s_abc")
    #expect(plan.extraEnv[DirijorEnv.socket] == "/tmp/d.sock")
}

@Test func claudeTranscriptPathEncoding() {
    let path = InjectionBuilder.claudeTranscriptPath(
        cwd: "/Users/giga/fun.stuff/anara", sessionUUID: "abc-123")
    let home = FileManager.default.homeDirectoryForCurrentUser.path
    #expect(path == "\(home)/.claude/projects/-Users-giga-fun-stuff-anara/abc-123.jsonl")
}

@Test func codexSpawnPlanInjectsNotify() {
    let plan = InjectionBuilder.plan(
        kind: .codex,
        sessionID: SessionID(rawValue: "s_c"),
        cwd: "/tmp",
        socketPath: "/tmp/d.sock",
        cliPath: "/Applications/My App/dirijor",
        injectDir: URL(fileURLWithPath: "/nonexistent")
    )
    // Codex must run as a child of the user's login shell. Its built-in updater
    // exits Codex after replacing the CLI; keeping the shell as the PTY session
    // leader lets that update return to a usable terminal instead of ending the
    // entire Dirijor session. Resolving the bare `codex` inside that fresh shell
    // also avoids pinning the daemon's lifetime-cached absolute binary path.
    #expect(plan.argv[0] == LoginEnvironment.loginShell)
    #expect(plan.argv[1...3] == ["-i", "-l", "-c"])
    let command = plan.argv[4]
    #expect(command.hasPrefix("'codex' "))
    #expect(command.contains("notify="))
    #expect(command.hasSuffix("; exec '\(LoginEnvironment.loginShell)' -i -l"))
    // Path with a space survives TOML quoting.
    #expect(command.contains("My App"))
    #expect(plan.agentSessionID == nil)
}

/// Skipped on CI, deliberately.
///
/// This is the one test here that needs a *real* interactive login shell:
/// `exec zsh -i -l` is the whole thing under test, since re-resolving PATH is
/// what an interactive shell does and a non-interactive one does not. On a
/// GitHub runner that shell does not reliably reach EOF and exit — it took 32
/// seconds on one run and hung the entire suite past the job timeout on the
/// next, with 93 tests never getting to run behind it.
///
/// Rather than weaken the assertion into something a headless shell can pass,
/// run it where a developer shell exists and skip it where one does not. Set
/// `CI=` (empty) to force it on locally, or `DIRIJOR_RUN_HANGING_TESTS=1` to
/// force it on a runner — see `HangingTestGate` and the `hang-repro` workflow.
@Test(.enabled(if: HangingTestGate.isEnabled))
func codexWrapperReentersShellAndResolvesFreshInteractivePath() throws {
    let root = FileManager.default.temporaryDirectory
        .appendingPathComponent("dirijor-codex-wrapper-\(UUID().uuidString)")
    let bin = root.appendingPathComponent("fresh-bin")
    try FileManager.default.createDirectory(at: bin, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }

    // The process starts with a deliberately stale PATH. The interactive shell
    // config selects a newer Codex location, mirroring nvm/mise being changed
    // after the daemon was launched.
    let fakeCodex = bin.appendingPathComponent("codex")
    try Data("#!/bin/sh\nprintf 'fresh-codex-ran\\n'\n".utf8).write(to: fakeCodex)
    try FileManager.default.setAttributes(
        [.posixPermissions: NSNumber(value: Int16(0o755))], ofItemAtPath: fakeCodex.path)
    try Data("export PATH='\(bin.path):/usr/bin:/bin'\n".utf8)
        .write(to: root.appendingPathComponent(".zshrc"))

    let wrapped = InjectionBuilder.returnToLoginShell(["codex"], shell: "/bin/zsh")
    let process = Process()
    process.executableURL = URL(fileURLWithPath: wrapped[0])
    process.arguments = Array(wrapped.dropFirst())
    var environment = ProcessInfo.processInfo.environment
    environment["HOME"] = root.path
    environment["ZDOTDIR"] = root.path
    environment["PATH"] = "/usr/bin:/bin"
    process.environment = environment

    let input = Pipe()
    let output = Pipe()
    process.standardInput = input
    process.standardOutput = output
    process.standardError = output
    try process.run()
    // A shell that never reaches EOF would otherwise wedge the whole run here:
    // readDataToEndOfFile() waits for the write end to close, and nothing else
    // is going to close it. Kill it instead and let the expectations report.
    let watchdog = Thread {
        Thread.sleep(forTimeInterval: 60)
        if process.isRunning { process.terminate() }
    }
    watchdog.start()
    input.fileHandleForWriting.write(Data("printf 'normal-shell-ready\\n'\nexit\n".utf8))
    input.fileHandleForWriting.closeFile()
    let rendered = String(
        decoding: output.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
    process.waitUntilExit()
    watchdog.cancel()

    #expect(rendered.contains("fresh-codex-ran"))
    #expect(rendered.contains("normal-shell-ready"))
    #expect(process.terminationStatus == 0)
}

@Test func cursorSpawnPlanIsBareLaunch() {
    let plan = InjectionBuilder.plan(
        kind: .cursor,
        sessionID: SessionID(rawValue: "s_cu"),
        cwd: "/tmp",
        socketPath: "/tmp/d.sock",
        cliPath: "/usr/local/bin/dirijor",
        injectDir: URL(fileURLWithPath: "/nonexistent")
    )
    // Cursor CLI has no per-launch hook/notify/MCP flags — just the binary.
    #expect(plan.argv[0].hasSuffix("cursor-agent"))
    #expect(plan.argv.count == 1)
    #expect(plan.agentSessionID == nil)
}

@Test func geminiSpawnPlanMintsSessionID() {
    let plan = InjectionBuilder.plan(
        kind: .gemini,
        sessionID: SessionID(rawValue: "s_g"),
        cwd: "/tmp",
        socketPath: "/tmp/d.sock",
        cliPath: "/usr/local/bin/dirijor",
        injectDir: URL(fileURLWithPath: "/nonexistent")
    )
    #expect(plan.argv[0].hasSuffix("gemini"))
    #expect(plan.argv[1] == "--session-id")
    // The minted UUID rides in both argv and the plan, enabling --resume later.
    let uuid = try! #require(plan.agentSessionID)
    #expect(UUID(uuidString: uuid) != nil)
    #expect(plan.argv[2] == uuid)
}

@Test func resumeArgvBuildsPerAgent() {
    var record = SessionRecord(
        kind: .claudeCode, cwd: "/tmp", projectID: ProjectID(root: "/tmp"), title: "t")
    record.agentSessionID = "uuid-1"
    // `returnToLoginShell` is a manifest property applied to spawn AND resume,
    // so a resumed claude drops back to a prompt on exit exactly like a fresh
    // one — the shell owns argv[0] and the real command line is its -c string.
    let claude = InjectionBuilder.resumeArgv(
        record: record, injectDir: URL(fileURLWithPath: "/nonexistent"))!
    #expect(claude[0] == LoginEnvironment.loginShell)
    #expect(claude[4].contains("'claude' '--resume' 'uuid-1'"))

    var codexRecord = SessionRecord(
        kind: .codex, cwd: "/tmp", projectID: ProjectID(root: "/tmp"), title: "t")
    codexRecord.agentSessionID = "thread-9"
    let codex = InjectionBuilder.resumeArgv(
        record: codexRecord, injectDir: URL(fileURLWithPath: "/nonexistent"))!
    #expect(codex[0] == LoginEnvironment.loginShell)
    #expect(codex[4].contains("'codex' 'resume' 'thread-9'"))
    #expect(codex[4].hasSuffix("; exec '\(LoginEnvironment.loginShell)' -i -l"))

    var geminiRecord = SessionRecord(
        kind: .gemini, cwd: "/tmp", projectID: ProjectID(root: "/tmp"), title: "t")
    geminiRecord.agentSessionID = "gem-uuid-3"
    let gemini = InjectionBuilder.resumeArgv(
        record: geminiRecord, injectDir: URL(fileURLWithPath: "/nonexistent"))!
    #expect(gemini[0].hasSuffix("gemini"))
    #expect(gemini.contains("--resume") && gemini.contains("gem-uuid-3"))

    // Cursor mints chat IDs server-side, so an id-targeted resume is not
    // buildable — but `cursor-agent resume` reopens the latest chat without
    // one. The recorded id must be IGNORED rather than passed along: a
    // server-side chat id is not something the CLI's resume accepts, so
    // appending it would turn a working command into a broken one.
    var cursorRecord = SessionRecord(
        kind: .cursor, cwd: "/tmp", projectID: ProjectID(root: "/tmp"), title: "t")
    cursorRecord.agentSessionID = "chat-1"
    let cursor = InjectionBuilder.resumeArgv(
        record: cursorRecord, injectDir: URL(fileURLWithPath: "/x"))!
    #expect(cursor[0].hasSuffix("cursor-agent"))
    #expect(cursor.contains("resume"))
    #expect(!cursor.contains("chat-1"))

    let shell = SessionRecord(
        kind: .shell, cwd: "/tmp", projectID: ProjectID(root: "/tmp"), title: "t")
    #expect(InjectionBuilder.resumeArgv(record: shell, injectDir: URL(fileURLWithPath: "/x")) == nil)
}

@Test func sanitizeInheritedEnvironmentDropsAgentNestingVars() {
    let env = [
        // Poison: a daemon relaunched from inside a Claude session carries
        // these; CLAUDE_CODE_CHILD_SESSION=1 disables transcript persistence
        // in the spawned child.
        "CLAUDE_CODE_CHILD_SESSION": "1",
        "CLAUDECODE": "1",
        "CLAUDE_CODE_SESSION_ID": "56a45a8e-dead-beef",
        "CLAUDE_CODE_ENTRYPOINT": "cli",
        "CLAUDE_EFFORT": "high",
        "CODEX_SANDBOX": "seatbelt",
        "DIRIJOR_SESSION_ID": "s_stale",
        // Legitimate vars that must survive.
        "PATH": "/usr/bin",
        "HOME": "/Users/x",
        "ANTHROPIC_API_KEY": "sk-test",
        "TERM": "xterm-256color",
    ]
    let clean = InjectionBuilder.sanitizeInheritedEnvironment(env)
    #expect(clean.keys.filter { $0.hasPrefix("CLAUDE") }.isEmpty)
    #expect(clean.keys.filter { $0.hasPrefix("CODEX") }.isEmpty)
    #expect(clean.keys.filter { $0.hasPrefix("DIRIJOR") }.isEmpty)
    #expect(clean["PATH"] == "/usr/bin")
    #expect(clean["HOME"] == "/Users/x")
    #expect(clean["ANTHROPIC_API_KEY"] == "sk-test")
    #expect(clean["TERM"] == "xterm-256color")
}

@Test func terminalEnvironmentOverridesInheritedColorSuppression() {
    // A daemon launched from inside an agent's Bash tool carries NO_COLOR=1 and
    // an empty COLORTERM; chalk/ink honor NO_COLOR and render monochrome.
    var env = [
        "NO_COLOR": "1",
        "FORCE_COLOR": "0",
        "CLICOLOR_FORCE": "0",
        "COLORTERM": "",
        "TERM": "dumb",
        "PATH": "/usr/bin",
    ]
    InjectionBuilder.applyTerminalEnvironment(to: &env)
    #expect(env["NO_COLOR"] == nil)
    #expect(env["FORCE_COLOR"] == nil)
    #expect(env["CLICOLOR_FORCE"] == nil)
    #expect(env["TERM"] == "xterm-256color")
    #expect(env["COLORTERM"] == "truecolor")
    #expect(env["LANG"] == "en_US.UTF-8")
    #expect(env["PATH"] == "/usr/bin")

    // A real LANG survives; it is a locale, not a color switch.
    var localized = ["LANG": "de_DE.UTF-8"]
    InjectionBuilder.applyTerminalEnvironment(to: &localized)
    #expect(localized["LANG"] == "de_DE.UTF-8")
}

@Test func claudeHooksFileIsValidAndEnvBased() throws {
    let dir = FileManager.default.temporaryDirectory
        .appendingPathComponent("dirijor-inject-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: dir) }

    try InjectionBuilder.writeClaudeHooksFile(into: dir)
    let data = try Data(contentsOf: dir.appendingPathComponent("claude-hooks.json"))
    let json = try JSONDecoder.dirijor.decode(JSONValue.self, from: data)
    let hooks = try #require(json["hooks"]?.objectValue)
    for event in ["SessionStart", "PermissionRequest", "Stop", "SubagentStop"] {
        #expect(hooks[event] != nil, "missing \(event)")
    }
    // Env-based command — no hardcoded absolute paths.
    let encoded = String(decoding: data, as: UTF8.self)
    #expect(encoded.contains("$DIRIJOR_CLI"))
}

@Test func claudeMcpConfigPrefersTheStandaloneProxy() throws {
    let dir = FileManager.default.temporaryDirectory
        .appendingPathComponent("dirijor-mcp-inject-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: dir) }
    let cli = dir.appendingPathComponent("dirijor")
    let proxy = dir.appendingPathComponent("dirijor-mcp")
    FileManager.default.createFile(atPath: cli.path, contents: Data())
    FileManager.default.createFile(atPath: proxy.path, contents: Data())
    try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: proxy.path)

    try InjectionBuilder.writeClaudeMcpFile(into: dir, cliPath: cli.path)
    let data = try Data(contentsOf: dir.appendingPathComponent("claude-mcp.json"))
    let json = try JSONSerialization.jsonObject(with: data) as! [String: Any]
    let servers = json["mcpServers"] as! [String: Any]
    let config = servers["dirijor"] as! [String: Any]

    #expect(config["command"] as? String == proxy.path)
    #expect((config["args"] as? [String]) == [])
}

@Test func eventBusSeqAndReplay() async {
    let bus = EventBus(ringCapacity: 8)
    await bus.publish(name: "a", params: .null)
    await bus.publish(name: "b", params: .null)

    // Late subscriber replays from sinceSeq.
    let stream = await bus.subscribe(sinceSeq: 1)
    await bus.publish(name: "c", params: .null)

    var got: [String] = []
    for await event in stream {
        got.append("\(event.name)@\(event.seq)")
        if got.count == 2 { break }
    }
    #expect(got == ["b@2", "c@3"])
}

@Test func eventBusReplayRingStoresCompactEncodedPayloads() async {
    let bus = EventBus(ringCapacity: 4_096)

    for eventIndex in 0..<1_000 {
        let checks: [JSONValue] = (0..<16).map { checkIndex in
            .object([
                "name": .string("check-\(eventIndex)-\(checkIndex)"),
                "status": .string(checkIndex.isMultiple(of: 2) ? "success" : "pending"),
                "url": .string("https://example.invalid/\(eventIndex)/\(checkIndex)"),
            ])
        }
        await bus.publish(
            name: EventName.sessionUpdated,
            params: .object([
                "id": .string("session-\(eventIndex)"),
                "title": .string("A representative session update \(eventIndex)"),
                "checks": .array(checks),
            ]))
    }

    #expect(await bus.replayEventCount == 1_000)
    let encodedBytes = await bus.replayStorageBytes
    #expect(encodedBytes > 0)
    #expect(encodedBytes < 4 * 1_024 * 1_024)

    // Logical JSON bytes alone missed a regression where Foundation Data kept
    // a much larger malloc allocation for every archived event.
    let residentBytes = await bus.replayResidentEstimateBytes
    #expect(residentBytes < encodedBytes * 2 + 128 * 1_024)
}

@Test func eventBusReplayRingIsBoundedByEncodedBytesAndStillDecodes() async {
    let bus = EventBus(ringCapacity: 100, ringByteCapacity: 700)
    for index in 0..<10 {
        await bus.publish(
            name: "payload",
            params: .object([
                "index": .number(Double(index)),
                "body": .string(String(repeating: Character("a"), count: 220)),
            ]))
    }

    let stream = await bus.subscribe(sinceSeq: 0)
    await bus.publish(name: "sentinel", params: .null)
    var replayed: [EventBus.Event] = []
    for await event in stream {
        replayed.append(event)
        if event.name == "sentinel" { break }
    }

    #expect(replayed.last?.seq == 11)
    #expect((replayed.first?.seq ?? 0) > 1, "byte limit should evict old replay entries")
    let payload = replayed.first { $0.name == "payload" }
    #expect(payload?.params["body"]?.stringValue?.count == 220)
}

@Test func hookParsingClaudePermission() {
    let payload = JSONValue.object([
        "tool_name": .string("Bash"),
        "tool_input": .object(["command": .string("rm -rf build")]),
        "session_id": .string("uuid-x"),
    ])
    let (signal, meta) = HookParsing.parseClaudeHook(event: "PermissionRequest", payload: payload)!
    guard case .claudeHook(.permissionRequest(let tool, let summary), let isSubagent) = signal else {
        Issue.record("wrong signal: \(signal)")
        return
    }
    #expect(tool == "Bash")
    #expect(summary == "rm -rf build")
    #expect(!isSubagent)
    let detail = try! #require(meta.needsInput)
    #expect(detail.kind == .permission)
    #expect(detail.riskHint == .destructive)
    #expect(detail.summary.contains("rm -rf build"))
}

@Test func hookParsingSubagentDoesNotProduceNeedsMe() {
    let payload = JSONValue.object([
        "agent_id": .string("sub-1"),
        "tool_name": .string("Bash"),
        "tool_input": .object(["command": .string("ls")]),
    ])
    let (signal, meta) = HookParsing.parseClaudeHook(event: "PermissionRequest", payload: payload)!
    guard case .claudeHook(_, let isSubagent) = signal else {
        Issue.record("wrong signal")
        return
    }
    #expect(isSubagent)
    #expect(meta.needsInput == nil)
}

/// The transcript moves to another project dir when the agent enters a
/// worktree mid-session, so identity metadata must be captured from every
/// hook event — not just SessionStart (which never fires again).
@Test func hookParsingCapturesTranscriptPathOnEveryEvent() {
    let movedPath = "/home/u/.claude/projects/-repo--claude-worktrees-x/uuid-x.jsonl"
    for event in ["UserPromptSubmit", "PreToolUse", "Stop", "SessionEnd"] {
        let payload = JSONValue.object([
            "session_id": .string("uuid-x"),
            "transcript_path": .string(movedPath),
        ])
        let (_, meta) = HookParsing.parseClaudeHook(event: event, payload: payload)!
        #expect(meta.agentSessionID == "uuid-x", "event \(event)")
        #expect(meta.transcriptPath == movedPath, "event \(event)")
    }
}

@Test func findClaudeTranscriptScansAllProjectDirs() throws {
    let projects = FileManager.default.temporaryDirectory
        .appendingPathComponent("dirijor-test-projects-\(UUID().uuidString)")
    defer { try? FileManager.default.removeItem(at: projects) }
    let original = projects.appendingPathComponent("-repo")
    let worktree = projects.appendingPathComponent("-repo--claude-worktrees-x")
    try FileManager.default.createDirectory(at: original, withIntermediateDirectories: true)
    try FileManager.default.createDirectory(at: worktree, withIntermediateDirectories: true)
    let uuid = "0f0e0d0c-0b0a-4908-8706-050403020100"
    let moved = worktree.appendingPathComponent("\(uuid).jsonl")
    try Data("{}".utf8).write(to: moved)

    // Compare via resolvingSymlinksInPath on both sides: macOS temp lives
    // under /var → /private/var and the two APIs disagree on the prefix.
    let found = try #require(
        InjectionBuilder.findClaudeTranscript(sessionUUID: uuid, projectsDir: projects))
    #expect(
        URL(fileURLWithPath: found).resolvingSymlinksInPath()
            == moved.resolvingSymlinksInPath())
    #expect(
        InjectionBuilder.findClaudeTranscript(
            sessionUUID: UUID().uuidString.lowercased(), projectsDir: projects) == nil)
}

@Test func hookParsingCodexNotify() {
    let payload = JSONValue.object([
        "type": .string("agent-turn-complete"),
        "thread-id": .string("t-42"),
        "input-messages": .array([.string("bump all the deps please")]),
        "last-assistant-message": .string("Done."),
    ])
    let (signal, meta) = HookParsing.parseCodexNotify(payload: payload)!
    guard case .codexTurnComplete(let last) = signal else {
        Issue.record("wrong signal")
        return
    }
    #expect(last == "Done.")
    #expect(meta.agentSessionID == "t-42")
    #expect(meta.firstPromptTitle == "bump all the deps please")
}
