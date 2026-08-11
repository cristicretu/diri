import DirijorCore
import DirijorProtocol
import Foundation
import Darwin

/// Builds spawn command lines + env with hook/notify injection for each agent
/// kind. Pure — fully unit-testable.
public enum InjectionBuilder {
    public struct SpawnPlan: Sendable {
        /// argv for PTY.spawn (argv[0] resolved via PATH inside a login shell).
        public var argv: [String]
        public var extraEnv: [String: String]
        /// Claude only: the pre-generated session UUID.
        public var agentSessionID: String?
        /// Claude only: predicted transcript path (known before the first byte).
        public var transcriptPath: String?
    }

    /// Resolves argv[0] against the user's login PATH so the launchd-bare daemon
    /// can find tools like `claude` / `codex`. Falls back to the bare name (the
    /// PATH we inject into the child env gives it a second chance).
    static func resolveBinary(_ argv: [String]) -> [String] {
        guard let first = argv.first else { return argv }
        let resolved = LoginEnvironment.resolve(first) ?? first
        return [resolved] + argv.dropFirst()
    }

    static func shellQuote(_ s: String) -> String {
        "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    /// Runs a first-class CLI as a child of a freshly initialized login shell,
    /// then leaves that shell open when the CLI exits. Codex intentionally asks
    /// the user to restart after a self-update; without this parent shell its
    /// exit also tears down the PTY and Dirijor can only show "agent exited".
    ///
    /// Keeping `argv[0]` bare is deliberate. The interactive login shell
    /// re-sources nvm/mise/Homebrew configuration for every new session and
    /// resolves the currently selected binary, rather than an absolute path
    /// cached when the daemon first started.
    static func returnToLoginShell(
        _ argv: [String], shell: String = LoginEnvironment.loginShell
    ) -> [String] {
        let command = argv.map(shellQuote).joined(separator: " ")
            + "; exec \(shellQuote(shell)) -i -l"
        return [shell, "-i", "-l", "-c", command]
    }

    /// Env shared by local and remote spawns (remote = the local ssh client's
    /// env; nothing here crosses the SSH boundary).
    static func baseEnvironment(
        sessionID: SessionID, socketPath: String, cliPath: String
    ) -> [String: String] {
        var env: [String: String] = [
            DirijorEnv.sessionID: sessionID.rawValue,
            DirijorEnv.socket: socketPath,
            DirijorEnv.cli: cliPath,
            // Give the child the user's real login PATH so nested tool lookups
            // (node, git, the agent's own subprocesses) resolve too.
            "PATH": LoginEnvironment.path,
        ]
        if BrowserPool.isAvailable {
            env["DIRIJOR_TEST_RUN_AVAILABLE"] = "1"
        }
        return env
    }

    public static func plan(
        kind: AgentKind,
        sessionID: SessionID,
        cwd: String,
        socketPath: String,
        cliPath: String,
        injectDir: URL
    ) -> SpawnPlan {
        var env = baseEnvironment(
            sessionID: sessionID, socketPath: socketPath, cliPath: cliPath)

        // The two kinds that aren't a CLI at all keep bespoke handling: there is
        // no binary for a manifest to name.
        if kind.id == BuiltinAgentID.shell {
            // The user's real login shell (fish/zsh/…), not launchd's SHELL.
            return SpawnPlan(argv: [LoginEnvironment.loginShell, "-l"], extraEnv: env)
        }
        let descriptor = kind.descriptor
        guard let binary = descriptor.binary else {
            env["DIRIJOR_GENERIC"] = "1"
            let command = kind.command ?? LoginEnvironment.loginShell
            return SpawnPlan(argv: [LoginEnvironment.loginShell, "-lc", command], extraEnv: env)
        }

        // Manifest-declared environment: workarounds and feature switches the
        // agent needs but the user shouldn't have to set. (Claude Code's
        // flicker optimization does partial repaints that leave committed
        // output stale on resize; CLAUDE_CODE_NO_FLICKER forces full repaints
        // so it reflows like a real terminal.)
        env.merge(descriptor.env) { _, manifest in manifest }

        var argv = [binary] + descriptor.spawnArgs
        // A caller-minted conversation UUID is what makes resume possible
        // *later* without the agent ever reporting an id to us.
        var agentSessionID: String?
        if let flag = descriptor.sessionIDFlag {
            let uuid = UUID().uuidString.lowercased()
            agentSessionID = uuid
            argv += [flag, uuid]
        }
        argv += injectionArgs(descriptor.injection, injectDir: injectDir, cliPath: cliPath)
        argv += cursorPluginArgs(
            descriptor.injection,
            sessionID: sessionID,
            socketPath: socketPath,
            cliPath: cliPath,
            injectDir: injectDir
        )

        return SpawnPlan(
            argv: descriptor.returnToLoginShell ? returnToLoginShell(argv) : resolveBinary(argv),
            extraEnv: env,
            agentSessionID: agentSessionID,
            // Only Claude's transcript path is predictable before the first
            // byte, because only Claude lets us choose the session UUID *and*
            // derives its jsonl path from the cwd.
            transcriptPath: descriptor.injection.claudeHooks && agentSessionID != nil
                ? claudeTranscriptPath(cwd: cwd, sessionUUID: agentSessionID!)
                : nil
        )
    }

    /// Per-launch config-injection arguments for the mechanisms a manifest
    /// opted into. Each is a Dirijor-implemented shim (hooks file, MCP server
    /// file, notify callback), so the manifest names the mechanism and the
    /// daemon owns the file it points at.
    static func injectionArgs(
        _ injection: AgentDescriptor.Injection, injectDir: URL, cliPath: String
    ) -> [String] {
        var argv: [String] = []
        if injection.claudeHooks {
            let hooks = injectDir.appendingPathComponent("claude-hooks.json")
            if FileManager.default.fileExists(atPath: hooks.path) {
                argv += ["--settings", hooks.path]
            }
        }
        if injection.claudeMCP { argv += claudeMcpArgs(injectDir: injectDir) }
        if injection.codexNotify {
            argv += ["-c", "notify=[\(tomlString(cliPath)), \"notify\"]"]
        }
        if injection.codexMCP { argv += codexMcpArgs(cliPath: cliPath) }
        // Cursor plugin args need a session id / socket; `plan` and `resumeArgv`
        // append them after writing the per-session plugin directory.
        return argv
    }

    /// Command line to resume a previously-exited session's conversation.
    /// Nil when the record has nothing to resume.
    ///
    /// The `agentSessionID` guard is what keeps a declared resume spec honest:
    /// most CLIs accept `--resume <id>` but give us no way to *learn* an id, so
    /// their manifests carry the spec (accurate, and ready the day a hook or
    /// transcript scanner supplies one) while resume stays inert in practice.
    public static func resumeArgv(
        record: SessionRecord,
        injectDir: URL,
        socketPath: String = "",
        cliPath: String = ""
    ) -> [String]? {
        let descriptor = record.kind.descriptor
        guard let binary = descriptor.binary, let resume = descriptor.resume else { return nil }
        var argv: [String]
        if let agentID = record.agentSessionID {
            argv = [binary] + resume.argv(id: agentID)
        } else {
            // `.latest` needs no id — it means "reopen whatever this CLI
            // considers the most recent conversation".
            guard resume.style == .latest else { return nil }
            argv = [binary] + resume.argv(id: "")
        }
        // Resume re-enters the same conversation, so it needs the same hook and
        // MCP wiring a fresh spawn gets — otherwise a resumed Claude silently
        // loses status detection and the dirijor MCP tools.
        //
        // Only the appendable `--flag value` mechanisms are replayed. Codex's
        // `-c key=value` overrides are global options that must precede the
        // resume SUBCOMMAND, and `codex -c … resume <id>` has never been
        // verified here — so a subcommand-style resume stays bare, exactly as
        // it was before this became manifest-driven. Cursor's `--plugin-dir`
        // is appendable like Claude's flags, so it is replayed.
        if descriptor.injection.claudeHooks {
            let hooks = injectDir.appendingPathComponent("claude-hooks.json")
            if FileManager.default.fileExists(atPath: hooks.path) {
                argv += ["--settings", hooks.path]
            }
        }
        if descriptor.injection.claudeMCP { argv += claudeMcpArgs(injectDir: injectDir) }
        if !socketPath.isEmpty, !cliPath.isEmpty {
            argv += cursorPluginArgs(
                descriptor.injection,
                sessionID: record.id,
                socketPath: socketPath,
                cliPath: cliPath,
                injectDir: injectDir
            )
        }
        return descriptor.returnToLoginShell ? returnToLoginShell(argv) : resolveBinary(argv)
    }

    // MARK: Remote-host spawning over ssh + tmux

    /// tmux session name on the remote host, derived from the persisted Dirijor session
    /// id so respawning the same session reattaches (`tmux new-session -A`).
    public static func remoteTmuxSessionName(sessionID: SessionID) -> String {
        var raw = sessionID.rawValue
        if raw.hasPrefix("s_") { raw.removeFirst(2) }
        return "diri-" + String(raw.prefix(8))
    }

    /// Quotes a remote path for the remote shell while preserving leading-`~`
    /// expansion (`~/my code` → `~/'my code'`).
    static func shellQuotePath(_ path: String) -> String {
        if path == "~" { return path }
        if path.hasPrefix("~/") { return "~/" + shellQuote(String(path.dropFirst(2))) }
        return shellQuote(path)
    }

    /// The PLAIN agent command line to run on the remote host. Deliberately no
    /// `--settings` hooks and no MCP injection — those flags reference local
    /// paths that don't exist on the remote machine. nil ⇒ no command (tmux
    /// starts the remote user's default login shell).
    static func remoteAgentCommand(
        kind: AgentKind, agentSessionID: String?, resume: Bool
    ) -> String? {
        // tmux starts the remote user's default login shell when given no
        // command, which is exactly what a shell session wants.
        if kind.id == BuiltinAgentID.shell { return nil }
        let descriptor = kind.descriptor
        guard let binary = descriptor.binary else { return kind.command }

        var words = [binary] + descriptor.spawnArgs
        if let uuid = agentSessionID {
            if resume, let spec = descriptor.resume {
                words += spec.argv(id: uuid)
            } else if !resume, let flag = descriptor.sessionIDFlag {
                words += [flag, uuid]
            }
        }
        // No local notify/MCP injection crosses the SSH boundary (those flags
        // reference paths that don't exist on the remote machine), so a remote
        // Codex never reports a thread id and resume falls back to a fresh
        // codex — tmux -A already reattaches the live one when it still exists.

        // Append `exec $SHELL` so exiting the agent drops to a shell in the
        // same tmux window (session stays alive) instead of ending it.
        let command = words.joined(separator: " ")
        return descriptor.returnToLoginShell ? command + "; exec \"${SHELL:-bash}\" -l" : command
    }

    /// argv for the LOCAL PTY: ssh runs tmux on the remote host, which keeps the agent
    /// alive across SSH drops. `-A` makes the same argv a reattach when the
    /// tmux session already exists (daemon restart, connection loss, resume).
    ///
    /// Quoting: ssh joins its command args with spaces and hands the string to
    /// the remote login shell, so each element must survive that parse —
    /// the tmux shell-command is one pre-quoted word, the cwd keeps `~`
    /// expandable, and `\;` reaches tmux as a literal `;` command separator
    /// (for `set status off`, which hides the status bar in the PTY).
    public static func remoteArgv(
        kind: AgentKind,
        sessionID: SessionID,
        host: HostEntry,
        remoteCwd: String,
        agentSessionID: String?,
        resume: Bool = false
    ) -> [String] {
        var argv = [
            "ssh", "-t", "-o", "StrictHostKeyChecking=accept-new",
            // Keepalives so an idle session survives NAT/Tailscale idle timeouts
            // instead of dropping with "connection reset by peer". tmux still
            // outlives a genuine disconnect; this just stops the gratuitous ones.
            "-o", "ServerAliveInterval=20", "-o", "ServerAliveCountMax=3",
            "-o", "TCPKeepAlive=yes",
            host.ssh, "--",
            "tmux", "new-session", "-A",
            "-s", remoteTmuxSessionName(sessionID: sessionID),
            "-c", shellQuotePath(remoteCwd),
        ]
        if let command = remoteAgentCommand(
            kind: kind, agentSessionID: agentSessionID, resume: resume)
        {
            argv.append(shellQuote(command))
        }
        argv += ["\\;", "set", "status", "off"]
        return argv
    }

    /// SpawnPlan for a session on a remote host. Pass `resume: true` (with the
    /// record's `agentSessionID`) to revive: same tmux name ⇒ reattach when the
    /// remote session survived, else a fresh agent resuming the conversation
    /// from the remote-side transcript.
    public static func remotePlan(
        kind: AgentKind,
        sessionID: SessionID,
        host: HostEntry,
        remoteCwd: String,
        socketPath: String,
        cliPath: String,
        agentSessionID: String? = nil,
        resume: Bool = false
    ) -> SpawnPlan {
        // Only agents that accept a caller-minted id get one; for the rest the
        // remote conversation id (if any) never reaches us.
        let uuid: String? =
            resume
            ? agentSessionID
            : (kind.descriptor.sessionIDFlag != nil ? UUID().uuidString.lowercased() : nil)
        return SpawnPlan(
            argv: resolveBinary(
                remoteArgv(
                    kind: kind, sessionID: sessionID, host: host,
                    remoteCwd: remoteCwd, agentSessionID: uuid, resume: resume)),
            extraEnv: baseEnvironment(
                sessionID: sessionID, socketPath: socketPath, cliPath: cliPath),
            agentSessionID: uuid,
            // The transcript lives on the remote host; there is no local path to watch.
            transcriptPath: nil
        )
    }

    // MARK: Inherited-environment hygiene

    /// Env prefixes that mark a process as running *inside* another agent.
    /// When the daemon itself is (re)started from within a Claude/Codex
    /// session — the usual dev loop — these leak into every child we spawn.
    /// A spawned Claude then believes it is a nested child session; most
    /// damagingly, `CLAUDE_CODE_CHILD_SESSION=1` silently disables transcript
    /// persistence, so the conversation never reaches disk and a later
    /// `claude --resume` exits with "No conversation found".
    ///
    /// The per-agent half comes from each manifest's `envScrubPrefixes`, so a
    /// new agent declares its own marker. Precision matters: gemini's manifest
    /// says "GEMINI_CLI", not "GEMINI", because Gemini CLI exports GEMINI_CLI=1
    /// to its children while GEMINI_API_KEY is the user's own credential and
    /// must survive the scrub. Cursor sets no known nesting marker, and
    /// scrubbing "CURSOR" would kill CURSOR_API_KEY. "DIRIJOR" is ours, not any
    /// agent's, so it stays here.
    static let agentNestingEnvPrefixes: [String] =
        AgentCatalog.shared.envScrubPrefixes + ["DIRIJOR"]

    /// Strips agent-nesting variables from an inherited environment. Dirijor's
    /// own variables (DIRIJOR_*, CLAUDE_CODE_NO_FLICKER) are re-applied per
    /// spawn via `extraEnv` after this runs.
    public static func sanitizeInheritedEnvironment(_ env: [String: String]) -> [String: String] {
        env.filter { item in
            !agentNestingEnvPrefixes.contains { item.key.hasPrefix($0) }
        }
    }

    /// Vars through which a *launcher* tells its children to suppress color.
    /// Claude Code's Bash tool sets `NO_COLOR=1` and blanks `COLORTERM` for
    /// every command it runs, so a daemon started from inside an agent shell —
    /// again, the usual dev loop — hands `NO_COLOR=1` to each session it
    /// spawns, and chalk/ink honor it unconditionally: the agent emits no SGR
    /// sequences at all and the terminal renders monochrome. These describe the
    /// launcher's stdout, not the PTY we allocate, so they never survive.
    static let colorSuppressionEnvVars = ["NO_COLOR", "FORCE_COLOR", "CLICOLOR_FORCE"]

    /// Asserts the terminal contract for a session's environment: our PTY is a
    /// 24-bit-color xterm regardless of how the daemon itself was launched.
    /// `COLORTERM=truecolor` also lifts agents off the 256-color palette — the
    /// grid protocol carries `TermColor.rgb` end to end.
    public static func applyTerminalEnvironment(to env: inout [String: String]) {
        for key in colorSuppressionEnvVars { env.removeValue(forKey: key) }
        env["TERM"] = "xterm-256color"
        env["COLORTERM"] = "truecolor"
        env["LANG"] = env["LANG"] ?? "en_US.UTF-8"
    }

    /// Applies the same scrub to the daemon's own process environment so
    /// non-PTY children (git, gh, browser-pool node) don't inherit the
    /// nesting markers either.
    public static func scrubProcessEnvironment() {
        for key in ProcessInfo.processInfo.environment.keys
        where agentNestingEnvPrefixes.contains(where: { key.hasPrefix($0) }) {
            unsetenv(key)
        }
    }

    /// Writes a session-local Cursor plugin and returns `--plugin-dir` argv.
    /// Cursor has no Claude-style `--mcp-config` / Codex `-c mcp_servers…`; the
    /// documented launch-scoped surface is `--plugin-dir` with `mcp.json` and
    /// optional `hooks/hooks.json`. Env for the MCP child is baked into
    /// `mcp.json` because Cursor's stdio MCP subprocess does not reliably
    /// inherit the agent process environment.
    static func cursorPluginArgs(
        _ injection: AgentDescriptor.Injection,
        sessionID: SessionID,
        socketPath: String,
        cliPath: String,
        injectDir: URL
    ) -> [String] {
        guard injection.cursorMCP || injection.cursorHooks else { return [] }
        let pluginDir = injectDir
            .appendingPathComponent("cursor-plugin", isDirectory: true)
            .appendingPathComponent(sessionID.rawValue, isDirectory: true)
        do {
            try writeCursorPlugin(
                into: pluginDir,
                mcp: injection.cursorMCP,
                hooks: injection.cursorHooks,
                sessionID: sessionID,
                socketPath: socketPath,
                cliPath: cliPath
            )
        } catch {
            return []
        }
        return ["--plugin-dir", pluginDir.path]
    }

    /// Session-local Cursor plugin: `.cursor-plugin/plugin.json` + optional
    /// `mcp.json` / `hooks/hooks.json`.
    ///
    /// Stages into a fresh temp directory and replaces the live plugin tree so
    /// a resume that disables MCP or hooks cannot leave the previous files.
    public static func writeCursorPlugin(
        into pluginDir: URL,
        mcp: Bool,
        hooks: Bool,
        sessionID: SessionID,
        socketPath: String,
        cliPath: String
    ) throws {
        let fm = FileManager.default
        let parent = pluginDir.deletingLastPathComponent()
        try fm.createDirectory(at: parent, withIntermediateDirectories: true)
        let staging = parent.appendingPathComponent(
            ".\(pluginDir.lastPathComponent).\(getpid()).tmp", isDirectory: true)
        try? fm.removeItem(at: staging)
        try fm.createDirectory(at: staging, withIntermediateDirectories: true)

        let manifestDir = staging.appendingPathComponent(".cursor-plugin", isDirectory: true)
        try fm.createDirectory(at: manifestDir, withIntermediateDirectories: true)
        let manifest: [String: Any] = [
            "name": "dirijor",
            "version": "0.1.0",
            "description": "Diri agent orchestration for Cursor sessions",
        ]
        try JSONSerialization.data(withJSONObject: manifest, options: [.prettyPrinted, .sortedKeys])
            .write(
                to: manifestDir.appendingPathComponent("plugin.json"), options: .atomic)

        if mcp {
            let launch = mcpLaunch(cliPath: cliPath)
            let server: [String: Any] = [
                "type": "stdio",
                // Cursor splits `command` on spaces before spawning it. Keep
                // App Support paths in argv, where JSON preserves one value.
                "command": "/usr/bin/env",
                "args": [launch.command] + launch.args,
                "env": [
                    DirijorEnv.sessionID: sessionID.rawValue,
                    DirijorEnv.socket: socketPath,
                    DirijorEnv.cli: cliPath,
                ],
            ]
            let mcpConfig: [String: Any] = ["mcpServers": ["dirijor": server]]
            try JSONSerialization.data(
                withJSONObject: mcpConfig, options: [.prettyPrinted, .sortedKeys]
            )
            .write(to: staging.appendingPathComponent("mcp.json"), options: .atomic)
        }

        if hooks {
            let hooksDir = staging.appendingPathComponent("hooks", isDirectory: true)
            try fm.createDirectory(at: hooksDir, withIntermediateDirectories: true)
            // Absolute CLI path: Cursor does not expand $DIRIJOR_CLI in hook
            // commands the way Claude's settings do. Reuse the Claude hook
            // parser — Stop is strong-idle (done chime); UserPromptSubmit
            // marks working for the next turn.
            let quoted = shellQuote(cliPath)
            let hooksConfig: [String: Any] = [
                "version": 1,
                "hooks": [
                    "stop": [["command": "\(quoted) hook Stop"]],
                    "beforeSubmitPrompt": [["command": "\(quoted) hook UserPromptSubmit"]],
                ],
            ]
            try JSONSerialization.data(
                withJSONObject: hooksConfig, options: [.prettyPrinted, .sortedKeys]
            )
            .write(to: hooksDir.appendingPathComponent("hooks.json"), options: .atomic)
        }

        let backup = parent.appendingPathComponent(
            ".\(pluginDir.lastPathComponent).\(getpid()).old", isDirectory: true)
        try? fm.removeItem(at: backup)
        if fm.fileExists(atPath: pluginDir.path) {
            try fm.moveItem(at: pluginDir, to: backup)
        }
        do {
            try fm.moveItem(at: staging, to: pluginDir)
            try? fm.removeItem(at: backup)
        } catch {
            try? fm.removeItem(at: staging)
            if fm.fileExists(atPath: backup.path) {
                try? fm.moveItem(at: backup, to: pluginDir)
            }
            throw error
        }
    }

    // MARK: Dirijor MCP injection (agents can spawn/orchestrate other agents)

    /// `--mcp-config` args pointing at the daemon-written server file, so every
    /// spawned/resumed Claude has the `dirijor` MCP tools (spawn_agent,
    /// wait_for_agent, get_artifacts, …). Merges with the user's own servers.
    static func claudeMcpArgs(injectDir: URL) -> [String] {
        let file = injectDir.appendingPathComponent("claude-mcp.json")
        guard FileManager.default.fileExists(atPath: file.path) else { return [] }
        return ["--mcp-config", file.path]
    }

    /// Codex takes MCP servers as `-c` TOML overrides; same shim, per-launch.
    static func codexMcpArgs(cliPath: String) -> [String] {
        let launch = mcpLaunch(cliPath: cliPath)
        let args = launch.args.map(tomlString).joined(separator: ",")
        return [
            "-c", "mcp_servers.dirijor.command=\(tomlString(launch.command))",
            "-c", "mcp_servers.dirijor.args=[\(args)]",
        ]
    }

    /// Writes the Claude `--mcp-config` file: the `dirijor` stdio server backed
    /// by the self-installed CLI shim. Written at daemon start (like the hooks
    /// file) so the path inside is always the current CLI install.
    public static func writeClaudeMcpFile(into injectDir: URL, cliPath: String) throws {
        let launch = mcpLaunch(cliPath: cliPath)
        let config: [String: Any] = [
            "mcpServers": [
                "dirijor": [
                    "type": "stdio",
                    "command": launch.command,
                    "args": launch.args,
                ]
            ]
        ]
        let data = try JSONSerialization.data(
            withJSONObject: config, options: [.prettyPrinted, .sortedKeys])
        try data.write(to: injectDir.appendingPathComponent("claude-mcp.json"), options: .atomic)
    }

    private static func mcpLaunch(cliPath: String) -> (command: String, args: [String]) {
        let proxy = URL(fileURLWithPath: cliPath)
            .deletingLastPathComponent()
            .appendingPathComponent("dirijor-mcp").path
        if FileManager.default.isExecutableFile(atPath: proxy) {
            return (proxy, [])
        }
        // Source checkouts can run the Swift package without building the Rust
        // workspace. Retain the old command as a compatibility fallback only.
        return (cliPath, ["mcp-stdio"])
    }

    static func tomlString(_ s: String) -> String {
        "\"" + s.replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "\"", with: "\\\"") + "\""
    }

    /// Locates a Claude session's transcript when the predicted path is stale.
    /// Claude relocates the jsonl to another project dir when the agent enters
    /// a worktree (EnterWorktree / `--worktree`), so a missing file at the
    /// recorded path does not mean the conversation is gone — scan every
    /// project dir for `<uuid>.jsonl` before giving up.
    public static func findClaudeTranscript(
        sessionUUID: String,
        projectsDir: URL = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".claude/projects", isDirectory: true)
    ) -> String? {
        guard
            let dirs = try? FileManager.default.contentsOfDirectory(
                at: projectsDir, includingPropertiesForKeys: nil,
                options: .skipsHiddenFiles)
        else { return nil }
        for dir in dirs {
            let candidate = dir.appendingPathComponent(sessionUUID + ".jsonl").path
            if FileManager.default.fileExists(atPath: candidate) { return candidate }
        }
        return nil
    }

    /// Claude Code's project-directory slug for a working directory: the cwd
    /// with "/" and "." replaced by "-" (verified against real dirs under
    /// ~/.claude/projects — e.g. `/Users/giga/.claude/worktrees/x` becomes
    /// `-Users-giga--claude-worktrees-x`). The migration transcript shuttle
    /// derives remote-side paths with the same rule.
    public static func claudeProjectSlug(cwd: String) -> String {
        cwd
            .replacingOccurrences(of: "/", with: "-")
            .replacingOccurrences(of: ".", with: "-")
    }

    /// ~/.claude/projects/<cwd with "/" and "." replaced by "-">/<uuid>.jsonl
    public static func claudeTranscriptPath(cwd: String, sessionUUID: String) -> String {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return "\(home)/.claude/projects/\(claudeProjectSlug(cwd: cwd))/\(sessionUUID).jsonl"
    }

    /// The static hooks file injected into every Claude session via --settings.
    /// Commands read $DIRIJOR_CLI / $DIRIJOR_SOCKET from the PTY env, so the
    /// file content is identical for all sessions and safe to write once.
    public static func writeClaudeHooksFile(into injectDir: URL) throws {
        let events = [
            "SessionStart", "UserPromptSubmit", "PreToolUse", "PermissionRequest",
            "Notification", "Stop", "SubagentStart", "SubagentStop", "SessionEnd",
        ]
        var hooks: [String: JSONValue] = [:]
        for event in events {
            hooks[event] = .array([
                .object([
                    "hooks": .array([
                        .object([
                            "type": .string("command"),
                            "command": .string("\"$\(DirijorEnv.cli)\" hook \(event)"),
                            "timeout": .number(10),
                        ])
                    ])
                ])
            ])
        }
        let root = JSONValue.object(["hooks": .object(hooks)])
        let data = try JSONEncoder.dirijor.encode(root)
        try data.write(to: injectDir.appendingPathComponent("claude-hooks.json"), options: .atomic)
    }
}
