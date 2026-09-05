import SwiftUI

/// One session: what it is showing, and the two ways to answer it.
struct SessionDetailView: View {
    let sessionID: String

    @Environment(AppModel.self) private var model
    @Environment(\.scenePhase) private var scenePhase
    @State private var screen: DiriClient.Screen?
    @State private var draft = ""
    @State private var wrapped = true
    @State private var busy = false
    @State private var error: String?
    @State private var screenError: String?
    @State private var showingChanges = false
    @State private var followOutput = true
    @FocusState private var composerFocused: Bool

    /// The screen is polled rather than streamed. `read_screen` is a cheap
    /// snapshot of the grid, and a snapshot cannot drift the way an applied
    /// delta can when a phone sleeps mid-stream.
    private let pollInterval: Duration = .milliseconds(500)

    private var session: SessionRecord? { model.session(sessionID) }

    var body: some View {
        VStack(spacing: 0) {
            if screen == nil && screenError == nil { ProgressView("Loading session…").padding() }
            if let screenError {
                Text("Reconnecting… \(screenError)").font(.caption).foregroundStyle(.orange).padding(8)
            }
            terminal
            if let detail = session?.needsInput, session?.attention == .needsInput {
                NeedsInputBanner(detail: detail)
            }
            composer
        }
        .background(Tokens.Ink.background)
        .navigationTitle(session.map(title) ?? "session")
        .navigationBarTitleDisplayMode(.inline)
        // Without an opaque bar the terminal scrolls underneath the title and
        // the two sets of text interleave — unreadable, and it looks broken.
        .toolbarBackground(.visible, for: .navigationBar)
        .toolbarBackground(Tokens.Ink.background, for: .navigationBar)
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                Menu {
                    Button { showingChanges = true } label: { Label("Review changes", systemImage: "doc.text.magnifyingglass") }
                    Toggle("Follow output", isOn: $followOutput)
                    Button {
                        wrapped.toggle()
                    } label: {
                        Label(wrapped ? "Show true columns" : "Wrap lines", systemImage: "text.alignleft")
                    }
                    Button {
                        Task { await model.markSeen(sessionID) }
                    } label: {
                        Label("Mark seen", systemImage: "checkmark.circle")
                    }
                } label: {
                    Image(systemName: "ellipsis.circle")
                }
            }
        }
        .task(id: "\(sessionID)-\(scenePhase == .active)") {
            guard scenePhase == .active else { return }
            await model.markSeen(sessionID)
            await poll()
        }
        .sheet(isPresented: $showingChanges) { SessionChangesView(sessionID: sessionID) }
        .alert("Couldn't send", isPresented: .init(
            get: { error != nil }, set: { if !$0 { error = nil } }
        )) {
            Button("OK", role: .cancel) { error = nil }
        } message: {
            Text(error ?? "")
        }
    }

    // MARK: - Terminal

    private var terminal: some View {
        ScrollViewReader { proxy in
            ScrollView([.vertical, wrapped ? [] : .horizontal]) {
                Text(screen?.text ?? "")
                    .font(Tokens.Typo.terminal)
                    .foregroundStyle(Color(f32: 0.812, 0.831, 0.855))
                    .textSelection(.enabled)
                    .frame(maxWidth: wrapped ? .infinity : nil, alignment: .leading)
                    .fixedSize(horizontal: !wrapped, vertical: true)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 10)
                    .id(Anchor.bottom)
            }
            .onChange(of: screen?.text) { _, _ in
                if followOutput { proxy.scrollTo(Anchor.bottom, anchor: .bottom) }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }

    private enum Anchor: Hashable { case bottom }

    // MARK: - Composer

    private var composer: some View {
        VStack(spacing: 8) {
            KeyRowView { key in
                guard !busy else { return }
                busy = true
                Task {
                    do { try await model.send(key, to: sessionID) } catch {
                        self.error = error.localizedDescription
                    }
                    busy = false
                    await refreshSoon()
                }
            }
            .disabled(busy || screenError != nil || screen == nil)

            HStack(alignment: .bottom, spacing: 8) {
                TextField("Message…", text: $draft, axis: .vertical)
                    .lineLimit(1 ... 6)
                    .font(.system(size: 16))
                    .padding(.horizontal, 11)
                    .padding(.vertical, 9)
                    .background(
                        RoundedRectangle(cornerRadius: 11, style: .continuous)
                            .fill(Tokens.Ink.floatingSurface)
                            .overlay(
                                RoundedRectangle(cornerRadius: 11, style: .continuous)
                                    .stroke(Tokens.Ink.floatingStroke, lineWidth: 1)
                            )
                    )
                    .focused($composerFocused)

                Button(action: submit) {
                    Image(systemName: "arrow.up")
                        .font(.system(size: 16, weight: .bold))
                        .foregroundStyle(Tokens.Ink.background)
                        .frame(width: 40, height: 40)
                        .background(Circle().fill(Tokens.Ink.clay))
                }
                .disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || busy || screenError != nil || screen == nil)
                .opacity(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? 0.35 : 1)
                .accessibilityLabel("Send")
            }
            .padding(.horizontal, 10)
        }
        .padding(.top, 8)
        .padding(.bottom, 8)
        .background(.ultraThinMaterial)
        .overlay(alignment: .top) {
            Rectangle().fill(Tokens.Ink.floatingStroke).frame(height: 1)
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
            await refreshSoon()
        }
    }

    // MARK: - Polling

    private func poll() async {
        while !Task.isCancelled {
            await fetchScreen()
            try? await Task.sleep(for: pollInterval)
        }
    }

    private func refreshSoon() async {
        try? await Task.sleep(for: .milliseconds(150))
        await fetchScreen()
    }

    private func fetchScreen() async {
        do {
            let fetched = try await model.screen(for: sessionID)
            try Task.checkCancellation()
            screen = fetched
            screenError = nil
        } catch {
            guard !Task.isCancelled else { return }
            screenError = error.localizedDescription
        }
    }

    private func title(_ session: SessionRecord) -> String {
        let trimmed = session.title.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmed.isEmpty { return trimmed }
        return (session.cwd as NSString).lastPathComponent
    }
}

/// What the session is actually asking, when it is asking something specific.
/// A bare "waiting for your input" is left to the glyph — repeating it here
/// would cost a band of screen and say nothing.
struct NeedsInputBanner: View {
    let detail: NeedsInputDetail

    var body: some View {
        if let text = detail.promptExcerpt ?? summaryIfSpecific {
            VStack(alignment: .leading, spacing: 4) {
                if let risk = detail.riskHint, risk != .neutral {
                    Text(risk.rawValue.uppercased())
                        .font(.system(size: 10, weight: .semibold))
                        .foregroundStyle(risk == .destructive ? Tokens.Ink.danger : Tokens.Ink.attention)
                }
                Text(text)
                    .font(Tokens.Typo.row)
                    .foregroundStyle(Tokens.Ink.primary)
                if let options = detail.options, !options.isEmpty {
                    ForEach(Array(options.enumerated()), id: \.offset) { index, option in
                        Text("\(index + 1). \(option)")
                            .font(Tokens.Typo.meta)
                            .foregroundStyle(Tokens.Ink.secondary)
                    }
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(10)
            .background(
                RoundedRectangle(cornerRadius: Tokens.Radius.card, style: .continuous)
                    .fill(Tokens.Ink.attention.opacity(0.10))
                    .overlay(
                        RoundedRectangle(cornerRadius: Tokens.Radius.card, style: .continuous)
                            .stroke(Tokens.Ink.attention.opacity(0.30), lineWidth: 1)
                    )
            )
            .padding(.horizontal, 10)
        }
    }

    private var summaryIfSpecific: String? {
        guard let summary = detail.summary else { return nil }
        let generic = summary.lowercased().contains("waiting for your input")
        return generic && (detail.options?.isEmpty ?? true) ? nil : summary
    }
}

/// The key row. Ordered by how often a phone needs it: answering a prompt,
/// moving a selection, then the escape hatches.
struct KeyRowView: View {
    let send: (TerminalKey) -> Void

    private let keys: [TerminalKey] = [
        .enter, .yes, .no, .up, .down, .escape,
        .tab, .shiftTab, .altEnter,
        .digit1, .digit2, .digit3,
        .ctrlU, .ctrlC, .ctrlR,
    ]

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 6) {
                ForEach(keys, id: \.self) { key in
                    Button {
                        UIImpactFeedbackGenerator(style: .light).impactOccurred()
                        send(key)
                    } label: {
                        Text(key.label)
                            .font(.system(size: 13, design: .monospaced))
                            .foregroundStyle(tint(key))
                            .padding(.horizontal, 11)
                            .padding(.vertical, 8)
                            .background(
                                RoundedRectangle(cornerRadius: 8, style: .continuous)
                                    .fill(Tokens.Ink.floatingSurface)
                                    .overlay(
                                        RoundedRectangle(cornerRadius: 8, style: .continuous)
                                            .stroke(Tokens.Ink.floatingStroke, lineWidth: 1)
                                    )
                            )
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel(key.wireName)
                }
            }
            .padding(.horizontal, 10)
        }
    }

    private func tint(_ key: TerminalKey) -> Color {
        switch key {
        case .yes: Tokens.Ink.fresh
        case .no, .ctrlC: Tokens.Ink.danger
        default: Tokens.Ink.primary.opacity(0.85)
        }
    }
}
