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
    /// The note's text and save state. A class, not view state: a save that
    /// is in flight, or the last one fired as the screen closes, must keep
    /// working after this view is gone.
    @State private var draft = NoteDraft()

    /// How often an untouched note checks for edits made elsewhere.
    private let followInterval: Duration = .seconds(4)

    private var session: SessionRecord? { model.session(sessionID) }
    private var markdown: String { draft.markdown }
    private var state: NoteDraft.SaveState { draft.state }

    var body: some View {
        Group {
            if let loadError = draft.loadError, draft.version == nil {
                VStack(spacing: 10) {
                    Text("Couldn't open this note")
                        .font(.system(size: 17, weight: .semibold))
                        .foregroundStyle(Tokens.Ink.primary)
                    Text(loadError)
                        .font(.system(size: 13))
                        .foregroundStyle(Tokens.Ink.tertiary)
                        .multilineTextAlignment(.center)
                    Button("Try again") { Task { await draft.load() } }
                        .foregroundStyle(Tokens.Ink.clay)
                }
                .padding(24)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if draft.version == nil {
                ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                NoteEditor(markdown: draft.markdown, revision: draft.revision) { text in
                    draft.edited(text)
                }
                .ignoresSafeArea(.container, edges: .bottom)
            }
        }
        .background(Tokens.Ink.background.ignoresSafeArea())
        .toolbar { toolbar }
        .navigationBarTitleDisplayMode(.inline)
        .task(id: scenePhase == .active) {
            draft.bind(model: model, sessionID: sessionID)
            guard scenePhase == .active else {
                draft.flush()
                return
            }
            if draft.version == nil { await draft.load() }
            await model.markSeen(sessionID)
            while !Task.isCancelled {
                try? await Task.sleep(for: followInterval)
                guard !Task.isCancelled else { return }
                await draft.follow()
            }
        }
        .onDisappear { draft.flush() }
        .alert(
            "This note changed on your Mac",
            isPresented: .init(get: { draft.conflict != nil }, set: { if !$0 { draft.conflict = nil } })
        ) {
            Button("Use the Mac's version") { draft.takeTheirs() }
            Button("Keep mine") { draft.keepMine() }
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
}

/// One open note's text and its saves. Saves never overlap: typing during a
/// save's round trip is saved after it, against the version it returned, so
/// the phone never conflicts with its own write.
@MainActor
@Observable
final class NoteDraft {
    enum SaveState: Equatable {
        case loading
        case saved
        case edited
        case saving
        case failed(String)
    }

    private(set) var markdown = ""
    /// Moves whenever `markdown` comes from the Mac rather than the editor.
    private(set) var revision = 0
    private(set) var version: String?
    private(set) var state = SaveState.loading
    var conflict: DiriClient.Note?
    private(set) var loadError: String?

    @ObservationIgnored private var model: AppModel?
    @ObservationIgnored private var sessionID = ""
    /// The pending debounce or retry. Only ever sleeping: a request in
    /// flight is never cancelled by new typing.
    @ObservationIgnored private var pending: Task<Void, Never>?
    @ObservationIgnored private var inFlight = false
    /// Set when the screen closes mid-save: what was typed during the round
    /// trip is saved at once rather than after the typing pause.
    @ObservationIgnored private var flushing = false

    /// How long typing must pause before the note is saved.
    private let saveDelay: Duration = .milliseconds(800)

    func bind(model: AppModel, sessionID: String) {
        self.model = model
        self.sessionID = sessionID
    }

    private var client: DiriClient? { model?.client(for: sessionID) }

    // MARK: - Loading

    func load() async {
        guard let client else {
            loadError = "Connect to your Mac first."
            return
        }
        do {
            show(try await client.note(sessionID: sessionID))
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
    func follow() async {
        guard state == .saved, conflict == nil, let client,
              let note = try? await client.note(sessionID: sessionID)
        else { return }
        if state == .saved, !inFlight, note.version != version { show(note) }
    }

    // MARK: - Saving

    func edited(_ text: String) {
        guard text != markdown else { return }
        markdown = text
        state = .edited
        // A save in flight picks this up when it returns.
        if !inFlight { schedule(after: saveDelay) }
    }

    /// Saves now if anything is unsaved: leaving the screen or the app must
    /// not drop the last few words. Runs in its own task, so it outlives
    /// the view and the scene-phase task that asked for it.
    func flush() {
        switch state {
        case .edited, .failed: break
        case .loading, .saved, .saving: return
        }
        if inFlight {
            flushing = true
            return
        }
        pending?.cancel()
        pending = nil
        Task { await self.save() }
    }

    private func schedule(after delay: Duration) {
        pending?.cancel()
        pending = Task {
            try? await Task.sleep(for: delay)
            guard !Task.isCancelled else { return }
            await self.save()
        }
    }

    private func save() async {
        guard !inFlight, let version, let client, conflict == nil else { return }
        pending = nil
        let text = markdown
        inFlight = true
        state = .saving
        defer { inFlight = false }
        do {
            switch try await client.saveNote(sessionID: sessionID, markdown: text, version: version) {
            case let .saved(next):
                self.version = next
                if markdown == text {
                    state = .saved
                    flushing = false
                } else {
                    // Typing that arrived during the round trip saves next.
                    state = .edited
                    inFlight = false
                    if flushing {
                        flushing = false
                        await save()
                    } else {
                        schedule(after: saveDelay)
                    }
                }
            case let .conflict(current):
                conflict = current
                state = .edited
                flushing = false
            }
        } catch {
            state = .failed(error.localizedDescription)
            flushing = false
            schedule(after: .seconds(3))
        }
    }

    func takeTheirs() {
        guard let current = conflict else { return }
        conflict = nil
        pending?.cancel()
        pending = nil
        show(current)
    }

    func keepMine() {
        guard let current = conflict else { return }
        conflict = nil
        // Saving against the version just seen replaces it, deliberately.
        version = current.version
        Task { await self.save() }
    }
}
