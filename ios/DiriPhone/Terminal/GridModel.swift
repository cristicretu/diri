import Foundation

// The terminal-stream wire types served by `diri-web`:
//
//   GET  /api/session/{id}/grid                     -> GridSnapshot
//   GET  /api/session/{id}/history?before=N&limit=M -> HistoryPage
//   SSE  /api/session/{id}/stream                   -> snapshot | diff | exit
//   POST /api/session/{id}/input {data: base64}
//
// Every field a newer server might drop decodes to a neutral default, so a
// missing key costs a colour, never the whole screen.

/// A cell colour: the terminal's default, a 256-colour palette index, or 24-bit.
enum TermColor: Hashable, Sendable {
    case standard
    case index(UInt8)
    case rgb(UInt32)
}

extension TermColor: Decodable {
    init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .standard
        } else if let index = try? container.decode(Int.self) {
            self = (0 ... 255).contains(index) ? .index(UInt8(index)) : .standard
        } else if let hex = try? container.decode(String.self), let value = TermColor.parseHex(hex) {
            self = .rgb(value)
        } else {
            self = .standard
        }
    }

    /// `#rrggbb` → 0xRRGGBB.
    static func parseHex(_ hex: String) -> UInt32? {
        var text = Substring(hex)
        if text.hasPrefix("#") { text = text.dropFirst() }
        guard text.count == 6, let value = UInt32(text, radix: 16) else { return nil }
        return value
    }
}

/// Run attribute bits.
struct TermAttributes: OptionSet, Hashable, Sendable {
    let rawValue: UInt8
    static let bold = TermAttributes(rawValue: 1)
    static let dim = TermAttributes(rawValue: 2)
    static let italic = TermAttributes(rawValue: 4)
    static let underline = TermAttributes(rawValue: 8)
    static let inverse = TermAttributes(rawValue: 16)
    static let strike = TermAttributes(rawValue: 32)
    static let invisible = TermAttributes(rawValue: 64)
}

/// A span of cells sharing one style. Wide characters occupy two columns and
/// appear once in `text`.
struct TermRun: Hashable, Sendable {
    var text: String
    var fg: TermColor
    var bg: TermColor
    var attributes: TermAttributes
    /// The run's grid cells, from the gateway (`g`): nil for plain ASCII or
    /// an older gateway, when one character is one column.
    var cells: [CellGroup]?

    init(_ text: String, fg: TermColor = .standard, bg: TermColor = .standard, attributes: TermAttributes = [],
         cells: [CellGroup]? = nil) {
        self.text = text
        self.fg = fg
        self.bg = bg
        self.attributes = attributes
        self.cells = cells
    }
}

/// `count` consecutive cells, each holding `units` UTF-16 units of the run's
/// text and covering `width` columns: where the grid put each character, so
/// the phone never works widths out from its own Unicode tables.
struct CellGroup: Hashable, Sendable {
    var count: Int
    var units: Int
    var width: Int
}

extension TermRun {
    /// The run as (text, columns) cells. From the gateway's cells when it
    /// sent them, otherwise one grapheme per cell with a local width guess.
    var cellTexts: [(text: Substring, width: Int)] {
        if let cells {
            // Cells are cut on Unicode scalars, not graphemes: the Engine may
            // give a vowel sign a cell of its own where Swift would fold it
            // into the consonant's Character.
            var result: [(Substring, Int)] = []
            let scalars = text.unicodeScalars
            var index = text.utf16.startIndex
            for group in cells {
                for _ in 0 ..< group.count {
                    guard let end = text.utf16.index(index, offsetBy: group.units, limitedBy: text.utf16.endIndex)
                    else { return result }
                    result.append((Substring(scalars[index ..< end]), group.width))
                    index = end
                }
            }
            return result
        }
        return text.indices.map { i in
            let character = text[i]
            return (text[i ..< text.index(after: i)], CellWidth.of(character))
        }
    }

    /// Columns the run covers.
    var columns: Int {
        if let cells { return cells.reduce(0) { $0 + $1.count * $1.width } }
        return CellWidth.of(text)
    }
}

extension TermRun: Decodable {
    private enum Keys: String, CodingKey { case t, fg, bg, a, g }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: Keys.self)
        text = (try? container.decode(String.self, forKey: .t)) ?? ""
        fg = (try? container.decodeIfPresent(TermColor.self, forKey: .fg)) ?? .standard
        bg = (try? container.decodeIfPresent(TermColor.self, forKey: .bg)) ?? .standard
        let bits = (try? container.decodeIfPresent(Int.self, forKey: .a)) ?? 0
        attributes = TermAttributes(rawValue: UInt8(truncatingIfNeeded: bits))
        let groups = (try? container.decodeIfPresent([[Int]].self, forKey: .g)) ?? nil
        cells = groups.flatMap { list in
            let parsed = list.compactMap { $0.count == 3 && $0[0] > 0 && $0[1] > 0 && $0[2] >= 0
                ? CellGroup(count: $0[0], units: $0[1], width: $0[2]) : nil }
            // A malformed list is ignored rather than trusted halfway.
            return parsed.count == list.count ? parsed : nil
        }
    }
}

struct TermLine: Hashable, Sendable {
    var runs: [TermRun]
    var wrapped: Bool

    init(runs: [TermRun], wrapped: Bool = false) {
        self.runs = runs
        self.wrapped = wrapped
    }

    static let blank = TermLine(runs: [])

    /// The line as plain text, trailing blanks removed.
    var text: String {
        var result = ""
        for run in runs where !run.attributes.contains(.invisible) { result += run.text }
        while result.last == " " { result.removeLast() }
        return result
    }

    /// True when any cell carries a colour other than the default.
    var isColoured: Bool {
        runs.contains { $0.fg != .standard || $0.bg != .standard || $0.attributes.contains(.inverse) }
    }
}

extension TermLine: Decodable {
    private enum Keys: String, CodingKey { case runs, wrapped }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: Keys.self)
        runs = (try? container.decode([TermRun].self, forKey: .runs)) ?? []
        wrapped = (try? container.decodeIfPresent(Bool.self, forKey: .wrapped)) ?? false
    }
}

struct TermCursor: Hashable, Sendable, Decodable {
    enum Shape: String, Sendable { case block, bar, underline }

    var x: Int
    var y: Int
    var visible: Bool
    var shape: Shape
    /// The program did not ask for a steady cursor (DECSCUSR).
    var blink: Bool

    init(x: Int = 0, y: Int = 0, visible: Bool = true, shape: Shape = .block, blink: Bool = false) {
        self.x = x
        self.y = y
        self.visible = visible
        self.shape = shape
        self.blink = blink
    }

    private enum Keys: String, CodingKey { case x, y, visible, shape, blink }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: Keys.self)
        x = (try? container.decode(Int.self, forKey: .x)) ?? 0
        y = (try? container.decode(Int.self, forKey: .y)) ?? 0
        visible = (try? container.decode(Bool.self, forKey: .visible)) ?? true
        let raw = ((try? container.decode(String.self, forKey: .shape)) ?? "block").lowercased()
        shape = raw.contains("bar") || raw.contains("beam") ? .bar : (raw.contains("under") ? .underline : .block)
        blink = (try? container.decode(Bool.self, forKey: .blink)) ?? false
    }
}

struct TermModes: Hashable, Sendable, Decodable {
    var altScreen = false
    var appCursor = false
    var bracketedPaste = false
    var mouse = false

    init(altScreen: Bool = false, appCursor: Bool = false, bracketedPaste: Bool = false, mouse: Bool = false) {
        self.altScreen = altScreen
        self.appCursor = appCursor
        self.bracketedPaste = bracketedPaste
        self.mouse = mouse
    }

    private enum Keys: String, CodingKey { case altScreen, appCursor, bracketedPaste, mouse }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: Keys.self)
        altScreen = (try? container.decode(Bool.self, forKey: .altScreen)) ?? false
        appCursor = (try? container.decode(Bool.self, forKey: .appCursor)) ?? false
        bracketedPaste = (try? container.decode(Bool.self, forKey: .bracketedPaste)) ?? false
        // A mouse mode may arrive as a flag or as the mode's name.
        if let flag = try? container.decode(Bool.self, forKey: .mouse) {
            mouse = flag
        } else if let name = try? container.decode(String.self, forKey: .mouse) {
            mouse = !name.isEmpty && name != "none" && name != "off"
        }
    }
}

/// The session's own colours. Absent fields fall back to Diri's dark theme.
struct TermPalette: Hashable, Sendable, Decodable {
    var foreground: UInt32
    var background: UInt32
    var ansi: [UInt32]

    static let standard = TermPalette(
        foreground: 0xD4D7DD,
        background: 0x121318,
        ansi: [
            0x1D1F27, 0xF0605A, 0x6CCB7E, 0xE8C46B, 0x6C9EF8, 0xC98AF0, 0x5FC7D6, 0xC8CCD4,
            0x5C6170, 0xFF7A73, 0x8BE19C, 0xF5D78A, 0x8DB5FF, 0xDBA7F7, 0x82DCE8, 0xF4F5F7,
        ]
    )

    init(foreground: UInt32, background: UInt32, ansi: [UInt32]) {
        self.foreground = foreground
        self.background = background
        self.ansi = ansi
    }

    private enum Keys: String, CodingKey { case fg, bg, ansi }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: Keys.self)
        let fallback = TermPalette.standard
        foreground = (try? container.decode(String.self, forKey: .fg)).flatMap(TermColor.parseHex) ?? fallback.foreground
        background = (try? container.decode(String.self, forKey: .bg)).flatMap(TermColor.parseHex) ?? fallback.background
        let parsed = ((try? container.decode([String].self, forKey: .ansi)) ?? []).compactMap(TermColor.parseHex)
        ansi = parsed.count == 16 ? parsed : fallback.ansi
    }

    /// Resolves an index of the 256-colour table.
    func color(index: UInt8) -> UInt32 {
        let i = Int(index)
        if i < 16 { return ansi[i] }
        if i < 232 {
            let n = i - 16
            let steps: [UInt32] = [0, 95, 135, 175, 215, 255]
            return steps[n / 36] << 16 | steps[(n / 6) % 6] << 8 | steps[n % 6]
        }
        let level = UInt32(8 + (i - 232) * 10)
        return level << 16 | level << 8 | level
    }
}

struct GridSnapshot: Sendable, Decodable {
    var cols: Int
    var rows: Int
    var seq: UInt64
    var historySize: Int
    var cursor: TermCursor
    var modes: TermModes
    var palette: TermPalette
    var lines: [TermLine]

    init(cols: Int, rows: Int, seq: UInt64, historySize: Int, cursor: TermCursor = TermCursor(),
         modes: TermModes = TermModes(), palette: TermPalette = .standard, lines: [TermLine]) {
        self.cols = cols
        self.rows = rows
        self.seq = seq
        self.historySize = historySize
        self.cursor = cursor
        self.modes = modes
        self.palette = palette
        self.lines = lines
    }

    private enum Keys: String, CodingKey { case cols, rows, seq, historySize, cursor, modes, palette, lines }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: Keys.self)
        lines = try container.decode([TermLine].self, forKey: .lines)
        cols = (try? container.decode(Int.self, forKey: .cols)) ?? 80
        rows = (try? container.decode(Int.self, forKey: .rows)) ?? lines.count
        seq = (try? container.decode(UInt64.self, forKey: .seq)) ?? 0
        historySize = (try? container.decode(Int.self, forKey: .historySize)) ?? 0
        cursor = (try? container.decode(TermCursor.self, forKey: .cursor)) ?? TermCursor()
        modes = (try? container.decode(TermModes.self, forKey: .modes)) ?? TermModes()
        palette = (try? container.decode(TermPalette.self, forKey: .palette)) ?? .standard
    }
}

struct GridDiff: Sendable, Decodable {
    var seq: UInt64
    /// Changed visible rows, keyed by row number.
    var lines: [Int: TermLine]
    var cursor: TermCursor?
    var modes: TermModes?
    var historySize: Int?
    var cols: Int?
    var rows: Int?
    /// Sent by a `resume` frame (and harmless on a diff).
    var palette: TermPalette?

    init(seq: UInt64, lines: [Int: TermLine], cursor: TermCursor? = nil, modes: TermModes? = nil,
         historySize: Int? = nil, cols: Int? = nil, rows: Int? = nil, palette: TermPalette? = nil) {
        self.seq = seq
        self.lines = lines
        self.cursor = cursor
        self.modes = modes
        self.historySize = historySize
        self.cols = cols
        self.rows = rows
        self.palette = palette
    }

    private enum Keys: String, CodingKey { case seq, lines, cursor, modes, historySize, cols, rows, palette }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: Keys.self)
        seq = try container.decode(UInt64.self, forKey: .seq)
        let keyed = (try? container.decode([String: TermLine].self, forKey: .lines)) ?? [:]
        var lines: [Int: TermLine] = [:]
        for (key, line) in keyed { if let row = Int(key) { lines[row] = line } }
        self.lines = lines
        cursor = try? container.decodeIfPresent(TermCursor.self, forKey: .cursor)
        modes = try? container.decodeIfPresent(TermModes.self, forKey: .modes)
        historySize = try? container.decodeIfPresent(Int.self, forKey: .historySize)
        cols = try? container.decodeIfPresent(Int.self, forKey: .cols)
        rows = try? container.decodeIfPresent(Int.self, forKey: .rows)
        palette = try? container.decodeIfPresent(TermPalette.self, forKey: .palette)
    }
}

struct HistoryPage: Sendable, Decodable {
    var lines: [TermLine]
    /// Absolute index of `lines[0]`.
    var start: Int
    var historySize: Int
}

// MARK: - Resume digests

/// What `diri-web` hashes to decide which rows a reconnecting client still
/// shows correctly (`?resume=COLSxROWS.HEX…`, answered by a `resume` frame).
///
/// The gateway takes FNV-1a over a line's JSON exactly as it serialises it:
/// `{"runs":[{"a":…,"bg":…,"fg":…,"t":"…"}],"wrapped":…}`, keys sorted,
/// serde_json's escapes. The line model keeps every field of that shape, so
/// the bytes are rebuilt here rather than kept from the wire. A digest that
/// differs only costs that row being sent again.
extension TermLine {
    var wireDigest: UInt32 {
        var hash = FNV1a()
        hash.add("{\"runs\":[")
        for (index, run) in runs.enumerated() {
            if index > 0 { hash.add(",") }
            hash.add("{\"a\":")
            hash.add(String(run.attributes.rawValue))
            hash.add(",\"bg\":")
            hash.add(run.bg)
            hash.add(",\"fg\":")
            hash.add(run.fg)
            if let cells = run.cells {
                hash.add(",\"g\":[")
                for (n, group) in cells.enumerated() {
                    hash.add((n > 0 ? ",[" : "[") + "\(group.count),\(group.units),\(group.width)]")
                }
                hash.add("]")
            }
            hash.add(",\"t\":")
            hash.addJSONString(run.text)
            hash.add("}")
        }
        hash.add("],\"wrapped\":")
        hash.add(wrapped ? "true" : "false")
        hash.add("}")
        return hash.value
    }
}

struct FNV1a {
    private(set) var value: UInt32 = 0x811C_9DC5

    mutating func add(byte: UInt8) {
        value = (value ^ UInt32(byte)) &* 0x0100_0193
    }

    mutating func add(_ text: String) {
        for byte in text.utf8 { add(byte: byte) }
    }

    mutating func add(_ color: TermColor) {
        switch color {
        case .standard: add("null")
        case let .index(index): add(String(index))
        case let .rgb(value):
            let hex = String(value, radix: 16)
            add("\"#")
            add(String(repeating: "0", count: max(0, 6 - hex.count)) + hex)
            add("\"")
        }
    }

    /// A JSON string as serde_json writes it: `"` and `\` escaped, the short
    /// forms for \b \f \n \r \t, `\u00xx` (lower-case) for other controls,
    /// and everything else as raw UTF-8.
    mutating func addJSONString(_ text: String) {
        add(byte: UInt8(ascii: "\""))
        for byte in text.utf8 {
            switch byte {
            case UInt8(ascii: "\""): add("\\\"")
            case UInt8(ascii: "\\"): add("\\\\")
            case 0x08: add("\\b")
            case 0x0C: add("\\f")
            case 0x0A: add("\\n")
            case 0x0D: add("\\r")
            case 0x09: add("\\t")
            case 0x00 ..< 0x20:
                let hex = String(byte, radix: 16)
                add("\\u00" + (hex.count == 1 ? "0" + hex : hex))
            default: add(byte: byte)
            }
        }
        add(byte: UInt8(ascii: "\""))
    }
}
