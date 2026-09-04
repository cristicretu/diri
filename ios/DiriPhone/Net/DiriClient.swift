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
        case daemonUnreachable(String)
        case http(Int, String)
        case malformed

        var errorDescription: String? {
            switch self {
            case .unauthorized: "That token was not accepted."
            case let .daemonUnreachable(detail): "The daemon is not reachable: \(detail)"
            case let .http(code, message): message.isEmpty ? "HTTP \(code)" : message
            case .malformed: "The response could not be read."
            }
        }
    }

    private let endpoint: Endpoint
    private let session: URLSession

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

    private func request(_ path: String, method: String = "GET", body: [String: Any]? = nil) throws -> URLRequest {
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

    private func send(_ request: URLRequest) async throws -> Data {
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
        case 401:
            throw Failure.unauthorized
        default:
            let message = (try? JSONSerialization.jsonObject(with: data) as? [String: Any])?["error"] as? String
            throw Failure.http(http.statusCode, message ?? "")
        }
    }

    private func decode<T: Decodable>(_ type: T.Type, from data: Data) throws -> T {
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

    // MARK: - Events

    /// Server-sent events, surfaced as a stream of event names.
    ///
    /// The payloads are deliberately ignored: every daemon event means the same
    /// thing to a phone, which is *refetch*. Applying deltas would risk leaving
    /// the list subtly wrong after a dropped stream, where a refetch cannot.
    func events() -> AsyncStream<String> {
        AsyncStream { continuation in
            let task = Task {
                do {
                    let request = try request("/api/events")
                    let (bytes, response) = try await session.bytes(for: request)
                    guard let http = response as? HTTPURLResponse, http.statusCode == 200 else {
                        continuation.finish()
                        return
                    }
                    for try await line in bytes.lines {
                        if Task.isCancelled { break }
                        if let name = line.strippingPrefix("event: ") {
                            continuation.yield(name)
                        }
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

    private func escape(_ value: String) -> String {
        value.addingPercentEncoding(withAllowedCharacters: .alphanumerics) ?? value
    }
}

private extension String {
    func strippingPrefix(_ prefix: String) -> String? {
        hasPrefix(prefix) ? String(dropFirst(prefix.count)) : nil
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
