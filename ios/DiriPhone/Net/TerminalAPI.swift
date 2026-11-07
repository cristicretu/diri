import Foundation

/// The terminal-stream endpoints. Decoding happens on the client's actor, so
/// the main thread only ever sees finished values.
extension DiriClient {
    enum TerminalEvent: Sendable {
        case snapshot(GridSnapshot)
        case diff(GridDiff)
        /// The answer to `?resume=`: only the rows that changed while away.
        case resume(GridDiff)
        case exit
    }

    func grid(sessionID: String) async throws -> GridSnapshot {
        try decode(GridSnapshot.self, from: try await send(try request("/api/session/\(escape(sessionID))/grid")))
    }

    func history(sessionID: String, before: Int, limit: Int) async throws -> HistoryPage {
        let path = "/api/session/\(escape(sessionID))/history?before=\(before)&limit=\(limit)"
        return try decode(HistoryPage.self, from: try await send(try request(path)))
    }

    /// Raw bytes to the PTY, exactly as typed. `batchID` names the batch so
    /// a resend after a lost reply is written once; the result says whether
    /// this gateway honours that, since an older one would write it twice.
    @discardableResult
    func input(_ bytes: [UInt8], to sessionID: String, batchID: String) async throws -> Bool {
        let path = "/api/session/\(escape(sessionID))/input"
        let body: [String: Any] = ["data": Data(bytes).base64EncodedString(), "id": batchID]
        let reply = try await send(try request(path, method: "POST", body: body))
        return (try? JSONSerialization.jsonObject(with: reply) as? [String: Any])?["once"] as? Bool ?? false
    }

    func resize(sessionID: String, cols: Int, rows: Int) async throws {
        let path = "/api/session/\(escape(sessionID))/resize"
        _ = try await send(try request(path, method: "POST", body: ["cols": cols, "rows": rows]))
    }

    /// Whether this gateway serves the terminal stream at all. An older
    /// `diri-web` answers 404, and the phone falls back to polling `/screen`.
    enum StreamOutcome: Sendable { case ended, unsupported, unauthorized }

    /// Silence after which a stream is presumed dead: two missed heartbeats.
    static let streamStallTimeout: TimeInterval = 45

    /// The live grid. The first event is a full snapshot — or, when `resume`
    /// describes what the phone still shows, a `resume` of only the rows that
    /// differ — and diffs follow. The stream ends when the connection drops;
    /// the caller reconnects, so a missed diff can never leave the screen
    /// wrong. A gateway without resume ignores the query and sends a snapshot.
    func terminalStream(sessionID: String, resume: String? = nil) -> (AsyncStream<TerminalEvent>, Task<StreamOutcome, Never>) {
        var continuation: AsyncStream<TerminalEvent>.Continuation!
        let stream = AsyncStream<TerminalEvent>(bufferingPolicy: .unbounded) { continuation = $0 }
        let output = continuation!
        let task = Task<StreamOutcome, Never> {
            defer { output.finish() }
            do {
                var path = "/api/session/\(escape(sessionID))/stream"
                if let resume { path += "?resume=" + resume }
                var request = try request(path)
                // The gateway writes a heartbeat every 20 s. Two missed ones
                // mean the connection is gone (a Wi-Fi to cellular hop often
                // leaves the old socket open but silent): give up and resume.
                request.timeoutInterval = Self.streamStallTimeout
                request.setValue("text/event-stream", forHTTPHeaderField: "Accept")
                let (bytes, response) = try await session.bytes(for: request)
                guard let http = response as? HTTPURLResponse else { return .ended }
                if http.statusCode == 404 { return .unsupported }
                if http.statusCode == 401 { return .unauthorized }
                guard http.statusCode == 200 else { return .ended }
                var parser = SSEParser()
                let decoder = JSONDecoder()
                for try await line in bytes.lines {
                    if Task.isCancelled { break }
                    for event in parser.feed(line) {
                        let data = Data(event.data.utf8)
                        switch event.name {
                        case "snapshot":
                            if let value = try? decoder.decode(GridSnapshot.self, from: data) { output.yield(.snapshot(value)) }
                        case "diff":
                            if let value = try? decoder.decode(GridDiff.self, from: data) { output.yield(.diff(value)) }
                        case "resume":
                            if let value = try? decoder.decode(GridDiff.self, from: data) { output.yield(.resume(value)) }
                        case "exit":
                            output.yield(.exit)
                        default:
                            break
                        }
                    }
                }
            } catch {
                // Dropped; the caller reconnects.
            }
            return .ended
        }
        output.onTermination = { _ in task.cancel() }
        return (stream, task)
    }
}
