import SwiftUI

/// Drag projects, or sessions among their siblings, into a new order. Each
/// drop is sent to the Mac at once, exactly as a drag in its sidebar would
/// be, so the two sidebars never disagree for longer than one round trip.
struct ReorderSheet: View {
    let gatewayID: String

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var mode = Mode.projects
    @State private var projects: [SidebarGroup] = []
    @State private var runs: [Run] = []
    @State private var error: String?

    enum Mode: String, CaseIterable, Identifiable {
        case projects = "Projects"
        case sessions = "Sessions"
        var id: String { rawValue }
    }

    /// Sessions that share a parent within one project: the only rows a drag
    /// can move between, on the Mac as here.
    struct Run: Identifiable {
        var id: String
        var title: String
        var sessions: [SessionRecord]
    }

    var body: some View {
        NavigationStack {
            List {
                switch mode {
                case .projects:
                    Section {
                        ForEach(projects) { group in
                            Label(group.project.displayName, systemImage: group.pinned ? "pin.fill" : "folder")
                                .foregroundStyle(Tokens.Ink.primary)
                        }
                        .onMove { from, to in
                            projects.move(fromOffsets: from, toOffset: to)
                            commit { try await model.reorderProjects(projects.map(\.id), gatewayID: gatewayID) }
                        }
                    } footer: {
                        Text("Pinned projects stay on top.")
                    }
                case .sessions:
                    if runs.isEmpty {
                        Text("No project has more than one session to reorder.")
                            .foregroundStyle(Tokens.Ink.tertiary)
                    }
                    ForEach($runs) { $run in
                        Section(run.title) {
                            ForEach(run.sessions) { session in
                                HStack(spacing: 10) {
                                    StatusGlyphView(kind: session.effectiveKind, state: session.statusState)
                                        .frame(width: Tokens.Metrics.glyph, height: Tokens.Metrics.glyph)
                                    Text(Self.title(session))
                                        .foregroundStyle(Tokens.Ink.primary)
                                        .lineLimit(1)
                                }
                            }
                            .onMove { from, to in
                                run.sessions.move(fromOffsets: from, toOffset: to)
                                let order = run.sessions.map(\.id)
                                commit { try await model.reorderSessions(order, gatewayID: gatewayID) }
                            }
                        }
                    }
                }
            }
            .environment(\.editMode, .constant(.active))
            .scrollContentBackground(.hidden)
            .background(Tokens.Ink.background)
            .safeAreaInset(edge: .top) {
                Picker("Reorder", selection: $mode) {
                    ForEach(Mode.allCases) { Text($0.rawValue).tag($0) }
                }
                .pickerStyle(.segmented)
                .padding(.horizontal, 16)
                .padding(.vertical, 8)
            }
            .navigationTitle("Reorder")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
            }
            .alert("Couldn't reorder", isPresented: .init(get: { error != nil }, set: { if !$0 { error = nil } })) {
                Button("OK", role: .cancel) { error = nil }
            } message: { Text(error ?? "") }
            .onAppear(perform: load)
        }
    }

    private func load() {
        let groups = model.unfoldedGroups(gatewayID: gatewayID)
        projects = groups
        runs = groups.flatMap { Self.runs(in: $0) }
    }

    private func commit(_ write: @escaping @MainActor () async throws -> Void) {
        Task {
            do { try await write() } catch {
                self.error = error.localizedDescription
                load()
            }
        }
    }

    /// Splits a project's tree-ordered rows into sibling runs. A row's parent
    /// is the nearest row above it one level shallower.
    nonisolated static func runs(in group: SidebarGroup) -> [Run] {
        var order: [String] = []
        var members: [String: [SessionRecord]] = [:]
        var titles: [String: String] = [:]
        var ancestors: [SidebarRow] = []
        for row in group.rows {
            ancestors.removeLast(max(0, ancestors.count - row.depth))
            let parent = ancestors.last
            let key = "\(group.id)/\(parent?.session.id ?? "")"
            if members[key] == nil {
                order.append(key)
                titles[key] = parent.map { "\(group.project.displayName) · under \(title($0.session))" }
                    ?? group.project.displayName
            }
            members[key, default: []].append(row.session)
            ancestors.append(row)
        }
        return order.compactMap { key in
            guard let sessions = members[key], sessions.count > 1 else { return nil }
            return Run(id: key, title: titles[key] ?? "", sessions: sessions)
        }
    }

    nonisolated static func title(_ session: SessionRecord) -> String {
        let trimmed = session.title.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? (session.cwd as NSString).lastPathComponent : trimmed
    }
}
