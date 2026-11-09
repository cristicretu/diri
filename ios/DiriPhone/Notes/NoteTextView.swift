import SwiftUI
import UIKit

/// The note editor: one text view holding the note's Markdown, styled in
/// place as it is typed (`MarkdownStyle`). Tapping a to-do box ticks it;
/// Return continues a list; the key bar above the keyboard writes Markdown.
@MainActor
final class NoteTextView: UITextView, UITextViewDelegate {
    /// Every text change, after styling.
    var onChange: (String) -> Void = { _ in }

    private static let bodySize: CGFloat = 16
    private var restyling = false
    /// Ticking a box should not pop the keyboard up for a note being read.
    fileprivate var wasEditingBeforeTap = false

    init() {
        super.init(frame: .zero, textContainer: nil)
        delegate = self
        backgroundColor = .clear
        textColor = UIColor(Tokens.Ink.primary)
        tintColor = UIColor(Tokens.Ink.clay)
        font = .systemFont(ofSize: Self.bodySize)
        keyboardAppearance = .dark
        autocapitalizationType = .sentences
        smartDashesType = .no
        smartQuotesType = .no
        alwaysBounceVertical = true
        keyboardDismissMode = .interactive
        textContainerInset = UIEdgeInsets(top: 12, left: 16, bottom: 120, right: 16)
        accessibilityIdentifier = "note-editor"
        inputAccessoryView = MarkdownAccessoryView(editor: self)
        let tap = UITapGestureRecognizer(target: self, action: #selector(tapped(_:)))
        tap.delegate = self
        addGestureRecognizer(tap)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    /// Replaces the whole text, as when the note is (re)loaded. Not undoable:
    /// it is the Mac's text, not an edit.
    func load(_ markdown: String) {
        guard markdown != text else { return }
        let selection = selectedRange
        text = markdown
        restyle()
        let length = (markdown as NSString).length
        selectedRange = NSRange(location: min(selection.location, length), length: 0)
        undoManager?.removeAllActions()
    }

    // MARK: - Styling

    func restyle() {
        guard !restyling else { return }
        restyling = true
        defer { restyling = false }
        let storage = textStorage
        let whole = NSRange(location: 0, length: storage.length)
        let paragraph = NSMutableParagraphStyle()
        paragraph.lineSpacing = 3
        paragraph.paragraphSpacing = 4
        storage.beginEditing()
        storage.setAttributes([
            .font: UIFont.systemFont(ofSize: Self.bodySize),
            .foregroundColor: UIColor(Tokens.Ink.primary).withAlphaComponent(0.9),
            .paragraphStyle: paragraph,
        ], range: whole)
        for run in MarkdownStyle.runs(in: storage.string) where NSMaxRange(run.range) <= storage.length {
            apply(run, to: storage)
        }
        storage.endEditing()
        // What the user types next takes the body style, not the last run's.
        typingAttributes = [
            .font: UIFont.systemFont(ofSize: Self.bodySize),
            .foregroundColor: UIColor(Tokens.Ink.primary).withAlphaComponent(0.9),
            .paragraphStyle: paragraph,
        ]
    }

    private func apply(_ run: MarkdownStyle.Run, to storage: NSTextStorage) {
        let range = run.range
        let tertiary = UIColor(Tokens.Ink.tertiary)
        switch run.kind {
        case let .heading(level):
            let (size, weight): (CGFloat, UIFont.Weight) = switch level {
            case 0: (26, .bold)
            case 1: (22, .bold)
            case 2: (19, .semibold)
            case 3: (17, .semibold)
            default: (16, .semibold)
            }
            storage.addAttributes([
                .font: UIFont.systemFont(ofSize: size, weight: weight),
                .foregroundColor: UIColor(Tokens.Ink.primary),
            ], range: range)
        case .marker:
            storage.addAttribute(.foregroundColor, value: tertiary, range: range)
        case let .todoBox(checked):
            storage.addAttributes([
                .font: UIFont.monospacedSystemFont(ofSize: Self.bodySize - 1, weight: .semibold),
                .foregroundColor: checked ? UIColor(Tokens.Ink.fresh) : UIColor(Tokens.Ink.clay),
            ], range: range)
        case .doneText:
            storage.addAttributes([
                .foregroundColor: tertiary,
                .strikethroughStyle: NSUnderlineStyle.single.rawValue,
            ], range: range)
        case .quote:
            storage.addAttribute(.foregroundColor, value: UIColor(Tokens.Ink.secondary), range: range)
            addTrait(.traitItalic, in: range, of: storage)
        case .rule:
            storage.addAttribute(.foregroundColor, value: tertiary, range: range)
        case .code, .codeBlock:
            storage.addAttributes([
                .font: UIFont.monospacedSystemFont(ofSize: Self.bodySize - 2, weight: .regular),
                .backgroundColor: UIColor.white.withAlphaComponent(0.06),
            ], range: range)
        case .bold:
            addTrait(.traitBold, in: range, of: storage)
        case .italic:
            addTrait(.traitItalic, in: range, of: storage)
        case .strike:
            storage.addAttribute(.strikethroughStyle, value: NSUnderlineStyle.single.rawValue, range: range)
        case .noteLink, .link:
            storage.addAttribute(.foregroundColor, value: UIColor(Tokens.Ink.clay), range: range)
        case .folded:
            // Still in the text (it is the link), just not drawn.
            storage.addAttributes([
                .font: UIFont.systemFont(ofSize: 0.01),
                .foregroundColor: UIColor.clear,
            ], range: range)
        }
    }

    /// Adds a trait to whatever font each part of `range` already has, so
    /// bold inside a heading stays heading-sized.
    private func addTrait(_ trait: UIFontDescriptor.SymbolicTraits, in range: NSRange, of storage: NSTextStorage) {
        storage.enumerateAttribute(.font, in: range) { value, part, _ in
            let font = (value as? UIFont) ?? .systemFont(ofSize: Self.bodySize)
            let traits = font.fontDescriptor.symbolicTraits.union(trait)
            guard let descriptor = font.fontDescriptor.withSymbolicTraits(traits) else { return }
            storage.addAttribute(.font, value: UIFont(descriptor: descriptor, size: font.pointSize), range: part)
        }
    }

    // MARK: - Edits

    /// Applies one change through `UITextInput`, so it is a single undo step
    /// and the keyboard's autocorrection state stays consistent.
    func apply(_ change: MarkdownEdit.Change) {
        guard let start = position(from: beginningOfDocument, offset: change.range.location),
              let end = position(from: start, offset: change.range.length),
              let range = textRange(from: start, to: end)
        else { return }
        replace(range, withText: change.replacement)
        let length = (text as NSString).length
        selectedRange = NSRange(
            location: min(change.selection.location, length),
            length: min(change.selection.length, max(0, length - change.selection.location))
        )
        restyle()
        onChange(text)
    }

    func wrap(_ open: String, _ close: String? = nil) {
        apply(MarkdownEdit.toggleWrap(text, selection: selectedRange, open: open, close: close))
    }

    func linePrefix(_ prefix: String) {
        apply(MarkdownEdit.toggleLinePrefix(text, selection: selectedRange, prefix: prefix))
    }

    /// Cycles the current line: body → `##` → `###` → body.
    func cycleHeading() {
        let source = text as NSString
        let line = source.substring(with: source.lineRange(for: selectedRange))
        let next = line.hasPrefix("## ") ? "### " : (line.hasPrefix("### ") ? "### " : "## ")
        apply(MarkdownEdit.toggleLinePrefix(text, selection: selectedRange, prefix: next))
    }

    func indent(outdent: Bool) {
        apply(MarkdownEdit.indent(text, selection: selectedRange, outdent: outdent))
    }

    // MARK: - UITextViewDelegate

    func textView(_ textView: UITextView, shouldChangeTextIn range: NSRange, replacementText replacement: String) -> Bool {
        guard replacement == "\n", let change = MarkdownEdit.newline(text, selection: range) else { return true }
        apply(change)
        return false
    }

    func textViewDidChange(_ textView: UITextView) {
        // Mid-composition (Japanese, Chinese, Korean…): restyling would
        // rewrite the attributes of the marked candidate and can commit or
        // garble it, and the text is not final yet. The commit calls back.
        guard textView.markedTextRange == nil else { return }
        restyleSoon()
        onChange(text)
    }

    /// Notes past this many UTF-16 units restyle once typing pauses rather
    /// than on every key: restyling re-parses the whole document.
    private static let immediateRestyleLimit = 20_000
    private var pendingRestyle: DispatchWorkItem?

    private func restyleSoon() {
        pendingRestyle?.cancel()
        guard textStorage.length > Self.immediateRestyleLimit else {
            pendingRestyle = nil
            restyle()
            return
        }
        let work = DispatchWorkItem { [weak self] in
            guard let self, self.markedTextRange == nil else { return }
            self.restyle()
        }
        pendingRestyle = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.15, execute: work)
    }

    // MARK: - To-do taps

    @objc private func tapped(_ gesture: UITapGestureRecognizer) {
        guard let index = todoBoxIndex(at: gesture.location(in: self)),
              let change = MarkdownEdit.toggleTodo(text, at: index)
        else { return }
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
        let selection = selectedRange
        apply(change)
        selectedRange = selection
        if !wasEditingBeforeTap {
            // The text view's own tap began editing too; a tick is not that.
            DispatchQueue.main.async { [weak self] in self?.resignFirstResponder() }
        }
    }

    /// The character under `point` when it is part of a to-do box.
    fileprivate func todoBoxIndex(at point: CGPoint) -> Int? {
        let inContainer = CGPoint(x: point.x - textContainerInset.left, y: point.y - textContainerInset.top)
        var fraction: CGFloat = 0
        let index = layoutManager.characterIndex(
            for: inContainer, in: textContainer, fractionOfDistanceBetweenInsertionPoints: &fraction
        )
        guard index < textStorage.length else { return nil }
        let source = text as NSString
        let line = source.lineRange(for: NSRange(location: index, length: 0))
        let content = source.substring(with: line)
        let indent = (content.prefix(while: { $0 == " " || $0 == "\t" }) as Substring).utf16.count
        // `- [ ]` is five characters; a finger is wider than a character, so
        // the marker and the box both count.
        let box = NSRange(location: line.location + indent, length: 5)
        guard NSLocationInRange(index, box), MarkdownEdit.toggleTodo(text, at: index) != nil else { return nil }
        return index
    }
}

extension NoteTextView: UIGestureRecognizerDelegate {
    /// Only a touch on a box is ours; every other tap places the caret.
    func gestureRecognizer(_ gesture: UIGestureRecognizer, shouldReceive touch: UITouch) -> Bool {
        guard todoBoxIndex(at: touch.location(in: self)) != nil else { return false }
        wasEditingBeforeTap = isFirstResponder
        return true
    }
}

/// The row above the keyboard while a note is edited: Markdown, not terminal
/// keys. Same glass capsule as the terminal's row, scrolling sideways when
/// the keys outgrow a narrow phone.
@MainActor
final class MarkdownAccessoryView: UIView {
    private weak var editor: NoteTextView?

    private struct Key {
        var title: String?
        var symbol: String?
        var label: String
        var id: String
        var action: (NoteTextView) -> Void
    }

    init(editor: NoteTextView) {
        self.editor = editor
        let glass: UIVisualEffectView
        if #available(iOS 26.0, *) {
            let effect = UIGlassEffect()
            effect.isInteractive = true
            glass = UIVisualEffectView(effect: effect)
        } else {
            glass = UIVisualEffectView(effect: UIBlurEffect(style: .systemThinMaterialDark))
        }
        super.init(frame: CGRect(x: 0, y: 0, width: 390, height: 54))
        autoresizingMask = .flexibleWidth
        backgroundColor = .clear

        glass.translatesAutoresizingMaskIntoConstraints = false
        glass.clipsToBounds = true
        glass.layer.cornerRadius = 21
        glass.layer.cornerCurve = .continuous
        addSubview(glass)

        let scroll = UIScrollView()
        scroll.showsHorizontalScrollIndicator = false
        scroll.translatesAutoresizingMaskIntoConstraints = false
        glass.contentView.addSubview(scroll)

        let stack = UIStackView()
        stack.axis = .horizontal
        stack.spacing = 2
        stack.alignment = .center
        stack.translatesAutoresizingMaskIntoConstraints = false
        scroll.addSubview(stack)

        let hide = button(for: Key(symbol: "keyboard.chevron.compact.down", label: "Hide keyboard", id: "md-hide") {
            $0.resignFirstResponder()
        })
        hide.translatesAutoresizingMaskIntoConstraints = false
        glass.contentView.addSubview(hide)

        NSLayoutConstraint.activate([
            glass.leadingAnchor.constraint(equalTo: safeAreaLayoutGuide.leadingAnchor, constant: 10),
            glass.trailingAnchor.constraint(equalTo: safeAreaLayoutGuide.trailingAnchor, constant: -10),
            glass.topAnchor.constraint(equalTo: topAnchor, constant: 4),
            glass.heightAnchor.constraint(equalToConstant: 42),
            scroll.leadingAnchor.constraint(equalTo: glass.contentView.leadingAnchor, constant: 6),
            scroll.trailingAnchor.constraint(equalTo: hide.leadingAnchor),
            scroll.topAnchor.constraint(equalTo: glass.contentView.topAnchor),
            scroll.bottomAnchor.constraint(equalTo: glass.contentView.bottomAnchor),
            hide.trailingAnchor.constraint(equalTo: glass.contentView.trailingAnchor, constant: -6),
            hide.centerYAnchor.constraint(equalTo: glass.contentView.centerYAnchor),
            stack.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor),
            stack.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor),
            stack.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor),
            stack.heightAnchor.constraint(equalTo: scroll.frameLayoutGuide.heightAnchor),
        ])

        let keys: [Key] = [
            Key(symbol: "number", label: "Heading", id: "md-heading") { $0.cycleHeading() },
            Key(symbol: "bold", label: "Bold", id: "md-bold") { $0.wrap("**") },
            Key(symbol: "italic", label: "Italic", id: "md-italic") { $0.wrap("*") },
            Key(symbol: "checklist", label: "To-do", id: "md-todo") { $0.linePrefix("- [ ] ") },
            Key(symbol: "list.bullet", label: "Bulleted list", id: "md-list") { $0.linePrefix("- ") },
            Key(title: "[[", label: "Link to a note", id: "md-link") { $0.wrap("[[", "]]") },
            Key(symbol: "chevron.left.forwardslash.chevron.right", label: "Code", id: "md-code") { $0.wrap("`") },
            Key(symbol: "text.quote", label: "Quote", id: "md-quote") { $0.linePrefix("> ") },
            Key(symbol: "strikethrough", label: "Strikethrough", id: "md-strike") { $0.wrap("~~") },
            Key(symbol: "decrease.indent", label: "Outdent", id: "md-outdent") { $0.indent(outdent: true) },
            Key(symbol: "increase.indent", label: "Indent", id: "md-indent") { $0.indent(outdent: false) },
        ]
        for key in keys { stack.addArrangedSubview(button(for: key)) }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    override var intrinsicContentSize: CGSize { CGSize(width: UIView.noIntrinsicMetric, height: 54) }

    private func button(for key: Key) -> UIButton {
        var config = UIButton.Configuration.plain()
        config.contentInsets = NSDirectionalEdgeInsets(top: 8, leading: 8, bottom: 8, trailing: 8)
        config.baseForegroundColor = UIColor.white.withAlphaComponent(0.82)
        if let symbol = key.symbol {
            config.image = UIImage(systemName: symbol, withConfiguration: UIImage.SymbolConfiguration(pointSize: 14, weight: .medium))
        }
        if let title = key.title {
            var attributed = AttributedString(title)
            attributed.font = UIFont.monospacedSystemFont(ofSize: 15, weight: .medium)
            config.attributedTitle = attributed
        }
        let button = UIButton(configuration: config)
        button.accessibilityLabel = key.label
        button.accessibilityIdentifier = key.id
        button.addAction(UIAction { [weak self] _ in
            guard let editor = self?.editor else { return }
            UIImpactFeedbackGenerator(style: .soft).impactOccurred(intensity: 0.6)
            key.action(editor)
        }, for: .primaryActionTriggered)
        button.widthAnchor.constraint(greaterThanOrEqualToConstant: 34).isActive = true
        return button
    }
}

/// `NoteTextView` in SwiftUI. The text flows one way only, view to model,
/// except when `revision` moves: then the model's text replaces the view's
/// (a load, a reload after a conflict).
struct NoteEditor: UIViewRepresentable {
    let markdown: String
    let revision: Int
    let onChange: (String) -> Void

    final class Coordinator {
        var revision = -1
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeUIView(context: Context) -> NoteTextView {
        let view = NoteTextView()
        view.onChange = onChange
        return view
    }

    func updateUIView(_ view: NoteTextView, context: Context) {
        view.onChange = onChange
        if context.coordinator.revision != revision {
            context.coordinator.revision = revision
            view.load(markdown)
        }
    }
}
