import DirijorCore
import DirijorGit
import DirijorHolderKit
import DirijorProtocol
import Foundation

/// Paths + binaries the daemon injects into spawned sessions. Injectable for tests.
public struct DaemonConfig: Sendable {
    public var socketPath: String
    public var cliPath: String
    public var injectDir: URL
    public var logsDir: URL
    public var holdersDir: URL
    public var holderExecutablePath: String
    public var stateFile: URL
    /// Remote-access config (TCP port + token). nil ⇒ remote listener disabled
    /// (the UDS-only default). Loaded from `DirijorPaths.remoteConfigFile`.
    public var remote: RemoteConfig?
    /// Remote host catalog (`hosts.json`), read lazily at spawn/resume time so
    /// edits apply without a daemon restart. Missing file ⇒ local spawns only.
    public var hostsConfigFile: URL

    public init(
        socketPath: String, cliPath: String, injectDir: URL, logsDir: URL, stateFile: URL,
        holdersDir: URL? = nil, holderExecutablePath: String? = nil,
        remote: RemoteConfig? = nil,
        hostsConfigFile: URL? = nil
    ) {
        self.socketPath = socketPath
        self.cliPath = cliPath
        self.injectDir = injectDir
        self.logsDir = logsDir
        self.holdersDir = HolderPaths.safeDirectory(
            preferred:
                holdersDir
                ?? stateFile.deletingLastPathComponent().appendingPathComponent(
                    "holders", isDirectory: true))
        self.holderExecutablePath =
            holderExecutablePath ?? HolderLauncher.defaultExecutablePath()
        self.stateFile = stateFile
        self.remote = remote
        self.hostsConfigFile = hostsConfigFile ?? DirijorPaths.hostsConfigFile
    }

    public static func standard() -> DaemonConfig {
        DaemonConfig(
            socketPath: DirijorPaths.socket.path,
            cliPath: DirijorPaths.binDir.appendingPathComponent("dirijor").path,
            injectDir: DirijorPaths.injectDir,
            logsDir: DirijorPaths.logsDir,
            stateFile: DirijorPaths.stateFile,
            holdersDir: DirijorPaths.holdersDir,
            remote: RemoteConfig.load(from: DirijorPaths.remoteConfigFile),
            hostsConfigFile: DirijorPaths.hostsConfigFile
        )
    }
}

/// The daemon's source of truth: session records + live AgentSessions + projects.
public actor SessionRegistry {
    private var records: [SessionID: SessionRecord] = [:]
    private var live: [SessionID: AgentSession] = [:]
    private var projects: [ProjectID: Project] = [:]

    private let config: DaemonConfig
    private let events: EventBus
    private let persistence: PersistenceStore
    private var statusEngine: StatusEngine?
    /// Subprocess seam for remote operations (ssh/rsync/scp/git); injectable
    /// so tests can fake a "remote" with local execution.
    private let runner: ShellRunner
    private let migrator: SessionMigrator
    private let locator: RepoLocator
    /// Sessions currently mid-`session.migrate`; a second request is refused.
    private var migrating: Set<SessionID> = []

    public init(config: DaemonConfig, events: EventBus, runner: ShellRunner = .live) {
        self.config = config
        self.events = events
        self.persistence = PersistenceStore(fileURL: config.stateFile)
        self.runner = runner
        self.migrator = SessionMigrator(runner: runner)
        self.locator = RepoLocator(runner: runner)
    }

    func bind(statusEngine: StatusEngine) {
        self.statusEngine = statusEngine
    }

    // MARK: Startup / shutdown

    /// Loads persisted state and adopts every still-live per-session holder.
    public func restoreFromDisk() async {
        guard let state = await persistence.load() else { return }
        try? FileManager.default.createDirectory(
            at: config.holdersDir, withIntermediateDirectories: true)
        let holderSessionIDs = Set(
            ((try? FileManager.default.contentsOfDirectory(
                at: config.holdersDir,
                includingPropertiesForKeys: nil)) ?? [])
                .filter {
                    $0.pathExtension == "sock"
                        && !HolderManagerPaths.isManagerSocket($0)
                }
                .map { $0.deletingPathExtension().lastPathComponent })
        DaemonLog.shared.log(
            "restore scanning holders \(config.holdersDir.path): \(holderSessionIDs.sorted())")
        for project in state.projects {
            projects[project.id] = project
        }
        for var record in state.sessions {
            if record.status.isRunning,
                holderSessionIDs.contains(record.id.rawValue)
            {
                let paths = HolderPaths(
                    directory: config.holdersDir, sessionID: record.id.rawValue)
                let client = HolderClient(socketPath: paths.socket.path)
                if let stat = try? client.stat(), stat.alive {
                    let session = AgentSession(
                        id: record.id,
                        kind: record.kind,
                        logDirectory: config.logsDir,
                        holderDirectory: config.holdersDir,
                        holderExecutablePath: config.holderExecutablePath,
                        cols: Int(stat.cols ?? 120),
                        rows: Int(stat.rows ?? 32))
                    do {
                        try await session.reattach(
                            stat: stat, hibernation: record.hibernation
                        ) { [weak self] sessionID, info in
                            await self?.handleSessionExit(sessionID, info: info)
                        }
                        record.resumability = .live
                        record.memoryBytes = nil
                        record.listeningPorts = nil
                        records[record.id] = record
                        live[record.id] = session
                        await configure(session, record: record)
                        continue
                    } catch {
                        DaemonLog.shared.log(
                            "session \(record.id) holder adoption failed: \(error)")
                    }
                }
            }

            // A tree left SIGSTOPped by the previous daemon is orphaned: kill it
            // (start-time verified — recycled pids are never signalled) before
            // marking the session exited like any other daemon-restart casualty.
            if let hibernation = record.hibernation {
                let samples = hibernation.treePids.map { pid in
                    ProcSample(
                        pid: pid_t(pid),
                        startSec: hibernation.treeStartTimes?[pid] ?? 0)
                }
                if record.status.isRunning {
                    record.status = .exited(ExitInfo(reason: .daemonRestart))
                    record.needsInput = nil
                    record.updatedAt = Date()
                }
                // Nothing is live after a restart; re-derive resumability for
                // every record so missing transcripts don't offer doomed Resume.
                record.resumability = offlineResumability(of: &record)
                record.memoryBytes = nil
                record.listeningPorts = nil
                records[record.id] = record
            }
            if record.status.isRunning {
                record.status = .exited(ExitInfo(reason: .daemonRestart))
                record.needsInput = nil
                record.updatedAt = Date()
            }
            // No live holder was adoptable; re-derive resumability so sessions
            // whose transcript never made it to disk show transcript-missing
            // instead of offering a doomed Resume.
            record.resumability = offlineResumability(of: &record)
            record.memoryBytes = nil
            record.listeningPorts = nil
            records[record.id] = record
        }
        SessionLogStorage.prune(directory: config.logsDir, keeping: Set(records.keys))
        DaemonLog.shared.log("restored \(records.count) sessions, \(projects.count) projects")
    }

    public func prepareShutdown() async {
        for session in live.values {
            await session.shutdownCleanup()
        }
        await persistence.schedule(snapshotState())
        await persistence.flushNow()
    }

    private func configure(_ session: AgentSession, record: SessionRecord) async {
        await session.setHibernationCallback { [weak self] sessionID, info in
            await self?.applyHibernationChange(sessionID: sessionID, info: info)
        }
        await session.setRemoteActiveCallback { [weak self] sessionID, active in
            await self?.applyRemoteActiveChange(sessionID: sessionID, active: active)
        }
        await statusEngine?.track(
            sessionID: record.id, kind: record.kind, session: session)
    }

    // MARK: Spawn / resume / kill

    /// Resolves a spawn/resume host id against `hosts.json` (read fresh each
    /// time so catalog edits apply without a daemon restart).
    private func resolveHost(_ hostID: String) throws -> HostEntry {
        guard
            let entry = HostsConfig.load(from: config.hostsConfigFile)?.host(id: hostID)
        else {
            throw ControlError.badRequest(
                "unknown host \"\(hostID)\" — not present in \(config.hostsConfigFile.path)")
        }
        return entry
    }

    /// Resolves `SessionSpawnParams.sameRepoAs`: the checkout of the reference
    /// session's repository on the target host. nil ⇒ no preservation applies
    /// (no reference, no origin, or repo not cloned there) — except the one
    /// hard fallback: a remote reference session spawning LOCALLY must never
    /// use its remote cwd as a local path, so home stands in when the repo has
    /// no local clone and the provided cwd doesn't exist here.
    private func sameRepoCwd(
        params: SessionSpawnParams, target: HostEntry?
    ) async -> String? {
        guard params.newWorktree != true,
            let reference = params.sameRepoAs,
            let source = records[reference]
        else { return nil }
        let sourceHost = source.host.flatMap { try? resolveHost($0) }
        if let origin = await locator.origin(ofCwd: source.cwd, host: sourceHost),
            let path = await locator.locate(
                origin: origin, on: target, localRoots: projects.values.map(\.root))
        {
            return path
        }
        if target == nil, source.host != nil,
            !FileManager.default.fileExists(atPath: params.cwd)
        {
            return FileManager.default.homeDirectoryForCurrentUser.path
        }
        return nil
    }

    public func spawn(_ params: SessionSpawnParams) async throws -> SessionRecord {
        var cwd = params.cwd
        var branch: String?
        var worktreePath: String?
        var hostEntry: HostEntry?

        if let hostID = params.host {
            // Remote spawn: the agent binary lives on the configured host (no local
            // readiness check) and worktrees are a local-git feature.
            guard params.newWorktree != true else {
                throw ControlError.badRequest(
                    "new_worktree is not supported when spawning on a remote host")
            }
            hostEntry = try resolveHost(hostID)
        } else {
            try AgentReadiness.require(params.kind)
        }

        // Repo-preserving spawn (⌘T host picker / palette host entries): keep
        // the REPO of the reference session, not its path — resolve the same
        // origin on the target host. Falls back to the provided cwd below.
        if let resolved = await sameRepoCwd(params: params, target: hostEntry) {
            cwd = resolved
        }

        // The cwd the session was ASKED to open in (post repo-resolution,
        // pre worktree creation) — the project bucket derives from it.
        let requestedCwd = cwd
        if hostEntry != nil {
            if cwd.isEmpty { cwd = hostEntry?.defaultCwd ?? "~" }
        } else {
            if params.newWorktree == true {
                let repoRoot = GitWorktrees.repositoryRoot(of: cwd) ?? cwd
                let worktree = try GitWorktrees.create(
                    repoPath: repoRoot, branch: params.worktreeBranch)
                cwd = worktree.path
                worktreePath = worktree.path
                branch = worktree.branch
            } else {
                branch = GitWorktrees.currentBranch(in: cwd)
            }
        }

        // Remote cwds are paths on another machine — no local git probing; the remote cwd is
        // its own project bucket.
        let projectRoot =
            hostEntry == nil
            ? GitWorktrees.repositoryRoot(of: requestedCwd) ?? requestedCwd
            : cwd
        let project = upsertProject(root: projectRoot)

        let id = SessionID.generate()
        let plan: InjectionBuilder.SpawnPlan
        if let hostEntry {
            plan = InjectionBuilder.remotePlan(
                kind: params.kind,
                sessionID: id,
                host: hostEntry,
                remoteCwd: cwd,
                socketPath: config.socketPath,
                cliPath: config.cliPath
            )
        } else {
            plan = InjectionBuilder.plan(
                kind: params.kind,
                sessionID: id,
                cwd: cwd,
                socketPath: config.socketPath,
                cliPath: config.cliPath,
                injectDir: config.injectDir
            )
        }

        var record = SessionRecord(
            id: id,
            kind: params.kind,
            cwd: cwd,
            projectID: project.id,
            worktreePath: worktreePath,
            gitBranch: branch,
            title: params.title ?? TitleMaker.placeholder(kind: params.kind, cwd: cwd),
            titleSource: params.title != nil ? .dirijorAssigned : .placeholder,
            agentSessionID: plan.agentSessionID,
            transcriptPath: plan.transcriptPath,
            // Any remote session is revivable: same tmux name ⇒ reattach.
            // Locally it takes a manifest that both declares a resume command
            // AND gives us a way to learn the conversation id — most CLIs offer
            // the first without the second, and a Resume button that can never
            // build a command line is worse than no button at all.
            resumability: params.host != nil || params.kind.descriptor.canResume
                ? .resumable : .notResumable,
            parent: params.parent,
            host: params.host
        )

        let session = AgentSession(
            id: id, kind: params.kind, logDirectory: config.logsDir,
            holderDirectory: config.holdersDir,
            holderExecutablePath: config.holderExecutablePath,
            cols: params.initialCols ?? 120, rows: params.initialRows ?? 32)
        // The real cwd of a remote session lives on its host (tmux -c); the
        // local ssh process just needs a valid local directory.
        let spawnCwd =
            hostEntry == nil ? cwd : FileManager.default.homeDirectoryForCurrentUser.path
        try await session.start(argv: plan.argv, cwd: spawnCwd, extraEnv: plan.extraEnv) {
            [weak self] sessionID, info in
            await self?.handleSessionExit(sessionID, info: info)
        }
        record.status = .starting
        records[id] = record
        live[id] = session
        await configure(session, record: record)

        if let prompt = params.initialPrompt {
            Task { [weak session] in
                await session?.injectInitialPrompt(prompt)
            }
        }

        await publishUpdate(record)
        persist()
        return record
    }

    /// Re-launches an exited session's conversation in a fresh PTY under the
    /// SAME dirijor id (the tmux promise after reboot).
    public func resume(sessionID: SessionID) async throws -> SessionRecord {
        guard var record = records[sessionID] else {
            throw ControlError.notFound(sessionID.rawValue)
        }
        guard live[sessionID] == nil else { return record }

        let argv: [String]
        let extraEnv: [String: String]
        let spawnCwd: String
        if let hostID = record.host {
            // Remote revive: identical ssh+tmux argv with the persisted tmux
            // name — `-A` reattaches a surviving remote session, otherwise the
            // agent resumes from the remote-side transcript. Local transcript/cwd
            // checks don't apply (both live on the remote host).
            let host = try resolveHost(hostID)
            let plan = InjectionBuilder.remotePlan(
                kind: record.kind,
                sessionID: sessionID,
                host: host,
                remoteCwd: record.cwd,
                socketPath: config.socketPath,
                cliPath: config.cliPath,
                agentSessionID: record.agentSessionID,
                resume: true
            )
            argv = plan.argv
            extraEnv = plan.extraEnv
            spawnCwd = FileManager.default.homeDirectoryForCurrentUser.path
        } else {
            guard
                let resumeArgv = InjectionBuilder.resumeArgv(
                    record: record,
                    injectDir: config.injectDir,
                    socketPath: config.socketPath,
                    cliPath: config.cliPath)
            else {
                throw ControlError.badRequest("session is not resumable")
            }
            // Claude resolves --resume from the transcript on disk; if we know
            // the path and it's gone, the spawn is doomed ("No conversation
            // found") — fail with an honest error instead. Codex keeps its own
            // index, so no transcript check there.
            if offlineResumability(of: &record) == .transcriptMissing {
                record.resumability = .transcriptMissing
                records[sessionID] = record
                await publishUpdate(record)
                persist()
                throw ControlError.badRequest(
                    "conversation transcript no longer exists on disk: \(record.transcriptPath ?? "?")"
                )
            }
            if !FileManager.default.fileExists(atPath: record.cwd) {
                throw ControlError.badRequest("cwd no longer exists: \(record.cwd)")
            }
            let plan = InjectionBuilder.plan(
                kind: record.kind,
                sessionID: sessionID,
                cwd: record.cwd,
                socketPath: config.socketPath,
                cliPath: config.cliPath,
                injectDir: config.injectDir
            )
            argv = resumeArgv
            extraEnv = plan.extraEnv
            spawnCwd = record.cwd
        }

        return try await relaunch(
            sessionID: sessionID, argv: argv, extraEnv: extraEnv, spawnCwd: spawnCwd)
    }

    /// Shared respawn tail for resume and migration: a fresh PTY under the
    /// SAME session id, keeping title/sidebar identity intact.
    ///
    /// Idempotent: when a holder for this session is still alive (a revive
    /// that was previously mislabeled exited, or a record gone stale across a
    /// daemon restart), the surviving holder is ADOPTED instead of spawning a
    /// second one — a second holder would interleave two writers into one
    /// output log and stack another ssh client onto the same remote tmux.
    /// Internal (not private) so tests can drive this seam directly.
    func relaunch(
        sessionID: SessionID, argv: [String], extraEnv: [String: String], spawnCwd: String
    ) async throws -> SessionRecord {
        guard var record = records[sessionID] else {
            throw ControlError.notFound(sessionID.rawValue)
        }
        let session: AgentSession
        let holderPaths = HolderPaths(
            directory: config.holdersDir, sessionID: sessionID.rawValue)
        let survivor = HolderClient(socketPath: holderPaths.socket.path)
        if let stat = try? survivor.stat(), stat.alive {
            session = AgentSession(
                id: sessionID,
                kind: record.kind,
                logDirectory: config.logsDir,
                holderDirectory: config.holdersDir,
                holderExecutablePath: config.holderExecutablePath,
                cols: Int(stat.cols ?? 120),
                rows: Int(stat.rows ?? 32))
            try await session.reattach(stat: stat, hibernation: record.hibernation) {
                [weak self] id, info in
                await self?.handleSessionExit(id, info: info)
            }
            DaemonLog.shared.log(
                "session \(sessionID) revive adopted live holder child=\(stat.childPID)")
        } else {
            session = AgentSession(
                id: sessionID,
                kind: record.kind,
                logDirectory: config.logsDir,
                holderDirectory: config.holdersDir,
                holderExecutablePath: config.holderExecutablePath)
            try await session.start(argv: argv, cwd: spawnCwd, extraEnv: extraEnv) {
                [weak self] id, info in
                await self?.handleSessionExit(id, info: info)
            }
        }
        // A resume marker in the log so reattaching clients see the seam.
        await session.write(Data())

        record.status = .starting
        record.resumability = .live
        record.needsInput = nil
        record.archivedAt = nil
        record.updatedAt = Date()
        records[sessionID] = record
        live[sessionID] = session
        await configure(session, record: record)
        await publishUpdate(record)
        persist()
        return record
    }

    // MARK: Migration / remote-host services

    /// `host.sync_prefs`: push local agent preferences to a configured host.
    public func syncPrefs(hostID: String) async throws -> HostSyncPrefsResult {
        let host = try resolveHost(hostID)
        return await PrefsSync.run(host: host, runner: runner)
    }

    /// `host.locate_repo`: find a checkout by origin URL (given directly or
    /// derived from a session's cwd + host). Cached inside `RepoLocator`.
    public func locateRepo(_ params: HostLocateRepoParams) async throws -> HostLocateRepoResult {
        let target = try params.host.map { try resolveHost($0) }
        var origin = params.originURL
        if origin == nil, let sessionID = params.sessionID,
            let record = records[sessionID]
        {
            let sourceHost = record.host.flatMap { try? resolveHost($0) }
            origin = await locator.origin(ofCwd: record.cwd, host: sourceHost)
        }
        guard let origin else { return HostLocateRepoResult(path: nil, originURL: nil) }
        let path = await locator.locate(
            origin: origin, on: target, localRoots: projects.values.map(\.root))
        return HostLocateRepoResult(path: path, originURL: origin)
    }

    /// `session.migrate`: hand a Claude session off between local and a remote
    /// host — WIP-commit + push, sync the target checkout, shuttle the
    /// transcript, then respawn THE SAME record on the target with
    /// `claude --resume`. `targetHostID == nil` migrates back to local.
    public func migrate(
        sessionID: SessionID, targetHostID: String?
    ) async throws -> SessionMigrateResult {
        guard var record = records[sessionID] else {
            throw ControlError.notFound(sessionID.rawValue)
        }
        guard record.kind == .claudeCode else {
            throw ControlError.badRequest(
                "only Claude Code sessions can move between hosts")
        }
        guard !migrating.contains(sessionID) else {
            throw ControlError.badRequest("session is already migrating")
        }
        guard record.host != targetHostID else {
            throw ControlError.badRequest(
                targetHostID.map { "session is already on \($0)" }
                    ?? "session is already local")
        }
        let sourceHost = try record.host.map { try resolveHost($0) }
        let targetHost = try targetHostID.map { try resolveHost($0) }
        migrating.insert(sessionID)
        defer { migrating.remove(sessionID) }

        // Locate the target checkout by origin (shared with host.locate_repo).
        guard let origin = await locator.origin(ofCwd: record.cwd, host: sourceHost) else {
            throw ControlError.badRequest(
                "session cwd is not inside a git repository with an 'origin' remote: \(record.cwd)"
            )
        }
        let localRoots = projects.values.map(\.root)
        guard
            let targetRepo = await locator.locate(
                origin: origin, on: targetHost, localRoots: localRoots)
        else {
            if let targetHost {
                throw ControlError.badRequest(
                    "repo not cloned on \(targetHost.displayName) — clone \(origin) under \(targetHost.defaultCwd ?? "~") first"
                )
            }
            throw ControlError.badRequest(
                "repo not cloned locally — no known project has origin \(origin)")
        }

        // Phase 1 (source agent still alive, everything retryable): WIP commit
        // + push, then fetch + hard-sync the target checkout.
        let prepared = try await migrator.prepare(
            sourceCwd: record.cwd,
            sourceHost: sourceHost,
            targetHost: targetHost,
            targetRepoRoot: targetRepo,
            targetName: targetHost?.displayName ?? "local")

        // Point of no return: stop the source agent. Detach from `live` first
        // (the archive pattern) so bookkeeping is ours, then wait for the exit
        // callback to land so a late `handleSessionExit` can't clobber the
        // respawned session.
        var warnings: [String] = []
        if let session = live.removeValue(forKey: sessionID) {
            await session.terminate()
            await statusEngine?.untrack(sessionID: sessionID)
            for _ in 0..<50 {
                if case .exited = records[sessionID]?.status { break }
                try? await Task.sleep(for: .milliseconds(100))
            }
        }
        if let sourceHost {
            // Also end the remote tmux session so the agent doesn't linger.
            if let warning = await migrator.killRemoteTmux(
                host: sourceHost, sessionID: sessionID)
            {
                warnings.append(warning)
            }
        }
        record = records[sessionID] ?? record

        // Phase 2: transcript shuttle (source stopped ⇒ the jsonl is final).
        // Never deletes the source copy; a missing transcript is non-fatal.
        let shuttle = await migrator.shuttleTranscript(
            record: record,
            sourceHost: sourceHost,
            targetHost: targetHost,
            prepared: prepared)
        if let warning = shuttle.warning { warnings.append(warning) }

        // Rewrite the record in place: same id/title/sidebar position, new
        // host + cwd. The UI just sees the record change + a new attachment.
        record.host = targetHost?.id
        record.cwd = prepared.targetRepoRoot
        record.projectID = upsertProject(
            root: targetHost == nil
                ? GitWorktrees.repositoryRoot(of: prepared.targetRepoRoot)
                    ?? prepared.targetRepoRoot
                : prepared.targetRepoRoot
        ).id
        record.worktreePath = nil
        record.gitBranch = prepared.branch
        record.transcriptPath = targetHost == nil ? shuttle.localTargetPath : nil
        record.status = .exited(ExitInfo(reason: .exited, code: 0))
        record.needsInput = nil
        record.hibernation = nil
        record.memoryBytes = nil
        record.listeningPorts = nil
        record.resumability = .resumable
        record.updatedAt = Date()
        records[sessionID] = record

        // Cutover: the normal resume path revives the conversation on the
        // target (`claude --resume <uuid>`, ssh+tmux-wrapped when remote).
        // Without a transcript there is nothing to resume — respawn fresh.
        let revived: SessionRecord
        if shuttle.migrated {
            revived = try await resume(sessionID: sessionID)
        } else {
            revived = try await respawnFresh(sessionID: sessionID)
        }
        DaemonLog.shared.log(
            "session \(sessionID) migrated to \(targetHost?.id ?? "local") at \(prepared.targetRepoRoot) (transcript: \(shuttle.migrated))"
        )
        return SessionMigrateResult(
            session: revived,
            transcriptMigrated: shuttle.migrated,
            warning: warnings.isEmpty ? nil : warnings.joined(separator: "; "))
    }

    /// Migration fallback when no transcript travelled: a brand-new
    /// conversation in the target checkout under the same session record.
    private func respawnFresh(sessionID: SessionID) async throws -> SessionRecord {
        guard var record = records[sessionID] else {
            throw ControlError.notFound(sessionID.rawValue)
        }
        let plan: InjectionBuilder.SpawnPlan
        let spawnCwd: String
        if let hostID = record.host {
            let host = try resolveHost(hostID)
            plan = InjectionBuilder.remotePlan(
                kind: record.kind,
                sessionID: sessionID,
                host: host,
                remoteCwd: record.cwd,
                socketPath: config.socketPath,
                cliPath: config.cliPath)
            spawnCwd = FileManager.default.homeDirectoryForCurrentUser.path
        } else {
            plan = InjectionBuilder.plan(
                kind: record.kind,
                sessionID: sessionID,
                cwd: record.cwd,
                socketPath: config.socketPath,
                cliPath: config.cliPath,
                injectDir: config.injectDir)
            spawnCwd = record.cwd
        }
        record.agentSessionID = plan.agentSessionID
        record.transcriptPath = plan.transcriptPath
        records[sessionID] = record
        return try await relaunch(
            sessionID: sessionID, argv: plan.argv, extraEnv: plan.extraEnv, spawnCwd: spawnCwd)
    }

    /// Explicit geometry-ownership change (hand-off "Take back control" / "Resume").
    public func setSessionOwner(sessionID: SessionID, role: ClientRole) async throws {
        guard let session = live[sessionID] else {
            throw ControlError.notFound(sessionID.rawValue)
        }
        await session.setOwner(role: role)
    }

    public func kill(sessionID: SessionID) async throws {
        guard let session = live[sessionID] else {
            throw ControlError.notFound(sessionID.rawValue)
        }
        await session.terminate()
    }

    /// Archives a session: kills its whole process tree (freeing all resources)
    /// but keeps the record so the conversation can be revived via `resume`.
    /// Removing it from `live` first means the exit-watcher callback finds the
    /// record already archived and leaves the `.archived` reason in place.
    public func archive(sessionID: SessionID) async throws {
        guard var record = records[sessionID] else {
            throw ControlError.notFound(sessionID.rawValue)
        }
        if let session = live.removeValue(forKey: sessionID) {
            await session.terminate()
            await statusEngine?.untrack(sessionID: sessionID)
        }
        record.archivedAt = Date()
        if record.status.isRunning {
            record.status = .exited(ExitInfo(reason: .archived))
        }
        record.needsInput = nil
        record.hibernation = nil
        record.memoryBytes = nil
        record.listeningPorts = nil
        record.resumability = offlineResumability(of: &record)
        record.updatedAt = Date()
        records[sessionID] = record
        await publishUpdate(record)
        persist()
    }

    /// Brings an archived record back into the normal list. Does NOT respawn —
    /// reviving the conversation is `resume`'s job (which also unarchives).
    public func unarchive(sessionID: SessionID) async throws {
        guard var record = records[sessionID] else {
            throw ControlError.notFound(sessionID.rawValue)
        }
        guard record.isArchived else { return }
        record.archivedAt = nil
        record.updatedAt = Date()
        records[sessionID] = record
        await publishUpdate(record)
        persist()
    }

    /// Sessions the user closed, newest last — the "reopen closed tab" stack.
    private var recentlyClosed: [SessionRecord] = []

    public func remove(sessionID: SessionID) async throws {
        if let session = live[sessionID] {
            await session.terminate(graceSeconds: 0.5)
        }
        if let record = records[sessionID] {
            recentlyClosed.append(record)
            if recentlyClosed.count > 10 { recentlyClosed.removeFirst() }
        }
        records.removeValue(forKey: sessionID)
        live.removeValue(forKey: sessionID)
        SessionLogStorage.remove(directory: config.logsDir, sessionID: sessionID)
        await statusEngine?.untrack(sessionID: sessionID)
        publishedFacts.removeValue(forKey: sessionID)
        await events.publish(
            name: EventName.sessionRemoved,
            encoding: SessionRemovedEvent(id: sessionID, reason: "released"),
            sessionID: sessionID
        )
        persist()
    }

    /// Reopens the most recently closed session (browser-style ⌘⇧T). First-class
    /// agents resume their conversation via the normal resume path; everything
    /// else respawns fresh in the same folder.
    public func reopenLastClosed() async throws -> SessionRecord {
        guard var record = recentlyClosed.popLast() else {
            throw ControlError.badRequest("no recently closed session")
        }
        guard record.host != nil || FileManager.default.fileExists(atPath: record.cwd) else {
            // Folder is gone; skip to the next candidate if any. (Remote cwds
            // live on the remote host and can't be checked locally.)
            return try await reopenLastClosed()
        }
        if record.host != nil
            || InjectionBuilder.resumeArgv(
                record: record,
                injectDir: config.injectDir,
                socketPath: config.socketPath,
                cliPath: config.cliPath) != nil
        {
            // Reinsert the record (as exited) and drive the existing resume path
            // so the conversation comes back under its original identity —
            // remote sessions always take this path (tmux -A reattaches).
            record.status = .exited(ExitInfo(reason: .exited, code: 0))
            record.hibernation = nil
            record.memoryBytes = nil
            record.listeningPorts = nil
            records[record.id] = record
            return try await resume(sessionID: record.id)
        }
        // Shell / generic: a fresh session in the same place.
        return try await spawn(
            SessionSpawnParams(
                kind: record.kind, cwd: record.cwd,
                title: record.titleSource == .userRename ? record.title : nil))
    }

    /// Agent-side conversation ids the daemon already tracks (live or exited).
    /// The History scan excludes these so it only surfaces conversations the
    /// sidebar doesn't already offer to resume/reopen.
    func trackedAgentSessionIDs() -> Set<String> {
        Set(records.values.compactMap(\.agentSessionID))
    }

    /// Resume a conversation discovered on disk by `HistoryScanner` into a
    /// fresh tracked session. Mints a synthetic exited record from the entry
    /// and drives the existing `resume` path, which re-checks the transcript
    /// and cwd and builds the resume argv via `InjectionBuilder.resumeArgv`.
    public func resumeFromHistory(_ entry: HistoryEntry) async throws -> SessionRecord {
        guard entry.kind.isFirstClass else {
            throw ControlError.badRequest("not a resumable agent kind")
        }
        guard FileManager.default.fileExists(atPath: entry.cwd) else {
            throw ControlError.badRequest("cwd no longer exists: \(entry.cwd)")
        }
        guard FileManager.default.fileExists(atPath: entry.transcriptPath) else {
            throw ControlError.badRequest(
                "conversation transcript no longer exists on disk: \(entry.transcriptPath)")
        }

        let projectRoot = GitWorktrees.repositoryRoot(of: entry.cwd) ?? entry.cwd
        let project = upsertProject(root: projectRoot)

        let id = SessionID.generate()
        let record = SessionRecord(
            id: id,
            kind: entry.kind,
            cwd: entry.cwd,
            projectID: project.id,
            gitBranch: GitWorktrees.currentBranch(in: entry.cwd),
            title: entry.title ?? TitleMaker.placeholder(kind: entry.kind, cwd: entry.cwd),
            titleSource: entry.title != nil ? .agentProvided : .placeholder,
            agentSessionID: entry.id,
            transcriptPath: entry.transcriptPath,
            status: .exited(ExitInfo(reason: .exited, code: 0)),
            resumability: .resumable)
        records[id] = record
        return try await resume(sessionID: id)
    }

    private func handleSessionExit(_ sessionID: SessionID, info: ExitInfo) async {
        let wasLive = live.removeValue(forKey: sessionID) != nil
        await statusEngine?.processExited(sessionID: sessionID, info: info)
        guard var record = records[sessionID] else { return }
        // An archive already detached this session and stamped the record
        // `.archived`; the tree's eventual death must not rewrite that reason.
        if !wasLive, record.isArchived {
            pruneSessionLogs()
            return
        }
        record.status = .exited(info)
        record.needsInput = nil
        record.hibernation = nil
        record.memoryBytes = nil
        record.listeningPorts = nil
        record.resumability = offlineResumability(of: &record)
        record.updatedAt = Date()
        records[sessionID] = record
        await publishUpdate(record)
        persist()
        pruneSessionLogs()
    }

    private func pruneSessionLogs() {
        SessionLogStorage.prune(
            directory: config.logsDir,
            keeping: Set(records.keys),
            protectedSessionIDs: Set(live.keys))
    }

    /// True resumability of a non-live record. First-class agents with a
    /// conversation id are resumable — downgraded to `.transcriptMissing` only
    /// when the transcript exists nowhere on disk (`claude --resume` would just
    /// exit with "No conversation found"). Claude relocates the transcript to
    /// another project dir when the agent enters a worktree, so a missing file
    /// at the recorded path first triggers a scan of all project dirs, and the
    /// record's path is repaired in place when found. Codex records carry no
    /// transcript path, so they stay `.resumable` unverified.
    private func offlineResumability(of record: inout SessionRecord) -> Resumability {
        // Remote sessions are always revivable: the same tmux name reattaches
        // (or restarts the agent on its remote host); nothing local to verify.
        if record.host != nil { return .resumable }
        guard let agentID = record.agentSessionID, record.kind.descriptor.canResume else {
            return .notResumable
        }
        guard record.kind == .claudeCode, let path = record.transcriptPath else {
            return .resumable
        }
        if FileManager.default.fileExists(atPath: path) { return .resumable }
        if let moved = InjectionBuilder.findClaudeTranscript(sessionUUID: agentID) {
            DaemonLog.shared.log(
                "session \(record.id) transcript moved: \(path) → \(moved)")
            record.transcriptPath = moved
            return .resumable
        }
        return .transcriptMissing
    }

    // MARK: Queries / mutations

    public func list() -> SessionListResult {
        SessionListResult(
            sessions: records.values.sorted { $0.createdAt > $1.createdAt },
            projects: projects.values.sorted { $0.name < $1.name }
        )
    }

    public func record(_ id: SessionID) -> SessionRecord? { records[id] }
    public func session(_ id: SessionID) -> AgentSession? { live[id] }

    /// Loads a session's Git patch on the machine that owns its cwd. Remote
    /// sessions are resolved through hosts.json and executed over bounded SSH.
    public func readDiff(
        sessionID: SessionID, base: SessionDiffBase = .defaultBranch
    ) async throws -> SessionReadDiffResult {
        guard let record = records[sessionID] else {
            throw ControlError.notFound(sessionID.rawValue)
        }
        let host = try record.host.map(resolveHost)
        return try await WorktreeDiffLoader.load(
            cwd: record.cwd, host: host, base: base, runner: runner)
    }

    public func rename(sessionID: SessionID, title: String) async throws {
        guard var record = records[sessionID] else {
            throw ControlError.notFound(sessionID.rawValue)
        }
        record.applyTitle(title, source: .userRename)
        record.updatedAt = Date()
        records[sessionID] = record
        await publishUpdate(record)
        persist()
    }

    public func markSeen(sessionID: SessionID) async throws {
        guard var record = records[sessionID] else {
            throw ControlError.notFound(sessionID.rawValue)
        }
        record.lastSeenAt = Date()
        records[sessionID] = record
        await publishUpdate(record)
        persist()
    }

    @discardableResult
    public func addProject(root: String) async -> Project {
        let project = upsertProject(root: root)
        Task { await events.publish(name: EventName.projectUpdated, params: (try? JSONValue(encoding: project)) ?? .null) }
        persist()
        return project
    }

    private func upsertProject(root: String) -> Project {
        let project = Project(root: root)
        if let existing = projects[project.id] { return existing }
        projects[project.id] = project
        return project
    }

    // MARK: Hibernation / resources

    /// Live sessions for the ResourceGovernor's scan loop.
    func liveSessionsSnapshot() -> [(id: SessionID, session: AgentSession)] {
        live.map { ($0.key, $0.value) }
    }

    /// Freezes a live session's tree. The session's hibernation callback
    /// patches + publishes the record.
    public func hibernate(sessionID: SessionID, reason: HibernationReason) async throws {
        guard let session = live[sessionID] else {
            throw ControlError.notFound(sessionID.rawValue)
        }
        await session.hibernate(reason: reason)
    }

    /// Wakes a hibernated session (no-op when it isn't hibernated).
    public func wakeSession(sessionID: SessionID) async throws {
        guard let session = live[sessionID] else {
            throw ControlError.notFound(sessionID.rawValue)
        }
        await session.wake()
    }

    /// Mirrors AgentSession hibernation state into the record — covers the
    /// auto-wake paths (attach / input) that never pass through the registry.
    func applyHibernationChange(sessionID: SessionID, info: HibernationInfo?) async {
        guard var record = records[sessionID] else { return }
        guard record.hibernation != info else { return }
        record.hibernation = info
        record.updatedAt = Date()
        records[sessionID] = record
        await publishUpdate(record)
        persist()
    }

    /// Mirrors AgentSession ownership into the record — drives the Mac "Active on
    /// iPhone" card and the phone "Active on your Mac" card. Fired from attach /
    /// detach / setOwner, none of which pass through the registry otherwise.
    func applyRemoteActiveChange(sessionID: SessionID, active: Bool) async {
        guard var record = records[sessionID] else { return }
        guard record.remoteActive != active else { return }
        record.remoteActive = active
        record.updatedAt = Date()
        records[sessionID] = record
        await publishUpdate(record)
        persist()
    }

    /// Governor scan results. Deliberately does NOT bump `updatedAt`: resource
    /// sampling must not reset the idle-hibernation clock.
    func applyResourceSample(
        sessionID: SessionID,
        memoryBytes: UInt64?,
        ports: [PortInfo]?,
        artifacts: [SessionArtifact]?
    ) async {
        guard var record = records[sessionID] else { return }
        var memoryChanged = false
        var portsChanged = false
        var artifactsChanged = false
        if let memoryBytes, record.memoryBytes != memoryBytes {
            record.memoryBytes = memoryBytes
            memoryChanged = true
        }
        if let ports, (record.listeningPorts ?? []) != ports {
            record.listeningPorts = ports
            portsChanged = true
        }
        if let artifacts, (record.artifacts ?? []) != artifacts {
            record.artifacts = artifacts
            artifactsChanged = true
        }
        guard memoryChanged || portsChanged || artifactsChanged else { return }
        records[sessionID] = record
        await events.publish(
            name: EventName.sessionResources,
            encoding: SessionResourcesEvent(
                id: sessionID,
                memoryBytes: memoryChanged ? record.memoryBytes : nil,
                listeningPorts: portsChanged ? record.listeningPorts : nil,
                artifacts: artifactsChanged ? record.artifacts : nil),
            sessionID: sessionID)

        // Preserve the narrow first-seen artifact events without routing this
        // high-frequency sample through the full SessionRecord funnel.
        if let previous = publishedFacts[sessionID] {
            for artifact in record.artifacts ?? []
            where !previous.artifactURLs.contains(artifact.url) {
                await events.publish(
                    name: EventName.sessionArtifact,
                    encoding: SessionArtifactEvent.artifact(id: sessionID, artifact),
                    sessionID: sessionID)
            }
            for port in record.listeningPorts ?? [] where !previous.ports.contains(port.port) {
                await events.publish(
                    name: EventName.sessionArtifact,
                    encoding: SessionArtifactEvent.port(id: sessionID, port),
                    sessionID: sessionID)
            }
        }
        publishedFacts[sessionID] = PublishedFacts(record)

        // Memory is ephemeral and resampled after restart. Avoid rewriting the
        // durable state file for a number that changes every governor sweep.
        if portsChanged || artifactsChanged { persist() }
    }

    /// PullRequestMonitor results. Doesn't bump `updatedAt` (a background poll
    /// isn't session activity) and skips publishing when only `fetchedAt`
    /// moved, so an unchanged PR doesn't churn persistence every sweep.
    func applyPullRequestStatuses(sessionID: SessionID, statuses: [PullRequestStatus]) async {
        guard var record = records[sessionID] else { return }
        let current = record.pullRequests ?? []
        let materiallySame =
            current.count == statuses.count
            && zip(current, statuses).allSatisfy { $0.sameStats(as: $1) }
        guard !materiallySame else { return }
        record.pullRequests = statuses.isEmpty ? nil : statuses
        records[sessionID] = record
        await publishUpdate(record)
        persist()
    }

    // MARK: Branch watching

    private var branchMonitorTask: Task<Void, Never>?

    /// Polls each running session's git branch on a short cadence so switching
    /// branches in a terminal is reflected live in the sidebar/pane header.
    public func startBranchMonitor(interval: Duration = .seconds(5)) {
        branchMonitorTask?.cancel()
        branchMonitorTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: interval)
                await self?.refreshBranches()
            }
        }
    }

    public func stopBranchMonitor() {
        branchMonitorTask?.cancel()
        branchMonitorTask = nil
    }

    /// Re-reads each running session's live location — the agent's real working
    /// directory (which Claude moves under `.claude/worktrees/…` via `--worktree`
    /// / EnterWorktree without the daemon knowing) plus the branch there — and
    /// publishes the ones that changed. Both reads are cheap libproc/`.git/HEAD`
    /// probes, no subprocess. `worktreePath` is recomputed from live state every
    /// tick so it self-heals when the agent enters or leaves a worktree.
    /// Deliberately does not bump `updatedAt` — moving dirs isn't session activity.
    func refreshBranches() async {
        var didChange = false
        // Remote sessions are skipped: their real cwd/branch live on another machine,
        // and the local ssh process's cwd would only produce junk.
        for (id, record) in records where record.status.isRunning && record.host == nil {
            let liveCwd = await live[id]?.agentWorkingDir() ?? record.cwd
            let worktreePath = GitHead.isLinkedWorktree(liveCwd) ? liveCwd : nil
            let branch = GitHead.branch(inWorkingDir: liveCwd)
            guard branch != record.gitBranch || worktreePath != record.worktreePath else {
                continue
            }
            var updated = record
            updated.gitBranch = branch
            updated.worktreePath = worktreePath
            records[id] = updated
            await publishUpdate(updated)
            didChange = true
        }
        if didChange { persist() }
    }

    // MARK: Status-engine callbacks

    /// Running Claude sessions whose title can still be improved by the
    /// AI-generated `ai-title` (i.e. not user-renamed), with transcript paths.
    func claudeTitleCandidates() -> [(SessionID, String)] {
        records.values.compactMap { record in
            guard record.effectiveKind == .claudeCode,
                record.status.isRunning,
                record.titleSource <= .agentProvided,
                let path = record.transcriptPath,
                live[record.id] != nil
            else { return nil }
            return (record.id, path)
        }
    }

    /// Running Codex sessions still showing a generated placeholder. Their
    /// notify payload may omit `input-messages`, so TitleWatcher uses the bound
    /// Codex thread id to recover the first prompt from its rollout transcript.
    func codexTitleCandidates() -> [(SessionID, String, Date)] {
        records.values.compactMap { record in
            guard record.effectiveKind == .codex,
                record.status.isRunning,
                record.titleSource == .placeholder,
                let agentID = record.agentSessionID,
                !agentID.isEmpty
            else { return nil }
            return (record.id, agentID, record.createdAt)
        }
    }

    /// Running Cursor sessions and their chat-creation anchor. User-renamed
    /// sessions remain here so their matched stores stay reserved and cannot be
    /// claimed by another concurrent Cursor session; title priority still
    /// prevents Cursor metadata from replacing the manual name.
    func cursorTitleCandidates() -> [(SessionID, Date)] {
        records.values.compactMap { record in
            guard record.effectiveKind == .cursor,
                record.status.isRunning,
                // Remote Cursor chats live in that host's ~/.cursor store; the
                // local store scan would claim an unrelated chat.
                record.host == nil,
                live[record.id] != nil
            else { return nil }
            let anchor = record.foregroundAgent == .cursor
                ? record.updatedAt
                : record.createdAt
            return (record.id, anchor)
        }
    }

    /// A shell tab started (or stopped) running a first-class agent in the PTY
    /// foreground — flip the record's adopted kind so UI + notifications follow,
    /// and refresh a placeholder title so the tab reads "Claude Code — …".
    func applyForegroundAgent(sessionID: SessionID, agent: AgentKind?) async {
        guard var record = records[sessionID], record.foregroundAgent != agent else { return }
        record.foregroundAgent = agent
        if record.titleSource == .placeholder {
            record.title = TitleMaker.placeholder(
                kind: record.effectiveKind, cwd: record.cwd, date: record.createdAt)
        }
        record.updatedAt = Date()
        records[sessionID] = record
        await publishUpdate(record)
        persist()
    }

    func applyStatusOutcome(
        sessionID: SessionID,
        status: SessionStatus?,
        needsInput: NeedsInputDetail?,
        turnCompleted: Bool
    ) async {
        guard var record = records[sessionID] else { return }
        var changed = false
        if let status, status != record.status {
            record.status = status
            if case .needsInput = status {} else { record.needsInput = nil }
            changed = true
        }
        if let needsInput {
            record.needsInput = needsInput
            changed = true
        }
        if turnCompleted {
            record.lastTurnCompletedAt = Date()
            changed = true
        }
        guard changed else { return }
        record.updatedAt = Date()
        records[sessionID] = record
        await publishUpdate(record)
        persist()
    }

    func applyHookMetadata(sessionID: SessionID, meta: HookMetadata) async {
        guard var record = records[sessionID] else { return }
        var changed = false
        if let agentID = meta.agentSessionID, agentID != record.agentSessionID {
            record.agentSessionID = agentID
            record.resumability = .live
            changed = true
        }
        if let transcript = meta.transcriptPath, transcript != record.transcriptPath {
            record.transcriptPath = transcript
            changed = true
        }
        if let title = meta.firstPromptTitle, record.titleSource == .placeholder {
            record.applyTitle(title, source: .firstPrompt)
            changed = true
        }
        guard changed else { return }
        record.updatedAt = Date()
        records[sessionID] = record
        await publishUpdate(record)
        persist()
    }

    /// First-prompt fallback recovered from an agent transcript. A user rename
    /// or any stronger title source always wins.
    func applyFirstPromptTitle(sessionID: SessionID, title: String) async {
        guard var record = records[sessionID], record.titleSource == .placeholder else { return }
        guard record.applyTitle(title, source: .firstPrompt) else { return }
        record.updatedAt = Date()
        records[sessionID] = record
        await publishUpdate(record)
        persist()
    }

    /// Title updates from transcript/index watchers (agentProvided level).
    public func applyAgentTitle(sessionID: SessionID, title: String) async {
        guard var record = records[sessionID] else { return }
        guard record.applyTitle(title, source: .agentProvided) else { return }
        record.updatedAt = Date()
        records[sessionID] = record
        await publishUpdate(record)
        persist()
    }

    // MARK: Plumbing

    /// What the last publish of a session looked like, reduced to just the
    /// fields the narrow events are derived from. Holding this instead of the
    /// whole previous record keeps the diff cheap and makes it obvious that
    /// adding a field to SessionRecord does not silently start emitting events.
    private struct PublishedFacts {
        var status: SessionStatus
        var blockerIdentity: Int?
        var artifactURLs: Set<String>
        var ports: Set<Int>
        var archived: Bool

        init(_ record: SessionRecord) {
            status = record.status
            blockerIdentity = record.needsInput?.blockerIdentity
            artifactURLs = Set((record.artifacts ?? []).map(\.url))
            ports = Set((record.listeningPorts ?? []).map(\.port))
            archived = record.isArchived
        }
    }

    private var publishedFacts: [SessionID: PublishedFacts] = [:]

    /// The single funnel every record mutation already went through. The coarse
    /// `session.updated` goes out first so a consumer that reacts to a narrow
    /// event and immediately re-reads state can never observe a stale record;
    /// the narrow events are then derived by diffing against the last publish,
    /// which is why there is no second notification path to keep in sync.
    private func publishUpdate(_ record: SessionRecord) async {
        guard let params = try? JSONValue(encoding: record) else { return }
        await events.publish(
            name: EventName.sessionUpdated, params: params, sessionID: record.id)

        let previous = publishedFacts[record.id]
        let facts = PublishedFacts(record)
        publishedFacts[record.id] = facts

        guard let previous else {
            await events.publish(
                name: EventName.sessionSpawned, params: params, sessionID: record.id)
            return
        }

        if previous.status != facts.status {
            await events.publish(
                name: EventName.sessionStatus,
                encoding: SessionStatusEvent(
                    id: record.id, from: previous.status, to: record.status,
                    needsInput: record.needsInput),
                sessionID: record.id)
        }
        // Dedupe on blocker identity, not on the status edge: an agent can ask a
        // second question without ever leaving `needsInput`, and that second
        // question is exactly what a watcher is waiting for.
        if let needsInput = record.needsInput,
            previous.blockerIdentity != needsInput.blockerIdentity
        {
            await events.publish(
                name: EventName.sessionNeedsInput,
                encoding: SessionNeedsInputEvent(id: record.id, needsInput: needsInput),
                sessionID: record.id)
        }
        for artifact in record.artifacts ?? []
        where !previous.artifactURLs.contains(artifact.url) {
            await events.publish(
                name: EventName.sessionArtifact,
                encoding: SessionArtifactEvent.artifact(id: record.id, artifact),
                sessionID: record.id)
        }
        for port in record.listeningPorts ?? [] where !previous.ports.contains(port.port) {
            await events.publish(
                name: EventName.sessionArtifact,
                encoding: SessionArtifactEvent.port(id: record.id, port),
                sessionID: record.id)
        }
        if !previous.archived, facts.archived {
            await events.publish(
                name: EventName.sessionArchived, params: params, sessionID: record.id)
        }
    }

    /// Coalesced PTY-output notification from the StatusEngine's scan loop.
    /// Routed through the registry rather than published from the engine so the
    /// bus keeps exactly one writer for session-scoped events.
    func publishOutputActivity(sessionID: SessionID, contentSeq: UInt64) async {
        await events.publish(
            name: EventName.sessionOutput,
            encoding: SessionOutputEvent(id: sessionID, contentSeq: contentSeq),
            sessionID: sessionID)
    }

    private func persist() {
        let state = snapshotState()
        Task { await persistence.schedule(state) }
    }

    private func snapshotState() -> PersistedState {
        PersistedState(
            projects: Array(projects.values),
            sessions: Array(records.values)
        )
    }
}
