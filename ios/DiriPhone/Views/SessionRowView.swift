import SwiftUI

/// One sidebar row, matching `session_row` in the desktop's `sidebar/view.rs`.
///
/// The lane order is fixed and load-bearing: indent rails, activity mark,
/// title, then the trailing fold control, pin and agent mark. Every row has
/// the same lanes even when a lane is empty, which is what lets each column
/// of marks scan straight down the list instead of stepping with the text.
struct SessionRowView: View {
    let row: SidebarRow
    let selected: Bool
    let onToggleCollapse: () -> Void

    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var session: SessionRecord { row.session }

    var body: some View {
        HStack(spacing: 8) {
            IndentRails(depth: row.depth, rails: row.rails)

            // What the session is doing leads the row, as on the Mac.
            ActivityMarkView(state: session.statusState)

            VStack(alignment: .leading, spacing: 1) {
                Text(displayTitle)
                    .font(.system(size: 15, weight: selected ? .semibold : .regular))
                    .foregroundStyle(Tokens.Ink.primary.opacity(selected ? 1.0 : 0.86))
                    .lineLimit(1)
                    .truncationMode(.tail)

                if let subtitle {
                    Text(subtitle)
                        .font(.system(size: 12))
                        .foregroundStyle(Tokens.Ink.tertiary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)

            trailing
        }
        .padding(.horizontal, 10)
        .frame(height: 52)
        .background(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .fill(selected ? RowFill.selected.color : RowFill.clear.color)
        )
        // Archived and hibernated rows are dimmed rather than hidden — the same
        // two opacities the desktop uses.
        .opacity(session.isArchived ? 0.58 : (session.isHibernated ? 0.74 : 1.0))
        .animation(reduceMotion ? nil : Tokens.Motion.rowSelect, value: selected)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(displayTitle)
        .accessibilityIdentifier("session-row-\(session.id)")
        .accessibilityAddTraits(selected ? .isSelected : [])
        .accessibilityValue(session.statusState.label)
    }

    /// `display_title` — a session with no title of its own falls back to where
    /// it is running, never to a raw id.
    private var displayTitle: String {
        let trimmed = session.title.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmed.isEmpty { return trimmed }
        let path = session.worktreePath ?? session.cwd
        let name = (path as NSString).lastPathComponent
        return name.isEmpty ? session.effectiveKind.displayName : name
    }

    /// The branch is the single most useful second line on a phone: it is how
    /// you tell two sessions in the same repo apart at a glance. The project
    /// is already the header above, so it is not repeated here.
    private var subtitle: String? {
        guard !session.isNote, let branch = session.gitBranch, !branch.isEmpty else { return nil }
        return branch
    }

    /// The fold control, pin and agent mark, in the Mac's trailing order.
    /// The fold slot is kept on every row so the agent marks line up.
    @ViewBuilder
    private var trailing: some View {
        HStack(spacing: 6) {
            if session.hasEnded {
                Text("ended")
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(Tokens.Ink.tertiary)
            }
            if row.pinned {
                Image(systemName: "pin.fill")
                    .font(.system(size: 9))
                    .foregroundStyle(Tokens.Ink.tertiary)
            }
            Group {
                if row.hasChildren {
                    Button(action: onToggleCollapse) {
                        Image(systemName: "chevron.right")
                            .font(.system(size: 9, weight: .semibold))
                            .foregroundStyle(Tokens.Ink.tertiary)
                            .rotationEffect(.degrees(row.collapsed ? 0 : 90))
                            .frame(width: 28, height: Tokens.Metrics.rowHeight)
                            .contentShape(.rect)
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel(row.collapsed ? "Expand" : "Collapse")
                } else {
                    Color.clear
                }
            }
            .frame(width: 28)

            StatusGlyphView(kind: session.effectiveKind, state: session.statusState)
                .frame(width: Tokens.Metrics.glyph, height: Tokens.Metrics.glyph)
        }
        .fixedSize()
    }
}

/// The lineage rails. One column per ancestor: a full-height line when that
/// ancestor still has siblings below, a half-height elbow on the column that
/// parents this row, and nothing at all otherwise — drawing a rail that neither
/// continues nor elbows would imply a relationship that is not there.
struct IndentRails: View {
    let depth: Int
    let rails: UInt32

    var body: some View {
        HStack(spacing: 0) {
            ForEach(0 ..< max(depth, 0), id: \.self) { column in
                let continues = rails & (1 << UInt32(min(column, 31))) != 0
                let lastColumn = column + 1 == depth
                Rectangle()
                    .fill(Tokens.Ink.primary.opacity(0.10))
                    .frame(
                        width: 1,
                        height: continues
                            ? Tokens.Metrics.rowHeight
                            : (lastColumn ? Tokens.Metrics.rowHeight / 2 : 0)
                    )
                    .frame(width: Tokens.Space.indent, height: Tokens.Metrics.rowHeight, alignment: .top)
            }
        }
    }
}
