import UIKit

/// The row above the software keyboard: the keys a phone keyboard lacks, as
/// quiet glyphs on one glass capsule. No per-key caps — the capsule is the
/// only surface, so the row reads as one control, not fifteen.
///
/// The same keys, in the same order, as the browser terminal's row
/// (`diri-web/src/ui/index.html`): esc, sticky ctrl, ⇥, arrows, ⏎, ⇧⇥, ^C,
/// `| / ~ -`. The keys scroll sideways; a key acts on release, so a swipe
/// along the row scrolls it instead of pressing what it started on. Arrows
/// repeat while held. Hiding the keyboard stays pinned at the end.
@MainActor
final class KeyAccessoryView: UIView {
    private weak var terminal: TerminalView?
    private let glass: UIVisualEffectView
    private let scroller = UIScrollView()
    private let stack = UIStackView()
    private var ctrlButton: UIButton?
    private var repeatTimer: Timer?
    private var repeated = false

    var ctrlLatched = false { didSet { styleCtrl() } }

    struct Key {
        var title: String?
        var symbol: String?
        var label: String
        var id: String
        var repeats = false
        var tint: UIColor?
        var action: @MainActor (TerminalView) -> Void
    }

    /// The row, in order.
    static let keys: [Key] = [
        Key(title: "esc", label: "Escape", id: "key-esc") { $0.press(.escape) },
        Key(title: "ctrl", label: "Control", id: "key-ctrl") { $0.toggleCtrl() },
        Key(symbol: "arrow.right.to.line", label: "Tab", id: "key-tab") { $0.press(.tab) },
        Key(symbol: "arrow.left", label: "Left", id: "key-left", repeats: true) { $0.press(.left) },
        Key(symbol: "arrow.up", label: "Up", id: "key-up", repeats: true) { $0.press(.up) },
        Key(symbol: "arrow.down", label: "Down", id: "key-down", repeats: true) { $0.press(.down) },
        Key(symbol: "arrow.right", label: "Right", id: "key-right", repeats: true) { $0.press(.right) },
        Key(symbol: "return", label: "Return", id: "key-enter") { $0.press(.enter) },
        Key(symbol: "arrow.left.to.line", label: "Shift Tab", id: "key-backtab") { $0.press(.tab, modifiers: .shift) },
        Key(title: "^C", label: "Control C", id: "key-ctrl-c", tint: UIColor(Tokens.Ink.danger)) {
            $0.press(.text("c"), modifiers: .ctrl)
        },
        Key(title: "|", label: "Pipe", id: "key-pipe") { $0.press(.text("|")) },
        Key(title: "/", label: "Slash", id: "key-slash") { $0.press(.text("/")) },
        Key(title: "~", label: "Tilde", id: "key-tilde") { $0.press(.text("~")) },
        Key(title: "-", label: "Dash", id: "key-dash") { $0.press(.text("-")) },
    ]

    init(terminal: TerminalView) {
        self.terminal = terminal
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

        scroller.translatesAutoresizingMaskIntoConstraints = false
        scroller.showsHorizontalScrollIndicator = false
        scroller.alwaysBounceHorizontal = true
        scroller.delaysContentTouches = false
        scroller.canCancelContentTouches = true
        scroller.accessibilityIdentifier = "key-row"
        glass.contentView.addSubview(scroller)

        stack.axis = .horizontal
        stack.spacing = 2
        stack.alignment = .center
        stack.translatesAutoresizingMaskIntoConstraints = false
        scroller.addSubview(stack)

        let hide = button(for: Key(symbol: "keyboard.chevron.compact.down", label: "Hide keyboard", id: "key-hide") {
            _ = $0.resignFirstResponder()
        })
        hide.translatesAutoresizingMaskIntoConstraints = false
        glass.contentView.addSubview(hide)
        let divider = UIView()
        divider.backgroundColor = UIColor.white.withAlphaComponent(0.14)
        divider.translatesAutoresizingMaskIntoConstraints = false
        glass.contentView.addSubview(divider)

        NSLayoutConstraint.activate([
            glass.leadingAnchor.constraint(equalTo: safeAreaLayoutGuide.leadingAnchor, constant: 10),
            glass.trailingAnchor.constraint(equalTo: safeAreaLayoutGuide.trailingAnchor, constant: -10),
            glass.topAnchor.constraint(equalTo: topAnchor, constant: 4),
            glass.heightAnchor.constraint(equalToConstant: 42),

            hide.trailingAnchor.constraint(equalTo: glass.contentView.trailingAnchor, constant: -6),
            hide.centerYAnchor.constraint(equalTo: glass.contentView.centerYAnchor),
            divider.trailingAnchor.constraint(equalTo: hide.leadingAnchor, constant: -4),
            divider.widthAnchor.constraint(equalToConstant: 1),
            divider.heightAnchor.constraint(equalToConstant: 22),
            divider.centerYAnchor.constraint(equalTo: glass.contentView.centerYAnchor),

            scroller.leadingAnchor.constraint(equalTo: glass.contentView.leadingAnchor),
            scroller.trailingAnchor.constraint(equalTo: divider.leadingAnchor, constant: -2),
            scroller.topAnchor.constraint(equalTo: glass.contentView.topAnchor),
            scroller.bottomAnchor.constraint(equalTo: glass.contentView.bottomAnchor),

            stack.leadingAnchor.constraint(equalTo: scroller.contentLayoutGuide.leadingAnchor, constant: 6),
            stack.trailingAnchor.constraint(equalTo: scroller.contentLayoutGuide.trailingAnchor, constant: -6),
            stack.topAnchor.constraint(equalTo: scroller.contentLayoutGuide.topAnchor),
            stack.bottomAnchor.constraint(equalTo: scroller.contentLayoutGuide.bottomAnchor),
            stack.heightAnchor.constraint(equalTo: scroller.frameLayoutGuide.heightAnchor),
        ])

        for key in Self.keys { stack.addArrangedSubview(button(for: key)) }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    override var intrinsicContentSize: CGSize { CGSize(width: UIView.noIntrinsicMetric, height: 54) }

    private func button(for key: Key) -> UIButton {
        var config = UIButton.Configuration.plain()
        config.contentInsets = NSDirectionalEdgeInsets(top: 8, leading: 7, bottom: 8, trailing: 7)
        config.baseForegroundColor = key.tint ?? UIColor.white.withAlphaComponent(0.82)
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
        // Release, not touch-down: a drag along the row scrolls it and the
        // scroll view cancels the press.
        button.addAction(UIAction { [weak self] _ in
            guard let self else { return }
            self.stopRepeat()
            if self.repeated { self.repeated = false; return }
            self.fire(key)
        }, for: .touchUpInside)
        button.addAction(UIAction { [weak self] _ in
            self?.stopRepeat()
            self?.repeated = false
        }, for: [.touchCancel, .touchUpOutside])
        if key.repeats {
            button.addAction(UIAction { [weak self] _ in self?.startRepeat(key) }, for: .touchDown)
        }
        button.widthAnchor.constraint(greaterThanOrEqualToConstant: 34).isActive = true
        if key.id == "key-ctrl" { ctrlButton = button }
        return button
    }

    private func fire(_ key: Key) {
        guard let terminal else { return }
        UIImpactFeedbackGenerator(style: .soft).impactOccurred(intensity: 0.6)
        key.action(terminal)
    }

    /// Held arrows repeat like a hardware key: after 0.4 s, 14 a second.
    private func startRepeat(_ key: Key) {
        stopRepeat()
        repeated = false
        repeatTimer = Timer.scheduledTimer(withTimeInterval: 0.4, repeats: false) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.repeated = true
                self.fire(key)
                self.repeatTimer = Timer.scheduledTimer(withTimeInterval: 0.07, repeats: true) { [weak self] _ in
                    MainActor.assumeIsolated {
                        guard let self, let terminal = self.terminal else { return }
                        key.action(terminal)
                    }
                }
            }
        }
    }

    private func stopRepeat() {
        repeatTimer?.invalidate()
        repeatTimer = nil
    }

    private func styleCtrl() {
        guard let ctrlButton, var config = ctrlButton.configuration else { return }
        config.baseForegroundColor = ctrlLatched ? UIColor(Tokens.Ink.clay) : UIColor.white.withAlphaComponent(0.82)
        var attributed = AttributedString("ctrl")
        attributed.font = UIFont.monospacedSystemFont(ofSize: 15, weight: ctrlLatched ? .bold : .medium)
        config.attributedTitle = attributed
        config.background.backgroundColor = ctrlLatched ? UIColor(Tokens.Ink.clay).withAlphaComponent(0.18) : .clear
        config.background.cornerRadius = 10
        ctrlButton.configuration = config
        ctrlButton.accessibilityValue = ctrlLatched ? "On" : "Off"
    }
}
