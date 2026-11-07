import Foundation

/// One session's terminal as the phone knows it: the live screen from the
/// stream, and the scrollback cache behind it.
///
/// Lines are addressed by absolute index: history is `0 ..< historySize`
/// (oldest first) and the visible screen follows at `historySize ..< total`.
/// That is the order they are drawn in, so the view maps a scroll offset to an
/// index with one division.
struct TerminalBuffer {
    private(set) var cols = 80
    private(set) var rows = 24
    private(set) var seq: UInt64 = 0
    private(set) var historySize = 0
    private(set) var cursor = TermCursor()
    private(set) var modes = TermModes()
    private(set) var palette = TermPalette.standard
    private(set) var screen: [TermLine] = []
    private(set) var hasSnapshot = false
    var cache: LineCache

    init(cacheCapacity: Int = 40_000) {
        cache = LineCache(capacity: cacheCapacity)
    }

    var totalLines: Int { historySize + screen.count }

    /// What changed, so the view repaints only that.
    struct Change: Equatable {
        /// Absolute indices whose content changed.
        var lines: IndexSet = []
        var geometry = false
        var cursor = false
        var everything = false

        static let none = Change()
        var isEmpty: Bool { lines.isEmpty && !geometry && !cursor && !everything }
    }

    enum DiffResult: Equatable {
        case applied(Change)
        /// Older than (or the same as) what is already shown; ignored.
        case stale
        /// Arrived before any snapshot; the caller must fetch one.
        case needsSnapshot
    }

    @discardableResult
    mutating func apply(_ snapshot: GridSnapshot) -> Change {
        let reflowed = hasSnapshot && (snapshot.cols != cols || snapshot.historySize < historySize)
        if reflowed { cache.removeAll() }
        cols = max(1, snapshot.cols)
        rows = max(1, snapshot.rows)
        seq = snapshot.seq
        historySize = max(0, snapshot.historySize)
        cursor = snapshot.cursor
        modes = snapshot.modes
        palette = snapshot.palette
        screen = Self.fit(snapshot.lines, rows: rows)
        hasSnapshot = true
        return Change(geometry: true, cursor: true, everything: true)
    }

    mutating func apply(_ diff: GridDiff) -> DiffResult {
        guard hasSnapshot else { return .needsSnapshot }
        guard diff.seq > seq else { return .stale }
        var change = Change()

        if let newCols = diff.cols, newCols != cols {
            // A width change reflows the history server-side; indices no
            // longer mean what the cache thinks they mean.
            cols = max(1, newCols)
            cache.removeAll()
            change.everything = true
            change.geometry = true
        }
        if let newRows = diff.rows, newRows != rows {
            rows = max(1, newRows)
            screen = Self.fit(screen, rows: rows)
            change.geometry = true
            change.everything = true
        }

        if let newHistory = diff.historySize, newHistory != historySize {
            // The rows that scrolled off are not inferred from the screen: the
            // server may send the scrolled rows and the new history size in
            // separate diffs, so the screen has already moved by the time the
            // size arrives. The newest history page is fetched instead (the
            // view always keeps two pages around the screen cached).
            if newHistory < historySize { cache.truncate(from: newHistory) }
            historySize = max(0, newHistory)
            change.geometry = true
        }

        for (row, line) in diff.lines where row >= 0 && row < rows {
            if screen[row] != line {
                screen[row] = line
                change.lines.insert(historySize + row)
            }
        }
        if let newCursor = diff.cursor, newCursor != cursor {
            cursor = newCursor
            change.cursor = true
        }
        if let newModes = diff.modes { modes = newModes }
        if let newPalette = diff.palette, newPalette != palette {
            palette = newPalette
            change.everything = true
        }
        seq = diff.seq
        return .applied(change)
    }

    /// `?resume=` for a reconnect: this screen's width, height and one digest
    /// per visible row. Nil before the first snapshot.
    var resumeToken: String? {
        guard hasSnapshot, !screen.isEmpty, screen.count <= 300 else { return nil }
        var token = "\(cols)x\(screen.count)"
        for line in screen {
            token += "."
            token += String(line.wireDigest, radix: 16)
        }
        return token
    }

    /// A `resume` frame: the rows that changed while the stream was down,
    /// with cursor, modes, palette and history size. It restarts the frame
    /// sequence, since a gateway that restarted counts from one again. Nil
    /// when it does not fit this screen; the caller then asks for a snapshot.
    mutating func resume(_ frame: GridDiff) -> Change? {
        guard hasSnapshot, (frame.cols ?? cols) == cols, (frame.rows ?? rows) == rows, frame.seq > 0 else { return nil }
        seq = 0
        guard case let .applied(change) = apply(frame) else { return nil }
        return change
    }

    /// Stores a history page.
    mutating func apply(_ page: HistoryPage, requestedStart: Int? = nil, protecting: ClosedRange<Int>? = nil) -> Change {
        var change = Change()
        if let requestedStart, page.start > requestedStart { cache.setFloor(page.start) }
        if page.historySize < historySize { cache.truncate(from: page.historySize) }
        let usable = page.lines.enumerated().filter { page.start + $0.offset < historySize }.map(\.element)
        cache.insert(usable, at: page.start, protecting: protecting)
        if !usable.isEmpty { change.lines.insert(integersIn: page.start ..< page.start + usable.count) }
        return change
    }

    /// The line at an absolute index, or nil if it is history not yet fetched.
    mutating func line(at index: Int) -> TermLine? {
        if index >= historySize {
            let row = index - historySize
            return row < screen.count ? screen[row] : nil
        }
        return cache.line(at: index)
    }

    /// Plain text of the visible screen, for accessibility and tests.
    var screenText: String {
        var lines = screen.map(\.text)
        while lines.last?.isEmpty == true { lines.removeLast() }
        return lines.joined(separator: "\n")
    }

    private static func fit(_ lines: [TermLine], rows: Int) -> [TermLine] {
        if lines.count == rows { return lines }
        if lines.count > rows { return Array(lines.suffix(rows)) }
        return lines + Array(repeating: .blank, count: rows - lines.count)
    }
}
