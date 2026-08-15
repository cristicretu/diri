import SwiftUI

/// The session list — the desktop sidebar, on a phone.
///
/// Grouping, ordering and lineage all come from `SidebarProjection`, which is a
/// transcription of the desktop's projection. What this file owns is only the
/// touch layer: swipe actions, long-press menus, and the jump-to-attention
/// affordance that a desktop gets from ⌘⇧A instead.
struct SidebarView: View {
    @Environment(AppModel.self) private var model
    @State private var spawning = false
    @State private var confirmingKill: SessionRecord?
    @State private var path: [String] = []

    var body: some View {
        @Bindable var model = model

        NavigationStack(path: $path) {
            Group {
                if model.groups.isEmpty {
                    EmptyStateView(connection: model.connection) { spawning = true }
                } else {
                    list
                }
            }
            .background(Tokens.Ink.background)
            .navigationTitle("diri")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { toolbar }
            .toolbarBackground(.visible, for: .navigationBar)
            .toolbarBackground(Tokens.Ink.background, for: .navigationBar)
            .navigationDestination(for: String.self) { id in
                SessionDetailView(sessionID: id)
            }
            .onAppear {
                if let initial = model.initialSessionID, path.isEmpty {
                    path.append(initial)
                }
            }
        }
        .tint(Tokens.Ink.clay)
        .sheet(isPresented: $spawning) {
            NewSessionSheet { record in
                path.append(record.id)
            }
        }
        .confirmationDialog(
            confirmingKill.map { "Kill “\(displayTitle($0))”?" } ?? "",
            isPresented: .init(
                get: { confirmingKill != nil },
                set: { if !$0 { confirmingKill = nil } }
            ),
            titleVisibility: .visible
        ) {
            Button("Kill session", role: .destructive) {
                guard let target = confirmingKill else { return }
                confirmingKill = nil
                Task { try? await model.kill(target.id) }
            }
            Button("Cancel", role: .cancel) { confirmingKill = nil }
        }
    }

    private var list: some View {
        List {
            ForEach(model.groups) { group in
                Section {
                    if !model.collapsedProjects.contains(group.project.id) {
                        ForEach(group.rows) { row in
                            rowLink(row)
                        }
                    }
                } header: {
                    ProjectHeader(
                        group: group,
                        collapsed: model.collapsedProjects.contains(group.project.id)
                    ) {
                        withAnimation(Tokens.Motion.snap) {
                            model.toggleProjectCollapsed(group.project.id)
                        }
                    }
                }
                .listRowSeparator(.hidden)
                .listRowBackground(Color.clear)
                .listRowInsets(EdgeInsets(top: 1, leading: 6, bottom: 1, trailing: 6))
            }
        }
        .listStyle(.plain)
        .scrollContentBackground(.hidden)
        .refreshable { await model.refresh() }
        .animation(Tokens.Motion.snap, value: model.groups)
    }

    private func rowLink(_ row: SidebarRow) -> some View {
        // A plain button rather than `NavigationLink`, because the link adds a
        // disclosure chevron to every row. The desktop has no such column, and
        // fifteen chevrons down the right edge is the single most obvious way
        // this list would stop looking like diri's.
        Button {
            model.selectedSessionID = row.session.id
            path.append(row.session.id)
        } label: {
            SessionRowView(
                row: row,
                selected: model.selectedSessionID == row.session.id
            ) {
                withAnimation(Tokens.Motion.snap) {
                    model.toggleCollapsed(row.session.id)
                }
            }
        }
        .buttonStyle(RowButtonStyle())
        .listRowSeparator(.hidden)
        .listRowBackground(Color.clear)
        .listRowInsets(EdgeInsets(top: 1, leading: 6, bottom: 1, trailing: 6))
        // Swipe is the phone's answer to the desktop's right-click menu: the
        // two things worth reaching for without opening the session.
        .swipeActions(edge: .trailing, allowsFullSwipe: false) {
            Button(role: .destructive) {
                confirmingKill = row.session
            } label: {
                Label("Kill", systemImage: "xmark")
            }
        }
        .swipeActions(edge: .leading, allowsFullSwipe: true) {
            Button {
                Task { await model.markSeen(row.session.id) }
            } label: {
                Label("Seen", systemImage: "checkmark")
            }
            .tint(Tokens.Ink.fresh)
        }
        .contextMenu {
            Button {
                Task { await model.markSeen(row.session.id) }
            } label: {
                Label("Mark seen", systemImage: "checkmark.circle")
            }
            if row.hasChildren {
                Button {
                    model.toggleCollapsed(row.session.id)
                } label: {
                    Label(row.collapsed ? "Expand" : "Collapse", systemImage: "chevron.right")
                }
            }
            Divider()
            Button(role: .destructive) {
                confirmingKill = row.session
            } label: {
                Label("Kill session", systemImage: "xmark.circle")
            }
        }
    }

    @ToolbarContentBuilder
    private var toolbar: some ToolbarContent {
        ToolbarItem(placement: .topBarLeading) {
            ConnectionDot(connection: model.connection, host: model.host)
        }
        ToolbarItemGroup(placement: .topBarTrailing) {
            if !model.needingInput.isEmpty {
                Button {
                    // Jump straight to whoever is waiting — the whole reason to
                    // pick the phone up.
                    if let next = model.needingInput.first { path.append(next.id) }
                } label: {
                    Image(systemName: "bell.badge.fill")
                        .foregroundStyle(Tokens.Ink.attention)
                }
                .accessibilityLabel("\(model.needingInput.count) waiting for input")
            }
            Button { spawning = true } label: { Image(systemName: "plus") }
                .accessibilityLabel("New session")
        }
    }

    private func displayTitle(_ session: SessionRecord) -> String {
        let trimmed = session.title.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? session.id : trimmed
    }
}

/// Press feedback standing in for the desktop's hover fill — same opacity, so
/// a finger-down reads the way a cursor-over does.
struct RowButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .background(
                RoundedRectangle(cornerRadius: Tokens.Radius.row, style: .continuous)
                    .fill(configuration.isPressed ? RowFill.pressed.color : RowFill.clear.color)
            )
            .contentShape(.rect)
    }
}

/// A project's header, matching the desktop's section header type.
struct ProjectHeader: View {
    let group: SidebarGroup
    let collapsed: Bool
    let onToggle: () -> Void

    var body: some View {
        Button(action: onToggle) {
            HStack(spacing: 6) {
                Image(systemName: "chevron.right")
                    .font(.system(size: 9, weight: .semibold))
                    .rotationEffect(.degrees(collapsed ? 0 : 90))
                    .foregroundStyle(Tokens.Ink.tertiary)
                Text(group.project.displayName)
                    .font(Tokens.Typo.sectionHeader)
                    .foregroundStyle(Tokens.Ink.secondary)
                    .textCase(nil)
                if group.pinned {
                    Image(systemName: "pin.fill")
                        .font(.system(size: 8))
                        .foregroundStyle(Tokens.Ink.tertiary)
                }
                Spacer(minLength: 0)
                Text("\(group.rows.count)")
                    .font(Tokens.Typo.metaMono)
                    .foregroundStyle(Tokens.Ink.tertiary)
            }
            .padding(.horizontal, Tokens.Space.rowH)
            .frame(height: Tokens.Metrics.sectionHeader)
            .contentShape(.rect)
        }
        .buttonStyle(.plain)
    }
}

struct ConnectionDot: View {
    let connection: AppModel.Connection
    let host: String

    var body: some View {
        HStack(spacing: 5) {
            Circle()
                .fill(tint)
                .frame(width: 7, height: 7)
            // Only the host name earns its width here. Every other state is
            // already said by the dot's colour, and spelling it out just
            // truncates to nonsense in a toolbar this narrow.
            if case let .online(host) = connection, !host.isEmpty {
                Text(host)
                    .font(Tokens.Typo.meta)
                    .foregroundStyle(Tokens.Ink.tertiary)
                    .lineLimit(1)
                    .fixedSize()
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Connection: \(label)")
    }

    private var tint: Color {
        switch connection {
        case .online: Tokens.Ink.fresh
        case .connecting: Tokens.Ink.attention
        case .offline: Tokens.Ink.danger
        case .unconfigured: Tokens.Ink.tertiary
        }
    }

    private var label: String {
        switch connection {
        case let .online(host): host
        case .connecting: "connecting"
        case .offline: "offline"
        case .unconfigured: "not set up"
        }
    }
}

struct EmptyStateView: View {
    let connection: AppModel.Connection
    let onNew: () -> Void

    var body: some View {
        VStack(spacing: 12) {
            Spacer()
            switch connection {
            case let .offline(detail):
                Image(systemName: "wifi.exclamationmark")
                    .font(.system(size: 28))
                    .foregroundStyle(Tokens.Ink.danger)
                Text("Can't reach the daemon")
                    .font(Tokens.Typo.displayTitle)
                    .foregroundStyle(Tokens.Ink.primary)
                Text(detail)
                    .font(Tokens.Typo.meta)
                    .foregroundStyle(Tokens.Ink.tertiary)
                    .multilineTextAlignment(.center)
                Text("Check that Tailscale is on.")
                    .font(Tokens.Typo.meta)
                    .foregroundStyle(Tokens.Ink.tertiary)
            case .connecting:
                ProgressView().tint(Tokens.Ink.secondary)
            default:
                Text("No sessions")
                    .font(Tokens.Typo.displayTitle)
                    .foregroundStyle(Tokens.Ink.secondary)
                Button("New session", action: onNew)
                    .font(Tokens.Typo.rowEmphasized)
                    .foregroundStyle(Tokens.Ink.clay)
            }
            Spacer()
        }
        .frame(maxWidth: .infinity)
        .padding(24)
    }
}
