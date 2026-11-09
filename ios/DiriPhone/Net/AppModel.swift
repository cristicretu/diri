import Foundation
import Network
import Observation
import SwiftUI

/// Everything the UI reads. One model, because the daemons are the only source
/// of truth and the phone's job is to mirror them, not to hold opinions.
///
/// Several gateways can be paired ("Mac", "forge"). They are probed together;
/// the highest-priority reachable one is *live* — it answers new-session,
/// agent and folder questions — and sessions are listed from every reachable
/// one, each tagged with where it runs.
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
    /// The live gateway's host label.
    private(set) var host: String = ""

    /// Projects in the Mac's sidebar order, sectioned by computer.
    private(set) var sections: [SidebarSection] = []
    private var layouts: [String: DiriClient.SidebarLayout] = [:]

    private(set) var gateways: [Gateway] = []
    private(set) var outcomes: [String: GatewayProbe.Outcome] = [:]
    private(set) var liveGatewayID: String?

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

    private let store: SecretStore
    /// False for a `-endpoint` launch: a seeded gateway is not written back.
    private var persists = true
    private var clients: [String: DiriClient] = [:]
    private var probeClients: [String: DiriClient] = [:]
    private var payloads: [String: DiriClient.SessionsResponse] = [:]
    private var sessionOwner: [String: String] = [:]
    private var eventTasks: [String: Task<Void, Never>] = [:]
    private var pollTask: Task<Void, Never>?
    private var refreshing: Set<String> = []
    private var probing = false
    private var probeQueued = false
    private var active = true
    private let pathMonitor = NWPathMonitor()

    /// How often gateways are re-probed and lists refetched when the event
    /// streams are quiet. The streams are the fast path; this exists so a
    /// silently dropped connection — or a host coming back — is noticed.
    private let backstopInterval: Duration = .seconds(15)
    private let probeTimeout: Duration = .seconds(3)

    init(store: SecretStore = KeychainSecretStore()) {
        self.store = store
        // `-endpoint <enrolment url>` seeds one gateway without going through
        // the keychain, which is how the simulator gets pointed at a real host.
        if let index = CommandLine.arguments.firstIndex(of: "-endpoint"),
           index + 1 < CommandLine.arguments.count,
           let seeded = DiriClient.Endpoint(enrolmentURL: CommandLine.arguments[index + 1])
        {
            gateways = [Gateway(name: Gateway.provisionalName(for: seeded), endpoint: seeded)]
            persists = false
        } else {
            gateways = GatewayStore.load(from: store)
        }
        pathMonitor.pathUpdateHandler = { [weak self] _ in
            Task { @MainActor in
                guard let self, self.active else { return }
                await self.reprobe()
            }
        }
        pathMonitor.start(queue: DispatchQueue(label: "diri.path"))
        restart()
    }

    var hasGateways: Bool { !gateways.isEmpty }

    var liveGateway: Gateway? { gateways.first { $0.id == liveGatewayID } }

    // MARK: - Gateways

    private func setGateways(_ list: [Gateway]) {
        gateways = list
        if persists { GatewayStore.save(list, to: store) }
        restart()
    }

    func removeGateway(_ id: String) {
        setGateways(gateways.filter { $0.id != id })
    }

    /// Moves a gateway to the top: it becomes live whenever it is reachable.
    func preferGateway(_ id: String) {
        guard let index = gateways.firstIndex(where: { $0.id == id }) else { return }
        var list = gateways
        list.insert(list.remove(at: index), at: 0)
        setGateways(list)
    }

    func connect(link: String) async throws {
        guard let candidate = DiriClient.Endpoint(enrolmentURL: link) else {
            throw DiriClient.Failure.http(400, "Use the pairing code or link in Diri → Settings → Phone access on your Mac.")
        }
        // Verify authenticated Engine access before saving credentials.
        let payload = try await DiriClient(endpoint: candidate, configuration: DiriClient.pairingConfiguration()).sessions()
        try Task.checkCancellation()
        let name = payload.host.isEmpty ? Gateway.provisionalName(for: candidate) : payload.host
        persists = true
        setGateways(GatewayStore.adding(Gateway(name: name, endpoint: candidate), to: gateways))
    }

    // MARK: - Lifecycle

    private func restart() {
        eventTasks.values.forEach { $0.cancel() }
        eventTasks = [:]
        pollTask?.cancel()
        pollTask = nil
        clients = [:]
        probeClients = [:]
        payloads = [:]
        outcomes = [:]
        liveGatewayID = nil

        guard !gateways.isEmpty else {
            connection = .unconfigured
            groups = []
            sessions = []
            sessionOwner = [:]
            agents = []
            projects = [:]
            host = ""
            selectedSessionID = nil
            banner = nil
            return
        }

        for gateway in gateways {
            clients[gateway.id] = DiriClient(endpoint: gateway.endpoint)
            probeClients[gateway.id] = DiriClient(
                endpoint: gateway.endpoint, configuration: DiriClient.probeConfiguration()
            )
        }
        connection = .connecting
        guard active else { return }

        Task { await reprobe() }
        let backstop = backstopInterval
        pollTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: backstop)
                guard let self, !Task.isCancelled else { return }
                await self.reprobe()
            }
        }
    }

    func setActive(_ active: Bool) {
        self.active = active
        if active {
            restart()
        } else {
            eventTasks.values.forEach { $0.cancel() }
            eventTasks = [:]
            pollTask?.cancel()
        }
    }

    /// Probes every gateway, picks the live one, and refetches what is reachable.
    func reprobe() async {
        guard !gateways.isEmpty else { return }
        if probing { probeQueued = true; return }
        probing = true
        defer { probing = false }
        repeat {
            probeQueued = false
            let list = gateways
            let probes = probeClients
            let results = await GatewayProbe.probeAll(list, timeout: probeTimeout) { gateway in
                guard let client = probes[gateway.id] else { return .unreachable("Not configured") }
                do {
                    let health = try await client.health()
                    return .reachable(host: health.host)
                } catch DiriClient.Failure.unauthorized {
                    return .unauthorized
                } catch {
                    return .unreachable(error.localizedDescription)
                }
            }
            guard list == gateways else { continue }
            apply(results)
            await refreshAll()
        } while probeQueued
    }

    private func apply(_ results: [String: GatewayProbe.Outcome]) {
        outcomes = results
        // A migrated or pasted gateway is named after its address until it
        // introduces itself.
        var renamed = gateways
        for index in renamed.indices {
            if case let .reachable(name) = results[renamed[index].id], !name.isEmpty,
               renamed[index].name == Gateway.provisionalName(for: renamed[index].endpoint),
               renamed[index].name != name
            {
                renamed[index].name = name
            }
        }
        if renamed != gateways {
            gateways = renamed
            if persists { GatewayStore.save(renamed, to: store) }
        }

        let previousLive = liveGatewayID
        let live = GatewayProbe.live(gateways, outcomes: results)
        liveGatewayID = live?.id
        if let live {
            host = live.name
            connection = .online(host: live.name)
            banner = nil
        } else {
            host = ""
            let unauthorized = results.values.contains(.unauthorized)
            connection = .offline(unauthorized ? "Token rejected" : "No paired computer answered")
            banner = unauthorized ? "That token was not accepted." : nil
        }
        if previousLive != liveGatewayID { Task { await loadAgents() } }

        for gateway in gateways {
            let reachable = results[gateway.id]?.isReachable == true
            if reachable, eventTasks[gateway.id] == nil, active {
                startEvents(for: gateway.id)
            } else if !reachable {
                eventTasks.removeValue(forKey: gateway.id)?.cancel()
                payloads.removeValue(forKey: gateway.id)
            }
        }
        rebuild()
    }

    private func startEvents(for id: String) {
        guard let client = clients[id] else { return }
        eventTasks[id] = Task { [weak self] in
            // The stream ends whenever the network moves; re-subscribing in a
            // loop is what makes a tunnel change invisible to the user.
            while !Task.isCancelled {
                for await _ in await client.events() {
                    if Task.isCancelled { return }
                    await self?.refresh(gateway: id)
                }
                if Task.isCancelled { return }
                // A dropped stream is the first sign a host went away: probe
                // now rather than at the next backstop tick.
                try? await Task.sleep(for: .seconds(2))
                if Task.isCancelled { return }
                await self?.reprobe()
            }
        }
    }

    // MARK: - Reads

    func refresh() async {
        await reprobe()
    }

    private func refreshAll() async {
        let ids = gateways.map(\.id).filter { outcomes[$0]?.isReachable == true }
        await withTaskGroup(of: Void.self) { group in
            for id in ids { group.addTask { await self.refresh(gateway: id) } }
        }
    }

    private func refresh(gateway id: String) async {
        guard let client = clients[id], !refreshing.contains(id) else { return }
        refreshing.insert(id)
        defer { refreshing.remove(id) }
        do {
            let payload = try await client.sessions()
            guard clients[id] === client, !Task.isCancelled else { return }
            payloads[id] = payload
        } catch DiriClient.Failure.unauthorized {
            guard clients[id] === client else { return }
            outcomes[id] = .unauthorized
            payloads.removeValue(forKey: id)
        } catch {
            guard clients[id] === client, !Task.isCancelled else { return }
            // Keep the last list; the next probe decides whether to fail over.
            return
        }
        rebuild()
    }

    private func rebuild() {
        let ordered = gateways.compactMap { gateway in payloads[gateway.id].map { (gateway, $0) } }
        let merged = Self.merge(ordered)
        sessions = merged.sessions
        projects = merged.projects
        sessionOwner = merged.owner
        layouts = merged.layouts
        reproject()
    }

    struct Merged {
        var sessions: [SessionRecord]
        var projects: [String: Project]
        var owner: [String: String]
        /// Each gateway's Mac sidebar order, by gateway id.
        var layouts: [String: DiriClient.SidebarLayout] = [:]
    }

    /// Joins lists from several gateways, in priority order. Sessions are
    /// tagged with their gateway only when more than one contributes — one
    /// gateway needs no label. An id seen twice belongs to the higher-priority
    /// gateway.
    static func merge(_ lists: [(Gateway, DiriClient.SessionsResponse)]) -> Merged {
        let tagged = lists.count > 1
        var result = Merged(sessions: [], projects: [:], owner: [:])
        for (gateway, payload) in lists {
            for var session in payload.sessions where result.owner[session.id] == nil {
                if tagged { session.gateway = gateway.name }
                result.owner[session.id] = gateway.id
                result.sessions.append(session)
            }
            for project in payload.projects ?? [] where result.projects[project.id] == nil {
                result.projects[project.id] = project
            }
            if let layout = payload.sidebar { result.layouts[gateway.id] = layout }
        }
        return result
    }

    private func loadAgents() async {
        guard let client = liveClient else { agents = []; return }
        let result = (try? await client.agents()) ?? []
        guard liveClient === client, !Task.isCancelled else { return }
        agents = result
    }

    private var liveClient: DiriClient? { liveGatewayID.flatMap { clients[$0] } }

    /// The client for a session's own gateway — writes must reach the daemon
    /// that owns the session, whichever gateway is live.
    func client(for sessionID: String) -> DiriClient? {
        sessionOwner[sessionID].flatMap { clients[$0] } ?? liveClient
    }

    func hosts() async throws -> [DiriClient.Host] {
        guard let client = liveClient else { throw DiriClient.Failure.daemonUnreachable("No paired computer answered.") }
        return try await client.hosts()
    }

    func agents(host: String?) async throws -> [DiriClient.AgentReadiness.Item] {
        guard let client = liveClient else { throw DiriClient.Failure.daemonUnreachable("No paired computer answered.") }
        return try await client.agents(host: host)
    }

    func directories(host: String?, path: String) async throws -> DiriClient.DirectoryListing {
        guard let client = liveClient else { throw DiriClient.Failure.daemonUnreachable("No paired computer answered.") }
        return try await client.directories(host: host, path: path)
    }

    private func reproject() {
        var prefs = SidebarProjection.Preferences()
        prefs.collapsedSessions = collapsedSessions
        prefs.collapsedProjects = collapsedProjects
        prefs.pinnedSessions = Set(sessions.filter(\.pinned).map(\.id))
        // The Mac's drawn order, gateway by gateway in priority order. Ranks
        // from it reproduce the Mac sidebar exactly, whatever it sorts by.
        for gateway in gateways {
            guard let layout = layouts[gateway.id] else { continue }
            prefs.projectOrder += layout.projectOrder
            prefs.sessionOrder += layout.sessionOrder
            prefs.pinnedSessions.formUnion(layout.pinnedSessions)
            prefs.pinnedProjects.formUnion(layout.pinnedProjects)
        }
        orderPrefs = prefs
        groups = SidebarProjection.build(
            sessions: sessions.filter { !$0.isArchived },
            projects: projects,
            prefs: prefs
        )
        sections = Self.section(groups, owner: sessionOwner, gateways: gateways)
    }

    private var orderPrefs = SidebarProjection.Preferences()

    /// One computer's projects with nothing folded away: what a reorder
    /// shows, since a folded row still has a place in the order.
    func unfoldedGroups(gatewayID: String) -> [SidebarGroup] {
        var prefs = orderPrefs
        prefs.collapsedSessions = []
        prefs.collapsedProjects = []
        return SidebarProjection.build(
            sessions: sessions.filter { !$0.isArchived && sessionOwner[$0.id] == gatewayID },
            projects: projects,
            prefs: prefs
        )
    }

    /// One section per contributing computer, in priority order. Only the
    /// label depends on how many there are: one computer needs no header.
    static func section(_ groups: [SidebarGroup], owner: [String: String], gateways: [Gateway]) -> [SidebarSection] {
        var byGateway: [String: [SidebarGroup]] = [:]
        for group in groups {
            let first = group.rows.first?.session ?? group.archived.first
            let gateway = first.flatMap { owner[$0.id] } ?? gateways.first?.id ?? ""
            byGateway[gateway, default: []].append(group)
        }
        let contributing = gateways.filter { byGateway[$0.id] != nil }
        return contributing.map { gateway in
            SidebarSection(
                gatewayID: gateway.id,
                name: contributing.count > 1 ? gateway.name : nil,
                groups: byGateway[gateway.id] ?? []
            )
        }
    }

    func session(_ id: String) -> SessionRecord? {
        sessions.first { $0.id == id }
    }

    func computerName(of session: SessionRecord) -> String {
        if let host = session.host, !host.isEmpty { return host }
        if let gateway = session.gateway, !gateway.isEmpty { return gateway }
        return host.isEmpty ? "This Mac" : host
    }

    /// Every visible session in drawn order, for ⌘[ and ⌘].
    var orderedSessionIDs: [String] { visibleRows.map(\.session.id) }

    /// Every drawn row, top to bottom.
    var visibleRows: [SidebarRow] { sections.flatMap { $0.groups.flatMap(\.rows) } }

    func neighbour(of id: String?, step: Int) -> String? {
        let ids = orderedSessionIDs
        guard !ids.isEmpty else { return nil }
        guard let id, let index = ids.firstIndex(of: id) else { return ids.first }
        return ids[(index + step + ids.count) % ids.count]
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

    /// Whether the Mac that owns `gatewayID` sorts its sidebar by hand. When
    /// it sorts by activity there is no order to move a row within.
    func canReorder(gatewayID: String) -> Bool {
        layouts[gatewayID]?.reorderable == true
    }

    /// Moves projects within one computer's section, as a drag on the Mac
    /// would. `order` is that section's project ids in their new order.
    func moveProject(_ id: String, in order: [String], gatewayID: String) async throws {
        try await move("project", id, in: order, gatewayID: gatewayID)
    }

    /// Moves a session within its sibling run (same project, same parent).
    func moveSession(_ id: String, in siblings: [String], gatewayID: String) async throws {
        try await move("session", id, in: siblings, gatewayID: gatewayID)
    }

    /// Sends one move, named by the row it now sits before (or after, at the
    /// end): the Mac applies it to its saved order the way its own drag
    /// does, so rows this phone did not touch keep the Mac's latest order.
    private func move(_ row: String, _ id: String, in order: [String], gatewayID: String) async throws {
        guard let client = clients[gatewayID] else { return }
        let place = Self.placement(of: id, in: order)
        do {
            try await client.moveSidebarRow(row, id: id, before: place.before, after: place.after)
        } catch {
            // A refusal means this phone's layout is stale; show the Mac's.
            await refresh(gateway: gatewayID)
            throw error
        }
        await refresh(gateway: gatewayID)
    }

    /// Where `moved` sits in `order`, by its neighbour: the row after it,
    /// or, when it is last, the row before it. Neither when it is alone.
    nonisolated static func placement(of moved: String, in order: [String]) -> (before: String?, after: String?) {
        guard let index = order.firstIndex(of: moved) else { return (nil, nil) }
        if index + 1 < order.count { return (order[index + 1], nil) }
        if index > 0 { return (nil, order[index - 1]) }
        return (nil, nil)
    }

    private func requireClient(_ id: String) throws -> DiriClient {
        guard let client = client(for: id) else {
            throw DiriClient.Failure.daemonUnreachable("Connect to your Mac first.")
        }
        return client
    }

    func screen(for id: String) async throws -> DiriClient.Screen {
        try await requireClient(id).screen(sessionID: id)
    }

    func diff(for id: String) async throws -> DiriClient.Diff {
        try await requireClient(id).diff(sessionID: id)
    }

    func send(_ text: String, to id: String) async throws {
        try await requireClient(id).send(text: text, to: id)
    }

    func send(_ key: TerminalKey, to id: String) async throws {
        try await requireClient(id).send(key: key, to: id)
    }

    func markSeen(_ id: String) async {
        guard let client = client(for: id) else { return }
        try? await client.markSeen(sessionID: id)
        if let owner = sessionOwner[id] { await refresh(gateway: owner) }
    }

    func kill(_ id: String) async throws {
        guard let client = client(for: id) else { return }
        try await client.kill(sessionID: id)
        if let owner = sessionOwner[id] { await refresh(gateway: owner) }
    }

    func spawn(kind: String, cwd: String, prompt: String?, host: String? = nil,
               worktree: Bool = false, branch: String? = nil, base: String? = nil) async throws -> SessionRecord {
        guard let client = liveClient, let live = liveGatewayID else {
            throw DiriClient.Failure.daemonUnreachable("No paired computer answered.")
        }
        let record = try await client.spawn(kind: kind, cwd: cwd, prompt: prompt,
            host: host, worktree: worktree, branch: branch, base: base)
        sessionOwner[record.id] = live
        await refresh(gateway: live)
        return record
    }

    /// Starts a note on the live gateway's Mac. Notes live in that Mac's
    /// notes store, so a note always belongs to a local project folder.
    func spawnNote(title: String, cwd: String) async throws -> SessionRecord {
        guard let client = liveClient, let live = liveGatewayID else {
            throw DiriClient.Failure.daemonUnreachable("No paired computer answered.")
        }
        let record = try await client.spawn(kind: AgentKind.note.id, cwd: cwd, prompt: nil, title: title)
        sessionOwner[record.id] = live
        await refresh(gateway: live)
        return record
    }

    /// The live gateway's own projects. A note is spawned there, so only its
    /// projects can hold one; another Mac's paths mean nothing to it.
    var liveProjects: [Project] {
        guard let live = liveGatewayID else { return [] }
        return payloads[live]?.projects ?? []
    }

    /// Whether `sessionID` belongs to the live gateway's Mac.
    func isOnLiveGateway(_ sessionID: String) -> Bool {
        liveGatewayID != nil && sessionOwner[sessionID] == liveGatewayID
    }

    /// Directories that already have sessions on the live gateway — almost
    /// always where the next one belongs.
    var recentDirectories: [String] {
        var seen: Set<String> = []
        var result: [String] = []
        let live = sessions.filter { sessionOwner[$0.id] == liveGatewayID }
        for session in live.sorted(by: { $0.updatedAt > $1.updatedAt }) {
            if seen.insert(session.cwd).inserted { result.append(session.cwd) }
            if result.count == 8 { break }
        }
        return result
    }
}

struct SidebarSection: Identifiable, Hashable {
    var gatewayID: String
    /// Set only when sessions come from more than one computer.
    var name: String?
    var groups: [SidebarGroup]
    var id: String { gatewayID }
}
