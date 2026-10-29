import Foundation

/// Talks to `diri-web`, which is a frontend on a `dirijord` control socket.
///
/// The phone deliberately does not speak the daemon's NDJSON protocol itself.
/// That protocol assumes a unix socket, a persistent connection and an event
/// cursor — all things a phone loses every time it changes cell tower. HTTP
/// against `diri-web` is stateless per request, so a reconnect is just the next
/// request succeeding.
actor DiriClient {
    struct Endpoint: Codable, Equatable, Sendable {
        var baseURL: URL
        var token: String

        /// Parses an enrolment link — `http://forge:7380/?token=…` — which is
        /// exactly what `diri-web url` prints, so the whole setup is one paste.
        init?(enrolmentURL: String) {
            let trimmed = enrolmentURL.trimmingCharacters(in: .whitespacesAndNewlines)
            guard var components = URLComponents(string: trimmed),
                  ["http", "https"].contains(components.scheme?.lowercased() ?? ""),
                  let host = components.host, !host.isEmpty,
                  components.user == nil, components.password == nil,
                  components.fragment == nil,
                  components.queryItems?.filter({ $0.name == "token" }).count == 1
            else { return nil }
            let token = components.queryItems?.first { $0.name == "token" }?.value
            guard let token, !token.isEmpty else { return nil }
            components.queryItems = nil
            components.query = nil
            components.path = ""
            guard let baseURL = components.url else { return nil }
            self.baseURL = baseURL
            self.token = token
        }

        init(baseURL: URL, token: String) {
            self.baseURL = baseURL
            self.token = token
        }
    }

    enum Failure: LocalizedError {
        case unauthorized
        /// Tailnet admission (#952) turned the device away for a reason
        /// pairing again would not fix: another Tailscale user, a tagged
        /// device, not over the tailnet, or the Mac cannot ask Tailscale.
        case refused(code: String, message: String)
        case daemonUnreachable(String)
        case http(Int, String)
        case malformed

        var errorDescription: String? {
            switch self {
            case .unauthorized: "This iPhone is not paired with that computer. Scan its pairing code again."
            case let .refused(code, message): PairingHelp.refusal(code: code, message: message)
            case let .daemonUnreachable(detail): "The daemon is not reachable: \(detail)"
            case let .http(code, message): message.isEmpty ? "HTTP \(code)" : message
            case .malformed: "The response could not be read."
            }
        }
    }

    private let endpoint: Endpoint
    let session: URLSession

    /// Enrollment must fail promptly with actionable setup guidance. Normal
    /// session traffic can wait across network changes; a first-time pairing
    /// must not inherit URLSession's multi-day resource timeout.
    static func pairingConfiguration() -> URLSessionConfiguration {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 15
        configuration.timeoutIntervalForResource = 20
        configuration.waitsForConnectivity = false
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        return configuration
    }

    /// Health probes decide failover, so a silent host must cost seconds, not
    /// the minutes `waitsForConnectivity` would spend on it.
    static func probeConfiguration() -> URLSessionConfiguration {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 3
        configuration.timeoutIntervalForResource = 4
        configuration.waitsForConnectivity = false
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        return configuration
    }

    /// `configuration` exists so tests can install a stub `URLProtocol`. The
    /// requests this builds are a contract with `diri-web` — a wrong path or a
    /// missing header fails identically to the daemon being down, which is the
    /// kind of bug that gets misdiagnosed for an hour.
    init(endpoint: Endpoint, configuration: URLSessionConfiguration? = nil) {
        self.endpoint = endpoint
        let configuration = configuration ?? {
            let configuration = URLSessionConfiguration.ephemeral
            configuration.timeoutIntervalForRequest = 15
            // A phone on a tailnet reconnects constantly; waiting for
            // connectivity beats surfacing a failure the user can do nothing
            // about.
            configuration.waitsForConnectivity = true
            configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
            return configuration
        }()
        session = URLSession(configuration: configuration, delegate: NoRedirects(), delegateQueue: nil)
    }

    // MARK: - Requests

    func request(_ path: String, method: String = "GET", body: [String: Any]? = nil) throws -> URLRequest {
        guard let url = URL(string: path, relativeTo: endpoint.baseURL) else {
            throw Failure.malformed
        }
        var request = URLRequest(url: url)
        request.httpMethod = method
        if path == "/api/spawn" { request.timeoutInterval = 330 }
        request.setValue(endpoint.token, forHTTPHeaderField: "X-Diri-Token")
        if let body {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = try JSONSerialization.data(withJSONObject: body)
        }
        return request
    }

    func send(_ request: URLRequest) async throws -> Data {
        let (data, response): (Data, URLResponse)
        do {
            (data, response) = try await session.data(for: request)
        } catch {
            throw Failure.daemonUnreachable(error.localizedDescription)
        }
        guard let http = response as? HTTPURLResponse else { throw Failure.malformed }
        switch http.statusCode {
        case 200 ..< 300:
            return data
        default:
            let body = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
            let message = body?["error"] as? String ?? ""
            // 401 is "pair (again)": a token gateway's wrong token, or tailnet
            // admission's `device_not_paired` (never paired, or removed).
            if http.statusCode == 401 { throw Failure.unauthorized }
            if let code = body?["code"] as? String, Self.admissionRefusals.contains(code) {
                throw Failure.refused(code: code, message: message)
            }
            throw Failure.http(http.statusCode, message)
        }
    }

    /// Tailnet admission's refusals that pairing again does not fix.
    static let admissionRefusals: Set<String> = [
        "tailnet_identity_unavailable", "not_a_tailnet_peer", "tagged_node_refused",
        "tailnet_owner_unknown", "not_the_owner", "foreign_origin",
    ]

    func decode<T: Decodable>(_ type: T.Type, from data: Data) throws -> T {
        do {
            return try JSONDecoder().decode(type, from: data)
        } catch {
            throw Failure.malformed
        }
    }

    // MARK: - API

    struct SessionsResponse: Decodable {
        var host: String
        var sessions: [SessionRecord]
        /// Absent on older `diri-web` builds, in which case group headers fall
        /// back to a name derived from the path.
        var projects: [Project]?
    }

    func sessions() async throws -> SessionsResponse {
        try decode(SessionsResponse.self, from: try await send(try request("/api/sessions")))
    }

    struct Health: Decodable {
        var ok: Bool
        var host: String
        var daemon: String
    }

    func health() async throws -> Health {
        try decode(Health.self, from: try await send(try request("/api/health")))
    }

    struct Screen: Decodable {
        var text: String
        var cols: Int
        var rows: Int
    }

    func screen(sessionID: String) async throws -> Screen {
        let path = "/api/session/\(escape(sessionID))/screen"
        return try decode(Screen.self, from: try await send(try request(path)))
    }

    struct Diff: Decodable, Sendable {
        var patch: String
        var repoRoot: String
        var truncated: Bool
    }

    func diff(sessionID: String) async throws -> Diff {
        try decode(Diff.self, from: try await send(try request("/api/session/\(escape(sessionID))/diff")))
    }

    struct AgentReadiness: Decodable {
        struct Item: Decodable {
            var kind: AgentKind
            var binary: String?
            var path: String?
        }

        var agents: [Item]
    }

    func agents(host: String? = nil) async throws -> [AgentReadiness.Item] {
        let payload = try decode(
            AgentReadiness.self, from: try await send(try request(queryPath("/api/agents", ["host": host])))
        )
        // `path` is set only when the daemon actually found the binary, so this
        // offers what this host can really start rather than the whole catalog.
        return payload.agents.filter { $0.path != nil || $0.kind.id == "shell" }
    }

    struct Host: Decodable, Identifiable, Sendable {
        var id: String
        var name: String
        var defaultCwd: String?
    }

    func hosts() async throws -> [Host] {
        struct Payload: Decodable { var hosts: [Host] }
        return try decode(Payload.self, from: try await send(try request("/api/hosts"))).hosts
    }

    struct DirectoryListing: Decodable, Sendable {
        struct Entry: Decodable, Identifiable, Sendable {
            var name: String
            var path: String
            var id: String { path }
        }
        var path: String
        var parent: String?
        var entries: [Entry]
        var truncated: Bool
    }

    func directories(host: String?, path: String) async throws -> DirectoryListing {
        try decode(DirectoryListing.self, from: try await send(try request(
            queryPath("/api/directories", ["host": host, "path": path])
        )))
    }

    private func queryPath(_ path: String, _ values: [String: String?]) -> String {
        var components = URLComponents()
        components.path = path
        components.queryItems = values.sorted { $0.key < $1.key }.compactMap { key, value in
            value.map { URLQueryItem(name: key, value: $0) }
        }
        return components.string ?? path
    }

    func send(text: String, to sessionID: String, submit: Bool = true) async throws {
        let path = "/api/session/\(escape(sessionID))/send"
        _ = try await send(try request(path, method: "POST", body: ["text": text, "submit": submit]))
    }

    func send(key: TerminalKey, to sessionID: String) async throws {
        let path = "/api/session/\(escape(sessionID))/key"
        _ = try await send(try request(path, method: "POST", body: ["key": key.wireName]))
    }

    /// Approves or denies the permission prompt the phone saw, with the
    /// agent's own keys. The gateway re-checks that the session is still on
    /// that prompt (`occurredAt`) and refuses with 409 otherwise; `answerID`
    /// makes a resend after a lost reply land once.
    func answer(sessionID: String, approve: Bool, occurredAt: Double, answerID: String) async throws {
        let path = "/api/session/\(escape(sessionID))/answer"
        let body: [String: Any] = ["answer": approve ? "approve" : "deny", "id": answerID, "occurredAt": occurredAt]
        _ = try await send(try request(path, method: "POST", body: body))
    }

    func kill(sessionID: String) async throws {
        _ = try await send(try request("/api/session/\(escape(sessionID))/kill", method: "POST"))
    }

    func markSeen(sessionID: String) async throws {
        _ = try await send(try request("/api/session/\(escape(sessionID))/seen", method: "POST"))
    }

    func spawn(kind: String, cwd: String, prompt: String?, host: String? = nil,
               worktree: Bool = false, branch: String? = nil, base: String? = nil) async throws -> SessionRecord {
        var body: [String: Any] = ["kind": kind, "cwd": cwd]
        if let host { body["host"] = host }
        if worktree {
            body["worktree"] = true
            body["base"] = base ?? "main"
            if let branch, !branch.isEmpty { body["branch"] = branch }
        }
        if let prompt, !prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            body["prompt"] = prompt
        }
        return try decode(
            SessionRecord.self, from: try await send(try request("/api/spawn", method: "POST", body: body))
        )
    }

    // MARK: - Voice

    struct Turn: Codable, Equatable, Sendable {
        var role: String
        var text: String
        var turn: Int?

        var isAgent: Bool { role == "agent" }
    }

    /// Oldest-first conversation turns.
    func transcript(sessionID: String, limit: Int = 20) async throws -> [Turn] {
        struct Payload: Decodable { var turns: [Turn] }
        // Not `queryPath`: URLComponents would percent-encode the already
        // escaped id a second time.
        let path = "/api/session/\(escape(sessionID))/transcript?limit=\(limit)"
        return try decode(Payload.self, from: try await send(try request(path))).turns
    }

    struct Spoken: Codable, Equatable, Sendable {
        var turn: Int?
        var text: String?
    }

    /// What the agent most recently said aloud (its `speak` summary), if any.
    func spoken(sessionID: String) async throws -> Spoken {
        try decode(Spoken.self, from: try await send(try request("/api/session/\(escape(sessionID))/spoken")))
    }

    private struct VoicePrefs: Codable { var readAloud: Bool }

    func readAloud() async throws -> Bool {
        try decode(VoicePrefs.self, from: try await send(try request("/api/voice"))).readAloud
    }

    func setReadAloud(_ on: Bool) async throws {
        _ = try await send(try request("/api/voice", method: "POST", body: ["readAloud": on]))
    }

    // MARK: - Events

    struct ServerEvent: Equatable, Sendable {
        var name: String
        var data: String
    }

    /// `turn_finished` — an agent ended its turn.
    struct TurnFinished: Decodable, Equatable, Sendable {
        var sessionID: String
        var turn: Int?
        var spoken: String?

        init(sessionID: String, turn: Int?, spoken: String?) {
            self.sessionID = sessionID
            self.turn = turn
            self.spoken = spoken
        }

        init?(_ event: ServerEvent) {
            guard event.name == "turn_finished",
                  let value = try? JSONDecoder().decode(Self.self, from: Data(event.data.utf8))
            else { return nil }
            self = value
        }
    }

    /// Server-sent events.
    ///
    /// The list treats every event as *refetch*: applying deltas would risk
    /// leaving it subtly wrong after a dropped stream, where a refetch cannot.
    /// Voice mode reads the payload of `turn_finished`.
    func events() -> AsyncStream<ServerEvent> {
        AsyncStream { continuation in
            let task = Task {
                do {
                    let request = try request("/api/events")
                    let (bytes, response) = try await session.bytes(for: request)
                    guard let http = response as? HTTPURLResponse, http.statusCode == 200 else {
                        continuation.finish()
                        return
                    }
                    var parser = SSEParser()
                    for try await line in bytes.lines {
                        if Task.isCancelled { break }
                        for event in parser.feed(line) { continuation.yield(event) }
                    }
                } catch {
                    // A dropped stream is normal on a phone. The caller's
                    // polling backstop keeps the list moving until it returns.
                }
                continuation.finish()
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    func escape(_ value: String) -> String {
        value.addingPercentEncoding(withAllowedCharacters: .alphanumerics) ?? value
    }
}

/// Line-at-a-time SSE framing. `AsyncLineSequence` drops empty lines, so a
/// frame cannot wait for its blank terminator: `diri-web` writes exactly one
/// `data:` line per event, so the frame is complete when its data arrives.
struct SSEParser {
    private var name: String?

    mutating func feed(_ line: String) -> [DiriClient.ServerEvent] {
        if let value = line.strippingField("event") {
            name = value
            return []
        }
        if let value = line.strippingField("data"), let name {
            self.name = nil
            return [DiriClient.ServerEvent(name: name, data: value)]
        }
        if line.isEmpty, let name {
            self.name = nil
            return [DiriClient.ServerEvent(name: name, data: "")]
        }
        return []
    }
}

private extension String {
    func strippingField(_ field: String) -> String? {
        guard hasPrefix(field + ":") else { return nil }
        let rest = dropFirst(field.count + 1)
        return String(rest.hasPrefix(" ") ? rest.dropFirst() : rest)
    }
}

/// The keys the daemon knows how to write to a PTY. Names match `key_sequence`
/// in `diri-web`; an unknown name is refused there rather than guessed.
enum TerminalKey: String, CaseIterable, Sendable {
    case enter, escape, tab, shiftTab, backspace
    case up, down, left, right, pageUp, pageDown
    case ctrlC, ctrlD, ctrlU, ctrlR, altEnter
    case yes, no
    case digit1, digit2, digit3

    var wireName: String {
        switch self {
        case .shiftTab: "shift-tab"
        case .pageUp: "page-up"
        case .pageDown: "page-down"
        case .ctrlC: "ctrl-c"
        case .ctrlD: "ctrl-d"
        case .ctrlU: "ctrl-u"
        case .ctrlR: "ctrl-r"
        case .altEnter: "alt-enter"
        case .digit1: "digit-1"
        case .digit2: "digit-2"
        case .digit3: "digit-3"
        default: rawValue
        }
    }

    var label: String {
        switch self {
        case .enter: "⏎"
        case .escape: "esc"
        case .tab: "⇥"
        case .shiftTab: "⇧⇥"
        case .backspace: "⌫"
        case .up: "↑"
        case .down: "↓"
        case .left: "←"
        case .right: "→"
        case .pageUp: "⇞"
        case .pageDown: "⇟"
        case .ctrlC: "^C"
        case .ctrlD: "^D"
        case .ctrlU: "^U"
        case .ctrlR: "^R"
        case .altEnter: "⌥⏎"
        case .yes: "y"
        case .no: "n"
        case .digit1: "1"
        case .digit2: "2"
        case .digit3: "3"
        }
    }
}
