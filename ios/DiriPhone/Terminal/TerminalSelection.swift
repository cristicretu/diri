import UIKit

/// A span of the terminal's text, by absolute line and column, inclusive at
/// both ends, in reading order.
struct TerminalSelection: Equatable {
    struct Point: Equatable, Comparable {
        var line: Int
        var column: Int
        static func < (a: Point, b: Point) -> Bool { (a.line, a.column) < (b.line, b.column) }
    }

    var start: Point
    var end: Point

    init(_ a: Point, _ b: Point) {
        start = min(a, b)
        end = max(a, b)
    }

    /// The selected text. Lines the program wrapped join without a newline;
    /// trailing blanks are dropped from every line, as a terminal copy does.
    /// History not yet fetched contributes nothing.
    func text(lines: (Int) -> TermLine?) -> String {
        var out = ""
        for index in start.line ... end.line {
            guard let line = lines(index) else { continue }
            let from = index == start.line ? start.column : 0
            let to = index == end.line ? end.column : Int.max
            var piece = ""
            for (text, column) in Self.cells(of: line) where column >= from && column <= to {
                piece += text
            }
            let wraps = line.wrapped && index < end.line
            if !wraps { while piece.last == " " { piece.removeLast() } }
            out += piece
            if index < end.line, !wraps { out += "\n" }
        }
        return out
    }

    /// Every cell of a line with the column it starts at, blanks included
    /// up to the last run (cells past it are empty).
    static func cells(of line: TermLine) -> [(text: String, column: Int)] {
        var result: [(String, Int)] = []
        var column = 0
        for run in line.runs {
            for cell in run.cellTexts {
                result.append((run.attributes.contains(.invisible) ? " " : String(cell.text), column))
                column += max(cell.width, 1)
            }
        }
        return result
    }

    /// The word around `column`: the run of non-blank cells it sits in, or
    /// the single cell when it is blank.
    static func word(in line: TermLine, at column: Int) -> ClosedRange<Int> {
        let cells = cells(of: line)
        guard let hit = cells.lastIndex(where: { $0.column <= column }), cells[hit].text != " " else {
            return column ... column
        }
        func isWord(_ text: String) -> Bool { !text.allSatisfy(\.isWhitespace) && !"()[]{}<>\"'`,;|".contains(text) }
        guard isWord(cells[hit].text) else { return cells[hit].column ... cells[hit].column }
        var lower = hit, upper = hit
        while lower > 0, isWord(cells[lower - 1].text) {
            lower -= 1
        }
        while upper < cells.count - 1, isWord(cells[upper + 1].text) { upper += 1 }
        let lastWidth = (upper + 1 < cells.count ? cells[upper + 1].column : cells[upper].column + 1) - cells[upper].column
        return cells[lower].column ... cells[upper].column + max(lastWidth, 1) - 1
    }
}

/// Long-press selection, like any iOS text: hold to select a word, keep
/// holding and drag to extend, let go for Copy. A tap clears it.
extension TerminalView: UIEditMenuInteractionDelegate {
    func installSelection() {
        let press = UILongPressGestureRecognizer(target: self, action: #selector(longPressed(_:)))
        press.minimumPressDuration = 0.4
        addGestureRecognizer(press)
        let menu = UIEditMenuInteraction(delegate: self)
        addInteraction(menu)
        editMenu = menu
        selectionLayer.fillColor = UIColor(Tokens.Ink.clay).withAlphaComponent(0.35).cgColor
        selectionLayer.actions = ["path": NSNull(), "hidden": NSNull()]
        selectionLayer.isHidden = true
        canvas.layer.addSublayer(selectionLayer)
    }

    /// The cell under a point in the canvas.
    func cellPoint(at location: CGPoint) -> TerminalSelection.Point {
        let total = session.buffer.totalLines
        let line = max(0, min(total - 1, Int(location.y / lineHeight)))
        let column = max(0, min(session.buffer.cols - 1, Int((location.x - padding) / font.cellWidth)))
        return TerminalSelection.Point(line: line, column: column)
    }

    @objc private func longPressed(_ gesture: UILongPressGestureRecognizer) {
        let point = cellPoint(at: gesture.location(in: canvas))
        switch gesture.state {
        case .began:
            editMenu?.dismissMenu()
            let line = session.line(at: point.line) ?? .blank
            let word = TerminalSelection.word(in: line, at: point.column)
            let start = TerminalSelection.Point(line: point.line, column: word.lowerBound)
            let end = TerminalSelection.Point(line: point.line, column: word.upperBound)
            selectionAnchor = start
            selection = TerminalSelection(start, end)
            UISelectionFeedbackGenerator().selectionChanged()
            drawSelection()
        case .changed:
            guard let anchor = selectionAnchor else { return }
            let next = TerminalSelection(anchor, point)
            if next != selection {
                selection = next
                drawSelection()
            }
        case .ended:
            showSelectionMenu(at: gesture.location(in: self))
        case .cancelled, .failed:
            break
        default:
            break
        }
    }

    func clearSelection() {
        selection = nil
        selectionAnchor = nil
        editMenu?.dismissMenu()
        drawSelection()
    }

    var selectedText: String? {
        guard let selection else { return nil }
        let session = self.session
        return selection.text { session.line(at: $0) }
    }

    func drawSelection() {
        guard let selection else {
            selectionLayer.isHidden = true
            return
        }
        let path = CGMutablePath()
        let cell = font.cellWidth
        let cols = session.buffer.cols
        for line in selection.start.line ... selection.end.line {
            let from = line == selection.start.line ? selection.start.column : 0
            let to = line == selection.end.line ? selection.end.column : cols - 1
            guard to >= from else { continue }
            path.addRect(CGRect(x: padding + CGFloat(from) * cell, y: CGFloat(line) * lineHeight,
                                width: CGFloat(to - from + 1) * cell, height: lineHeight))
        }
        selectionLayer.path = path
        selectionLayer.isHidden = false
    }

    private func showSelectionMenu(at location: CGPoint) {
        guard selection != nil else { return }
        let configuration = UIEditMenuConfiguration(identifier: "terminal-selection", sourcePoint: location)
        editMenu?.presentEditMenu(with: configuration)
    }

    func editMenuInteraction(_ interaction: UIEditMenuInteraction, menuFor configuration: UIEditMenuConfiguration,
                             suggestedActions: [UIMenuElement]) -> UIMenu? {
        let copy = UIAction(title: "Copy", image: UIImage(systemName: "doc.on.doc")) { [weak self] _ in
            self?.copySelection()
        }
        let paste = UIAction(title: "Paste", image: UIImage(systemName: "doc.on.clipboard")) { [weak self] _ in
            self?.clearSelection()
            self?.paste(nil)
        }
        let all = UIAction(title: "Select Screen", image: UIImage(systemName: "rectangle.dashed")) { [weak self] _ in
            self?.selectScreen()
        }
        // Paste here is how a phone without a hardware keyboard pastes into
        // the program itself (bracketed when it asked), not the composer.
        return UIMenu(children: UIPasteboard.general.hasStrings ? [copy, paste, all] : [copy, all])
    }

    func editMenuInteraction(_ interaction: UIEditMenuInteraction, targetRectFor configuration: UIEditMenuConfiguration) -> CGRect {
        guard let selection else { return .zero }
        let rect = CGRect(x: padding + CGFloat(selection.start.column) * font.cellWidth,
                          y: CGFloat(selection.start.line) * lineHeight,
                          width: font.cellWidth, height: lineHeight)
        return canvas.convert(rect, to: self)
    }

    func copySelection() {
        guard let text = selectedText, !text.isEmpty else { return }
        UIPasteboard.general.string = text
        UINotificationFeedbackGenerator().notificationOccurred(.success)
        clearSelection()
    }

    func selectScreen() {
        let buffer = session.buffer
        selection = TerminalSelection(
            TerminalSelection.Point(line: buffer.historySize, column: 0),
            TerminalSelection.Point(line: buffer.totalLines - 1, column: buffer.cols - 1)
        )
        drawSelection()
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.showSelectionMenu(at: CGPoint(x: self.bounds.midX, y: self.bounds.midY))
        }
    }

    override func copy(_ sender: Any?) { copySelection() }
}
