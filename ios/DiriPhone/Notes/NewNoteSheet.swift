import SwiftUI

/// Starts a note on the Mac, in one of its projects, and opens it.
struct NewNoteSheet: View {
    let onCreated: (SessionRecord) -> Void

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var title = ""
    @State private var cwd = ""
    @State private var busy = false
    @State private var error: String?
    @FocusState private var titleFocused: Bool

    /// Notes are kept on the Mac, so only its own projects can hold one.
    private var projects: [Project] {
        model.projects.values.filter { $0.host == nil && $0.root != nil }
            .sorted { $0.displayName.localizedStandardCompare($1.displayName) == .orderedAscending }
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Title", text: $title)
                        .focused($titleFocused)
                        .submitLabel(.done)
                        .onSubmit(create)
                        .accessibilityIdentifier("new-note-title")
                }
                Section("Project") {
                    if projects.isEmpty {
                        Text("Start a session in a project on your Mac first.")
                            .foregroundStyle(.secondary)
                    } else {
                        Picker("Project", selection: $cwd) {
                            ForEach(projects, id: \.id) { project in
                                Text(project.displayName).tag(project.root ?? "")
                            }
                        }
                        .accessibilityIdentifier("new-note-project")
                    }
                }
                if let error { Section { Text(error).foregroundStyle(.red) } }
            }
            .disabled(busy)
            .navigationTitle("New note")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Create", action: create)
                        .disabled(busy || cwd.isEmpty)
                        .accessibilityIdentifier("new-note-create")
                }
            }
            .onAppear {
                titleFocused = true
                if cwd.isEmpty { cwd = defaultProject ?? projects.first?.root ?? "" }
            }
        }
    }

    /// The project of the session open now, which is usually where the
    /// thought that needs writing down came from.
    private var defaultProject: String? {
        guard let open = model.selectedSessionID.flatMap(model.session),
              let project = model.projects[open.projectID], project.host == nil
        else { return nil }
        return project.root
    }

    private func create() {
        guard !busy, !cwd.isEmpty else { return }
        busy = true
        error = nil
        Task {
            do {
                let record = try await model.spawnNote(
                    title: title.trimmingCharacters(in: .whitespacesAndNewlines), cwd: cwd
                )
                dismiss()
                onCreated(record)
            } catch {
                busy = false
                self.error = error.localizedDescription
            }
        }
    }
}
