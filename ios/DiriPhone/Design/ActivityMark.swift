import SwiftUI

/// The desktop's line icons (`diri-ui/assets/icons`), as their closest
/// SF Symbols.
enum LineIcon: String {
    case warning = "exclamationmark.triangle"
    case check = "checkmark"
    case moon = "moon"
    case file = "doc"
}

struct LineIconView: View {
    let icon: LineIcon
    let size: CGFloat
    let color: Color

    var body: some View {
        Image(systemName: icon.rawValue)
            .font(.system(size: size * 0.86, weight: icon == .check ? .semibold : .medium))
            .foregroundStyle(color)
            .frame(width: size, height: size)
            .accessibilityHidden(true)
    }
}

/// `activity_mark` — the leading mark of a sidebar row: what the session is
/// doing, before its title is read. Idle and ended rows leave the slot empty
/// so the column stays aligned.
struct ActivityMarkView: View {
    let state: StatusState

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.marksAnimate) private var marksAnimate

    var body: some View {
        Group {
            switch state {
            case .working:
                if reduceMotion || !marksAnimate {
                    WorkingDots(frame: 3)
                } else {
                    // The desktop's `FrameLoop`: eight frames, one sweep a second.
                    TimelineView(.periodic(from: .now, by: 0.125)) { context in
                        WorkingDots(frame: Int(context.date.timeIntervalSinceReferenceDate * 8) % 8)
                    }
                }
            case let .needsInput(destructive):
                LineIconView(icon: .warning, size: 14, color: destructive ? Tokens.Ink.danger : Tokens.Ink.attention)
            case .doneUnseen:
                LineIconView(icon: .check, size: 14, color: Tokens.Ink.fresh)
            case .hibernated:
                LineIconView(icon: .moon, size: 13, color: Tokens.Ink.tertiary)
            case .idleSeen, .none:
                Color.clear
            }
        }
        .frame(width: 18, height: 18)
        .accessibilityElement()
        .accessibilityLabel(state.label)
    }
}

/// `icons/working-N.svg`: three dots with a light sweeping across them.
private struct WorkingDots: View {
    let frame: Int

    /// Each frame's dot opacities, left to right.
    private static let frames: [[Double]] = [
        [0.61, 0.28, 0.28],
        [0.90, 0.33, 0.28],
        [0.61, 0.61, 0.28],
        [0.33, 0.90, 0.33],
        [0.28, 0.61, 0.61],
        [0.28, 0.33, 0.90],
        [0.28, 0.28, 0.61],
        [0.33, 0.28, 0.33],
    ]

    var body: some View {
        Canvas { context, size in
            let scale = min(size.width, size.height) / 16
            for (index, x) in [2.3, 8.0, 13.7].enumerated() {
                let radius = 1.65 * scale
                let center = CGPoint(x: x * scale, y: 8 * scale)
                context.fill(
                    Path(ellipseIn: CGRect(x: center.x - radius, y: center.y - radius, width: radius * 2, height: radius * 2)),
                    with: .color(Tokens.Ink.secondary.opacity(Self.frames[frame % 8][index]))
                )
            }
        }
        .frame(width: 14, height: 14)
    }
}

extension EnvironmentValues {
    /// False for a list that is alive but not on screen (the closed drawer):
    /// its working marks hold still instead of ticking eight times a second.
    @Entry var marksAnimate = true
}
