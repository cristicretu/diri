import SwiftUI

/// One session: its terminal, edge to edge, with glass chrome floating over
/// it — the title bar on top, the composer (or voice) at the bottom.
struct SessionScreen: View {
    let sessionID: String
    var showsSidebarButton: Bool
    var onShowSidebar: () -> Void = {}
    var onCommand: (TerminalView.Command) -> Void = { _ in }

    @Environment(AppModel.self) private var model
    @Environment(\.scenePhase) private var scenePhase
    @State private var draft = ""
    @State private var busy = false
    @State private var error: String?
    @State private var showingChanges = false
    @State private var voiceMode = false
    @State private var typing = false
    @State private var resizeError: String?
    @State private var composerHeight: CGFloat = 64
    @State private var voiceHeight: CGFloat = 0
    @FocusState private var composerFocused: Bool

    private var session: SessionRecord? { model.session(sessionID) }

    var body: some View {
        if let client = model.client(for: sessionID) {
            content(TerminalRegistry.shared.session(for: sessionID, client: client), client: client)
        } else if let terminal = TerminalRegistry.shared.existingSession(sessionID) {
            // The computer stopped answering probes. Keep the screen it was
            // showing, dimmed, while the terminal keeps trying to resume:
            // a spinner here would throw away exactly what you were reading.
            content(terminal, client: terminal.client)
        } else {
            ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity).background(Tokens.Ink.background)
        }
    }

    private func content(_ terminal: TerminalSession, client: DiriClient) -> some View {
        GeometryReader { geometry in
            // The chrome lays out inside the safe area (and so above the
            // keyboard); the terminal and its fades sit behind it as a
            // background, edge to edge, and take no part in that layout.
            VStack(spacing: 8) {
                Spacer(minLength: 0)
                linkBanner(terminal)
                if !voiceMode, let current = session, let detail = current.needsInput, current.attention == .needsInput {
                    NeedsInputBanner(detail: detail) { approve in
                        Task {
                            do { try await model.answer(current, approve: approve) } catch {
                                self.error = SessionListView.describe(error)
                            }
                        }
                    }
                }
                if voiceMode {
                    VoicePanel(sessionID: sessionID, client: client) { voiceMode = false }
                        .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { voiceHeight = $0 }
                        .transition(.move(edge: .bottom).combined(with: .opacity))
                } else if !typing {
                    composer(terminal)
                        .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { composerHeight = $0 }
                }
            }
            .padding(.horizontal, 10)
            .padding(.bottom, typing ? 0 : 6)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background {
                ZStack {
                    TerminalHost(
                        session: terminal,
                        insets: EdgeInsets(
                            top: geometry.safeAreaInsets.top,
                            leading: 0,
                            bottom: geometry.safeAreaInsets.bottom + bottomChrome,
                            trailing: 0
                        ),
                        onCommand: onCommand,
                        onFocus: { focused in typing = focused }
                    )
                    .accessibilityIdentifier("terminal-host")
                    .opacity(terminal.stale ? 0.5 : 1)
                    .animation(.easeOut(duration: 0.25), value: terminal.stale)

                    // Fades the terminal out under the bars so text never
                    // collides with the title or the composer.
                    VStack(spacing: 0) {
                        edgeFade(top: true).frame(height: geometry.safeAreaInsets.top + 10)
                        Spacer()
                        edgeFade(top: false).frame(height: geometry.safeAreaInsets.bottom + bottomChrome + 8)
                    }
                    .allowsHitTesting(false)
                }
                .ignoresSafeArea()
            }
        }
        .background(Tokens.Ink.background.ignoresSafeArea())
        .toolbar { toolbar(terminal) }
        .navigationBarTitleDisplayMode(.inline)
        .toolbarBackground(.hidden, for: .navigationBar)
        .task(id: "\(sessionID)-\(scenePhase != .background)") {
            guard scenePhase != .background else {
                TerminalRegistry.shared.deactivateAll()
                return
            }
            TerminalRegistry.shared.activate(sessionID)
            await model.markSeen(sessionID)
            if CommandLine.arguments.contains("-flingBenchmark") {
                await runBenchmark()
            }
        }
        .sheet(isPresented: $showingChanges) { SessionChangesView(sessionID: sessionID) }
        .alert("Couldn't send", isPresented: .init(get: { error != nil }, set: { if !$0 { error = nil } })) {
            Button("OK", role: .cancel) { error = nil }
        } message: { Text(error ?? "") }
        .alert("Couldn't resize", isPresented: .init(get: { resizeError != nil }, set: { if !$0 { resizeError = nil } })) {
            Button("OK", role: .cancel) { resizeError = nil }
        } message: { Text(resizeError ?? "") }
        .alert("Keys may not have arrived", isPresented: .init(
            get: { terminal.inputFailed }, set: { if !$0 { terminal.inputFailed = false } }
        )) {
            Button("OK", role: .cancel) { terminal.inputFailed = false }
        } message: { Text("Check the screen before typing again.") }
    }

    private var bottomChrome: CGFloat {
        if typing { return 0 }
        return voiceMode ? voiceHeight + 6 : composerHeight + 6
    }

    private func edgeFade(top: Bool) -> some View {
        LinearGradient(
            colors: [Tokens.Ink.background.opacity(0.92), Tokens.Ink.background.opacity(0)],
            startPoint: top ? .top : .bottom, endPoint: top ? .bottom : .top
        )
    }

    // MARK: - Toolbar

    @ToolbarContentBuilder
    private func toolbar(_ terminal: TerminalSession) -> some ToolbarContent {
        if showsSidebarButton {
            ToolbarItem(placement: .topBarLeading) {
                Button(action: onShowSidebar) { Image(systemName: "sidebar.leading") }
                    .accessibilityLabel("Sessions")
                    .accessibilityIdentifier("show-sessions")
            }
        }
        ToolbarItem(placement: .principal) {
            VStack(spacing: 0) {
                Text(session.map(title) ?? "session")
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(Tokens.Ink.primary)
                    .lineLimit(1)
                if let session {
                    HStack(spacing: 5) {
                        StatusGlyphView(kind: session.effectiveKind, state: session.statusState, size: 10)
                        Text(model.computerName(of: session))
                    }
                    .font(.system(size: 11, weight: .medium))
                    .foregroundStyle(Tokens.Ink.tertiary)
                    .lineLimit(1)
                }
            }
            .frame(maxWidth: 220)
        }
        ToolbarItem(placement: .topBarTrailing) {
            Menu {
                Button { showingChanges = true } label: { Label("Review changes", systemImage: "doc.text.magnifyingglass") }
                Divider()
                Button { fitText() } label: { Label("Fit text to width", systemImage: "arrow.left.and.right.text.vertical") }
                Button { fitSession(terminal) } label: { Label("Resize session to this phone", systemImage: "rectangle.portrait.arrowtriangle.2.inward") }
                Divider()
                Button { Task { await model.markSeen(sessionID) } } label: { Label("Mark seen", systemImage: "checkmark.circle") }
            } label: {
                Image(systemName: "ellipsis")
            }
            .accessibilityLabel("Session options")
            .accessibilityIdentifier("session-menu")
        }
    }

    private func fitText() {
        guard let view = TerminalRegistry.shared.existingView(sessionID) else { return }
        view.setFontSize(view.fitFontSize)
    }

    private func fitSession(_ terminal: TerminalSession) {
        guard let view = TerminalRegistry.shared.existingView(sessionID) else { return }
        let grid = view.fittingGrid
        Task {
            do { try await terminal.resize(cols: grid.cols, rows: grid.rows) } catch {
                resizeError = error.localizedDescription
            }
        }
    }

    // MARK: - Composer

    private func composer(_ terminal: TerminalSession) -> some View {
        let empty = draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        return GlassGroup(spacing: 8) {
            HStack(alignment: .bottom, spacing: 8) {
                GlassIconButton(symbol: "keyboard", label: "Type in the terminal", size: 44) {
                    TerminalRegistry.shared.existingView(sessionID)?.becomeFirstResponder()
                }
                .accessibilityIdentifier("terminal-keyboard")

                HStack(alignment: .bottom, spacing: 6) {
                    TextField("Message", text: $draft, axis: .vertical)
                        .lineLimit(1 ... 5)
                        .font(.system(size: 16))
                        .foregroundStyle(Tokens.Ink.primary)
                        .padding(.leading, 16)
                        .padding(.vertical, 11)
                        .focused($composerFocused)
                        .submitLabel(.send)
                        .accessibilityIdentifier("composer-field")

                    if empty {
                        Button { voiceMode = true } label: {
                            Image(systemName: "waveform")
                                .font(.system(size: 16, weight: .semibold))
                                .foregroundStyle(Tokens.Ink.primary.opacity(0.9))
                                .frame(width: 36, height: 36)
                                .contentShape(Circle())
                        }
                        .buttonStyle(.plain)
                        .padding(4)
                        .accessibilityLabel("Voice mode")
                        .accessibilityIdentifier("voice-mode-open")
                    } else {
                        Button(action: submit) {
                            Image(systemName: "arrow.up")
                                .font(.system(size: 15, weight: .bold))
                                .foregroundStyle(Tokens.Ink.background)
                                .frame(width: 36, height: 36)
                                .background(Circle().fill(Tokens.Ink.primary))
                        }
                        .buttonStyle(.plain)
                        .padding(4)
                        .disabled(busy)
                        .accessibilityLabel("Send")
                        .accessibilityIdentifier("composer-send")
                    }
                }
                .glass(in: RoundedRectangle(cornerRadius: 22, style: .continuous), interactive: true)
            }
        }
    }

    private func submit() {
        let text = draft
        guard !busy, !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        busy = true
        Task {
            do {
                try await model.send(text, to: sessionID)
                if draft == text { draft = "" }
            } catch {
                self.error = "\(error.localizedDescription) Your draft is saved. Check the session before resending; delivery may have succeeded."
            }
            busy = false
        }
    }

    @ViewBuilder
    private func linkBanner(_ terminal: TerminalSession) -> some View {
        switch terminal.link {
        // Only once the screen is stale: a quick resume after unlocking
        // should not flash a banner.
        case let .reconnecting(detail) where terminal.stale:
            Text(detail)
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(Tokens.Ink.attention)
                .padding(.horizontal, 12)
                .padding(.vertical, 6)
                .glass(in: Capsule())
                .accessibilityIdentifier("terminal-reconnecting")
        default:
            EmptyView()
        }
    }

    private func title(_ session: SessionRecord) -> String {
        let trimmed = session.title.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmed.isEmpty { return trimmed }
        return (session.cwd as NSString).lastPathComponent
    }

    // MARK: - Benchmark

    /// `-flingBenchmark`: once the scrollback is cached, fling through it and
    /// log the frame probe's report.
    private func runBenchmark() async {
        guard let terminal = model.client(for: sessionID).map({ TerminalRegistry.shared.session(for: sessionID, client: $0) }) else { return }
        // Wait for the snapshot, then give the warm-up a head start only if
        // asked; the default measures a cold cache, the worst case.
        while terminal.buffer.historySize == 0, !Task.isCancelled { try? await Task.sleep(for: .milliseconds(100)) }
        if CommandLine.arguments.contains("-warmFirst") {
            while !Task.isCancelled {
                let missing = terminal.buffer.cache.missingPages(
                    in: 0 ..< terminal.buffer.historySize, historySize: terminal.buffer.historySize, focus: 0
                )
                if missing.isEmpty { break }
                try? await Task.sleep(for: .milliseconds(100))
            }
        }
        try? await Task.sleep(for: .seconds(1))
        guard let view = TerminalRegistry.shared.existingView(sessionID) else { return }
        await withCheckedContinuation { continuation in
            view.runFlingBenchmark { continuation.resume() }
        }
    }
}

/// What the session is asking, when it is asking something specific. Options
/// answer with a tap.
struct NeedsInputBanner: View {
    let detail: NeedsInputDetail
    /// Approve (true) or deny a permission prompt with the agent's own keys.
    var onAnswer: (Bool) -> Void = { _ in }

    @State private var confirmingDestructive = false

    private var isPermission: Bool { if case .permission = detail.kind { return true } else { return false } }

    var body: some View {
        if let text = detail.promptExcerpt ?? summaryIfSpecific {
            VStack(alignment: .leading, spacing: 8) {
                Text(text)
                    .font(.system(size: 14, weight: .medium))
                    .foregroundStyle(Tokens.Ink.primary)
                    .lineLimit(3)
                // Options are shown, not typed: a picker may focus something
                // else first, so a blind number can choose the wrong answer.
                // Answer them in the terminal (the key row has the arrows).
                if let options = detail.options, !options.isEmpty, !isPermission {
                    ForEach(Array(options.enumerated()), id: \.offset) { index, option in
                        Text("\(index + 1)  \(option)")
                            .font(.system(size: 14))
                            .foregroundStyle(Tokens.Ink.secondary)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
                if isPermission {
                    HStack(spacing: 8) {
                        Button("Deny") { onAnswer(false) }
                            .buttonStyle(BannerButtonStyle(fill: Color.white.opacity(0.12), ink: Tokens.Ink.primary))
                            .accessibilityIdentifier("banner-deny")
                        Button("Approve") {
                            if detail.riskHint == .destructive { confirmingDestructive = true } else { onAnswer(true) }
                        }
                        .buttonStyle(BannerButtonStyle(fill: risk, ink: .black))
                        .accessibilityIdentifier("banner-approve")
                    }
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(14)
            .glass(in: RoundedRectangle(cornerRadius: 22, style: .continuous), tint: risk.opacity(0.18))
            .confirmationDialog("Approve a destructive action?", isPresented: $confirmingDestructive, titleVisibility: .visible) {
                Button("Approve", role: .destructive) { onAnswer(true) }
                Button("Cancel", role: .cancel) {}
            }
        }
    }

    private var risk: Color {
        detail.riskHint == .destructive ? Tokens.Ink.danger : Tokens.Ink.attention
    }

    private var summaryIfSpecific: String? {
        guard let summary = detail.summary else { return nil }
        let generic = summary.lowercased().contains("waiting for your input")
        return generic && (detail.options?.isEmpty ?? true) ? nil : summary
    }
}

private struct BannerButtonStyle: ButtonStyle {
    let fill: Color
    let ink: Color

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.subheadline.weight(.semibold))
            .foregroundStyle(ink)
            .frame(maxWidth: .infinity, minHeight: 34)
            .background(Capsule().fill(fill.opacity(configuration.isPressed ? 0.7 : 1)))
    }
}
