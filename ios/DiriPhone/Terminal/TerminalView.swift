import UIKit

/// The terminal: a scroll view of recycled, Core Text–drawn line layers.
///
/// Only the lines on screen (plus a small overscan) have layers, and a layer
/// is redrawn only when its line's content changes, so a fling costs a few
/// line draws per frame regardless of how long the scrollback is. Lines come
/// from the session's cache; one that is not there yet draws blank and its
/// page is requested — the scroll never waits for the network.
@MainActor
final class TerminalView: UIScrollView, UITextInput {
    let session: TerminalSession

    /// Insets the SwiftUI chrome floats over the terminal with.
    var chromeInsets = UIEdgeInsets.zero {
        didSet { if chromeInsets != oldValue { applyInsets() } }
    }

    var onCommand: ((Command) -> Void)?
    enum Command { case switcher, previous, next, dismissKeyboard }

    /// Sticky ctrl from the accessory row: applies to the next key only.
    private(set) var ctrlLatched = false { didSet { accessory.ctrlLatched = ctrlLatched } }

    private(set) var font: TerminalFont
    private(set) var colors = TerminalColors(palette: .standard)
    private var lineLayers: [Int: LineLayer] = [:]
    private var pool: [LineLayer] = []
    private let cursorLayer = CALayer()
    let canvas = UIView()
    private var following = true
    private var lastOffset: CGFloat = 0
    private var lastOffsetTime: CFTimeInterval = 0
    private var velocity: CGFloat = 0
    let padding: CGFloat = 10
    private let overscan = 6
    private lazy var accessory = KeyAccessoryView(terminal: self)

    // Input-method state (see TerminalTextInput.swift). The document the
    // keyboard sees is only the text being composed: committed text goes
    // straight to the PTY, which owns everything already typed.
    var marked = ""
    var markedSelection = NSRange(location: 0, length: 0)
    weak var inputDelegate: UITextInputDelegate?
    lazy var tokenizer: UITextInputTokenizer = UITextInputStringTokenizer(textInput: self)
    let markedLabel = UILabel()

    static let fontSizeKey = "terminal.fontSize"
    static let defaultFontSize: CGFloat = 11

    init(session: TerminalSession) {
        self.session = session
        let stored = UserDefaults.standard.double(forKey: Self.fontSizeKey)
        font = TerminalFont(size: stored >= 6 ? stored : Self.defaultFontSize, scale: UIScreen.main.scale)
        super.init(frame: .zero)
        backgroundColor = UIColor(cgColor: colors.background)
        indicatorStyle = .white
        alwaysBounceVertical = true
        isDirectionalLockEnabled = true
        contentInsetAdjustmentBehavior = .never
        keyboardDismissMode = .interactive
        decelerationRate = .normal
        delaysContentTouches = false
        addSubview(canvas)
        cursorLayer.actions = ["position": NSNull(), "bounds": NSNull(), "hidden": NSNull(), "frame": NSNull()]
        canvas.layer.addSublayer(cursorLayer)
        markedLabel.isHidden = true
        markedLabel.accessibilityIdentifier = "terminal-composition"
        canvas.addSubview(markedLabel)

        addGestureRecognizer(UITapGestureRecognizer(target: self, action: #selector(tapped)))
        addGestureRecognizer(UIPinchGestureRecognizer(target: self, action: #selector(pinched(_:))))

        isAccessibilityElement = true
        accessibilityTraits = [.staticText, .allowsDirectInteraction]
        accessibilityIdentifier = "terminal-text"

        session.onChange = { [weak self] change in self?.apply(change) }
        relayout(keepingLine: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    // MARK: - Geometry

    private var lineHeight: CGFloat { font.lineHeight }
    /// A line layer's width, whole device pixels: an opaque layer with a
    /// fractional edge leaves an undrawn hairline down the screen.
    private var lineWidth: CGFloat {
        let scale = max(1, traitCollection.displayScale)
        return ceil(CGFloat(session.buffer.cols) * font.cellWidth * scale) / scale
    }
    private var totalLines: Int { session.buffer.totalLines }

    private func applyInsets() {
        let wasFollowing = following
        contentInset = UIEdgeInsets(top: chromeInsets.top + 4, left: 0, bottom: chromeInsets.bottom + 6, right: 0)
        verticalScrollIndicatorInsets = UIEdgeInsets(top: chromeInsets.top, left: 0, bottom: chromeInsets.bottom, right: 0)
        if wasFollowing { scrollToBottom() }
    }

    private func relayout(keepingLine anchor: Int?) {
        let width = CGFloat(session.buffer.cols) * font.cellWidth + padding * 2
        let height = CGFloat(totalLines) * lineHeight
        contentSize = CGSize(width: max(width, bounds.width), height: height)
        canvas.frame = CGRect(origin: .zero, size: contentSize)
        canvas.backgroundColor = backgroundColor
        for (_, layer) in lineLayers { recycle(layer) }
        lineLayers.removeAll()
        if following {
            scrollToBottom()
        } else if let anchor {
            setContentOffset(CGPoint(x: contentOffset.x, y: CGFloat(anchor) * lineHeight - bounds.height / 2), animated: false)
        }
        setNeedsLayout()
    }

    var isAtBottom: Bool {
        contentOffset.y + bounds.height - adjustedContentInset.bottom >= contentSize.height - lineHeight * 1.5
    }

    func scrollToBottom() {
        let y = max(-adjustedContentInset.top, contentSize.height - bounds.height + adjustedContentInset.bottom)
        if abs(contentOffset.y - y) > 0.5 { contentOffset.y = y }
        following = true
    }

    override var bounds: CGRect {
        didSet {
            guard bounds.size != oldValue.size else { return }
            contentSize.width = max(CGFloat(session.buffer.cols) * font.cellWidth + padding * 2, bounds.width)
            canvas.frame.size.width = contentSize.width
            if following { scrollToBottom() }
            autoFit()
        }
    }

    /// With no size chosen by pinch, the text is sized so the session's
    /// columns fill the width (never below a legible floor; past that the
    /// terminal scrolls sideways instead).
    private func autoFit() {
        guard UserDefaults.standard.double(forKey: Self.fontSizeKey) < 6, bounds.width > 0, session.buffer.hasSnapshot else { return }
        let target = min(Self.defaultFontSize + 2, max(7.5, fitFontSize))
        if abs(target - font.size) >= 0.25 { setFontSize(target, remember: false) }
    }

    // MARK: - Layout

    override func layoutSubviews() {
        super.layoutSubviews()
        let started = CACurrentMediaTime()
        trackVelocity(started)

        let lineHeight = self.lineHeight
        let total = totalLines
        let top = contentOffset.y
        let first = max(0, Int(floor(top / lineHeight)) - overscan)
        let last = min(total, Int(ceil((top + bounds.height) / lineHeight)) + overscan)
        let range = first ..< max(first, last)

        for (index, layer) in lineLayers where !range.contains(index) {
            recycle(layer)
            lineLayers.removeValue(forKey: index)
        }
        let width = lineWidth
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        for index in range {
            let line = session.line(at: index)
            if line == nil, benchmarking || isDecelerating || isTracking { FrameProbe.shared.noteMiss(index) }
            if let layer = lineLayers[index] {
                layer.show(line, font: font, colors: colors)
            } else {
                let layer = dequeue()
                layer.frame = CGRect(x: padding, y: CGFloat(index) * lineHeight, width: width, height: lineHeight)
                layer.show(line, font: font, colors: colors)
                lineLayers[index] = layer
            }
        }
        placeCursor()
        placeMarked()
        CATransaction.commit()

        // Lines-per-second, negative towards history, drives how far ahead
        // the cache reaches.
        let linesPerSecond = velocity / lineHeight
        session.prefetch(around: (first + last) / 2, visible: range, lookahead: Int(linesPerSecond * 1.2))

        if isTracking || isDecelerating || benchmarking {
            FrameProbe.shared.noteScrolling()
            FrameProbe.shared.noteLayout(seconds: CACurrentMediaTime() - started, firstLine: first, lastLine: last)
        }
    }

    private func trackVelocity(_ now: CFTimeInterval) {
        let dt = now - lastOffsetTime
        if dt > 0, dt < 0.2 {
            let instant = (contentOffset.y - lastOffset) / CGFloat(dt)
            velocity = velocity * 0.6 + instant * 0.4
        } else {
            velocity = 0
        }
        lastOffset = contentOffset.y
        lastOffsetTime = now
        if isTracking || isDecelerating { following = isAtBottom }
    }

    private func dequeue() -> LineLayer {
        if let layer = pool.popLast() {
            layer.isHidden = false
            return layer
        }
        let layer = LineLayer()
        layer.contentsScale = traitCollection.displayScale > 0 ? traitCollection.displayScale : UIScreen.main.scale
        canvas.layer.insertSublayer(layer, below: cursorLayer)
        return layer
    }

    private func recycle(_ layer: LineLayer) {
        layer.isHidden = true
        pool.append(layer)
    }

    private func placeCursor() {
        let buffer = session.buffer
        let cursor = buffer.cursor
        guard cursor.visible, buffer.hasSnapshot, cursor.y < buffer.rows else {
            cursorLayer.isHidden = true
            return
        }
        let x = padding + CGFloat(cursor.x) * font.cellWidth
        let y = CGFloat(buffer.historySize + cursor.y) * lineHeight
        let w = font.cellWidth
        let h = lineHeight
        cursorLayer.isHidden = false
        cursorLayer.backgroundColor = colors.foreground.copy(alpha: cursor.shape == .block ? 0.55 : 0.9)
        switch cursor.shape {
        case .block: cursorLayer.frame = CGRect(x: x, y: y, width: w, height: h)
        case .bar: cursorLayer.frame = CGRect(x: x, y: y + 1, width: 2, height: h - 2)
        case .underline: cursorLayer.frame = CGRect(x: x, y: y + h - 2, width: w, height: 2)
        }
    }

    // MARK: - Updates

    private func apply(_ change: TerminalBuffer.Change) {
        if session.buffer.palette != colors.palette {
            colors = TerminalColors(palette: session.buffer.palette)
            backgroundColor = UIColor(cgColor: colors.background)
            canvas.backgroundColor = backgroundColor
            for (_, layer) in lineLayers { layer.invalidate() }
        }
        if change.geometry || change.everything {
            let height = CGFloat(totalLines) * lineHeight
            let width = max(CGFloat(session.buffer.cols) * font.cellWidth + padding * 2, bounds.width)
            if contentSize.height != height || contentSize.width != width {
                contentSize = CGSize(width: width, height: height)
                canvas.frame = CGRect(origin: .zero, size: contentSize)
            }
            if change.everything {
                // Rows moved under their indices; drop every layer.
                for (_, layer) in lineLayers { recycle(layer) }
                lineLayers.removeAll()
            } else {
                let cellWidth = lineWidth
                for (_, layer) in lineLayers where layer.frame.width != cellWidth { layer.frame.size.width = cellWidth }
            }
            if following { scrollToBottom() }
            autoFit()
        }
        // `layoutSubviews` re-reads every visible line and redraws only the
        // ones whose content differs, so a diff costs exactly its rows.
        setNeedsLayout()
        if change.cursor { placeCursor() }
        accessibilityLabelCache = nil
    }

    // MARK: - Font size

    func setFontSize(_ size: CGFloat, remember: Bool = true) {
        let clamped = min(24, max(6, remember ? (size * 2).rounded() / 2 : (size * 4).rounded(.down) / 4))
        guard clamped != font.size else { return }
        let anchor = Int((contentOffset.y + bounds.height / 2) / lineHeight)
        font = TerminalFont(size: clamped, scale: UIScreen.main.scale)
        if remember { UserDefaults.standard.set(Double(clamped), forKey: Self.fontSizeKey) }
        relayout(keepingLine: anchor)
    }

    /// The size at which the session's columns fill the phone's width.
    var fitFontSize: CGFloat {
        let available = bounds.width - padding * 2
        let perPoint = font.cellWidth / font.size
        return floor(available / CGFloat(max(session.buffer.cols, 1)) / perPoint * 4) / 4
    }

    /// Columns and rows that fit the phone at the current font size.
    var fittingGrid: (cols: Int, rows: Int) {
        let cols = Int((bounds.width - padding * 2) / font.cellWidth)
        let rows = Int((bounds.height - adjustedContentInset.top - adjustedContentInset.bottom) / lineHeight)
        return (max(20, cols), max(8, rows))
    }

    private var pinchStart: CGFloat = 11

    @objc private func pinched(_ gesture: UIPinchGestureRecognizer) {
        switch gesture.state {
        case .began: pinchStart = font.size
        case .changed: setFontSize(pinchStart * gesture.scale)
        default: break
        }
    }

    // MARK: - Benchmark

    private(set) var benchmarking = false
    private var benchmarkLink: CADisplayLink?
    private var benchmarkVelocity: CGFloat = 0
    private var benchmarkDirection: CGFloat = -1
    private var benchmarkLast: CFTimeInterval = 0
    private var benchmarkDone: (() -> Void)?

    /// Repeated flicks to the top of the scrollback and back, each starting at
    /// `speed` points per second and decaying like UIScrollView's normal
    /// deceleration. Drives the same layout path a finger does.
    func runFlingBenchmark(speed: CGFloat = 16000, done: @escaping () -> Void) {
        following = false
        benchmarking = true
        benchmarkDone = done
        benchmarkDirection = -1
        benchmarkVelocity = speed
        benchmarkLast = 0
        let link = CADisplayLink(target: self, selector: #selector(benchmarkTick(_:)))
        link.preferredFrameRateRange = CAFrameRateRange(minimum: 60, maximum: 120, preferred: 120)
        link.add(to: .main, forMode: .common)
        benchmarkLink = link
    }

    private var benchmarkSpeed: CGFloat = 16000

    @objc private func benchmarkTick(_ link: CADisplayLink) {
        if benchmarkLast == 0 { benchmarkLast = link.timestamp; benchmarkSpeed = benchmarkVelocity; return }
        let dt = CGFloat(link.timestamp - benchmarkLast)
        benchmarkLast = link.timestamp
        let minY = -adjustedContentInset.top
        let maxY = max(minY, contentSize.height - bounds.height + adjustedContentInset.bottom)
        var y = contentOffset.y + benchmarkDirection * benchmarkVelocity * dt
        // UIScrollView's normal rate: velocity × 0.998 per millisecond.
        benchmarkVelocity *= pow(0.998, dt * 1000)
        if benchmarkVelocity < 400 { benchmarkVelocity = benchmarkSpeed } // next flick
        if y <= minY {
            y = minY
            benchmarkDirection = 1
            benchmarkVelocity = benchmarkSpeed
        } else if y >= maxY, benchmarkDirection > 0 {
            contentOffset.y = maxY
            link.invalidate()
            benchmarkLink = nil
            benchmarking = false
            FrameProbe.shared.finish()
            benchmarkDone?()
            return
        }
        contentOffset.y = y
    }

    // MARK: - Keyboard

    override var canBecomeFirstResponder: Bool { true }

    var onFocus: ((Bool) -> Void)?

    @discardableResult
    override func becomeFirstResponder() -> Bool {
        let became = super.becomeFirstResponder()
        if became { onFocus?(true) }
        return became
    }

    @discardableResult
    override func resignFirstResponder() -> Bool {
        let resigned = super.resignFirstResponder()
        if resigned { onFocus?(false) }
        return resigned
    }
    override var inputAccessoryView: UIView? { accessory }

    var hasText: Bool { true }

    /// Bytes for the PTY. Tests replace this to observe what typing sends.
    var emit: (([UInt8]) -> Void)?

    func send(_ bytes: [UInt8]) {
        if let emit { emit(bytes) } else { session.send(bytes) }
    }

    /// Committed text from the keyboard: a key, a chosen IME candidate, an
    /// emoji, a dictated phrase. Text still being composed is never sent.
    func insertText(_ text: String) {
        if !marked.isEmpty { clearMarked() }
        if ctrlLatched {
            ctrlLatched = false
            send(KeyEncoder.encodeText(text, modifiers: .ctrl))
        } else {
            send(KeyEncoder.encodeText(text))
        }
        snapToBottomForInput()
    }

    func deleteBackward() {
        // While composing, the input method edits its own marked text (it
        // calls `setMarkedText` with the shorter string); nothing reaches the
        // program until a candidate is committed.
        if !marked.isEmpty {
            setMarkedText(String(marked.dropLast()), selectedRange: NSRange(location: max(0, marked.utf16.count - 1), length: 0))
            return
        }
        send(KeyEncoder.encode(.backspace))
        snapToBottomForInput()
    }

    /// Accessory and hardware keys.
    func press(_ key: TermKey, modifiers: KeyModifiers = []) {
        var modifiers = modifiers
        if ctrlLatched {
            modifiers.insert(.ctrl)
            ctrlLatched = false
        }
        if case let .text(text) = key {
            send(KeyEncoder.encodeText(text, modifiers: modifiers))
        } else {
            send(KeyEncoder.encode(key, modifiers: modifiers, appCursor: session.buffer.modes.appCursor))
        }
        snapToBottomForInput()
    }

    func toggleCtrl() { ctrlLatched.toggle() }

    private func snapToBottomForInput() {
        if !following { scrollToBottom() }
    }

    var autocorrectionType: UITextAutocorrectionType { get { .no } set {} }
    var autocapitalizationType: UITextAutocapitalizationType { get { .none } set {} }
    var spellCheckingType: UITextSpellCheckingType { get { .no } set {} }
    var smartQuotesType: UITextSmartQuotesType { get { .no } set {} }
    var smartDashesType: UITextSmartDashesType { get { .no } set {} }
    var smartInsertDeleteType: UITextSmartInsertDeleteType { get { .no } set {} }
    var keyboardAppearance: UIKeyboardAppearance { get { .dark } set {} }
    // `.default`, not `.asciiCapable`: the ASCII keyboard type hides every
    // non-Latin keyboard (Pinyin, Kana, Hangul) and the emoji keyboard.
    var keyboardType: UIKeyboardType { get { .default } set {} }
    var returnKeyType: UIReturnKeyType { get { .default } set {} }
    var inlinePredictionType: UITextInlinePredictionType { get { .no } set {} }

    @objc private func tapped() {
        if isFirstResponder { return }
        becomeFirstResponder()
    }

    override func paste(_ sender: Any?) {
        guard let text = UIPasteboard.general.string, !text.isEmpty else { return }
        if let emit {
            emit(KeyEncoder.paste(text, bracketed: session.buffer.modes.bracketedPaste))
        } else {
            session.paste(text)
        }
        snapToBottomForInput()
    }

    override func canPerformAction(_ action: Selector, withSender sender: Any?) -> Bool {
        if action == #selector(paste(_:)) { return UIPasteboard.general.hasStrings }
        return super.canPerformAction(action, withSender: sender)
    }

    // MARK: Hardware keyboard

    override var keyCommands: [UIKeyCommand]? {
        let commands = [
            UIKeyCommand(title: "Switch Session", action: #selector(commandSwitcher), input: "k", modifierFlags: .command),
            UIKeyCommand(title: "Previous Session", action: #selector(commandPrevious), input: "[", modifierFlags: .command),
            UIKeyCommand(title: "Next Session", action: #selector(commandNext), input: "]", modifierFlags: .command),
            UIKeyCommand(title: "Paste", action: #selector(paste(_:)), input: "v", modifierFlags: .command),
        ]
        commands.forEach { $0.wantsPriorityOverSystemBehavior = true }
        return commands
    }

    @objc private func commandSwitcher() { onCommand?(.switcher) }
    @objc private func commandPrevious() { onCommand?(.previous) }
    @objc private func commandNext() { onCommand?(.next) }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        // A hardware keyboard with an input method: arrows, return and escape
        // belong to the composition until it is committed.
        if !marked.isEmpty {
            super.pressesBegan(presses, with: event)
            return
        }
        var unhandled = Set<UIPress>()
        for press in presses {
            guard let key = press.key, let mapped = Self.map(key) else {
                unhandled.insert(press)
                continue
            }
            self.press(mapped.key, modifiers: mapped.modifiers)
        }
        if !unhandled.isEmpty { super.pressesBegan(unhandled, with: event) }
    }

    /// Hardware keys the text system would not deliver as the bytes a
    /// terminal needs. Plain characters fall through to `insertText`.
    static func map(_ key: UIKey) -> (key: TermKey, modifiers: KeyModifiers)? {
        var modifiers: KeyModifiers = []
        if key.modifierFlags.contains(.shift) { modifiers.insert(.shift) }
        if key.modifierFlags.contains(.alternate) { modifiers.insert(.alt) }
        if key.modifierFlags.contains(.control) { modifiers.insert(.ctrl) }
        if key.modifierFlags.contains(.command) { return nil }
        switch key.keyCode {
        case .keyboardUpArrow: return (.up, modifiers)
        case .keyboardDownArrow: return (.down, modifiers)
        case .keyboardLeftArrow: return (.left, modifiers)
        case .keyboardRightArrow: return (.right, modifiers)
        case .keyboardEscape: return (.escape, [])
        case .keyboardTab: return (.tab, modifiers)
        case .keyboardHome: return (.home, modifiers)
        case .keyboardEnd: return (.end, modifiers)
        case .keyboardPageUp: return (.pageUp, modifiers)
        case .keyboardPageDown: return (.pageDown, modifiers)
        case .keyboardDeleteForward: return (.delete, modifiers)
        case .keyboardReturnOrEnter: return (.enter, modifiers.subtracting(.shift))
        case .keyboardDeleteOrBackspace: return (.backspace, modifiers.subtracting(.shift))
        default: break
        }
        // Ctrl and Option combos: the text system would swallow them or
        // insert a dead-key character.
        if modifiers.contains(.ctrl) || modifiers.contains(.alt) {
            let base = key.charactersIgnoringModifiers
            guard !base.isEmpty else { return nil }
            return (.text(base), modifiers.subtracting(.shift))
        }
        return nil
    }

    // MARK: - Accessibility

    private var accessibilityLabelCache: String?

    override var accessibilityLabel: String? {
        get {
            if let accessibilityLabelCache { return accessibilityLabelCache }
            let text = session.buffer.screenText
            accessibilityLabelCache = text
            return text
        }
        set {}
    }

    /// For UI tests: where the view is and what it shows.
    override var accessibilityValue: String? {
        get {
            let top = max(0, Int((contentOffset.y + adjustedContentInset.top) / lineHeight))
            let coloured = session.buffer.screen.filter(\.isColoured).count
            return "top \(top) of \(totalLines); coloured \(coloured); cols \(session.buffer.cols); snapshots \(session.snapshots); resumes \(session.resumes); resumed rows \(session.resumedRows); stale \(session.stale)"
        }
        set {}
    }
}

/// One terminal line. Redraws only when given a different line.
final class LineLayer: CALayer {
    private var line: TermLine?
    private var loaded = false
    private weak var font: TerminalFont?
    private weak var colors: TerminalColors?

    override init() {
        super.init()
        isOpaque = true
        needsDisplayOnBoundsChange = true
        actions = ["contents": NSNull(), "position": NSNull(), "bounds": NSNull(), "hidden": NSNull()]
    }

    override init(layer: Any) { super.init(layer: layer) }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    func show(_ line: TermLine?, font: TerminalFont, colors: TerminalColors) {
        let changed = line != self.line || (line != nil) != loaded || font !== self.font || colors !== self.colors
        guard changed else { return }
        self.line = line
        loaded = line != nil
        self.font = font
        self.colors = colors
        backgroundColor = colors.background
        let height = font.lineHeight
        if frame.height != height { frame.size.height = height }
        setNeedsDisplay()
    }

    func invalidate() {
        line = nil
        loaded = false
        font = nil
    }

    override func draw(in ctx: CGContext) {
        guard let font, let colors else { return }
        ctx.setFillColor(colors.background)
        ctx.fill(bounds)
        guard let line else { return }
        let started = CACurrentMediaTime()
        LineRenderer.draw(line, in: ctx, font: font, colors: colors)
        let elapsed = CACurrentMediaTime() - started
        MainActor.assumeIsolated {
            FrameProbe.shared.noteDraw(seconds: elapsed)
        }
    }
}
