import SwiftUI

/// A session that is waiting on you, as a card at the top of the list: what
/// it asks, in its own words, and, for a permission prompt, Approve and Deny
/// right there. Tapping the card opens the session.
///
/// Approve and Deny type the agent's own keys (its manifest's, as the Mac's
/// attention inbox does) through the gateway, which first re-checks that
/// the agent is still on this prompt. Questions with options are answered
/// in the session: a numbered option typed blind can pick the wrong thing
/// when a picker focuses something else first.
struct WaitingCard: View {
    let session: SessionRecord
    let computer: String
    let onOpen: () -> Void
    let onAnswer: (Bool) -> Void

    @State private var confirmingDestructive = false

    private var detail: NeedsInputDetail? { session.needsInput }
    private var destructive: Bool { detail?.riskHint == .destructive }
    private var tint: Color { destructive ? Tokens.Ink.danger : Tokens.Ink.attention }
    private var isPermission: Bool { if case .permission = detail?.kind { return true } else { return false } }

    private var question: String {
        let excerpt = detail?.promptExcerpt?.trimmingCharacters(in: .whitespacesAndNewlines)
        if let excerpt, !excerpt.isEmpty { return excerpt }
        let summary = detail?.summary?.trimmingCharacters(in: .whitespacesAndNewlines)
        if let summary, !summary.isEmpty { return summary }
        return "Waiting for your input"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Button(action: onOpen) {
                VStack(alignment: .leading, spacing: 6) {
                    HStack(spacing: 6) {
                        Circle().fill(tint).frame(width: 7, height: 7)
                        Text(title)
                            .font(.subheadline.weight(.semibold))
                            .foregroundStyle(Tokens.Ink.primary)
                            .lineLimit(1)
                        Spacer(minLength: 4)
                        Text(computer)
                            .font(.caption)
                            .foregroundStyle(Tokens.Ink.tertiary)
                            .lineLimit(1)
                    }
                    Text(question)
                        .font(.callout)
                        .foregroundStyle(Tokens.Ink.secondary)
                        .lineLimit(4)
                        .multilineTextAlignment(.leading)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    if let options = detail?.options, !options.isEmpty, !isPermission {
                        Text(options.enumerated().map { "\($0.offset + 1). \($0.element)" }.joined(separator: "   "))
                            .font(.caption)
                            .foregroundStyle(Tokens.Ink.tertiary)
                            .lineLimit(2)
                    }
                }
                .contentShape(.rect)
            }
            .buttonStyle(.plain)
            .accessibilityIdentifier("waiting-\(session.id)")

            HStack(spacing: 8) {
                if isPermission {
                    answerButton("Deny", role: .cancel, prominent: false) { onAnswer(false) }
                        .accessibilityIdentifier("deny-\(session.id)")
                    answerButton("Approve", role: nil, prominent: true) {
                        if destructive { confirmingDestructive = true } else { onAnswer(true) }
                    }
                    .accessibilityIdentifier("approve-\(session.id)")
                } else {
                    answerButton("Answer", role: nil, prominent: true, action: onOpen)
                        .accessibilityIdentifier("answer-\(session.id)")
                }
            }
        }
        .padding(14)
        .background {
            RoundedRectangle(cornerRadius: 18, style: .continuous)
                .fill(tint.opacity(0.12))
                .overlay(RoundedRectangle(cornerRadius: 18, style: .continuous).strokeBorder(tint.opacity(0.35), lineWidth: 1))
        }
        .confirmationDialog("Approve a destructive action?", isPresented: $confirmingDestructive, titleVisibility: .visible) {
            Button("Approve", role: .destructive) { onAnswer(true) }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text(question)
        }
    }

    private var title: String {
        let trimmed = session.title.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? (session.cwd as NSString).lastPathComponent : trimmed
    }

    private func answerButton(_ label: String, role: ButtonRole?, prominent: Bool, action: @escaping () -> Void) -> some View {
        Button(role: role, action: action) {
            Text(label)
                .font(.subheadline.weight(.semibold))
                .frame(maxWidth: .infinity, minHeight: 36)
                .foregroundStyle(prominent ? Color.black : Tokens.Ink.primary)
                .background(
                    Capsule().fill(prominent ? (destructive ? Tokens.Ink.danger : Tokens.Ink.attention) : Color.white.opacity(0.10))
                )
        }
        .buttonStyle(.plain)
    }
}
