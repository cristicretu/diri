import UIKit

/// `UITextInput` for the terminal, so input methods work: Pinyin, Kana,
/// Hangul, the emoji keyboard and dictation all need a marked-text
/// document, which `UIKeyInput` alone does not offer.
///
/// The document is only the composition in progress. While it is marked it
/// is drawn over the cursor, underlined, and nothing reaches the program;
/// the committed candidate arrives through `insertText` and is sent once.
/// Committed text is never kept here, so the keyboard cannot "correct" or
/// delete what the program already received.
final class TextPosition: UITextPosition {
    let offset: Int
    init(_ offset: Int) { self.offset = offset }
}

final class TextRange: UITextRange {
    let lower: Int
    let upper: Int
    init(_ lower: Int, _ upper: Int) {
        self.lower = min(lower, upper)
        self.upper = max(lower, upper)
    }

    override var start: UITextPosition { TextPosition(lower) }
    override var end: UITextPosition { TextPosition(upper) }
    override var isEmpty: Bool { lower == upper }
}

extension TerminalView {
    private var length: Int { marked.utf16.count }

    private func clamp(_ offset: Int) -> Int { max(0, min(length, offset)) }

    private func offset(_ position: UITextPosition) -> Int { clamp((position as? TextPosition)?.offset ?? 0) }

    private func bounds(_ range: UITextRange) -> (Int, Int) {
        guard let range = range as? TextRange else { return (0, 0) }
        return (clamp(range.lower), clamp(range.upper))
    }

    private func substring(_ lower: Int, _ upper: Int) -> String {
        let utf16 = Array(marked.utf16)
        return String(decoding: utf16[lower ..< upper], as: UTF16.self)
    }

    // MARK: Composition

    func setMarkedText(_ markedText: String?, selectedRange: NSRange) {
        let text = markedText ?? ""
        inputDelegate?.textWillChange(self)
        marked = text
        markedSelection = NSRange(location: min(selectedRange.location, text.utf16.count), length: 0)
        inputDelegate?.textDidChange(self)
        placeMarked()
    }

    /// The input method commits by inserting the candidate (handled in
    /// `insertText`) or, for some keyboards, by unmarking what it has.
    func unmarkText() {
        guard !marked.isEmpty else { return }
        let text = marked
        clearMarked()
        insertText(text)
    }

    func clearMarked() {
        inputDelegate?.textWillChange(self)
        marked = ""
        markedSelection = NSRange(location: 0, length: 0)
        inputDelegate?.textDidChange(self)
        placeMarked()
    }

    var markedTextRange: UITextRange? { marked.isEmpty ? nil : TextRange(0, length) }

    var markedTextStyle: [NSAttributedString.Key: Any]? {
        get { nil }
        set {}
    }

    var selectedTextRange: UITextRange? {
        get { TextRange(markedSelection.location, markedSelection.location + markedSelection.length) }
        set {
            guard let range = newValue as? TextRange else { return }
            markedSelection = NSRange(location: clamp(range.lower), length: clamp(range.upper) - clamp(range.lower))
        }
    }

    /// Draws the composition at the cursor: the program has not seen it, so
    /// it must not look like terminal text.
    func placeMarked() {
        guard !marked.isEmpty else {
            markedLabel.isHidden = true
            return
        }
        let caret = caretRect
        let attributes: [NSAttributedString.Key: Any] = [
            .font: UIFont.monospacedSystemFont(ofSize: font.size, weight: .regular),
            .foregroundColor: UIColor.white,
            .underlineStyle: NSUnderlineStyle.single.rawValue,
            .underlineColor: UIColor(Tokens.Ink.clay),
        ]
        markedLabel.attributedText = NSAttributedString(string: marked, attributes: attributes)
        markedLabel.backgroundColor = UIColor(cgColor: colors.background).withAlphaComponent(0.96)
        markedLabel.sizeToFit()
        markedLabel.frame = CGRect(x: caret.minX, y: caret.minY, width: markedLabel.bounds.width + 2, height: caret.height)
        markedLabel.isHidden = false
        canvas.bringSubviewToFront(markedLabel)
    }

    /// The terminal cursor's cell, where candidates and the emoji popover
    /// anchor.
    var caretRect: CGRect {
        let buffer = session.buffer
        let x = padding + CGFloat(buffer.cursor.x) * font.cellWidth
        let y = CGFloat(buffer.historySize + min(buffer.cursor.y, max(0, buffer.rows - 1))) * font.lineHeight
        return CGRect(x: x, y: y, width: font.cellWidth, height: font.lineHeight)
    }

    // MARK: Document

    func text(in range: UITextRange) -> String? {
        let (lower, upper) = bounds(range)
        return substring(lower, upper)
    }

    func replace(_ range: UITextRange, withText text: String) {
        // Only ever asked of the composition (autocorrect is off); anything
        // else is a plain insertion.
        if marked.isEmpty {
            insertText(text)
            return
        }
        let (lower, upper) = bounds(range)
        let replaced = substring(0, lower) + text + substring(upper, length)
        setMarkedText(replaced, selectedRange: NSRange(location: lower + text.utf16.count, length: 0))
    }

    var beginningOfDocument: UITextPosition { TextPosition(0) }
    var endOfDocument: UITextPosition { TextPosition(length) }

    func textRange(from fromPosition: UITextPosition, to toPosition: UITextPosition) -> UITextRange? {
        TextRange(offset(fromPosition), offset(toPosition))
    }

    func position(from position: UITextPosition, offset: Int) -> UITextPosition? {
        let target = self.offset(position) + offset
        return (0 ... length).contains(target) ? TextPosition(target) : nil
    }

    func position(from position: UITextPosition, in direction: UITextLayoutDirection, offset: Int) -> UITextPosition? {
        switch direction {
        case .left, .up: self.position(from: position, offset: -offset)
        default: self.position(from: position, offset: offset)
        }
    }

    func compare(_ position: UITextPosition, to other: UITextPosition) -> ComparisonResult {
        let (a, b) = (offset(position), offset(other))
        return a < b ? .orderedAscending : (a > b ? .orderedDescending : .orderedSame)
    }

    func offset(from: UITextPosition, to toPosition: UITextPosition) -> Int { offset(toPosition) - offset(from) }

    func position(within range: UITextRange, farthestIn direction: UITextLayoutDirection) -> UITextPosition? {
        let (lower, upper) = bounds(range)
        switch direction {
        case .left, .up: return TextPosition(lower)
        default: return TextPosition(upper)
        }
    }

    func characterRange(byExtending position: UITextPosition, in direction: UITextLayoutDirection) -> UITextRange? {
        let at = offset(position)
        switch direction {
        case .left, .up: return TextRange(0, at)
        default: return TextRange(at, length)
        }
    }

    func baseWritingDirection(for position: UITextPosition, in direction: UITextStorageDirection) -> NSWritingDirection { .leftToRight }

    func setBaseWritingDirection(_ writingDirection: NSWritingDirection, for range: UITextRange) {}

    // MARK: Geometry

    func firstRect(for range: UITextRange) -> CGRect {
        let (lower, upper) = bounds(range)
        let caret = caretRect
        let cell = font.cellWidth
        return CGRect(x: caret.minX + CGFloat(lower) * cell, y: caret.minY,
                      width: max(cell, CGFloat(upper - lower) * cell), height: caret.height)
    }

    func caretRect(for position: UITextPosition) -> CGRect {
        let caret = caretRect
        return CGRect(x: caret.minX + CGFloat(offset(position)) * font.cellWidth, y: caret.minY, width: 2, height: caret.height)
    }

    func selectionRects(for range: UITextRange) -> [UITextSelectionRect] { [] }

    func closestPosition(to point: CGPoint) -> UITextPosition? { TextPosition(length) }

    func closestPosition(to point: CGPoint, within range: UITextRange) -> UITextPosition? { range.end }

    func characterRange(at point: CGPoint) -> UITextRange? { nil }
}
