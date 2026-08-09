import Darwin
import DirijorCore
import DirijorDetection
import DirijorHolderKit
import DirijorProtocol
import Foundation

/// Where a session's output frames go (an attached data connection).
public protocol SessionOutputSink: Sendable, AnyObject {
    var sinkID: UUID { get }
    /// Bytes queued but not yet accepted by the transport.
    var pendingBytes: Int { get }
    func deliver(_ frame: Frame)
    func closeSink()
}

/// Owns the daemon-side view of one holder-backed session: holder control,
/// offset-addressed log tailing, and the headless screen.
public actor AgentSession {
    public let id: SessionID
    public let kind: AgentKind

    private let log: OutputLog
    private let screen: HeadlessScreen
    private let logDirectory: URL
    private let holderPaths: HolderPaths
    private let holderExecutablePath: String
    private var holder: HolderClient?
    public private(set) var pid: pid_t = -1
    private var exitSource: DispatchSourceProcess?
    private var logSource: DispatchSourceFileSystemObject?
    private var logReadOffset: UInt64 = 0
    private var logMarkerBuffer = Data()
    /// Debounced durable visible-screen checkpoint. It is an acceleration cache
    /// for daemon reattach; the holder's output log remains authoritative.
    private var checkpointTask: Task<Void, Never>?
    /// Set by output without canceling/recreating the sleeper. Busy TUIs can
    /// write hundreds of chunks per second; task churn there showed up in the
    /// daemon's idle-ish CPU profile even though no checkpoint was written.
    private var checkpointDirty = false
    /// The per-session log survives relaunches under the same session id, so
    /// it can still contain a PREVIOUS incarnation's exit marker (e.g. the
    /// signaled-9 written when a migrate killed the old agent). Exit markers
    /// wholly below this stream offset belong to prior incarnations: they are
    /// replayed for screen content but never applied to this child. Comes
    /// from the holder's stat epoch (pre-spawn log tail as the fallback for
    /// holders built before the field existed); 0 on reattach to such holders
    /// preserves the old daemon-restart semantics.
    private var exitMarkerFloor: UInt64 = 0
    private let queue: DispatchQueue
    private var sinks: [UUID: any SessionOutputSink] = [:]
    /// Role of each attached sink. Under the hand-off model a `.mobile` sink that
    /// is attached (and not currently reclaimed by the Mac) OWNS the geometry: the
    /// PTY is sized to the phone and desktop resizes are ignored until control
    /// returns to the Mac.
    private var sinkRoles: [UUID: ClientRole] = [:]
    /// Last size each role requested, so we can pick the effective geometry and
    /// hand geometry between the Mac and a still-attached phone.
    private var desktopSize: (cols: Int, rows: Int)?
    private var mobileSize: (cols: Int, rows: Int)?
    /// The Mac reclaimed control while a phone is still attached ("Take back
    /// control"). While true an attached `.mobile` sink does NOT own geometry —
    /// the desktop size drives and the phone shows an "Active on your Mac" card.
    /// Cleared when a fresh phone attaches, when the phone taps "Resume", or when
    /// no desktop sink remains.
    private var desktopHold: Bool = false
    /// Last remoteActive value published, so the callback only fires on a flip.
    private var lastRemoteActive: Bool = false
    /// Sinks that missed a grid diff (backpressure) and must be re-seeded with a
    /// full snapshot once they drain — otherwise their grid stays desynced (a
    /// diff stream can't self-correct a gap).
    private var gridDesyncedSinks: Set<UUID> = []
    /// Last (altScreen, mouseReporting) broadcast on the data channel, so we only
    /// emit a `.modes` frame when the value actually changes (tracked per session,
    /// not per sink; a freshly attached sink gets the current value directly).
    private var lastSentModes: (altScreen: Bool, mouseReporting: Bool)?
    public private(set) var exitInfo: ExitInfo?

    // MARK: Hibernation state

    private enum LifeState { case running, hibernating, hibernated, waking }
    private var lifeState: LifeState = .running
    /// Non-nil while the process tree is SIGSTOPped.
    public private(set) var hibernationInfo: HibernationInfo?
    /// When the last sink detached (sinks became empty) — idle-policy input.
    public private(set) var lastDetachAt: Date?
    /// Input received while hibernated; flushed right after SIGCONT.
    private var queuedInput = Data()
    /// Input arriving during the deferred holder launch; flushed after stat
    /// confirms the holder is ready so early keystrokes are never dropped.
    private var queuedLaunchInput = Data()
    /// Last resize requested while hibernated; applied on wake.
    private var queuedResize: (cols: Int, rows: Int)?

    /// Resizes closer together than this are treated as one live drag, so the
    /// per-resize tail runs once at the end instead of on every step.
    private static let resizeDragWindow: Duration = .milliseconds(200)
    /// How long the geometry must hold still before the drag is considered over.
    private static let resizeSettleDelay: Duration = .milliseconds(120)
    private var lastResizeAt: ContinuousClock.Instant?
    private var resizeSettle: Task<Void, Never>?
    /// Screen captured before a drag's first resize, held for its settle pass.
    private var resizeRecovery: [String]?

    public var sinkCount: Int { sinks.count }
    public var isHibernated: Bool { hibernationInfo != nil }

    /// The first-class agent currently in the PTY foreground, if any — how a
    /// `claude` launched by hand inside a shell tab gets recognized. Reads the
    /// foreground process group from holder stat and matches its leader's (or
    /// a direct child's) executable name.
    ///
    /// Returns the last probed value and refreshes it in the background: the
    /// probe is a blocking holder round-trip plus libproc walks, and it used
    /// to run inline inside the status engine's serial tick loop, where one
    /// slow holder stalled status for every session. Callers poll on a ~2s
    /// cadence, so a one-probe lag is invisible.
    public func foregroundAgentKind() -> AgentKind? {
        guard let holder, exitInfo == nil, !isHibernated else { return nil }
        if !foregroundProbeRunning {
            foregroundProbeRunning = true
            let ownPid = pid
            Task.detached(priority: .utility) { [weak self] in
                let kind = Self.probeForegroundAgent(holder: holder, ownPid: ownPid)
                await self?.storeForegroundProbe(kind)
            }
        }
        return cachedForegroundAgent
    }

    private var cachedForegroundAgent: AgentKind?
    private var foregroundProbeRunning = false

    private func storeForegroundProbe(_ kind: AgentKind?) {
        cachedForegroundAgent = kind
        foregroundProbeRunning = false
    }

    private static func probeForegroundAgent(holder: HolderClient, ownPid: pid_t) -> AgentKind? {
        guard let fg = try? holder.stat().foregroundPID, fg > 0 else { return nil }
        // Don't re-report the session's own spawned agent.
        var candidates: [pid_t] = fg == ownPid ? [] : [fg]
        candidates.append(contentsOf: ProcessTree.childPids(fg))
        for candidate in candidates {
            if let kind = agentKind(forExecutableOf: candidate) { return kind }
        }
        return nil
    }

    /// The live working directory of the session's agent process, or nil when it
    /// can't be read. Claude/Codex `chdir` themselves into a worktree (`--worktree`,
    /// EnterWorktree) without the daemon spawning it, so the spawn-time cwd goes
    /// stale; this reads the real cwd off the agent (or foreground) process so the
    /// branch monitor can notice the move. Prefers the process that *is* the agent
    /// — that's the one that chdir'd — searching the session leader, its children,
    /// and the foreground group; falls back to the foreground leader (a manual
    /// `cd` in a shell tab) and finally the session leader.
    ///
    /// Cached and refreshed off-actor for the same reason as
    /// `foregroundAgentKind()`; the branch monitor polls every 5s and tolerates
    /// one probe of lag.
    public func agentWorkingDir() -> String? {
        guard pid > 0, exitInfo == nil, !isHibernated else { return nil }
        if !cwdProbeRunning {
            cwdProbeRunning = true
            let ownPid = pid
            let holderClient = holder
            Task.detached(priority: .utility) { [weak self] in
                let cwd = Self.probeAgentWorkingDir(pid: ownPid, holder: holderClient)
                await self?.storeCwdProbe(cwd)
            }
        }
        return cachedAgentCwd
    }

    private var cachedAgentCwd: String?
    private var cwdProbeRunning = false

    private func storeCwdProbe(_ cwd: String?) {
        cachedAgentCwd = cwd
        cwdProbeRunning = false
    }

    private static func probeAgentWorkingDir(pid: pid_t, holder: HolderClient?) -> String? {
        var candidates: [pid_t] = [pid]
        candidates.append(contentsOf: ProcessTree.childPids(pid))
        var foreground: pid_t = -1
        if let holder, let stat = try? holder.stat(), let fg = stat.foregroundPID, fg > 0 {
            foreground = fg
            candidates.append(fg)
            candidates.append(contentsOf: ProcessTree.childPids(fg))
        }
        for candidate in candidates where agentKind(forExecutableOf: candidate) != nil {
            if let cwd = ProcessTree.currentWorkingDir(candidate) { return cwd }
        }
        // No agent process (plain shell) or its cwd was unreadable: track the
        // foreground group leader so a manual `cd` into a worktree still counts.
        if foreground > 0, let cwd = ProcessTree.currentWorkingDir(foreground) { return cwd }
        return ProcessTree.currentWorkingDir(pid)
    }

    /// Matches a process to a first-class agent by its EXECUTABLE PATH — the
    /// only reliable signal: Claude retitles its process to its version string
    /// AND its real binary is `~/.local/share/claude/versions/<x.y.z>`, so both
    /// proc_name and the exec basename read "2.1.204". A path *component* named
    /// claude/codex (or that basename) identifies the agent.
    /// Names come from each manifest's `foregroundExecNames`. Matching a path
    /// COMPONENT (not just the basename) is what makes this work: Cursor CLI
    /// runs a bundled node under `~/.local/share/cursor-agent/versions/…`, so
    /// the component check is the one that fires.
    ///
    /// The technique has a known blind spot for CLIs that are plain node
    /// scripts — Gemini's exec path is the user's `node`, so an adopted gemini
    /// in a shell tab is only recognized when it lives in a dedicated install
    /// directory. Same caveat applies to every node-based agent in the catalog.
    static func agentKind(forExecutableOf pid: pid_t) -> AgentKind? {
        guard let path = ProcessTree.execPath(pid) else { return nil }
        let components = path.split(separator: "/")
        for descriptor in AgentCatalog.shared.ordered {
            for name in descriptor.foregroundExecNames
            where components.contains(Substring(name)) || components.last == Substring(name) {
                return AgentKind(id: descriptor.id)
            }
        }
        return nil
    }

    /// Set by the daemon: called when the child exits.
    private var onExit: (@Sendable (SessionID, ExitInfo) async -> Void)?

    /// Set by the daemon: called when hibernation begins (info) or ends (nil),
    /// so the registry can patch + publish the record — including the auto-wake
    /// paths (attach / input) that never go through the registry.
    private var onHibernationChange: (@Sendable (SessionID, HibernationInfo?) async -> Void)?

    public func setHibernationCallback(
        _ callback: @escaping @Sendable (SessionID, HibernationInfo?) async -> Void
    ) {
        onHibernationChange = callback
    }

    /// Set by the daemon: called when ownership flips between the Mac and a phone,
    /// so the registry can patch `record.remoteActive` + publish — mirrors the
    /// hibernation callback. Fires only when the value actually changes.
    private var onRemoteActiveChange: (@Sendable (SessionID, Bool) async -> Void)?

    public func setRemoteActiveCallback(
        _ callback: @escaping @Sendable (SessionID, Bool) async -> Void
    ) {
        onRemoteActiveChange = callback
    }

    /// True when a phone currently owns the session: a `.mobile` sink is attached
    /// AND the Mac hasn't reclaimed via `setOwner(.desktop)`.
    public var remoteActive: Bool {
        sinkRoles.values.contains(.mobile) && !desktopHold
    }

    /// Watermark above which a slow sink stops receiving live frames (it will
    /// see an offset gap and can re-request a replay; offsets make that safe).
    private static let sinkPendingWatermark = 8 << 20
    /// Maximum raw PTY tail parsed when adopting a holder after daemon restart.
    /// Kept internal so a regression test enforces the architecture contract.
    static let restartReplayBudget = 256 << 10
    private static let checkpointSettleDelay: Duration = .seconds(1)

    private let initialCols: Int
    private let initialRows: Int

    public init(
        id: SessionID,
        kind: AgentKind,
        logDirectory: URL,
        holderDirectory: URL? = nil,
        holderExecutablePath: String? = nil,
        cols: Int = 120,
        rows: Int = 32
    ) {
        self.id = id
        self.kind = kind
        self.initialCols = cols
        self.initialRows = rows
        self.pendingCols = cols
        self.pendingRows = rows
        self.queue = DispatchQueue(label: "dirijor.session.\(id.rawValue)")
        self.logDirectory = logDirectory
        self.holderPaths = HolderPaths(
            directory: holderDirectory ?? logDirectory.appendingPathComponent("holders"),
            sessionID: id.rawValue)
        self.holderExecutablePath =
            holderExecutablePath ?? HolderLauncher.defaultExecutablePath()
        self.log = OutputLog(
            directory: logDirectory, sessionID: id.rawValue, readOnly: true)
        self.screen = HeadlessScreen(cols: cols, rows: rows)
    }

    // MARK: Lifecycle

    // Deferred-launch state: the agent is not exec'd until the attaching client
    // reports its real terminal size, so a TUI's one-shot banner is rendered at
    // the exact width (no post-spawn reflow / wrapping). Falls back to the
    // initial size if no client attaches promptly (e.g. MCP-spawned agents).
    private var launchInfo: (argv: [String], cwd: String, env: [String: String])?
    private var launched = false
    private var pendingCols: Int
    private var pendingRows: Int

    public func start(
        argv: [String],
        cwd: String,
        extraEnv: [String: String],
        onExit: @escaping @Sendable (SessionID, ExitInfo) async -> Void
    ) throws {
        precondition(pid == -1, "session already started")
        self.onExit = onExit

        // Scrubbed, not raw: a daemon relaunched from inside an agent session
        // otherwise passes CLAUDE_CODE_CHILD_SESSION=1 to every child, which
        // makes spawned Claudes skip transcript persistence entirely.
        var env = InjectionBuilder.sanitizeInheritedEnvironment(
            ProcessInfo.processInfo.environment)
        InjectionBuilder.applyTerminalEnvironment(to: &env)
        for (key, value) in extraEnv { env[key] = value }

        launchInfo = (argv, cwd, env)

        // Fallback: if no client resize arrives at all (e.g. MCP-spawned agent
        // with no view), launch at the estimated size after a short window.
        Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(400))
            await self?.launchIfNeeded()
        }
    }

    private var launchDebounce: Task<Void, Never>?

    /// Debounced launch: while unlaunched, each client resize pushes the exec
    /// back ~120ms so the agent starts at the SETTLED viewport, not a transient
    /// first-layout size — otherwise its one-shot banner bakes at a narrow width.
    private func scheduleDebouncedLaunch() {
        launchDebounce?.cancel()
        launchDebounce = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(120))
            guard !Task.isCancelled else { return }
            await self?.launchIfNeeded()
        }
    }

    /// Exec the agent at `pendingCols×pendingRows`. Idempotent.
    private func launchIfNeeded() async {
        guard !launched, let info = launchInfo else { return }
        launched = true

        let cols = max(2, pendingCols)
        let rows = max(2, pendingRows)
        do {
            // Incarnation boundary fallback (see exitMarkerFloor): everything
            // already in the log predates the child we are about to spawn.
            _ = log.refreshFromDisk()
            let preSpawnTail = log.tailOffset
            let spec = HolderLaunchSpec(
                sessionID: id.rawValue,
                socketPath: holderPaths.socket.path,
                pidFilePath: holderPaths.pidFile.path,
                logFilePath: logDirectory.appendingPathComponent("\(id.rawValue).bin").path,
                argv: info.argv,
                cwd: info.cwd,
                environment: info.env,
                cols: UInt16(cols),
                rows: UInt16(rows))
            let holderPID = try HolderLauncher.launch(
                executablePath: holderExecutablePath,
                paths: holderPaths,
                spec: spec)
            let client = HolderClient(socketPath: holderPaths.socket.path)
            var stat: HolderStat?
            for _ in 0..<250 {
                if let current = try? client.stat(), current.alive {
                    stat = current
                    break
                }
                try? await Task.sleep(for: .milliseconds(20))
            }
            guard let stat else {
                throw HolderError.launch("holder did not become ready")
            }
            holder = client
            pid = stat.childPID
            exitMarkerFloor = stat.epochOffset ?? preSpawnTail
            screen.resize(cols: cols, rows: rows)
            replayExistingLog()
            startLogTailing()
            // The spawned holder refuses to double-run when a live holder for
            // this session already serves the socket (a lingering revive). The
            // pid file names whichever holder actually serves — watch that one;
            // watching our dead spawn would synthesize a bogus SIGHUP exit.
            let servingPID = holderPIDFromDisk() ?? holderPID
            watchHolderExit(holderPID: servingPID, reap: servingPID == holderPID)
            if servingPID != holderPID {
                // Our spawn deferred and exited; reap it so it doesn't zombie.
                Task.detached {
                    var status: Int32 = 0
                    while waitpid(holderPID, &status, 0) < 0, errno == EINTR {}
                }
            }
            if !queuedLaunchInput.isEmpty {
                try client.write(queuedLaunchInput)
                queuedLaunchInput.removeAll(keepingCapacity: false)
            }
            DaemonLog.shared.log(
                "session \(id) holder=\(servingPID) child=\(pid) \(cols)x\(rows) argv=\(info.argv.first ?? "?")")
        } catch {
            DaemonLog.shared.log("session \(id) launch failed: \(error)")
            let info = ExitInfo(reason: .exited, code: 127)
            exitInfo = info
            let onExit = self.onExit
            let id = self.id
            Task { await onExit?(id, info) }
        }
    }

    /// Reconstitutes a live session owned by a holder from a previous daemon.
    public func reattach(
        stat: HolderStat,
        hibernation: HibernationInfo?,
        onExit: @escaping @Sendable (SessionID, ExitInfo) async -> Void
    ) throws {
        guard stat.alive else { throw HolderError.launch("holder child is not alive") }
        self.onExit = onExit
        holder = HolderClient(socketPath: holderPaths.socket.path)
        launched = true
        pid = stat.childPID
        // Exit markers below the adopted holder's epoch were written by prior
        // incarnations of this session id — never by this child. Markers at or
        // above it (including one written while the daemon was down) apply.
        exitMarkerFloor = stat.epochOffset ?? 0
        if let cols = stat.cols, let rows = stat.rows {
            pendingCols = Int(cols)
            pendingRows = Int(rows)
            screen.resize(cols: Int(cols), rows: Int(rows))
        }
        hibernationInfo = hibernation
        lifeState = hibernation == nil ? .running : .hibernated
        replayExistingLog()
        startLogTailing()
        if let holderPID = holderPIDFromDisk() {
            watchHolderExit(holderPID: holderPID, reap: false)
        }
        DaemonLog.shared.log(
            "session \(id) reattached holder child=\(pid) at log offset \(logReadOffset)")
    }

    private func replayExistingLog() {
        _ = log.refreshFromDisk()
        if !restoreScreenCheckpoint(tailOffset: log.tailOffset) {
            logReadOffset = log.preferredReplayStart(budget: Self.restartReplayBudget)
            logMarkerBuffer.removeAll(keepingCapacity: false)
        }
        drainLog(replaying: true)
    }

    private var checkpointURL: URL {
        logDirectory.appendingPathComponent("\(id.rawValue).screen.plist")
    }

    /// Use only a checkpoint whose tail is still within the normal replay
    /// budget. This preserves the hard startup-work bound even if a checkpoint
    /// was left stale after a crash during a sustained output flood.
    private func restoreScreenCheckpoint(tailOffset: UInt64) -> Bool {
        guard let checkpoint = ScreenCheckpoint.load(from: checkpointURL),
            checkpoint.logOffset <= tailOffset,
            tailOffset - checkpoint.logOffset <= UInt64(Self.restartReplayBudget),
            let grid = checkpoint.grid,
            screen.restore(
                checkpoint: grid,
                altScreen: checkpoint.altScreen,
                bracketedPaste: checkpoint.bracketedPaste,
                mouseReporting: checkpoint.mouseReporting)
        else { return false }
        logReadOffset = checkpoint.logOffset
        logMarkerBuffer = checkpoint.markerBuffer
        return true
    }

    private func scheduleScreenCheckpoint() {
        if checkpointTask != nil {
            checkpointDirty = true
            return
        }
        checkpointDirty = false
        checkpointTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: Self.checkpointSettleDelay)
                guard !Task.isCancelled else { return }
                guard let settled = await self?.checkpointSettlePoll() else { return }
                if settled { return }
            }
        }
    }

    /// Returns true after a full quiet interval and writes the checkpoint. A
    /// dirty interval is acknowledged and reuses the same sleeping task.
    private func checkpointSettlePoll() -> Bool {
        if checkpointDirty {
            checkpointDirty = false
            return false
        }
        persistScreenCheckpoint()
        return true
    }

    /// Everything a checkpoint's content is a function of. Grid and cursor
    /// state derive from fed log bytes (tracked by `logOffset` and the screen's
    /// `contentSeq`), so equal keys mean a byte-identical checkpoint.
    private struct CheckpointKey: Equatable {
        var logOffset: UInt64
        var contentSeq: UInt64
        var markerBytes: Int
        var altScreen: Bool
        var bracketedPaste: Bool
        var mouseReporting: Bool
    }

    private var lastCheckpointKey: CheckpointKey?

    private func persistScreenCheckpoint() {
        checkpointTask = nil
        let key = CheckpointKey(
            logOffset: logReadOffset,
            contentSeq: screen.contentSeq,
            markerBytes: logMarkerBuffer.count,
            altScreen: screen.isAltScreen,
            bracketedPaste: screen.bracketedPasteActive,
            mouseReporting: screen.mouseReporting)
        // A settle poll after e.g. a cursor blink or a re-delivered quiet
        // interval would otherwise serialize and rewrite the whole grid for
        // identical content.
        if key == lastCheckpointKey { return }
        let checkpoint = ScreenCheckpoint(
            logOffset: logReadOffset,
            grid: screen.fullSnapshot(),
            markerBuffer: logMarkerBuffer,
            altScreen: screen.isAltScreen,
            bracketedPaste: screen.bracketedPasteActive,
            mouseReporting: screen.mouseReporting)
        do {
            try checkpoint.writeAtomically(to: checkpointURL)
            lastCheckpointKey = key
        } catch {
            DaemonLog.shared.log("screen checkpoint \(id) failed: \(error)")
        }
    }

    private func startLogTailing() {
        armLogSource()
        drainLog(replaying: false)
    }

    private func armLogSource() {
        logSource?.cancel()
        logSource = nil
        let path = logDirectory.appendingPathComponent("\(id.rawValue).bin").path
        let descriptor = open(path, O_EVTONLY)
        guard descriptor >= 0 else { return }
        let source = DispatchSource.makeFileSystemObjectSource(
            fileDescriptor: descriptor,
            eventMask: [.write, .extend, .rename, .delete],
            queue: queue)
        logSource = source
        source.setEventHandler { [weak self, weak source] in
            let mustRearm = source?.data.intersection([.rename, .delete]).isEmpty == false
            Task { [weak self] in
                await self?.logDidChange(rearm: mustRearm)
            }
        }
        source.setCancelHandler { close(descriptor) }
        source.activate()
    }

    private func logDidChange(rearm: Bool) {
        if rearm { log.invalidateReadHandle() }
        drainLog(replaying: false)
        if rearm { armLogSource() }
    }

    private func drainLog(replaying: Bool) {
        _ = log.refreshFromDisk()
        var fedOutput = false
        while logReadOffset < log.tailOffset {
            // Never read across the incarnation boundary in one chunk, so an
            // exit marker's attribution is decided by which side of the floor
            // the chunk lies on (markers are written atomically by a single
            // incarnation and cannot straddle it).
            var budget = UInt64(64 << 10)
            if logReadOffset < exitMarkerFloor {
                budget = min(budget, exitMarkerFloor - logReadOffset)
            }
            let (offset, data) = log.read(fromOffset: logReadOffset, maxBytes: Int(budget))
            guard !data.isEmpty else {
                logReadOffset = max(logReadOffset, offset)
                break
            }
            logReadOffset = offset + UInt64(data.count)
            logMarkerBuffer.append(data)
            let drained = HolderExitMarker.drain(&logMarkerBuffer)
            if !drained.output.isEmpty {
                fedOutput = true
                // No raw-byte `.output` frame here: the GPUI client renders
                // from grid frames and discards byte replay, so building and
                // sending a copy of every chunk per sink was pure overhead
                // that also pushed real grid frames toward the backpressure
                // watermark.
                let responses = screen.feed(drained.output)
                if !replaying, !responses.isEmpty { writeRaw(responses) }
                scheduleGridFlush()
            }
            if let status = drained.exit {
                // A chunk that ends at or below the floor is entirely prior-
                // incarnation bytes: its exit marker describes a PREVIOUS
                // child of this session id (e.g. migrate's kill), not ours.
                // Replay it for screen content only.
                guard logReadOffset > exitMarkerFloor else { continue }
                let info =
                    status.reason == .signaled
                    ? ExitInfo(reason: .signaled, signal: status.signal)
                    : ExitInfo(reason: .exited, code: status.code)
                Task { [weak self] in await self?.handleExit(info) }
            }
        }
        if fedOutput {
            if replaying {
                persistScreenCheckpoint()
            } else {
                scheduleScreenCheckpoint()
            }
        }
    }

    private var gridFlushScheduled = false
    private var lastGridFlushAt: ContinuousClock.Instant?
    /// The coalescing ceiling during bursts. Sustained output paints at this
    /// cadence; a lone frame after quiet is never delayed by it. ~60fps: the
    /// conversion cost only exists while output flows AND a sink is attached,
    /// so background sessions and idle budgets are untouched. Matched by the
    /// client's `ACTIVE_REPAINT_INTERVAL` and `RESIZE_CADENCE`.
    private static let gridFlushInterval: Duration = .milliseconds(16)
    /// Cursor state (col, row, visible) carried by the last broadcast grid
    /// frame, so cursor-only movement still produces a frame (see flushGrid).
    private var lastSentCursor: (col: Int, row: Int, visible: Bool)?

    /// Leading-edge coalescing, like the client's RepaintPacer: the first
    /// output after a quiet spell flushes IMMEDIATELY — that frame is the echo
    /// of a keystroke at an idle prompt, the latency-critical case, and a
    /// trailing-only timer used to add a flat 50ms to it. Only when another
    /// flush happened within the interval does the timer coalesce, capping
    /// burst work. Idle cost is unchanged: no output, no timer.
    private func scheduleGridFlush() {
        guard !gridFlushScheduled else { return }
        let now = ContinuousClock.now
        if let last = lastGridFlushAt, now - last < Self.gridFlushInterval {
            gridFlushScheduled = true
            let remaining = Self.gridFlushInterval - (now - last)
            Task { [weak self] in
                try? await Task.sleep(for: remaining)
                await self?.flushGrid()
            }
            return
        }
        flushGrid()
    }

    private func flushGrid() {
        gridFlushScheduled = false
        lastGridFlushAt = ContinuousClock.now
        scanArtifactsIfDue()
        // Feeding the emulator above keeps snapshots and later attachments
        // authoritative. With no consumers, however, converting every row into
        // protocol cells is pure work: attach() always sends a fresh full grid.
        guard !sinks.isEmpty else {
            gridDesyncedSinks.removeAll(keepingCapacity: true)
            return
        }
        let update = screen.gridUpdate(full: false)
        // The cursor rides inside GridUpdate, so an "empty" diff must still go
        // out when only the cursor changed: a space typed over a blank cell, or
        // arrow keys at a prompt, change no row content — dropping those frames
        // froze the client caret until the next glyph changed.
        let cursor = (col: update.cursorCol, row: update.cursorRow, visible: update.cursorVisible)
        if !update.changedRows.isEmpty || lastSentCursor == nil || lastSentCursor! != cursor {
            broadcastGrid(update)
            lastSentCursor = cursor
        }
        // Re-seed any sink that fell behind, even if the screen is now idle (the
        // reason a burst-then-stop leaves stale rows on screen). Keep polling
        // while any sink is still backpressured so the repair isn't starved.
        resyncGridSinks()
        broadcastModesIfChanged()
        // A still-backpressured sink is repaired on its own slower timer: at
        // flush cadence the retry would recompute a full snapshot per sink,
        // 60 times a second, precisely while the client is already too slow.
        if !gridDesyncedSinks.isEmpty { scheduleDesyncRetry() }
    }

    private var desyncRetryScheduled = false
    private static let desyncRetryInterval: Duration = .milliseconds(250)

    private func scheduleDesyncRetry() {
        guard !desyncRetryScheduled else { return }
        desyncRetryScheduled = true
        Task { [weak self] in
            try? await Task.sleep(for: Self.desyncRetryInterval)
            await self?.retryDesyncedSinks()
        }
    }

    private func retryDesyncedSinks() {
        desyncRetryScheduled = false
        guard !sinks.isEmpty, !gridDesyncedSinks.isEmpty else { return }
        resyncGridSinks()
        if !gridDesyncedSinks.isEmpty { scheduleDesyncRetry() }
    }

    /// Emits a `.modes` frame to every sink when alt-screen / mouse-reporting
    /// changed since the last broadcast. Rides the coalesced grid flush so mode
    /// flips (e.g. a TUI entering 1049h) reach clients promptly.
    private func broadcastModesIfChanged() {
        let modes = (altScreen: screen.isAltScreen, mouseReporting: screen.mouseReporting)
        if let last = lastSentModes, last == modes { return }
        lastSentModes = modes
        let frame = Frame.modes(altScreen: modes.altScreen, mouseReporting: modes.mouseReporting)
        for (_, sink) in sinks { sink.deliver(frame) }
    }

    /// Sends a fresh full snapshot to each desynced sink that has drained below
    /// the watermark, then clears its desync flag.
    private func resyncGridSinks() {
        guard !gridDesyncedSinks.isEmpty else { return }
        var repaired: [UUID] = []
        for id in gridDesyncedSinks {
            guard let sink = sinks[id] else { repaired.append(id); continue }
            if sink.pendingBytes < Self.sinkPendingWatermark {
                sink.deliver(.grid(screen.fullSnapshot()))
                repaired.append(id)
            }
        }
        for id in repaired { gridDesyncedSinks.remove(id) }
    }

    // MARK: Artifacts

    /// URLs captured from the screen so far (PRs, Linear issues, previews).
    /// The ResourceGovernor pulls this on its scan and patches the record.
    public private(set) var artifacts: [SessionArtifact] = []
    private var lastArtifactScanAt: Date?
    private var lastArtifactScanSeq: UInt64?

    /// Rescan the visible screen for artifact URLs, at most every ~2s. Rides
    /// the already-coalesced grid flush so it only runs while output flows.
    /// Detached sessions still scan — background agents' PR links must land —
    /// but an unchanged screen, and a screen that cannot contain an artifact,
    /// skip the extraction and regex passes entirely.
    private func scanArtifactsIfDue() {
        let now = Date()
        if let last = lastArtifactScanAt, now.timeIntervalSince(last) < 2.0 { return }
        lastArtifactScanAt = now
        let seq = screen.contentSeq
        if lastArtifactScanSeq == seq { return }
        lastArtifactScanSeq = seq
        let text = screen.captureVisibleLines().joined(separator: "\n")
        guard !text.isEmpty else { return }
        // Every scanner rule requires one of these substrings; most screens
        // have none of them.
        guard
            text.contains("http") || text.contains("github.com")
                || text.contains("linear.app")
        else { return }
        artifacts = ArtifactScanner.scan(text, existing: artifacts, now: now)
    }

    private func broadcastGrid(_ update: GridUpdate) {
        let frame = Frame.grid(update)
        for (id, sink) in sinks {
            if sink.pendingBytes >= Self.sinkPendingWatermark {
                // Dropping this diff would desync the sink's grid forever (later
                // diffs assume it landed). Mark it for a full re-seed on drain.
                gridDesyncedSinks.insert(id)
                continue
            }
            // A drained-but-still-flagged sink is handled by resyncGridSinks with
            // a full snapshot; don't also feed it this partial diff.
            if gridDesyncedSinks.contains(id) { continue }
            sink.deliver(frame)
        }
    }

    private func watchHolderExit(holderPID: pid_t, reap: Bool) {
        let source = DispatchSource.makeProcessSource(
            identifier: holderPID, eventMask: .exit, queue: queue)
        exitSource = source
        source.setEventHandler { [weak self] in
            if reap {
                var status: Int32 = 0
                _ = waitpid(holderPID, &status, WNOHANG)
            }
            Task { [weak self] in
                // The holder writes the exit marker before it closes. Give the
                // filesystem notification a moment, then synthesize only if the
                // marker could not be consumed (holder crash).
                try? await Task.sleep(for: .milliseconds(50))
                await self?.holderDidExit()
            }
        }
        source.activate()
    }

    private func holderDidExit() async {
        drainLog(replaying: false)
        guard exitInfo == nil else { return }
        await handleExit(ExitInfo(reason: .signaled, signal: SIGHUP))
    }

    private func holderPIDFromDisk() -> pid_t? {
        guard let text = try? String(contentsOf: holderPaths.pidFile, encoding: .utf8),
            let value = Int32(text.trimmingCharacters(in: .whitespacesAndNewlines)),
            value > 1
        else { return nil }
        return value
    }

    private func handleExit(_ info: ExitInfo) async {
        guard exitInfo == nil else { return }
        exitInfo = info
        logSource?.cancel()
        logSource = nil
        holder = nil
        exitSource?.cancel()
        exitSource = nil
        DaemonLog.shared.log("session \(id) exited: \(info)")
        await onExit?(id, info)
    }

    // MARK: Hibernation

    /// Freezes the whole descendant tree with SIGSTOP (Chrome-tab style): zero
    /// CPU, RAM pages become cold and compressible. Returns the recorded info
    /// (also delivered via the hibernation callback), or nil if not applicable.
    @discardableResult
    public func hibernate(reason: HibernationReason) -> HibernationInfo? {
        guard exitInfo == nil, launched, pid > 0, let holder else { return nil }
        guard lifeState == .running else { return hibernationInfo }
        lifeState = .hibernating
        guard let samples = try? holder.signal(SIGSTOP) else {
            lifeState = .running
            return nil
        }
        let info = HibernationInfo(
            since: Date(),
            reason: reason,
            treePids: samples.map(\.pid),
            treeStartTimes: Dictionary(
                uniqueKeysWithValues: samples.map { ($0.pid, $0.startSec) })
        )
        hibernationInfo = info
        lifeState = .hibernated
        DaemonLog.shared.log(
            "session \(id) hibernated (\(reason.rawValue)) tree=\(info.treePids)")
        notifyHibernationChange(info)
        return info
    }

    /// SIGCONTs the frozen tree bottom-up, flushes input queued while stopped,
    /// applies a deferred resize (or resize-jiggles so the TUI repaints).
    public func wake() {
        guard lifeState == .hibernated, hibernationInfo != nil, let holder else { return }
        lifeState = .waking
        guard (try? holder.signal(SIGCONT)) != nil else {
            lifeState = .hibernated
            return
        }
        hibernationInfo = nil
        lifeState = .running

        if !queuedInput.isEmpty {
            let data = queuedInput
            queuedInput = Data()
            writeRaw(data)
        }
        if let size = queuedResize {
            queuedResize = nil
            // Already arbitrated when queued; apply directly and force a repaint.
            applyResize(cols: size.cols, rows: size.rows, force: true)
        } else {
            // No client geometry arrived while frozen: jiggle the PTY width so
            // the TUI receives SIGWINCH and repaints its screen after CONT.
            scheduleWakeJiggle()
        }
        DaemonLog.shared.log("session \(id) woke")
        notifyHibernationChange(nil)
    }

    private func notifyHibernationChange(_ info: HibernationInfo?) {
        guard let onHibernationChange else { return }
        let id = self.id
        Task { await onHibernationChange(id, info) }
    }

    /// A holder and its child outlive daemon restarts, so the process can be
    /// stopped even when an old or partially-written record says it is awake.
    /// A fresh attach is a cold boundary where one harmless SIGCONT tree walk
    /// repairs that mismatch without adding work to every keystroke.
    private func ensureAwakeForAttach() {
        if lifeState == .hibernated {
            wake()
            return
        }
        guard lifeState == .running, exitInfo == nil, launched, let holder else { return }
        _ = try? holder.signal(SIGCONT)
    }

    /// ~120ms after wake, shrink the PTY one column and restore it: two
    /// SIGWINCHes that force a full TUI repaint (tmux-style reattach nudge).
    /// The emulator keeps its real geometry; only the kernel winsize wiggles.
    private func scheduleWakeJiggle() {
        Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(120))
            guard let self else { return }
            guard let dims = await self.jiggleNarrow() else { return }
            try? await Task.sleep(for: .milliseconds(40))
            await self.jiggleRestore(dims)
        }
    }

    private func jiggleNarrow() -> (cols: Int, rows: Int)? {
        guard lifeState == .running, exitInfo == nil, let holder, queuedResize == nil
        else { return nil }
        let snapshot = screen.snapshot()
        guard snapshot.cols > 2 else { return nil }
        try? holder.resize(cols: UInt16(snapshot.cols - 1), rows: UInt16(snapshot.rows))
        return (snapshot.cols, snapshot.rows)
    }

    private func jiggleRestore(_ dims: (cols: Int, rows: Int)) {
        guard exitInfo == nil, let holder else { return }
        try? holder.resize(cols: UInt16(dims.cols), rows: UInt16(dims.rows))
    }

    // MARK: I/O

    public func write(_ data: Data) {
        if deliverViaWake(data) { return }
        writeRaw(data)
    }

    public func sendText(_ text: String, submit: Bool) {
        // Interactive, non-submitting input (numbered-picker answers, a partial
        // keystroke) goes through raw: pickers and permission dialogs read the
        // literal keypress, and framing a "2" as a paste would defeat that.
        guard submit else {
            let data = Data(text.utf8)
            if deliverViaWake(data) { return }
            writeRaw(data)
            return
        }

        // A submitted prompt. When the child has bracketed-paste mode on, wrap the
        // body so a multi-line prompt's embedded newlines are inserted verbatim
        // instead of each one submitting the composer early, and so TUIs that treat
        // pastes specially don't run every character through slash/autocomplete
        // menus. The submitting Enter is a SEPARATE write issued after the paste is
        // fully flushed — never riding the same buffer (the old code appended CR to
        // the body, so a truncated paste also lost or misfired the Enter).
        let framed = screen.bracketedPasteActive
            ? "\u{1b}[200~" + text + "\u{1b}[201~"
            : text
        let body = Data(framed.utf8)
        let cr = Data([0x0D])

        // Hibernated: queue body+Enter together; wake() replays them in order right
        // after SIGCONT (the deferred-write path can't stage two timed writes).
        if lifeState == .hibernated {
            _ = deliverViaWake(body + cr)
            return
        }

        writeRaw(body)
        // Let the TUI parse the paste-end sequence and settle the composer before
        // Enter lands; a same-instant CR can be swallowed by paste-mode handling.
        Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(30))
            await self?.writeRaw(cr)
        }
    }

    /// Types an MCP-provided initial prompt into a freshly spawned agent, gated on
    /// the TUI actually being ready and verified afterward. Replaces a blind fixed
    /// delay that raced Claude Code's (slower-than-Codex) boot: keystrokes typed
    /// into a not-yet-built composer were swallowed and the session sat empty.
    public func injectInitialPrompt(_ prompt: String) async {
        guard !prompt.isEmpty else { return }
        await waitUntilReady()
        let probe = Self.verificationProbe(for: prompt)
        for attempt in 0..<3 {
            if exitInfo != nil { return }
            let before = screen.text()
            sendText(prompt, submit: true)
            if await promptSettled(probe: probe, before: before) { return }
            DaemonLog.shared.log(
                "session \(id) initial prompt not visible after attempt \(attempt + 1); retrying")
        }
    }

    /// Waits until the agent can actually receive typed input. First for the exec
    /// (deferred launch fires ≤400ms after start), then for the input line to come
    /// alive — bracketed-paste mode is the tell across Claude/Codex/Cursor/Gemini.
    /// Falls back to "screen non-blank and settled" so a bare shell (or any agent
    /// that never enables paste mode) is still handled, and hard-caps the wait.
    private func waitUntilReady() async {
        for _ in 0..<40 {  // ≤ ~4s for the PTY to be spawned
            if exitInfo != nil { return }
            if holder != nil && pid > 0 { break }
            try? await Task.sleep(for: .milliseconds(100))
        }
        var lastText = ""
        var stableTicks = 0
        for tick in 0..<200 {  // ≤ ~20s hard cap; Claude's first paint can be slow
            if exitInfo != nil { return }
            if screen.bracketedPasteActive {
                try? await Task.sleep(for: .milliseconds(80))  // one more frame to paint
                return
            }
            let text = screen.text()
            if !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, text == lastText {
                stableTicks += 1
                if stableTicks >= 6, tick >= 10 { return }  // ~600ms stable, ≥1s in
            } else {
                stableTicks = 0
                lastText = text
            }
            try? await Task.sleep(for: .milliseconds(100))
        }
    }

    /// Polls the screen (≤ ~2s) for evidence the prompt was received. True as soon
    /// as the probe is visible OR the screen diverged from `before` — either means
    /// input landed, and we must NOT retry (a second submit would duplicate the
    /// prompt). Only an entirely unchanged screen returns false → safe to retype.
    private func promptSettled(probe: String, before: String) async -> Bool {
        for _ in 0..<20 {
            try? await Task.sleep(for: .milliseconds(100))
            if exitInfo != nil { return true }  // dead pty: don't retype into it
            let now = screen.text()
            if !probe.isEmpty, now.contains(probe) { return true }
            if now != before { return true }
        }
        return false
    }

    /// A distinctive slice of the prompt to look for on screen: the first non-empty
    /// line, trimmed and capped short enough to survive composer wrapping or the
    /// transcript truncating a long prompt when it echoes back.
    private static func verificationProbe(for prompt: String) -> String {
        let firstLine = prompt.split(whereSeparator: \.isNewline).first.map(String.init) ?? prompt
        return String(firstLine.trimmingCharacters(in: .whitespaces).prefix(24))
    }

    /// A mouse-wheel scroll: the emulator encodes the wheel event using the
    /// child's active mouse mode/protocol and we forward the bytes. When the app
    /// isn't in mouse mode (a bare shell) the emulator returns nothing, so a
    /// shell never sees stray escape codes. Skipped while hibernated (the frozen
    /// tree can't drain the PTY).
    public func scroll(dir: UInt8, lines: Int, col: Int, row: Int) {
        guard lifeState != .hibernated else { return }
        let bytes = screen.mouseWheel(up: dir == 0, lines: lines, col: col, row: row)
        if !bytes.isEmpty { writeRaw(bytes) }
    }

    /// Never write to the holder's PTY while its tree is stopped (nobody drains the slave;
    /// the queue fills and the daemon's writes wedge). Queue + wake instead —
    /// wake() flushes the queue right after SIGCONT.
    private func deliverViaWake(_ data: Data) -> Bool {
        guard lifeState == .hibernated else { return false }
        queuedInput.append(data)
        wake()
        return true
    }

    private func writeRaw(_ data: Data) {
        guard !data.isEmpty else { return }
        guard let holder else {
            if exitInfo == nil { queuedLaunchInput.append(data) }
            return
        }
        // The holder owns the EAGAIN-safe loop because it owns the master fd.
        // Each holder request is a blocking connect + round-trip; running it
        // on the serial write queue keeps keystrokes ordered while the actor
        // stays free to drain output.
        Self.holderWriteQueue.async {
            try? holder.write(data)
        }
    }

    /// One serial queue for all sessions' PTY writes: ordering per holder is
    /// preserved by serial dispatch, and the writes themselves are tiny
    /// round-trips that never accumulate.
    private static let holderWriteQueue = DispatchQueue(
        label: "diri.holder.write", qos: .userInteractive)

    /// A client asks to resize the shared PTY. Geometry follows ownership: when a
    /// phone owns the session (`remoteActive`), the mobile size drives the PTY and
    /// a `.desktop` request is recorded but NOT applied (the Mac shows a hand-off
    /// card instead of the phone-sized grid). Otherwise the desktop size drives.
    /// Defaults to `.desktop` so legacy callers/tests keep working.
    public func resize(cols: Int, rows: Int, role: ClientRole = .desktop) {
        // Record the last-requested size for this role (used for arbitration and
        // for handing control to the phone when the last desktop detaches).
        if role == .desktop {
            desktopSize = (cols, rows)
        } else {
            mobileSize = (cols, rows)
        }

        // Before the agent is exec'd, the FIRST client size decides the launch
        // geometry — regardless of role — then start the agent so its banner
        // renders at that width.
        if !launched {
            pendingCols = cols
            pendingRows = rows
            scheduleDebouncedLaunch()
            return
        }

        // Effective geometry: a desktop client, when attached, is authoritative;
        // otherwise the mobile size drives. Falls back to the just-requested size.
        let effective = effectiveSize(requested: (cols, rows))

        // While the tree is stopped, defer geometry to wake: the pending SIGWINCH
        // delivers after SIGCONT and the TUI repaints at the effective size.
        if lifeState == .hibernated {
            queuedResize = effective
            return
        }
        applyResize(cols: effective.cols, rows: effective.rows, force: false)
    }

    /// The size the PTY should currently be: the phone's when it owns the session
    /// (`remoteActive`), else the Mac's, falling back to the requested size.
    private func effectiveSize(requested: (cols: Int, rows: Int)) -> (cols: Int, rows: Int) {
        if remoteActive {
            return mobileSize ?? requested
        }
        return desktopSize ?? requested
    }

    /// Actually resize the PTY + emulator to `cols×rows` and repaint. `force`
    /// bypasses the no-op guard (used on wake so the TUI always repaints). Skips a
    /// redundant resize to the current geometry to avoid SIGWINCH churn.
    private func applyResize(cols: Int, rows: Int, force: Bool) {
        guard let holder, lifeState != .hibernated else { return }
        // No-op guard: pendingCols/pendingRows track the live geometry once
        // launched, so an effective size equal to it means nothing changed.
        if !force, cols == pendingCols, rows == pendingRows { return }
        pendingCols = cols
        pendingRows = rows

        // A live window/sidebar drag paces resizes at ~20Hz, and the heavy tail
        // of a resize (scrollback re-budget, blank-capture, an extra forced full
        // snapshot on top of the one the program's repaint will produce) is not
        // worth paying twenty times a second. An interactive step does only what
        // makes the program repaint at the new size; `settleResize` runs the
        // full path once the geometry stops moving.
        let interactive = !force && isMidResizeDrag()
        lastResizeAt = ContinuousClock.now

        // herdr-style resize recovery: capture the current screen so that if the
        // resize + the program's repaint leaves it blank (some TUIs don't fully
        // repaint on SIGWINCH), we can restore the content re-wrapped at the new
        // width. CLAUDE_CODE_NO_FLICKER + Codex's native reflow make this a rare
        // fallback, but it guarantees content is never lost on resize.
        //
        // Mid-drag this capture is skipped: it allocates a String per row, and
        // the content it would grab is a half-repainted intermediate. The one
        // taken on the drag's first step is held in `resizeRecovery` and reused
        // by the settle pass, so the net covers the whole drag from a screen
        // that was known good before any of it started.
        let recovery = interactive || screen.isBlank() ? nil : screen.captureVisibleLines()
        if !interactive { resizeRecovery = recovery }

        // Resize both the PTY (→ SIGWINCH, the program repaints) and our
        // authoritative emulator, then paint a fresh full snapshot at the new
        // geometry so the client is immediately correct; the program's repaint
        // then streams in as diffs. The holder round-trip rides the serial
        // write queue: a live drag steps at frame cadence, and blocking the
        // actor per step stalled output draining.
        Self.holderWriteQueue.async {
            try? holder.resize(cols: UInt16(cols), rows: UInt16(rows))
        }
        screen.resize(cols: cols, rows: rows, historyBudget: interactive ? .trimOnly : .full)
        if interactive {
            // `screen.resize` already reset the diff baseline, so the next
            // coalesced flush carries a full snapshot at the new geometry —
            // one screen→cells conversion per step instead of two.
            scheduleGridFlush()
            scheduleResizeSettle()
        } else {
            broadcastGrid(screen.gridUpdate(full: true))
        }

        if let recovery, !recovery.isEmpty {
            Task { [weak self] in
                try? await Task.sleep(for: .milliseconds(220))
                await self?.recoverIfBlank(recovery)
            }
        }
    }

    /// True when the previous resize is recent enough that this one is part of
    /// the same drag. Inferred here rather than flagged by the client so any
    /// client (Mac, phone, a test) gets the same pacing without a protocol bit.
    private func isMidResizeDrag() -> Bool {
        guard let lastResizeAt else { return false }
        return ContinuousClock.now - lastResizeAt < Self.resizeDragWindow
    }

    /// Arms the settle pass that pays a drag's deferred costs exactly once.
    /// Restarted by every resize, so it fires only after the size holds still.
    private func scheduleResizeSettle() {
        resizeSettle?.cancel()
        resizeSettle = Task { [weak self] in
            try? await Task.sleep(for: Self.resizeSettleDelay)
            guard !Task.isCancelled else { return }
            await self?.settleResize()
        }
    }

    /// The tail an interactive resize skipped: re-budget the scrollback for the
    /// final width, re-seed clients with a clean full snapshot, and re-arm the
    /// blank-screen net using the capture taken before the drag began. Geometry
    /// is already applied, so the PTY and emulator are not touched again.
    private func settleResize() {
        resizeSettle = nil
        let recovery = resizeRecovery
        resizeRecovery = nil
        guard holder != nil, exitInfo == nil, lifeState != .hibernated else { return }
        screen.rebudgetHistory()
        broadcastGrid(screen.gridUpdate(full: true))
        if let recovery, !recovery.isEmpty {
            Task { [weak self] in
                try? await Task.sleep(for: .milliseconds(220))
                await self?.recoverIfBlank(recovery)
            }
        }
    }

    /// If the screen is still blank a moment after a resize (the program didn't
    /// repaint), re-display the captured lines so content is never lost.
    private func recoverIfBlank(_ lines: [String]) {
        guard holder != nil, exitInfo == nil, screen.isBlank() else { return }
        screen.replay(lines: lines)
        broadcastGrid(screen.gridUpdate(full: true))
    }

    public func terminate(graceSeconds: Double = 3.0) {
        _ = graceSeconds
        guard exitInfo == nil else { return }
        guard let holder, pid > 0 else {
            // Explicit kill/archive can race the deferred launch window. Cancel
            // the launch and report a normal signaled exit without ever creating
            // a holder.
            launchDebounce?.cancel()
            launchInfo = nil
            launched = true
            let info = ExitInfo(reason: .signaled, signal: SIGTERM)
            exitInfo = info
            let callback = onExit
            let id = self.id
            Task { await callback?(id, info) }
            return
        }
        if hibernationInfo != nil {
            _ = try? holder.signal(SIGCONT)
            hibernationInfo = nil
            lifeState = .running
            notifyHibernationChange(nil)
        }
        try? holder.killTree()
    }

    // MARK: Attach / detach

    /// Attaches a sink and paints its initial screen.
    ///
    /// For a **running** session we do NOT replay the historical byte log: that
    /// log was produced across whatever widths the session has been through, and
    /// replaying it at the client's current width scatters absolute-positioned
    /// TUI content (mangled reattach). Instead we clear the client and force the
    /// live program to repaint its current screen at the current width via a
    /// resize jiggle — exactly how tmux/screen present a reattached session.
    ///
    /// For an **exited** session there's no program to repaint, so we replay the
    /// log tail (best-effort) so the final screen is viewable.
    public func attach(_ sink: any SessionOutputSink, fromOffset: UInt64?) {
        attach(sink, fromOffset: fromOffset, role: .desktop)
    }

    public func attach(_ sink: any SessionOutputSink, fromOffset: UInt64?, role: ClientRole) {
        // The daemon's emulator already holds the current screen — just paint it
        // as a full grid snapshot. No byte replay, no jiggle: this is why the
        // mosh model has no reattach-mangle. The client's follow-up resize (to
        // its real viewport) re-snapshots at the correct size.
        sinks[sink.sinkID] = sink
        sinkRoles[sink.sinkID] = role
        // A fresh phone open TAKES CONTROL: drop any prior Mac reclaim so the
        // session becomes phone-owned again.
        if role == .mobile { desktopHold = false }
        sink.deliver(.grid(screen.gridUpdate(full: true)))
        // Send the current modes to the newly attached sink (broadcasts only fire
        // on change, so a fresh sink would otherwise not learn the initial state).
        sink.deliver(.modes(altScreen: screen.isAltScreen, mouseReporting: screen.mouseReporting))
        // Selecting reconciles the live process tree even if its persisted
        // hibernation marker was lost; the program resumes underneath the
        // snapshot painted above.
        ensureAwakeForAttach()
        // Recompute geometry for the new ownership and publish any remoteActive flip.
        applyOwnership()
    }

    public func detach(sinkID: UUID) {
        let removedRole = sinkRoles.removeValue(forKey: sinkID)
        sinks.removeValue(forKey: sinkID)
        gridDesyncedSinks.remove(sinkID)
        if sinks.isEmpty { lastDetachAt = Date() }
        // The phone leaving auto-hands control back to the Mac: clear any reclaim
        // hold so remoteActive → false and geometry returns to the desktop size.
        if removedRole == .mobile { desktopHold = false }
        // A desktop that fully left can no longer "hold" the session — let a lone
        // phone reclaim control.
        if !sinkRoles.values.contains(.desktop) { desktopHold = false }
        applyOwnership()
    }

    /// Explicit ownership change from `session.set_owner`. `.desktop` → the Mac
    /// reclaims (phone shows "Active on your Mac"); `.mobile` → the phone (re)claims
    /// (Mac shows "Active on iPhone"). Recomputes geometry + fires the callback.
    public func setOwner(role: ClientRole) {
        switch role {
        case .desktop: desktopHold = true
        case .mobile: desktopHold = false
        }
        applyOwnership()
    }

    /// Resizes the PTY to the size the current ownership dictates and publishes a
    /// remoteActive change when it flipped. Safe to call before launch (no-op
    /// resize) and while hibernated (defers the resize to wake).
    private func applyOwnership() {
        if launched {
            let effective = effectiveSize(requested: (pendingCols, pendingRows))
            if lifeState == .hibernated {
                queuedResize = effective
            } else {
                applyResize(cols: effective.cols, rows: effective.rows, force: false)
            }
        }
        let active = remoteActive
        if active != lastRemoteActive {
            lastRemoteActive = active
            notifyRemoteActiveChange(active)
        }
    }

    private func notifyRemoteActiveChange(_ active: Bool) {
        guard let onRemoteActiveChange else { return }
        let id = self.id
        Task { await onRemoteActiveChange(id, active) }
    }

    // MARK: State for detection / previews

    public func makeSnapshot() -> ScreenSnapshot {
        screen.snapshot()
    }

    public func readScrollback() -> ReadScrollbackResult {
        screen.scrollback()
    }

    public func readScrollbackCells(firstRow: Int, maxRows: Int) -> ReadScrollbackCellsResult {
        screen.scrollbackCells(firstRow: firstRow, maxRows: maxRows)
    }

    public func screenText() -> String {
        screen.text()
    }

    public var contentSeq: UInt64 { screen.contentSeq }
    #if DEBUG
    var gridExtractionCountForTesting: Int { screen.gridExtractionCount }
    #endif
    public var isRunning: Bool { exitInfo == nil && pid > 0 }
    public var logTailOffset: UInt64 {
        _ = log.refreshFromDisk()
        return log.tailOffset
    }

    public func shutdownCleanup() {
        // Daemon shutdown detaches only the reader. The holder remains the sole
        // PTY/log owner and intentionally survives this actor disappearing.
        checkpointTask?.cancel()
        checkpointTask = nil
        checkpointDirty = false
        persistScreenCheckpoint()
        logSource?.cancel()
        logSource = nil
        exitSource?.cancel()
        exitSource = nil
    }
}
