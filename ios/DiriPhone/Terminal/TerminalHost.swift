import Network
import SwiftUI
import UIKit

/// Keeps each opened session's terminal alive — its stream, its scrollback
/// cache and its view — so switching away and back lands on the same scroll
/// position with the screen already drawn. Only the session on screen
/// streams; the others keep their cache and reconnect when shown.
@MainActor
final class TerminalRegistry {
    static let shared = TerminalRegistry()

    private var sessions: [String: TerminalSession] = [:]
    private var views: [String: TerminalView] = [:]
    private var order: [String] = []
    /// Terminals kept warm. Each is a few MB of scrollback at most.
    private let keep = 8
    private var active: String?
    private let pathMonitor = NWPathMonitor()
    private var lastPath: (interfaces: [String], satisfied: Bool)?

    private init() {
        // A network change (Wi-Fi to cellular, a tailnet reconnect) often
        // leaves the stream's socket open but dead. Reconnect the terminal on
        // screen at once; resume makes that cost only the rows that changed.
        pathMonitor.pathUpdateHandler = { path in
            let interfaces = path.availableInterfaces.map(\.name)
            let satisfied = path.status == .satisfied
            Task { @MainActor in TerminalRegistry.shared.pathChanged(interfaces: interfaces, satisfied: satisfied) }
        }
        pathMonitor.start(queue: DispatchQueue(label: "diri.terminal.path"))
    }

    private func pathChanged(interfaces: [String], satisfied: Bool) {
        defer { lastPath = (interfaces, satisfied) }
        // The first callback reports the path as it already is.
        guard let lastPath, satisfied, lastPath.interfaces != interfaces || !lastPath.satisfied else { return }
        if let active { sessions[active]?.reconnectNow() }
    }

    func session(for id: String, client: DiriClient) -> TerminalSession {
        if let existing = sessions[id] { return existing }
        let session = TerminalSession(sessionID: id, client: client)
        sessions[id] = session
        return session
    }

    func view(for session: TerminalSession) -> TerminalView {
        let id = session.sessionID
        touch(id)
        if let existing = views[id] { return existing }
        let view = TerminalView(session: session)
        views[id] = view
        return view
    }

    func existingView(_ id: String) -> TerminalView? { views[id] }

    func existingSession(_ id: String) -> TerminalSession? { sessions[id] }

    /// Makes `id` the live terminal: it streams, the others pause.
    func activate(_ id: String) {
        active = id
        for (other, session) in sessions where other != id { session.stop() }
        sessions[id]?.start()
    }

    func deactivateAll() {
        active = nil
        for session in sessions.values { session.stop() }
    }

    private func touch(_ id: String) {
        order.removeAll { $0 == id }
        order.append(id)
        while order.count > keep {
            let evicted = order.removeFirst()
            sessions.removeValue(forKey: evicted)?.stop()
            views.removeValue(forKey: evicted)
        }
    }
}

/// Hosts a session's `TerminalView` in SwiftUI.
struct TerminalHost: UIViewRepresentable {
    let session: TerminalSession
    var insets: EdgeInsets
    var onCommand: (TerminalView.Command) -> Void = { _ in }
    var onFocus: (Bool) -> Void = { _ in }

    func makeUIView(context: Context) -> TerminalContainer {
        TerminalContainer()
    }

    func updateUIView(_ container: TerminalContainer, context: Context) {
        let view = TerminalRegistry.shared.view(for: session)
        container.host(view)
        view.onCommand = onCommand
        view.onFocus = onFocus
        view.chromeInsets = UIEdgeInsets(top: insets.top, left: insets.leading, bottom: insets.bottom, right: insets.trailing)
    }

    static func dismantleUIView(_ container: TerminalContainer, coordinator: ()) {
        container.host(nil)
    }
}

/// A plain container so the cached terminal view can move between SwiftUI
/// hosts without SwiftUI owning (and discarding) it.
final class TerminalContainer: UIView {
    private(set) weak var hosted: TerminalView?

    func host(_ view: TerminalView?) {
        guard hosted !== view else { return }
        if let hosted, hosted.superview === self {
            if hosted.isFirstResponder { hosted.resignFirstResponder() }
            hosted.removeFromSuperview()
        }
        hosted = view
        guard let view else { return }
        view.removeFromSuperview()
        view.frame = bounds
        view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        addSubview(view)
    }
}
