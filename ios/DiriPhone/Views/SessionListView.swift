import SwiftUI

/// Every session, grouped by the computer it runs on. The same list is the
/// home screen, the drawer over a terminal, and the iPad sidebar.
struct SessionListView: View {
    let selection: String?
    let onSelect: (String) -> Void
    let onNew: () -> Void
    let onPair: () -> Void

    @Environment(AppModel.self) private var model
    @State private var actionError: String?
    @State private var confirmingKill: SessionRecord?
    @State private var answerError: String?

    var body: some View {
        Group {
            if model.groups.isEmpty {
                EmptyStateView(connection: model.connection, onNew: onNew)
            } else {
                list
            }
        }
        .navigationTitle("diri")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar { toolbar }
        .alert("Answer not sent", isPresented: .init(
            get: { answerError != nil }, set: { if !$0 { answerError = nil } }
        )) { Button("OK", role: .cancel) { answerError = nil } }
        message: { Text(answerError ?? "") }
        .alert("Couldn't stop the session", isPresented: .init(
            get: { actionError != nil }, set: { if !$0 { actionError = nil } }
        )) { Button("OK", role: .cancel) { actionError = nil } }
        message: { Text(actionError ?? "") }
        .confirmationDialog(
            confirmingKill.map { "Kill “\(displayTitle($0))”?" } ?? "",
            isPresented: .init(get: { confirmingKill != nil }, set: { if !$0 { confirmingKill = nil } }),
            titleVisibility: .visible
        ) {
            Button("Kill session", role: .destructive) {
                guard let target = confirmingKill else { return }
                confirmingKill = nil
                Task {
                    do { try await model.kill(target.id) } catch { actionError = error.localizedDescription }
                }
            }
            Button("Cancel", role: .cancel) { confirmingKill = nil }
        }
    }

    private var list: some View {
        List {
            if !model.waiting.isEmpty {
                Section {
                    ForEach(model.waiting) { session in
                        WaitingCard(session: session, computer: model.computerName(of: session)) {
                            onSelect(session.id)
                        } onAnswer: { approve in
                            Task {
                                do { try await model.answer(session, approve: approve) } catch {
                                    answerError = Self.describe(error)
                                }
                            }
                        }
                        .listRowSeparator(.hidden)
                        .listRowBackground(Color.clear)
                        .listRowInsets(EdgeInsets(top: 4, leading: 12, bottom: 4, trailing: 12))
                    }
                } header: {
                    Text(model.waiting.count == 1 ? "Waiting for you" : "\(model.waiting.count) waiting for you")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(Tokens.Ink.attention)
                        .textCase(nil)
                        .padding(.leading, 10)
                        .accessibilityAddTraits(.isHeader)
                        .accessibilityIdentifier("waiting-header")
                }
            }
            ForEach(model.computers) { computer in
                Section {
                    ForEach(computer.rows) { row in rowButton(row) }
                } header: {
                    Text(computer.name)
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(Tokens.Ink.tertiary)
                        .textCase(nil)
                        .padding(.leading, 10)
                        .accessibilityAddTraits(.isHeader)
                }
                .listRowSeparator(.hidden)
                .listRowBackground(Color.clear)
                .listRowInsets(EdgeInsets(top: 0, leading: 8, bottom: 0, trailing: 8))
            }
        }
        .listStyle(.plain)
        .listSectionSpacing(8)
        .scrollContentBackground(.hidden)
        .refreshable { await model.refresh() }
    }

    private func rowButton(_ row: SidebarRow) -> some View {
        Button {
            onSelect(row.session.id)
        } label: {
            SessionRowView(row: row, selected: selection == row.session.id) {
                model.toggleCollapsed(row.session.id)
            }
        }
        .buttonStyle(RowButtonStyle())
        .listRowSeparator(.hidden)
        .listRowBackground(Color.clear)
        .listRowInsets(EdgeInsets(top: 0, leading: 8, bottom: 0, trailing: 8))
        .swipeActions(edge: .trailing, allowsFullSwipe: false) {
            Button(role: .destructive) { confirmingKill = row.session } label: { Label("Kill", systemImage: "xmark") }
        }
        .swipeActions(edge: .leading, allowsFullSwipe: true) {
            Button { Task { await model.markSeen(row.session.id) } } label: { Label("Seen", systemImage: "checkmark") }
                .tint(Tokens.Ink.fresh)
        }
        .contextMenu {
            Button { Task { await model.markSeen(row.session.id) } } label: {
                Label("Mark seen", systemImage: "checkmark.circle")
            }
            if row.hasChildren {
                Button { model.toggleCollapsed(row.session.id) } label: {
                    Label(row.collapsed ? "Expand" : "Collapse", systemImage: "chevron.right")
                }
            }
            Divider()
            Button(role: .destructive) { confirmingKill = row.session } label: {
                Label("Kill session", systemImage: "xmark.circle")
            }
        }
    }

    @ToolbarContentBuilder
    private var toolbar: some ToolbarContent {
        ToolbarItem(placement: .topBarLeading) {
            Menu {
                Section("Computers, in priority order") {
                    ForEach(model.gateways) { gateway in
                        Menu {
                            if gateway.id != model.gateways.first?.id {
                                Button("Prefer this computer", systemImage: "arrow.up") { model.preferGateway(gateway.id) }
                            }
                            Button("Forget this computer", systemImage: "trash", role: .destructive) {
                                model.removeGateway(gateway.id)
                            }
                        } label: {
                            Label(gateway.name, systemImage: gatewaySymbol(gateway))
                            Text(gatewayStatus(gateway))
                        }
                        .accessibilityIdentifier("gateway-\(gateway.name)")
                    }
                }
                Button("Retry connection", systemImage: "arrow.clockwise") { Task { await model.refresh() } }
                Button("Add a computer…", systemImage: "plus", action: onPair)
                    .accessibilityIdentifier("add-gateway")
            } label: {
                ConnectionDot(connection: model.connection, host: model.host)
                    .padding(.horizontal, 4)
            }
            .accessibilityLabel("Mac connection options")
            .accessibilityIdentifier("connection-menu")
        }
        ToolbarItemGroup(placement: .topBarTrailing) {
            if let next = model.needingInput.first {
                Button { onSelect(next.id) } label: {
                    Image(systemName: "bell.badge").foregroundStyle(Tokens.Ink.attention)
                }
                .accessibilityLabel("\(model.needingInput.count) waiting for input")
            }
            Button(action: onNew) { Image(systemName: "plus") }
                .accessibilityLabel("New session")
                .accessibilityIdentifier("new-session")
        }
    }

    private func gatewaySymbol(_ gateway: Gateway) -> String {
        if gateway.id == model.liveGatewayID { return "checkmark.circle.fill" }
        return model.outcomes[gateway.id]?.isReachable == true ? "circle.fill" : "circle.dashed"
    }

    private func gatewayStatus(_ gateway: Gateway) -> String {
        if gateway.id == model.liveGatewayID { return "Live" }
        switch model.outcomes[gateway.id] {
        case .reachable: return "Reachable"
        case .unauthorized: return "Pairing expired — pair again"
        case let .unreachable(detail): return "Unreachable · \(detail)"
        case nil: return "Checking…"
        }
    }

    /// A refused answer says why in words: the gateway's own message.
    static func describe(_ error: Error) -> String {
        if case let DiriClient.Failure.http(code, message) = error, code == 409 || code == 422 {
            return message.isEmpty ? "The agent is no longer waiting on that." : message
        }
        return error.localizedDescription
    }

    private func displayTitle(_ session: SessionRecord) -> String {
        let trimmed = session.title.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? session.id : trimmed
    }
}

/// Press feedback standing in for the desktop's hover fill.
struct RowButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .background(
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .fill(configuration.isPressed ? RowFill.pressed.color : RowFill.clear.color)
            )
            .contentShape(.rect)
    }
}

struct ConnectionDot: View {
    let connection: AppModel.Connection
    let host: String

    var body: some View {
        HStack(spacing: 6) {
            Circle().fill(tint).frame(width: 7, height: 7)
            if case let .online(host) = connection, !host.isEmpty {
                Text(host)
                    .font(.system(size: 13, weight: .medium))
                    .foregroundStyle(Tokens.Ink.secondary)
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
        VStack(spacing: 10) {
            Spacer()
            switch connection {
            case let .offline(detail):
                Image(systemName: "wifi.exclamationmark")
                    .font(.system(size: 24))
                    .foregroundStyle(Tokens.Ink.danger)
                Text("Can’t reach your Mac")
                    .font(.system(size: 17, weight: .semibold))
                    .foregroundStyle(Tokens.Ink.primary)
                Text(detail)
                    .font(.system(size: 13))
                    .foregroundStyle(Tokens.Ink.tertiary)
                    .multilineTextAlignment(.center)
                Text("Keep your Mac awake with Diri open and Tailscale connected. If access was restarted, use the connection menu to pair again.")
                    .font(.system(size: 13))
                    .foregroundStyle(Tokens.Ink.tertiary)
                    .multilineTextAlignment(.center)
            case .connecting:
                ProgressView().tint(Tokens.Ink.secondary)
            default:
                Text("No sessions")
                    .font(.system(size: 17, weight: .semibold))
                    .foregroundStyle(Tokens.Ink.secondary)
                Button("New session", action: onNew)
                    .font(.system(size: 15, weight: .medium))
                    .foregroundStyle(Tokens.Ink.clay)
            }
            Spacer()
        }
        .frame(maxWidth: .infinity)
        .padding(24)
    }
}
