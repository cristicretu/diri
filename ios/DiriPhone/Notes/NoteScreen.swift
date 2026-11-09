import SwiftUI

/// A Diri note: its Markdown, editable in place. Saves itself shortly after
/// typing stops; follows edits made on the Mac (or by an agent) while it is
/// not being typed into; and never overwrites a change it has not seen.
struct NoteScreen: View {
    let sessionID: String
    var showsSidebarButton: Bool
    var onShowSidebar: () -> Void = {}

    @Environment(AppModel.self) private var model
    @Environment(\.scenePhase) private var scenePhase
    @State private var markdown = ""
    /// Moves whenever `markdown` comes from the Mac rather than the editor.
    @State private var revision = 0
    @State private var version: String?
    @State private var state = SaveState.loading
    @State private var conflict: DiriClient.Note?
    @State private var loadError: String?
    @State private var saveTask: Task<Void, Never>?

    enum SaveState: Equatable {
        case loading
        case saved
        case edited
        case saving
        case failed(String)
    }

    /// How long typing must pause before the note is saved.
    private let saveDelay: Duration = .milliseconds(800)
    /// How often an untouched note checks for edits made elsewhere.
    private let followInterval: Duration = .seconds(4)

    private var session: SessionRecord? { model.session(sessionID) }

    var body: some View {
        Group {
            if let loadError, version == nil {
                VStack(spacing: 10) {
                    Text("Couldn't open this note")
                        .font(.system(size: 17, weight: .semibold))
                        .foregroundStyle(Tokens.Ink.primary)
                    Text(loadError)
                        .font(.system(size: 13))
                        .foregroundStyle(Tokens.Ink.tertiary)
                        .multilineTextAlignment(.center)
                    Button("Try again") { Task { await load() } }
                        .foregroundStyle(Tokens.Ink.clay)
                }
                .padding(24)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if version == nil {
                ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                NoteEditor(markdown: markdown, revision: revision) { text in
                    edited(text)
                }
                .ignoresSafeArea(.container, edges: .bottom)
            }
        }
        .background(Tokens.Ink.background.ignoresSafeArea())
        .toolbar { toolbar }
        .navigationBarTitleDisplayMode(.inline)
        .task(id: scenePhase == .active) {
            guard scenePhase == .active else {
                await flush()
                return
            }
            if version == nil { await load() }
            await model.markSeen(sessionID)
            while !Task.isCancelled {
                try? await Task.sleep(for: followInterval)
                guard !Task.isCancelled else { return }
                await follow()
            }
        }
        .onDisappear { Task { await flush() } }
        .alert(
            "This note changed on your Mac",
            isPresented: .init(get: { conflict != nil }, set: { if !$0 { conflict = nil } })
        ) {
            Button("Use the Mac's version") { takeTheirs() }
            Button("Keep mine") { keepMine() }
        } message: {
            Text("It was edited there while you were typing here. Keep yours to replace it, or use the Mac's to drop your edits since the last save.")
        }
    }

    // MARK: - Toolbar

    @ToolbarContentBuilder
    private var toolbar: some ToolbarContent {
        if showsSidebarButton {
            ToolbarItem(placement: .topBarLeading) {
                Button(action: onShowSidebar) { Image(systemName: "sidebar.leading") }
                    .accessibilityLabel("Sessions")
                    .accessibilityIdentifier("show-sessions")
            }
        }
        ToolbarItem(placement: .principal) {
            VStack(spacing: 0) {
                Text(title)
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(Tokens.Ink.primary)
                    .lineLimit(1)
                Text(stateLabel)
                    .font(.system(size: 11, weight: .medium))
                    .foregroundStyle(stateColor)
                    .lineLimit(1)
                    .accessibilityIdentifier("note-save-state")
            }
            .frame(maxWidth: 220)
        }
    }

    private var title: String {
        let first = markdown.split(separator: "\n", maxSplits: 1).first.map(String.init) ?? ""
        if first.hasPrefix("# ") {
            let heading = first.dropFirst(2).trimmingCharacters(in: .whitespaces)
            if !heading.isEmpty { return heading }
        }
        let recorded = session?.title.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return recorded.isEmpty ? "Untitled" : recorded
    }

    private var stateLabel: String {
        switch state {
        case .loading: "Opening…"
        case .saved: "Note · saved"
        case .edited, .saving: "Saving…"
        case .failed: "Not saved · retrying"
        }
    }

    private var stateColor: Color {
        if case .failed = state { return Tokens.Ink.attention }
        return Tokens.Ink.tertiary
    }

    // MARK: - Loading

    private func load() async {
        guard let client = model.client(for: sessionID) else {
            loadError = "Connect to your Mac first."
            return
        }
        do {
            let note = try await client.note(sessionID: sessionID)
            show(note)
            loadError = nil
        } catch {
            loadError = error.localizedDescription
        }
    }

    /// Takes the Mac's text into the editor.
    private func show(_ note: DiriClient.Note) {
        markdown = note.markdown
        version = note.version
        revision += 1
        state = .saved
    }

    /// Picks up an edit made elsewhere, unless this phone is mid-edit: then
    /// the next save finds the conflict and asks.
    private func follow() async {
        guard state == .saved, conflict == nil, let client = model.client(for: sessionID),
              let note = try? await client.note(sessionID: sessionID)
        else { return }
        if state == .saved, note.version != version { show(note) }
    }

    // MARK: - Saving

    private func edited(_ text: String) {
        guard text != markdown else { return }
        markdown = text
        state = .edited
        saveTask?.cancel()
        saveTask = Task {
            try? await Task.sleep(for: saveDelay)
            guard !Task.isCancelled else { return }
            await save()
        }
    }

    /// Saves now if anything is unsaved: leaving the screen or the app must
    /// not drop the last few words.
    private func flush() async {
        switch state {
        case .edited, .failed: break
        case .loading, .saved, .saving: return
        }
        saveTask?.cancel()
        await save()
    }

    private func save() async {
        guard let version, let client = model.client(for: sessionID), conflict == nil else { return }
        let text = markdown
        state = .saving
        do {
            switch try await client.saveNote(sessionID: sessionID, markdown: text, version: version) {
            case let .saved(next):
                self.version = next
                // Typing that arrived during the round trip saves next.
                if markdown == text {
                    state = .saved
                } else {
                    state = .edited
                    saveTask = Task {
                        try? await Task.sleep(for: saveDelay)
                        guard !Task.isCancelled else { return }
                        await save()
                    }
                }
            case let .conflict(current):
                conflict = current
                state = .edited
            }
        } catch {
            state = .failed(error.localizedDescription)
            saveTask = Task {
                try? await Task.sleep(for: .seconds(3))
                guard !Task.isCancelled else { return }
                await save()
            }
        }
    }

    private func takeTheirs() {
        guard let current = conflict else { return }
        conflict = nil
        saveTask?.cancel()
        show(current)
    }

    private func keepMine() {
        guard let current = conflict else { return }
        conflict = nil
        // Saving against the version just seen replaces it, deliberately.
        version = current.version
        Task { await save() }
    }
}
