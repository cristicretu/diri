import SwiftUI

struct SessionChangesView: View {
    let sessionID: String
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var diff: DiriClient.Diff?
    @State private var error: String?

    var body: some View {
        NavigationStack {
            Group {
                if let diff {
                    ScrollView([.vertical, .horizontal]) {
                        VStack(alignment: .leading, spacing: 12) {
                            Text(diff.repoRoot).font(.caption).foregroundStyle(.secondary)
                            if diff.truncated { Text("Large diff — only the first part is shown.").foregroundStyle(.orange) }
                            Text(diff.patch.isEmpty ? "No tracked changes compared with HEAD." : diff.patch)
                                .font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                            Text("Untracked files are not included.").font(.caption).foregroundStyle(.secondary)
                        }.padding()
                    }
                } else if let error { ContentUnavailableView("Cannot load changes", systemImage: "exclamationmark.triangle", description: Text(error)) }
                else { ProgressView("Loading changes…") }
            }
            .navigationTitle("Changes")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
            .task {
                do { diff = try await model.diff(for: sessionID) }
                catch { self.error = error.localizedDescription }
            }
        }
    }
}
