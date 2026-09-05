import SwiftUI

struct NewSessionSheet: View {
    let onStarted: (SessionRecord) -> Void
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var host = ""
    @State private var hosts: [DiriClient.Host] = []
    @State private var agents: [DiriClient.AgentReadiness.Item] = []
    @State private var kind = "claude-code"
    @State private var cwd = ""
    @State private var prompt = ""
    @State private var worktree = true
    @State private var branch = ""
    @State private var base = "main"
    @State private var loading = true
    @State private var busy = false
    @State private var dismissedWhileStarting = false
    @State private var browsing = false
    @State private var error: String?

    private var selectedHost: String? { host.isEmpty ? nil : host }
    private var projects: [Project] {
        model.projects.values.filter { $0.host == selectedHost && $0.root != nil }
            .sorted { $0.displayName.localizedStandardCompare($1.displayName) == .orderedAscending }
    }

    var body: some View {
        NavigationStack {
            Form {
                Section("Where to work") {
                    Picker("Computer", selection: $host) {
                        Text(model.host.isEmpty ? "Your Mac" : model.host).tag("")
                        ForEach(hosts) { Text($0.name).tag($0.id) }
                    }
                    if !projects.isEmpty {
                        Picker("Project", selection: $cwd) {
                            Text("Choose a project").tag("")
                            ForEach(projects, id: \.id) { project in
                                Text(project.displayName).tag(project.root ?? "")
                            }
                            if !cwd.isEmpty && !projects.contains(where: { $0.root == cwd }) {
                                Text((cwd as NSString).lastPathComponent).tag(cwd)
                            }
                        }
                    }
                    Button { browsing = true } label: {
                        Label(cwd.isEmpty ? "Choose a folder…" : "Change folder…", systemImage: "folder")
                    }
                    if !cwd.isEmpty { Text(cwd).font(.caption.monospaced()).foregroundStyle(.secondary) }
                }
                .disabled(busy)
                Section {
                    Toggle("Separate workspace", isOn: $worktree)
                    if worktree {
                        Text("Start a new branch from \(base.isEmpty ? "main" : base), leaving your existing work untouched.")
                            .font(.caption).foregroundStyle(.secondary)
                        DisclosureGroup("Branch options") {
                            TextField("Base branch", text: $base)
                            TextField("New branch name (automatic)", text: $branch)
                        }
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                    }
                } footer: {
                    Text(worktree ? "Uses the branch already on this computer; it does not fetch or pull. If main is missing, choose the correct base in Branch options." : "The agent edits the selected folder directly.")
                }
                .disabled(busy)
                Section("Agent") {
                    if loading { ProgressView("Checking installed agents…") }
                    else if agents.isEmpty {
                        Text("No agents are available here. Set up an agent in Diri on your Mac first.")
                    } else {
                        Picker("Agent", selection: $kind) {
                            ForEach(agents, id: \.kind.id) { Text($0.kind.displayName).tag($0.kind.id) }
                        }
                    }
                }
                .disabled(busy)
                Section("What would you like to build?") {
                    TextField("Describe the change…", text: $prompt, axis: .vertical).lineLimit(3...8)
                }
                .disabled(busy)
                if let error { Section { Text(error).foregroundStyle(.red) } }
                if busy {
                    Section {
                        ProgressView("Starting your session…")
                        Text("Taking a while? View sessions to answer any login or permission prompt. Starting continues in the background.")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
            .navigationTitle("New session")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button(busy ? "View sessions" : "Cancel") {
                        dismissedWhileStarting = busy
                        dismiss()
                    }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Start", action: start)
                        .disabled(busy || loading || cwd.isEmpty || !agents.contains { $0.kind.id == kind })
                }
            }
            .interactiveDismissDisabled(busy)
            .sheet(isPresented: $browsing) {
                FolderPicker(host: selectedHost, initialPath: cwd.isEmpty ? (hosts.first { $0.id == host }?.defaultCwd ?? "~") : cwd) { cwd = $0 }
            }
            .task { do { hosts = try await model.hosts() } catch { self.error = error.localizedDescription } }
            .task(id: host) {
                loading = true
                agents = []
                cwd = projects.first?.root ?? ""
                do {
                    let available = try await model.agents(host: selectedHost)
                    try Task.checkCancellation()
                    agents = available
                    if !agents.contains(where: { $0.kind.id == kind }) { kind = agents.first?.kind.id ?? "" }
                    loading = false
                } catch {
                    guard !Task.isCancelled else { return }
                    loading = false
                    self.error = error.localizedDescription
                }
            }
        }
    }

    private func start() {
        guard !busy else { return }
        busy = true
        error = nil
        Task {
            do {
                let record = try await model.spawn(kind: kind, cwd: cwd, prompt: prompt,
                    host: selectedHost, worktree: worktree,
                    branch: branch.trimmingCharacters(in: .whitespacesAndNewlines),
                    base: base.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? "main" : base.trimmingCharacters(in: .whitespacesAndNewlines))
                busy = false
                if !dismissedWhileStarting {
                    dismiss()
                    onStarted(record)
                }
            } catch {
                busy = false
                self.error = "\(error.localizedDescription)\nIf the connection dropped, check your sessions before starting again. The first attempt may have succeeded."
                await model.refresh()
            }
        }
    }
}

private struct FolderPicker: View {
    let host: String?
    let initialPath: String
    let onSelect: (String) -> Void
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var path: String?
    @State private var listing: DiriClient.DirectoryListing?
    @State private var error: String?
    @State private var loading = false

    var body: some View {
        NavigationStack {
            List {
                if let error { Text(error).foregroundStyle(.red) }
                if loading { ProgressView("Opening folder…") }
                if let listing {
                    Section {
                        Text(listing.path).font(.caption.monospaced())
                        if let parent = listing.parent {
                            Button { path = parent } label: { Label("Parent folder", systemImage: "arrow.up") }
                        }
                    }
                    ForEach(listing.entries) { entry in
                        Button { path = entry.path } label: { Label(entry.name, systemImage: "folder") }
                    }
                    if listing.truncated { Text("Showing the first 512 folders.").foregroundStyle(.secondary) }
                }
            }
            .navigationTitle("Choose a folder")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Use folder") { if let listing { onSelect(listing.path); dismiss() } }
                        .disabled(loading || listing == nil || error != nil)
                }
            }
            .task(id: path ?? initialPath) {
                loading = true
                error = nil
                do {
                    let result = try await model.directories(host: host, path: path ?? initialPath)
                    try Task.checkCancellation()
                    listing = result
                    loading = false
                } catch {
                    guard !Task.isCancelled else { return }
                    self.error = error.localizedDescription
                    loading = false
                }
            }
        }
    }
}
