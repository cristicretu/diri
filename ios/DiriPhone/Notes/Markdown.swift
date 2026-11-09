import Foundation

/// What the note editor draws: Markdown styled in place. The text stays
/// plain Markdown (it is what the Mac's notes store keeps), and the styling
/// is how it reads as a document while it is typed into: headings large,
/// emphasis emphasised, markers dimmed rather than hidden.
///
/// Pure and line based, so it is cheap enough to rerun on every keystroke
/// for a note of any size the store will open, and testable without UIKit.
enum MarkdownStyle {
    enum Kind: Equatable {
        /// `#` levels; level 1 on the first line is the note's title.
        case heading(Int)
        /// Syntax that stays visible but quiet: `#`, `-`, `**`, `[[`.
        case marker
        case todoBox(checked: Bool)
        case doneText
        case quote
        case rule
        case code
        case codeBlock
        case bold
        case italic
        case strike
        /// `[[Title]]`, a link to another note.
        case noteLink
        case link
        /// Syntax folded out of sight: the `](diri://note/…)` of a note
        /// mention, which says nothing a reader needs.
        case folded
    }

    struct Run: Equatable {
        var range: NSRange
        var kind: Kind
    }

    static func runs(in text: String) -> [Run] {
        let source = text as NSString
        var runs: [Run] = []
        var inFence = false
        var firstLine = true
        source.enumerateSubstrings(
            in: NSRange(location: 0, length: source.length), options: [.byLines, .substringNotRequired]
        ) { _, line, _, _ in
            defer { firstLine = false }
            let content = source.substring(with: line)
            if content.hasPrefix("```") {
                runs.append(Run(range: line, kind: .codeBlock))
                runs.append(Run(range: line, kind: .marker))
                inFence.toggle()
                return
            }
            if inFence {
                runs.append(Run(range: line, kind: .codeBlock))
                return
            }
            runs += blockRuns(content, line: line, firstLine: firstLine)
            runs += inlineRuns(content, offset: line.location)
        }
        return runs
    }

    // MARK: - Blocks

    private static let heading = regex(#"^(#{1,6}) "#)
    private static let todo = regex(#"^(\s*)([-*+] \[([ xX])\]) "#)
    private static let bullet = regex(#"^(\s*)([-*+]|\d+[.)]) "#)
    private static let quote = regex(#"^> ?"#)
    private static let rule = regex(#"^ {0,3}([-*_])( *\1){2,} *$"#)

    private static func blockRuns(_ content: String, line: NSRange, firstLine: Bool) -> [Run] {
        let local = NSRange(location: 0, length: (content as NSString).length)
        func shifted(_ range: NSRange) -> NSRange { NSRange(location: range.location + line.location, length: range.length) }

        if let match = heading.firstMatch(in: content, range: local) {
            let level = match.range(at: 1).length
            return [
                Run(range: line, kind: .heading(firstLine && level == 1 ? 0 : level)),
                Run(range: shifted(match.range(at: 1)), kind: .marker),
            ]
        }
        if rule.firstMatch(in: content, range: local) != nil {
            return [Run(range: line, kind: .rule)]
        }
        if let match = todo.firstMatch(in: content, range: local) {
            let checked = (content as NSString).substring(with: match.range(at: 3)) != " "
            var runs = [Run(range: shifted(match.range(at: 2)), kind: .todoBox(checked: checked))]
            if checked {
                let start = match.range.location + match.range.length
                runs.append(Run(range: shifted(NSRange(location: start, length: local.length - start)), kind: .doneText))
            }
            return runs
        }
        if let match = bullet.firstMatch(in: content, range: local) {
            return [Run(range: shifted(match.range(at: 2)), kind: .marker)]
        }
        if let match = quote.firstMatch(in: content, range: local) {
            return [Run(range: line, kind: .quote), Run(range: shifted(match.range), kind: .marker)]
        }
        return []
    }

    // MARK: - Inline

    /// (pattern, kind of the whole match, delimiter length on each side).
    private static let inline: [(NSRegularExpression, Kind, Int)] = [
        (regex(#"`[^`\n]+`"#), .code, 1),
        (regex(#"\[\[[^\]\n]+\]\]"#), .noteLink, 2),
        (regex(#"(?<!\*)\*\*[^*\n]+\*\*(?!\*)"#), .bold, 2),
        (regex(#"(?<![\w_])__[^_\n]+__(?![\w_])"#), .bold, 2),
        (regex(#"(?<![*\w])\*[^*\s][^*\n]*(?<!\s)\*(?![*\w])"#), .italic, 1),
        (regex(#"(?<![_\w])_[^_\s][^_\n]*(?<!\s)_(?![_\w])"#), .italic, 1),
        (regex(#"~~[^~\n]+~~"#), .strike, 2),
    ]
    private static let escape = regex(#"\\[\\`*_{}\[\]()#+\-.!>~|]"#)
    private static let link = regex(#"(?<!\!)\[([^\]\n]+)\]\(([^)\s]+)\)"#)

    private static func inlineRuns(_ content: String, offset: Int) -> [Run] {
        let local = NSRange(location: 0, length: (content as NSString).length)
        var runs: [Run] = []
        var code: [NSRange] = []
        for (pattern, kind, delimiter) in inline {
            for match in pattern.matches(in: content, range: local) {
                let range = match.range
                // Nothing inside a code span is Markdown.
                if kind != .code, code.contains(where: { NSIntersectionRange($0, range).length > 0 }) { continue }
                if kind == .code { code.append(range) }
                runs.append(Run(range: NSRange(location: range.location + offset, length: range.length), kind: kind))
                runs.append(Run(range: NSRange(location: range.location + offset, length: delimiter), kind: .marker))
                runs.append(Run(
                    range: NSRange(location: range.location + range.length - delimiter + offset, length: delimiter),
                    kind: .marker
                ))
            }
        }
        // The notes store escapes a literal `*`, `[` or `_` with a backslash;
        // the backslash is syntax, so it reads as quietly as a marker.
        for match in escape.matches(in: content, range: local)
            where !code.contains(where: { NSLocationInRange(match.range.location, $0) }) {
            runs.append(Run(range: NSRange(location: match.range.location + offset, length: 1), kind: .marker))
        }
        for match in link.matches(in: content, range: local) {
            if code.contains(where: { NSIntersectionRange($0, match.range).length > 0 }) { continue }
            let text = match.range(at: 1)
            let mention = (content as NSString).substring(with: match.range(at: 2)).hasPrefix("diri://")
            runs.append(Run(range: NSRange(location: text.location + offset, length: text.length),
                            kind: mention ? .noteLink : .link))
            // `[`, then `](url)`. A note mention reads as its title alone,
            // `@Plan`, as on the Mac.
            runs.append(Run(range: NSRange(location: match.range.location + offset, length: 1),
                            kind: mention ? .folded : .marker))
            let tail = text.location + text.length
            runs.append(Run(
                range: NSRange(location: tail + offset, length: match.range.location + match.range.length - tail),
                kind: mention ? .folded : .marker
            ))
        }
        return runs
    }

    private static func regex(_ pattern: String) -> NSRegularExpression {
        // The patterns are literals; a typo is a programming error caught by
        // the first test run, not a runtime condition.
        try! NSRegularExpression(pattern: pattern)
    }
}

/// The edits the Markdown key bar makes. Each returns the replacement for
/// one range and where the selection lands, so the text view can apply it as
/// one undoable change.
enum MarkdownEdit {
    struct Change: Equatable {
        var range: NSRange
        var replacement: String
        var selection: NSRange
    }

    /// Wraps the selection in `marker` (`**`, `*`, `` ` ``), or unwraps it
    /// when it is wrapped already. An empty selection gets an empty pair with
    /// the caret inside.
    static func toggleWrap(_ text: String, selection: NSRange, open: String, close: String? = nil) -> Change {
        let source = text as NSString
        let close = close ?? open
        let openLength = (open as NSString).length
        let closeLength = (close as NSString).length
        let selected = source.substring(with: selection)
        if selected.hasPrefix(open), selected.hasSuffix(close), (selected as NSString).length >= openLength + closeLength {
            let inner = (selected as NSString).substring(with: NSRange(
                location: openLength, length: (selected as NSString).length - openLength - closeLength
            ))
            return Change(range: selection, replacement: inner,
                          selection: NSRange(location: selection.location, length: (inner as NSString).length))
        }
        let before = NSRange(location: selection.location - openLength, length: openLength)
        let after = NSRange(location: selection.location + selection.length, length: closeLength)
        if before.location >= 0, after.location + after.length <= source.length,
           source.substring(with: before) == open, source.substring(with: after) == close
        {
            let whole = NSRange(location: before.location, length: openLength + selection.length + closeLength)
            return Change(range: whole, replacement: selected,
                          selection: NSRange(location: before.location, length: selection.length))
        }
        return Change(range: selection, replacement: open + selected + close,
                      selection: NSRange(location: selection.location + openLength, length: selection.length))
    }

    /// The prefixes a line can carry, longest first so `- [ ] ` is not read
    /// as a bullet with text `[ ] `.
    static let linePrefixes = ["- [ ] ", "- [x] ", "- [X] ", "### ", "## ", "# ", "> ", "- ", "* ", "+ "]

    /// Gives every line the selection touches `prefix`, or takes it away when
    /// they all have it already. A line's other prefix is replaced, so a
    /// bullet becomes a to-do rather than `- [ ] - item`.
    static func toggleLinePrefix(_ text: String, selection: NSRange, prefix: String) -> Change {
        let source = text as NSString
        let lines = source.lineRange(for: selection)
        let block = source.substring(with: lines)
        var parts = block.components(separatedBy: "\n")
        let trailingNewline = block.hasSuffix("\n")
        if trailingNewline { parts.removeLast() }
        let all = !parts.isEmpty && parts.allSatisfy {
            strip(indentOf($0).rest).prefix == prefix || (prefix == "- [ ] " && isTodo($0))
        }
        var delta = 0
        let rewritten = parts.map { line -> String in
            let (indent, rest) = indentOf(line)
            let (current, body) = strip(rest)
            let next = all ? "" : prefix
            delta += (next as NSString).length - (current as NSString).length
            return indent + next + body
        }
        let replacement = rewritten.joined(separator: "\n") + (trailingNewline ? "\n" : "")
        let caret = max(lines.location, selection.location + (parts.count == 1 ? delta : 0))
        return Change(
            range: lines, replacement: replacement,
            selection: parts.count == 1
                ? NSRange(location: caret, length: selection.length)
                : NSRange(location: lines.location, length: (replacement as NSString).length - (trailingNewline ? 1 : 0))
        )
    }

    /// Indents (or outdents) every line the selection touches by two spaces,
    /// which is how the notes store nests list items.
    static func indent(_ text: String, selection: NSRange, outdent: Bool) -> Change {
        let source = text as NSString
        let lines = source.lineRange(for: selection)
        let block = source.substring(with: lines)
        var parts = block.components(separatedBy: "\n")
        let trailingNewline = block.hasSuffix("\n")
        if trailingNewline { parts.removeLast() }
        var firstDelta = 0
        let rewritten = parts.enumerated().map { index, line -> String in
            let removed = outdent ? min(2, line.prefix(while: { $0 == " " }).count) : -2
            if index == 0 { firstDelta = -removed }
            return outdent ? String(line.dropFirst(removed)) : "  " + line
        }
        let replacement = rewritten.joined(separator: "\n") + (trailingNewline ? "\n" : "")
        let caret = max(lines.location, selection.location + firstDelta)
        return Change(range: lines, replacement: replacement,
                      selection: NSRange(location: caret, length: parts.count == 1 ? selection.length : 0))
    }

    /// Flips the to-do box on the line containing `location`, if it has one.
    static func toggleTodo(_ text: String, at location: Int) -> Change? {
        let source = text as NSString
        guard location <= source.length else { return nil }
        let line = source.lineRange(for: NSRange(location: location, length: 0))
        let content = source.substring(with: line)
        let (indent, rest) = indentOf(content)
        let box: String
        if rest.hasPrefix("- [ ] ") { box = "[x]" } else if rest.hasPrefix("- [x] ") || rest.hasPrefix("- [X] ") { box = "[ ]" } else { return nil }
        let at = line.location + (indent as NSString).length + 2
        return Change(range: NSRange(location: at, length: 3), replacement: box,
                      selection: NSRange(location: location, length: 0))
    }

    /// Return inside a list item continues the list; Return on an empty item
    /// ends it. `nil` means a plain newline.
    static func newline(_ text: String, selection: NSRange) -> Change? {
        guard selection.length == 0 else { return nil }
        let source = text as NSString
        let line = source.lineRange(for: selection)
        let lineEnd = line.location + line.length - (source.substring(with: line).hasSuffix("\n") ? 1 : 0)
        let before = source.substring(with: NSRange(location: line.location, length: selection.location - line.location))
        let (indent, rest) = indentOf(before)
        let marker: String
        if rest.hasPrefix("- [ ] ") || rest.hasPrefix("- [x] ") || rest.hasPrefix("- [X] ") {
            marker = "- [ ] "
        } else if let bullet = ["- ", "* ", "+ ", "> "].first(where: rest.hasPrefix) {
            marker = bullet
        } else if let (number, delimiter) = ordinal(rest) {
            marker = "\(number + 1)\(delimiter) "
        } else {
            return nil
        }
        let wholeLine = source.substring(with: NSRange(location: line.location, length: lineEnd - line.location))
        let (prefix, body) = strip(indentOf(wholeLine).rest)
        if !prefix.isEmpty, body.trimmingCharacters(in: .whitespaces).isEmpty {
            // An empty item: Return ends the list instead of adding another.
            let range = NSRange(location: line.location, length: lineEnd - line.location)
            return Change(range: range, replacement: "", selection: NSRange(location: line.location, length: 0))
        }
        let insertion = "\n" + indent + marker
        return Change(range: selection, replacement: insertion,
                      selection: NSRange(location: selection.location + (insertion as NSString).length, length: 0))
    }

    private static func isTodo(_ line: String) -> Bool {
        let rest = indentOf(line).rest
        return rest.hasPrefix("- [ ] ") || rest.hasPrefix("- [x] ") || rest.hasPrefix("- [X] ")
    }

    private static func indentOf(_ line: String) -> (indent: String, rest: String) {
        let indent = String(line.prefix(while: { $0 == " " || $0 == "\t" }))
        return (indent, String(line.dropFirst(indent.count)))
    }

    private static func strip(_ rest: String) -> (prefix: String, body: String) {
        if let prefix = linePrefixes.first(where: rest.hasPrefix) {
            return (prefix, String(rest.dropFirst(prefix.count)))
        }
        if let (number, delimiter) = ordinal(rest) {
            let prefix = "\(number)\(delimiter) "
            return (prefix, String(rest.dropFirst(prefix.count)))
        }
        return ("", rest)
    }

    /// `12. ` or `12) ` at the start of `rest`.
    private static func ordinal(_ rest: String) -> (Int, Character)? {
        let digits = rest.prefix(while: { $0.isASCII && $0.isNumber })
        guard !digits.isEmpty, digits.count < 10, let number = Int(digits) else { return nil }
        let tail = rest.dropFirst(digits.count)
        guard let delimiter = tail.first, delimiter == "." || delimiter == ")", tail.dropFirst().first == " " else { return nil }
        return (number, delimiter)
    }
}
