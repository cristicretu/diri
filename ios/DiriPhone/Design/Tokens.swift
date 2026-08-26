import SwiftUI

/// A direct port of `diri-ui/src/tokens.rs`.
///
/// Every value here has a counterpart in the desktop app, and the point is that
/// they stay identical: the sidebar is meant to read as the same surface on
/// both, so a colour invented here would be a bug even if it looked fine. When
/// the Rust tokens move, these move with them.
enum Tokens {}

// MARK: - Colour

extension Color {
    /// `rgba_f32` — the desktop authors colour in normalized channels, so the
    /// literals below can be transcribed rather than converted (and diffed
    /// against the Rust file by eye).
    init(f32 red: Double, _ green: Double, _ blue: Double, _ alpha: Double = 1.0) {
        self.init(.sRGB, red: red, green: green, blue: blue, opacity: alpha)
    }
}

extension Tokens {
    /// `SemanticColors::dark()` / `::sidebar(Dark)`.
    ///
    /// Only the dark family is ported: the phone app is pinned to dark in
    /// `Info.plist`, matching how this surface is used — a glance at a phone in
    /// a pocket, not a document you read in daylight.
    enum Ink {
        static let primary = Color.white
        static let background = Color(f32: 0.071, 0.075, 0.094)
        /// Sidebar materials get firmer supporting tones than stock label
        /// opacities, because on the desktop they sit over live content.
        static let secondary = Color.white.opacity(0.70)
        static let tertiary = Color.white.opacity(0.44)
        static let floatingSurface = Color(f32: 0.141, 0.161, 0.196)
        static let floatingStroke = Color.white.opacity(0.08)

        static let attention = Color(f32: 0.961, 0.651, 0.137)
        static let danger = Color(f32: 0.961, 0.271, 0.227)
        static let fresh = Color(f32: 0.204, 0.780, 0.349)
        static let genericWorking = Color(f32: 0.541, 0.561, 0.596)

        static let clay = Color(f32: 0.851, 0.467, 0.341)
        static let geminiBlue = Color(f32: 0.306, 0.510, 0.933)
    }

    /// `Fill` — row backgrounds, expressed as opacities on `primary`.
    enum Fill {
        static let hover = 0.06
        static let multiSelected = 0.08
        static let selected = 0.10
        static let subtle = 0.06
    }

    /// `Radius`.
    enum Radius {
        static let chip: CGFloat = 5
        static let badge: CGFloat = 6
        static let row: CGFloat = 7
        static let card: CGFloat = 10
        static let panel: CGFloat = 12
    }

    /// `Space`.
    enum Space {
        static let indent: CGFloat = 12
        static let rowH: CGFloat = 8
        static let inset: CGFloat = 10
    }

    /// `Metrics`, with one deliberate departure.
    enum Metrics {
        /// The desktop row is 28pt, which is a mouse target. A finger needs 44,
        /// so the row grows and everything inside it keeps its desktop size —
        /// the type, the glyph, the indent and the corner radius are all
        /// unchanged, so the row reads identically and simply breathes more.
        static let rowHeight: CGFloat = 44
        /// What the desktop uses, kept so the deviation above stays visible and
        /// the ratio is available to anything that needs to reason about it.
        static let desktopRowHeight: CGFloat = 28
        static let glyph: CGFloat = 16
        static let sectionHeader: CGFloat = 28
    }

    /// `Typo`. Sizes are points and match the desktop exactly.
    enum Typo {
        static let meta = Font.system(size: 11, weight: .medium)
        static let sectionHeader = Font.system(size: 11, weight: .semibold)
        static let row = Font.system(size: 13, weight: .regular)
        static let rowEmphasized = Font.system(size: 13, weight: .medium)
        static let title = Font.system(size: 13, weight: .semibold)
        static let displayTitle = Font.system(size: 15, weight: .semibold)
        static let metaMono = Font.system(size: 11, weight: .medium, design: .monospaced)
        static let terminal = Font.system(size: 11, weight: .regular, design: .monospaced)
    }

    /// `Motion`. The desktop describes springs as (response, dampingFraction),
    /// which is exactly SwiftUI's `.spring(response:dampingFraction:)`.
    enum Motion {
        static let snap = Animation.spring(response: 0.32, dampingFraction: 0.74)
        static let pop = Animation.spring(response: 0.40, dampingFraction: 0.60)
        static let settle = Animation.spring(response: 0.55, dampingFraction: 0.82)
        static let rowSelect = Animation.easeOut(duration: 0.16)
        static let overlayFade = Animation.easeInOut(duration: 0.12)
    }
}

// MARK: - Row fill

/// `RowFill` — which background a row is wearing.
enum RowFill {
    case selected
    case multiSelected
    case pressed
    case clear

    var color: Color {
        switch self {
        case .selected: Tokens.Ink.primary.opacity(Tokens.Fill.selected)
        case .multiSelected: Tokens.Ink.primary.opacity(Tokens.Fill.multiSelected)
        // The desktop's hover has no touch equivalent; the press highlight
        // borrows its opacity so a finger-down looks like a cursor-over.
        case .pressed: Tokens.Ink.primary.opacity(Tokens.Fill.hover)
        case .clear: .clear
        }
    }
}
