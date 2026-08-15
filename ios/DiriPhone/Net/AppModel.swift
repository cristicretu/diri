import Foundation
import Observation
import SwiftUI

/// Everything the UI reads. One model, because the daemon is the only source of
/// truth and the phone's job is to mirror it, not to hold opinions of its own.
@MainActor
@Observable
final class AppModel {
    enum Connection: Equatable {
        case unconfigured
        case connecting
        case online(host: String)
        case offline(String)
    }

    private(set) var connection: Connection = .unconfigured
    private(set) var groups: [SidebarGroup] = []
    private(set) var sessions: [SessionRecord] = []
    private(set) var agents: [DiriClient.AgentReadiness.Item] = []
    private(set) var projects: [String: Project] = [:]
    private(set) var host: String = ""

    var selectedSessionID: String?
    /// `-session <id>`, the companion to `-endpoint`: opens straight into one
    /// session so the detail screen can be driven without a human tapping.
    let initialSessionID: String? = {
        guard let index = CommandLine.arguments.firstIndex(of: "-session"),
              index + 1 < CommandLine.arguments.count
        else { return nil }
        return CommandLine.arguments[index + 1]
    }()
    var collapsedSessions: Set<String> = []
    var collapsedProjects: Set<String> = []
    var banner: String?

    private var client: DiriClient?
    private var eventTask: Task<Void, Never>?
    private var pollTask: Task<Void, Never>?

    /// How often the list is refetched when the event stream is quiet. The
    /// stream is the fast path; this only exists so a silently dropped
    /// connection cannot leave the list frozen.
    private let backstopInterval: Duration = .seconds(15)

    var endpoint: DiriClient.Endpoint? {
        didSet {
            guard endpoint != oldValue else { return }
            Credentials.save(endpoint)
            restart()
        }
    }

    init() {
        // `-endpoint <enrolment url>` seeds the connection without going
        // through the keychain, which is how the simulator gets pointed at a
        // real host for screenshots and manual testing. It is a launch
        // argument, so it cannot be set by anything but whoever started the
        // process.
        if let index = CommandLine.arguments.firstIndex(of: "-endpoint"),
           index + 1 < CommandLine.arguments.count,
           let seeded = DiriClient.Endpoint(enrolmentURL: CommandLine.arguments[index + 1])
        {
            endpoint = seeded
        } else {
            endpoint = Credentials.load()
        }
        restart()
    }

    // MARK: - Lifecycle

    private func restart() {
        eventTask?.cancel()
        pollTask?.cancel()
        eventTask = nil
        pollTask = nil

        guard let endpoint else {
            connection = .unconfigured
            client = nil
            groups = []
            sessions = []
            return
        }

        let client = DiriClient(endpoint: endpoint)
        self.client = client
        connection = .connecting

        Task { await refresh() }
        Task { await loadAgents() }

        eventTask = Task { [weak self] in
            // The stream ends whenever the network moves; re-subscribing in a
            // loop is what makes a tunnel change invisible to the user.
            while !Task.isCancelled {
                guard let self, let client = self.client else { return }
                for await _ in await client.events() {
                    if Task.isCancelled { return }
                    await self.refresh()
                }
                if Task.isCancelled { return }
                try? await Task.sleep(for: .seconds(2))
            }
        }

        let backstop = backstopInterval
        pollTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: backstop)
                guard let self, !Task.isCancelled else { return }
                await self.refresh()
            }
        }
    }

    // MARK: - Reads

    func refresh() async {
        guard let client else { return }
        do {
            let payload = try await client.sessions()
            host = payload.host
            sessions = payload.sessions
            projects = Dictionary(
                (payload.projects ?? []).map { ($0.id, $0) },
                uniquingKeysWith: { first, _ in first }
            )
            reproject()
            connection = .online(host: payload.host)
        } catch DiriClient.Failure.unauthorized {
            connection = .offline("Token rejected")
            banner = "That token was not accepted."
        } catch {
            connection = .offline(error.localizedDescription)
        }
    }

    private func loadAgents() async {
        guard let client else { return }
        agents = (try? await client.agents()) ?? []
    }

    private func reproject() {
        var prefs = SidebarProjection.Preferences()
        prefs.collapsedSessions = collapsedSessions
        prefs.collapsedProjects = collapsedProjects
        prefs.pinnedSessions = Set(sessions.filter(\.pinned).map(\.id))
        groups = SidebarProjection.build(
            sessions: sessions.filter { !$0.isArchived },
            projects: projects,
            prefs: prefs
        )
    }

    func session(_ id: String) -> SessionRecord? {
        sessions.first { $0.id == id }
    }

    /// Sessions asking for a human, in the order the sidebar draws them —
    /// what the "jump to next" affordance walks.
    var needingInput: [SessionRecord] {
        groups.flatMap(\.rows).map(\.session).filter { $0.attention == .needsInput }
    }

    // MARK: - Writes

    func toggleCollapsed(_ id: String) {
        if collapsedSessions.contains(id) {
            collapsedSessions.remove(id)
        } else {
            collapsedSessions.insert(id)
        }
        reproject()
    }

    func toggleProjectCollapsed(_ id: String) {
        if collapsedProjects.contains(id) {
            collapsedProjects.remove(id)
        } else {
            collapsedProjects.insert(id)
        }
        reproject()
    }

    func screen(for id: String) async -> DiriClient.Screen? {
        guard let client else { return nil }
        return try? await client.screen(sessionID: id)
    }

    func send(_ text: String, to id: String) async throws {
        guard let client else { return }
        try await client.send(text: text, to: id)
    }

    func send(_ key: TerminalKey, to id: String) async throws {
        guard let client else { return }
        try await client.send(key: key, to: id)
    }

    func markSeen(_ id: String) async {
        try? await client?.markSeen(sessionID: id)
        await refresh()
    }

    func kill(_ id: String) async throws {
        guard let client else { return }
        try await client.kill(sessionID: id)
        await refresh()
    }

    func spawn(kind: String, cwd: String, prompt: String?) async throws -> SessionRecord {
        guard let client else { throw DiriClient.Failure.daemonUnreachable("not configured") }
        let record = try await client.spawn(kind: kind, cwd: cwd, prompt: prompt)
        await refresh()
        return record
    }

    /// Directories that already have sessions — almost always where the next
    /// one belongs, and the only way to avoid typing a path on a phone.
    var recentDirectories: [String] {
        var seen: Set<String> = []
        var result: [String] = []
        for session in sessions.sorted(by: { $0.updatedAt > $1.updatedAt }) {
            if seen.insert(session.cwd).inserted { result.append(session.cwd) }
            if result.count == 8 { break }
        }
        return result
    }
}

// MARK: - Credentials

/// The endpoint lives in the keychain, not `UserDefaults`: it is a bearer token
/// that can start and kill processes on a real machine.
enum Credentials {
    private static let service = "com.cristicretu.diri.phone"
    private static let account = "endpoint"

    static func save(_ endpoint: DiriClient.Endpoint?) {
        guard let endpoint, let data = try? JSONEncoder().encode(endpoint) else {
            SecItemDelete(baseQuery() as CFDictionary)
            return
        }
        var query = baseQuery()
        SecItemDelete(query as CFDictionary)
        query[kSecValueData as String] = data
        query[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
        SecItemAdd(query as CFDictionary, nil)
    }

    static func load() -> DiriClient.Endpoint? {
        var query = baseQuery()
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
              let data = item as? Data
        else { return nil }
        return try? JSONDecoder().decode(DiriClient.Endpoint.self, from: data)
    }

    private static func baseQuery() -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }
}
