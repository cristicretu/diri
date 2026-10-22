import Foundation
import Observation

/// One session's live terminal: the stream that feeds it, the scrollback
/// fetcher behind it, and the ordered byte queue in front of it.
///
/// The buffer is deliberately not observed. SwiftUI sees only the few facts
/// its chrome shows (connected, exited); the grid goes straight to the
/// terminal view through `onChange`, a frame at a time.
@MainActor
@Observable
final class TerminalSession {
    enum Link: Equatable {
        case connecting
        case live
        /// An older gateway without the terminal stream: the screen is polled
        /// as plain text.
        case legacy
        case reconnecting(String)
    }

    let sessionID: String
    private(set) var link: Link = .connecting
    private(set) var exited = false
    /// A keystroke batch did not reach the gateway. Cleared by the chrome.
    var inputFailed = false
    /// Mirrors of buffer facts the chrome reads.
    private(set) var bracketedPaste = false
    private(set) var appCursor = false
    private(set) var cols = 0
    private(set) var rows = 0

    @ObservationIgnored private(set) var buffer = TerminalBuffer()
    /// The view's repaint hook. Called on the main actor, coalesced per change.
    @ObservationIgnored var onChange: ((TerminalBuffer.Change) -> Void)?

    @ObservationIgnored private let client: DiriClient
    @ObservationIgnored private var streamTask: Task<Void, Never>?
    @ObservationIgnored private var fetches = 0
    @ObservationIgnored private var wanted: [Int] = []
    @ObservationIgnored private var outbox: [UInt8] = []
    @ObservationIgnored private var sending = false
    /// Learned from the first answered batch; until then nothing is resent.
    @ObservationIgnored private var gatewayDedupesInput = false
    @ObservationIgnored private var warmed = false
    @ObservationIgnored private var lastFocus = 0

    /// Requests in flight for history at once. Enough to hide a 150 ms round
    /// trip behind a fast fling without queueing behind the stream.
    private let maxFetches = 4
    /// Scrollback fetched eagerly on open, newest first.
    private let warmLines = 10_000

    init(sessionID: String, client: DiriClient) {
        self.sessionID = sessionID
        self.client = client
    }

    // MARK: - Lifecycle

    func start() {
        guard streamTask == nil else { return }
        streamTask = Task { [weak self] in await self?.run() }
    }

    func stop() {
        streamTask?.cancel()
        streamTask = nil
    }

    private func run() async {
        var failures = 0
        while !Task.isCancelled {
            let (events, outcome) = await client.terminalStream(sessionID: sessionID)
            for await event in events {
                if Task.isCancelled { outcome.cancel(); return }
                failures = 0
                handle(event)
            }
            switch await outcome.value {
            case .unsupported:
                await pollLegacy()
                return
            case .unauthorized:
                link = .reconnecting("Pairing was not accepted")
            case .ended:
                if !exited { link = .reconnecting("Reconnecting…") }
            }
            if Task.isCancelled || exited { return }
            failures += 1
            try? await Task.sleep(for: .milliseconds(min(4000, 250 * (1 << min(failures, 4)))))
        }
    }

    private func handle(_ event: DiriClient.TerminalEvent) {
        switch event {
        case let .snapshot(snapshot):
            let change = buffer.apply(snapshot)
            link = .live
            publish(change)
            warm()
        case let .diff(diff):
            switch buffer.apply(diff) {
            case let .applied(change):
                publish(change)
                if change.geometry { prefetch(around: lastFocus, lookahead: 0) }
            case .stale, .needsSnapshot:
                break
            }
        case .exit:
            exited = true
        }
    }

    private func publish(_ change: TerminalBuffer.Change) {
        bracketedPaste = buffer.modes.bracketedPaste
        appCursor = buffer.modes.appCursor
        if cols != buffer.cols { cols = buffer.cols }
        if rows != buffer.rows { rows = buffer.rows }
        guard !change.isEmpty else { return }
        onChange?(change)
    }

    /// A line for drawing; nil while its history page is on the way.
    func line(at index: Int) -> TermLine? { buffer.line(at: index) }

    /// For tests and previews: feeds events without a network.
    func inject(_ event: DiriClient.TerminalEvent) { handle(event) }

    // MARK: - Legacy

    private func pollLegacy() async {
        link = .legacy
        while !Task.isCancelled {
            if let screen = try? await client.screen(sessionID: sessionID) {
                let lines = screen.text.components(separatedBy: "\n").map { TermLine(runs: [TermRun($0)]) }
                let snapshot = GridSnapshot(
                    cols: max(screen.cols, 20), rows: max(lines.count, 1), seq: buffer.seq + 1,
                    historySize: 0, cursor: TermCursor(visible: false), lines: lines
                )
                publish(buffer.apply(snapshot))
            }
            try? await Task.sleep(for: .milliseconds(500))
        }
    }

    // MARK: - Scrollback

    /// Called by the view on every scroll step with the visible range and the
    /// scroll velocity in lines per second (negative is towards history). It
    /// only ever enqueues; it never waits.
    func prefetch(around focus: Int, visible: Range<Int>? = nil, lookahead: Int) {
        lastFocus = focus
        guard buffer.hasSnapshot, buffer.historySize > 0 else { return }
        let page = LineCache.pageSize
        let span = visible ?? (focus ..< focus + 1)
        // Two pages around the screen always, more in the direction of travel.
        let lower = span.lowerBound - 2 * page - max(0, -lookahead)
        let upper = span.upperBound + 2 * page + max(0, lookahead)
        let missing = buffer.cache.missingPages(in: lower ..< upper, historySize: buffer.historySize, focus: focus)
        // Nearest first; whatever was queued for an old position is dropped.
        wanted = missing
        pump()
    }

    private func warm() {
        guard !warmed, buffer.historySize > 0 else { return }
        warmed = true
        Task { [weak self] in
            guard let self else { return }
            // Newest first, with the regular queue keeping priority.
            while !Task.isCancelled {
                let floor = max(0, buffer.historySize - warmLines)
                let missing = buffer.cache.missingPages(
                    in: floor ..< buffer.historySize, historySize: buffer.historySize, focus: buffer.historySize
                )
                guard let next = missing.first else { return }
                if wanted.isEmpty, fetches < maxFetches - 1 {
                    wanted = [next]
                    pump()
                }
                try? await Task.sleep(for: .milliseconds(30))
            }
        }
    }

    private func pump() {
        while fetches < maxFetches, !wanted.isEmpty {
            let number = wanted.removeFirst()
            guard !buffer.cache.inFlight.contains(number) else { continue }
            let start = number * LineCache.pageSize
            let before = min(start + LineCache.pageSize, buffer.historySize)
            guard before > start else { continue }
            buffer.cache.markRequested(number)
            fetches += 1
            let client = client
            let id = sessionID
            Task { [weak self] in
                let page = try? await client.history(sessionID: id, before: before, limit: before - start)
                // A failed page is not asked for again straight away; the
                // next scroll step after a pause retries it.
                if page == nil { try? await Task.sleep(for: .seconds(1)) }
                guard let self else { return }
                self.fetches -= 1
                self.buffer.cache.markAnswered(number)
                if let page {
                    let focusPage = LineCache.page(of: self.lastFocus)
                    let change = self.buffer.apply(page, requestedStart: start, protecting: (focusPage - 2) ... (focusPage + 2))
                    self.publish(change)
                }
                self.pump()
            }
        }
    }

    // MARK: - Input

    /// Bytes for the PTY, delivered in order. While one request is in flight
    /// later keystrokes collect and go together, so fast typing over a slow
    /// link costs one round trip per batch, not per key.
    func send(_ bytes: [UInt8]) {
        guard !bytes.isEmpty else { return }
        outbox.append(contentsOf: bytes)
        flush()
    }

    func send(_ key: TermKey, modifiers: KeyModifiers = []) {
        send(KeyEncoder.encode(key, modifiers: modifiers, appCursor: buffer.modes.appCursor))
    }

    func paste(_ text: String) {
        send(KeyEncoder.paste(text, bracketed: buffer.modes.bracketedPaste))
    }

    private func flush() {
        guard !sending, !outbox.isEmpty else { return }
        sending = true
        let batch = outbox
        outbox = []
        let client = client
        let id = sessionID
        let batchID = UUID().uuidString
        let resendable = gatewayDedupesInput
        Task { [weak self] in
            // Keystrokes are not idempotent. A batch is resent only to a
            // gateway that writes each batch id once; a lost reply to an
            // older one is reported instead, and the screen shows whether it
            // landed.
            var attempt = 0
            var failed = false
            while true {
                do {
                    let once = try await client.input(batch, to: id, batchID: batchID)
                    self?.gatewayDedupesInput = once
                    break
                } catch {
                    guard resendable, attempt < Self.inputRetryDelays.count, Self.isTransient(error) else {
                        failed = true
                        break
                    }
                    try? await Task.sleep(for: Self.inputRetryDelays[attempt])
                    attempt += 1
                }
            }
            guard let self else { return }
            if failed { self.inputFailed = true }
            self.sending = false
            self.flush()
        }
    }

    /// Backoff for resending a batch whose reply was lost: covers a Wi-Fi to
    /// cellular hop or a tailnet reconnect, not a Mac that is off.
    static let inputRetryDelays: [Duration] = [.milliseconds(300), .seconds(1), .seconds(2), .seconds(4)]

    /// A dropped connection or a gateway that could not reach the Engine. A
    /// refusal (bad session, bad token) would fail the same way again.
    static func isTransient(_ error: Error) -> Bool {
        switch error {
        case DiriClient.Failure.daemonUnreachable: true
        case let DiriClient.Failure.http(code, _): code == 502 || code == 503 || code == 504
        default: false
        }
    }

    /// Fits the PTY to the phone. Only ever on request: other viewers of the
    /// session keep their width otherwise.
    func resize(cols: Int, rows: Int) async throws {
        try await client.resize(sessionID: sessionID, cols: cols, rows: rows)
    }
}
