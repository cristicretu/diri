import SwiftUI

/// The app's frame. On a narrow screen the open session fills it and the
/// session list is a drawer pulled from the left edge; on iPad and in a wide
/// landscape window the list is a sidebar beside it.
struct RootView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var spawning = false
    @State private var noting = false
    @State private var pairing = false
    @State private var switcher = false
    @State private var drawerOpen = false
    @State private var drawerDrag: CGFloat = 0
    @State private var splitVisibility = NavigationSplitViewVisibility.all

    var body: some View {
        GeometryReader { geometry in
            Group {
                if geometry.size.width >= 700 {
                    split
                } else {
                    compact(width: geometry.size.width)
                }
            }
        }
        .background(Tokens.Ink.background.ignoresSafeArea())
        .tint(Tokens.Ink.primary)
        .sheet(isPresented: $spawning) {
            NewSessionSheet { record in select(record.id) }
        }
        .sheet(isPresented: $noting) {
            NewNoteSheet { record in select(record.id) }
        }
        .sheet(isPresented: $pairing) {
            ConnectView(onConnected: { pairing = false; model.selectedSessionID = nil })
        }
        .sheet(isPresented: $switcher) {
            SessionSwitcher { id in
                switcher = false
                select(id)
            }
            .presentationDetents([.medium, .large])
            .presentationBackground(.thinMaterial)
        }
        .background(shortcuts)
        .onAppear {
            if let initial = model.initialSessionID, model.selectedSessionID == nil { model.selectedSessionID = initial }
        }
    }

    private func select(_ id: String) {
        model.selectedSessionID = id
        withAnimation(reduceMotion ? nil : .snappy(duration: 0.28)) { drawerOpen = false }
    }

    private func command(_ command: TerminalView.Command) {
        switch command {
        case .switcher: switcher = true
        case .previous: if let id = model.neighbour(of: model.selectedSessionID, step: -1) { select(id) }
        case .next: if let id = model.neighbour(of: model.selectedSessionID, step: 1) { select(id) }
        case .dismissKeyboard: break
        }
    }

    /// Hardware-keyboard shortcuts that work whether or not the terminal has
    /// the keyboard.
    private var shortcuts: some View {
        ZStack {
            Button("") { command(.switcher) }.keyboardShortcut("k", modifiers: .command)
            Button("") { command(.previous) }.keyboardShortcut("[", modifiers: .command)
            Button("") { command(.next) }.keyboardShortcut("]", modifiers: .command)
            Button("") { spawning = true }.keyboardShortcut("n", modifiers: .command)
        }
        .opacity(0)
        .accessibilityHidden(true)
    }

    // MARK: - Wide

    private var split: some View {
        NavigationSplitView(columnVisibility: $splitVisibility) {
            list
                .navigationSplitViewColumnWidth(min: 260, ideal: 300, max: 360)
        } detail: {
            NavigationStack {
                if let id = model.selectedSessionID {
                    SessionScreen(sessionID: id, showsSidebarButton: false, onCommand: command)
                        .id(id)
                } else {
                    Text("Choose a session")
                        .font(.body)
                        .foregroundStyle(Tokens.Ink.tertiary)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                        .background(Tokens.Ink.background)
                }
            }
        }
        .navigationSplitViewStyle(.balanced)
    }

    private var list: some View {
        SessionListView(
            selection: model.selectedSessionID,
            onSelect: select,
            onNew: { spawning = true },
            onNewNote: { noting = true },
            onPair: { pairing = true }
        )
        .background(Tokens.Ink.background)
    }

    // MARK: - Narrow

    private func compact(width: CGFloat) -> some View {
        let drawerWidth = min(340, width * 0.86)
        let revealed = drawerOpen ? max(0, drawerWidth + min(0, drawerDrag)) : min(drawerWidth, max(0, drawerDrag))
        let fraction = drawerWidth > 0 ? revealed / drawerWidth : 0

        return ZStack(alignment: .leading) {
            NavigationStack {
                if let id = model.selectedSessionID {
                    SessionScreen(
                        sessionID: id,
                        showsSidebarButton: true,
                        onShowSidebar: { openDrawer() },
                        onCommand: command
                    )
                    .id(id)
                } else {
                    list
                }
            }
            .overlay {
                Color.black.opacity(0.45 * fraction)
                    .ignoresSafeArea()
                    .allowsHitTesting(drawerOpen)
                    .onTapGesture { closeDrawer() }
            }

            if revealed > 0 || drawerOpen {
                NavigationStack { list }
                    .frame(width: drawerWidth)
                    .clipShape(RoundedRectangle(cornerRadius: 34, style: .continuous))
                    .glass(in: RoundedRectangle(cornerRadius: 34, style: .continuous), tint: Tokens.Ink.background.opacity(0.55))
                    .padding(.vertical, 6)
                    .padding(.leading, 6)
                    .offset(x: revealed - drawerWidth - 6)
                    .gesture(closeGesture(drawerWidth: drawerWidth))
                    .accessibilityIdentifier("session-drawer")
            }

            // The left edge pulls the drawer out.
            if model.selectedSessionID != nil, !drawerOpen {
                Color.clear
                    .frame(width: 18)
                    .contentShape(Rectangle())
                    .gesture(openGesture(drawerWidth: drawerWidth))
                    .ignoresSafeArea()
            }
        }
    }

    private func openDrawer() {
        withAnimation(reduceMotion ? nil : .snappy(duration: 0.3)) { drawerOpen = true }
    }

    private func closeDrawer() {
        withAnimation(reduceMotion ? nil : .snappy(duration: 0.28)) { drawerOpen = false }
    }

    private func openGesture(drawerWidth: CGFloat) -> some Gesture {
        DragGesture(minimumDistance: 8, coordinateSpace: .global)
            .onChanged { value in drawerDrag = max(0, value.translation.width) }
            .onEnded { value in
                let open = value.translation.width + value.velocity.width * 0.2 > drawerWidth * 0.4
                withAnimation(reduceMotion ? nil : .snappy(duration: 0.28)) {
                    drawerOpen = open
                    drawerDrag = 0
                }
            }
    }

    private func closeGesture(drawerWidth: CGFloat) -> some Gesture {
        DragGesture(minimumDistance: 12, coordinateSpace: .global)
            .onChanged { value in
                if abs(value.translation.width) > abs(value.translation.height) {
                    drawerDrag = min(0, value.translation.width)
                }
            }
            .onEnded { value in
                let close = value.translation.width + value.velocity.width * 0.2 < -drawerWidth * 0.35
                withAnimation(reduceMotion ? nil : .snappy(duration: 0.28)) {
                    if close { drawerOpen = false }
                    drawerDrag = 0
                }
            }
    }
}

/// ⌘K: type to filter, return to open the first match.
struct SessionSwitcher: View {
    let onPick: (String) -> Void
    @Environment(AppModel.self) private var model
    @State private var query = ""
    @FocusState private var focused: Bool

    private var matches: [SidebarRow] {
        let rows = model.visibleRows
        let needle = query.trimmingCharacters(in: .whitespaces).lowercased()
        guard !needle.isEmpty else { return rows }
        return rows.filter { row in
            let s = row.session
            return [s.title, s.cwd, s.gitBranch ?? "", model.computerName(of: s)]
                .contains { $0.lowercased().contains(needle) }
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Image(systemName: "magnifyingglass").foregroundStyle(Tokens.Ink.tertiary)
                TextField("Switch to session", text: $query)
                    .focused($focused)
                    .submitLabel(.go)
                    .onSubmit { if let first = matches.first { onPick(first.session.id) } }
                    .accessibilityIdentifier("switcher-field")
            }
            .padding(.horizontal, 14)
            .frame(height: 44)
            .glass(in: Capsule())
            .padding(.horizontal, 16)
            .padding(.top, 18)
            .padding(.bottom, 8)

            List(matches) { row in
                Button { onPick(row.session.id) } label: {
                    SessionRowView(row: row, selected: row.session.id == model.selectedSessionID) {}
                }
                .buttonStyle(RowButtonStyle())
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 0, leading: 10, bottom: 0, trailing: 10))
            }
            .listStyle(.plain)
            .scrollContentBackground(.hidden)
        }
        .onAppear { focused = true }
    }
}
