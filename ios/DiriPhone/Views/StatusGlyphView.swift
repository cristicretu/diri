import SwiftUI

/// `StatusGlyph` — the agent's mark, tinted by what the session is doing.
///
/// The desktop deliberately renders these static: "leaving a working or
/// needs-input glyph on screen never schedules another frame". The phone keeps
/// that rule for everything except needs-input, which is the one state worth
/// spending a repaint on — it is the reason you picked the phone up.
struct StatusGlyphView: View {
    let kind: AgentKind
    let state: StatusState
    var size: CGFloat = Tokens.Metrics.glyph

    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        Group {
            if let mark = kind.brandMark {
                BrandMarkView(kind: mark, size: size, color: color)
            } else {
                ShellCaretView(size: size, color: color)
            }
        }
        .modifier(PulseModifier(active: pulses && !reduceMotion))
        .accessibilityElement()
        .accessibilityLabel(state.label)
    }

    private var pulses: Bool {
        if case .needsInput = state { return true }
        return false
    }

    /// `static_status_color`.
    private var color: Color {
        switch state {
        case .working: workingColor.opacity(0.96)
        case let .needsInput(destructive): destructive ? Tokens.Ink.danger : Tokens.Ink.attention
        case .doneUnseen: Tokens.Ink.fresh
        case .idleSeen: Tokens.Ink.primary.opacity(0.42)
        case .none: Tokens.Ink.primary.opacity(0.28)
        case .hibernated: Tokens.Ink.primary.opacity(0.36)
        }
    }

    /// `Ink::working` — each agent works in its own colour.
    private var workingColor: Color {
        switch kind.id {
        case "claude-code": Tokens.Ink.clay
        case "codex", "cursor": Tokens.Ink.primary.opacity(0.82)
        case "gemini": Tokens.Ink.geminiBlue
        default: Tokens.Ink.genericWorking
        }
    }
}

/// The desktop's `PulsingMark`: opacity riding a 0.5→1 map so the mark never
/// fully disappears, which would read as a glitch rather than a heartbeat.
private struct PulseModifier: ViewModifier {
    let active: Bool
    @State private var on = false

    func body(content: Content) -> some View {
        content
            .opacity(active ? (on ? 1.0 : 0.5) : 1.0)
            .animation(
                active ? .easeInOut(duration: 1.1).repeatForever(autoreverses: true) : .default,
                value: on
            )
            .onAppear { if active { on = true } }
            .onChange(of: active) { _, isActive in on = isActive }
    }
}
