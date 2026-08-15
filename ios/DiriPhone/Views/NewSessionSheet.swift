import SwiftUI

/// Starting an agent. The desktop's new-agent popover, as a sheet.
struct NewSessionSheet: View {
    let onStarted: (SessionRecord) -> Void

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    @State private var kind = "claude-code"
    @State private var cwd = ""
    @State private var prompt = ""
    @State private var busy = false
    @State private var error: String?

    var body: some View {
        NavigationStack {
            Form {
                Section("Agent") {
                    Picker("Agent", selection: $kind) {
                        ForEach(available, id: \.self) { id in
                            Text(id).tag(id)
                        }
                    }
                    .pickerStyle(.menu)
                }

                Section("Directory") {
                    TextField("/home/you/code", text: $cwd)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .font(Tokens.Typo.metaMono)
                    // Typing an absolute path on a phone keyboard is miserable,
                    // and the answer is nearly always somewhere you already are.
                    if !model.recentDirectories.isEmpty {
                        ScrollView(.horizontal, showsIndicators: false) {
                            HStack(spacing: 6) {
                                ForEach(model.recentDirectories, id: \.self) { path in
                                    Button {
                                        cwd = path
                                    } label: {
                                        RowChip(
                                            text: (path as NSString).lastPathComponent,
                                            tint: cwd == path ? Tokens.Ink.clay : Tokens.Ink.secondary
                                        )
                                    }
                                    .buttonStyle(.plain)
                                }
                            }
                        }
                    }
                }

                Section("First prompt") {
                    TextField("What should it start on?", text: $prompt, axis: .vertical)
                        .lineLimit(3 ... 8)
                }
            }
            .navigationTitle("New session")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button(busy ? "Starting…" : "Start", action: start)
                        .disabled(cwd.trimmingCharacters(in: .whitespaces).isEmpty || busy)
                }
            }
            .alert("Couldn't start", isPresented: .init(
                get: { error != nil }, set: { if !$0 { error = nil } }
            )) {
                Button("OK", role: .cancel) { error = nil }
            } message: {
                Text(error ?? "")
            }
        }
        .onAppear {
            if cwd.isEmpty { cwd = model.recentDirectories.first ?? "" }
            if !available.contains(kind), let first = available.first { kind = first }
        }
    }

    private var available: [String] {
        let ids = model.agents.map(\.kind.id).filter { $0 != "unknown" }
        return ids.isEmpty ? ["claude-code", "codex", "shell"] : ids
    }

    private func start() {
        busy = true
        Task {
            do {
                let record = try await model.spawn(
                    kind: kind,
                    cwd: cwd.trimmingCharacters(in: .whitespaces),
                    prompt: prompt
                )
                busy = false
                dismiss()
                onStarted(record)
            } catch {
                busy = false
                self.error = error.localizedDescription
            }
        }
    }
}

/// First run: paste the link `diri-web url` prints and the app is set up.
struct ConnectView: View {
    @Environment(AppModel.self) private var model
    @State private var link = ""
    @State private var invalid = false

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Spacer()
            Text("diri")
                .font(.system(size: 34, weight: .bold))
                .foregroundStyle(Tokens.Ink.primary)
            Text("Paste the link from `diri-web url` on your host.")
                .font(Tokens.Typo.row)
                .foregroundStyle(Tokens.Ink.secondary)

            TextField("http://forge:7380/?token=…", text: $link, axis: .vertical)
                .lineLimit(2 ... 5)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .font(Tokens.Typo.metaMono)
                .padding(12)
                .background(
                    RoundedRectangle(cornerRadius: Tokens.Radius.card, style: .continuous)
                        .fill(Tokens.Ink.floatingSurface)
                        .overlay(
                            RoundedRectangle(cornerRadius: Tokens.Radius.card, style: .continuous)
                                .stroke(invalid ? Tokens.Ink.danger : Tokens.Ink.floatingStroke, lineWidth: 1)
                        )
                )

            if invalid {
                Text("That link has no token in it.")
                    .font(Tokens.Typo.meta)
                    .foregroundStyle(Tokens.Ink.danger)
            }

            Button {
                if let endpoint = DiriClient.Endpoint(enrolmentURL: link) {
                    invalid = false
                    model.endpoint = endpoint
                } else {
                    invalid = true
                }
            } label: {
                Text("Connect")
                    .font(Tokens.Typo.rowEmphasized)
                    .foregroundStyle(Tokens.Ink.background)
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 13)
                    .background(
                        RoundedRectangle(cornerRadius: 11, style: .continuous).fill(Tokens.Ink.clay)
                    )
            }
            .buttonStyle(.plain)

            Text("Requires Tailscale to be connected.")
                .font(Tokens.Typo.meta)
                .foregroundStyle(Tokens.Ink.tertiary)
            Spacer()
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Tokens.Ink.background)
    }
}
