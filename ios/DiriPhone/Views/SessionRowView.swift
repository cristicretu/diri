import SwiftUI

/// One sidebar row, matching `session_row` in the desktop's `sidebar/view.rs`.
///
/// The lane order is fixed and load-bearing: indent rails, disclosure slot,
/// status glyph, title, then trailing chips. Every row has the same lanes even
/// when a lane is empty, which is what lets the column of glyphs scan straight
/// down the list instead of stepping left and right with the text.
struct SessionRowView: View {
    let row: SidebarRow
    let selected: Bool
    let onToggleCollapse: () -> Void

    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var session: SessionRecord { row.session }

    var body: some View {
        HStack(spacing: 8) {
            IndentRails(depth: row.depth, rails: row.rails)

            // The fold control's column is always present, so a row with no
            // children does not slide its title left of its siblings.
            Group {
                if row.hasChildren {
                    Button(action: onToggleCollapse) {
                        Image(systemName: "chevron.right")
                            .font(.system(size: 9, weight: .semibold))
                            .foregroundStyle(Tokens.Ink.tertiary)
                            .rotationEffect(.degrees(row.collapsed ? 0 : 90))
                            .frame(width: Tokens.Space.indent, height: Tokens.Metrics.rowHeight)
                            .contentShape(.rect)
                    }
                    .buttonStyle(.plain)
                } else {
                    Color.clear.frame(width: Tokens.Space.indent)
                }
            }
            .frame(width: Tokens.Space.indent)

            StatusGlyphView(kind: session.effectiveKind, state: session.statusState)
                .frame(width: Tokens.Metrics.glyph, height: Tokens.Metrics.glyph)

            VStack(alignment: .leading, spacing: 1) {
                Text(displayTitle)
                    .font(selected ? Tokens.Typo.rowEmphasized : Tokens.Typo.row)
                    .foregroundStyle(Tokens.Ink.primary.opacity(selected ? 1.0 : 0.75))
                    .lineLimit(1)
                    .truncationMode(.tail)

                if let subtitle {
                    Text(subtitle)
                        .font(Tokens.Typo.meta)
                        .foregroundStyle(Tokens.Ink.tertiary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)

            trailing
        }
        .padding(.horizontal, Tokens.Space.rowH)
        .frame(height: Tokens.Metrics.rowHeight)
        .background(
            RoundedRectangle(cornerRadius: Tokens.Radius.row, style: .continuous)
                .fill(selected ? RowFill.selected.color : RowFill.clear.color)
        )
        // Archived and hibernated rows are dimmed rather than hidden — the same
        // two opacities the desktop uses.
        .opacity(session.isArchived ? 0.58 : (session.isHibernated ? 0.74 : 1.0))
        .animation(reduceMotion ? nil : Tokens.Motion.rowSelect, value: selected)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(displayTitle)
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
    /// you tell two sessions in the same repo apart at a glance.
    private var subtitle: String? {
        if let branch = session.gitBranch, !branch.isEmpty { return branch }
        let path = session.worktreePath ?? session.cwd
        let name = (path as NSString).lastPathComponent
        return name.isEmpty ? nil : name
    }

    @ViewBuilder
    private var trailing: some View {
        HStack(spacing: 4) {
            if row.pinned {
                Image(systemName: "pin.fill")
                    .font(.system(size: 9))
                    .foregroundStyle(Tokens.Ink.tertiary)
            }
            if let host = session.host, !host.isEmpty {
                RowChip(text: host, tint: Tokens.Ink.secondary)
            }
            if session.hasEnded {
                RowChip(text: "ended", tint: Tokens.Ink.primary.opacity(0.35))
            }
            if row.hasChildren, row.collapsed {
                // A folded subtree still has to declare that it exists.
                RowChip(text: "•••", tint: Tokens.Ink.tertiary)
            }
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

/// The row's shared chip: one state, in the smallest space that still reads.
/// Every chip on a row is the same shape so they scan as one lane.
struct RowChip: View {
    let text: String
    var tint: Color = Tokens.Ink.secondary

    var body: some View {
        Text(text)
            .font(Tokens.Typo.meta)
            .foregroundStyle(tint)
            .lineLimit(1)
            .padding(.horizontal, 5)
            .padding(.vertical, 2)
            .background(
                RoundedRectangle(cornerRadius: Tokens.Radius.chip, style: .continuous)
                    .fill(Tokens.Ink.primary.opacity(Tokens.Fill.subtle))
            )
    }
}
