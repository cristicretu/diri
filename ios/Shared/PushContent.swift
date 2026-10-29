import Foundation
import UserNotifications

/// What a Diri push carries and how a notification is written from it.
/// Shared by the app (actions) and the notification service extension
/// (contents), so the two agree on every identifier.
///
/// The push itself holds no session content: `s` (session id), `k`
/// (`needs`, `failed`, `done`), `o` (when the prompt was raised) and `b`
/// (the gateway this phone registered from). The extension fetches the
/// rest from `b` over the tailnet, where the gateway knows this phone by its
/// Tailscale identity, so no credential is needed and Apple sees nothing.
enum PushContent {
    static let needsCategory = "diri.needs"
    /// A permission prompt with a verified approve key.
    static let permissionCategory = "diri.permission"
    /// The same, flagged destructive: Approve needs the phone unlocked.
    static let destructiveCategory = "diri.permission.destructive"
    static let newsCategory = "diri.news"
    static let approveAction = "diri.approve"
    static let denyAction = "diri.deny"

    struct Reference: Equatable, Sendable {
        var session: String
        var kind: String
        var base: URL
        var occurredAt: Double?

        init?(userInfo: [AnyHashable: Any]) {
            guard let diri = userInfo["diri"] as? [String: Any],
                  let session = diri["s"] as? String, !session.isEmpty,
                  let kind = diri["k"] as? String,
                  let raw = diri["b"] as? String, let base = URL(string: raw),
                  ["http", "https"].contains(base.scheme?.lowercased() ?? "")
            else { return nil }
            self.session = session
            self.kind = kind
            self.base = base
            occurredAt = (diri["o"] as? NSNumber)?.doubleValue
        }
    }

    /// `GET /api/session/:id/attention`.
    struct Attention: Decodable, Equatable, Sendable {
        var title: String?
        var agent: String?
        var waiting: Bool?
        var question: String?
        var options: [String]?
        var kind: String?
        var risk: String?
        var occurredAt: Double?
        var quickAnswer: Bool?
    }

    static func attentionURL(_ reference: Reference) -> URL? {
        let id = reference.session.addingPercentEncoding(withAllowedCharacters: .alphanumerics) ?? reference.session
        return URL(string: "/api/session/\(id)/attention", relativeTo: reference.base)
    }

    /// Fills `content` from what the gateway says now. A prompt that is no
    /// longer waiting gets no actions: answering it would be refused anyway.
    static func apply(_ attention: Attention, to content: UNMutableNotificationContent, reference: Reference) {
        let title = attention.title?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        if !title.isEmpty { content.title = title }
        if let agent = attention.agent { content.subtitle = displayName(agent) }
        switch reference.kind {
        case "needs":
            if let question = attention.question?.trimmingCharacters(in: .whitespacesAndNewlines), !question.isEmpty {
                var body = question
                if let options = attention.options, !options.isEmpty, attention.quickAnswer != true {
                    body += "\n" + options.enumerated().map { "\($0.offset + 1). \($0.element)" }.joined(separator: "  ")
                }
                content.body = body
            } else {
                content.body = "Waiting for your input"
            }
            if attention.waiting == true, attention.quickAnswer == true {
                content.categoryIdentifier = attention.risk == "destructive" ? destructiveCategory : permissionCategory
            } else {
                content.categoryIdentifier = needsCategory
            }
            var info = content.userInfo
            if var diri = info["diri"] as? [String: Any], let occurred = attention.occurredAt {
                // Answer the prompt the phone shows, as the gateway sees it.
                diri["o"] = occurred
                info["diri"] = diri
                content.userInfo = info
            }
        case "failed":
            content.body = "Stopped with an error"
            content.categoryIdentifier = newsCategory
        default:
            content.body = "Finished"
            content.categoryIdentifier = newsCategory
        }
    }

    static func displayName(_ agent: String) -> String {
        switch agent {
        case "claude-code": "Claude Code"
        case "codex": "Codex"
        case "gemini": "Gemini"
        case "cursor": "Cursor"
        case "shell": "Terminal"
        default: agent
        }
    }

    /// Fetches the details and writes them into `content`. Leaves it as is
    /// when the gateway cannot be reached in `timeout`.
    static func compose(_ content: UNMutableNotificationContent, timeout: TimeInterval = 8) async {
        guard let reference = Reference(userInfo: content.userInfo), let url = attentionURL(reference) else { return }
        var fetch = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: timeout)
        fetch.httpMethod = "GET"
        let configuration = URLSessionConfiguration.ephemeral
        configuration.waitsForConnectivity = false
        guard let (data, response) = try? await URLSession(configuration: configuration).data(for: fetch),
              (response as? HTTPURLResponse)?.statusCode == 200,
              let attention = try? JSONDecoder().decode(Attention.self, from: data)
        else { return }
        apply(attention, to: content, reference: reference)
    }

    /// The content an APNs payload asks for, before the details: what the
    /// lock screen shows if the extension cannot reach the gateway.
    static func content(fromPayload payload: [String: Any]) -> UNMutableNotificationContent {
        let content = UNMutableNotificationContent()
        let aps = payload["aps"] as? [String: Any]
        let alert = aps?["alert"] as? [String: Any]
        content.title = alert?["title"] as? String ?? "Diri"
        content.body = alert?["body"] as? String ?? ""
        content.categoryIdentifier = aps?["category"] as? String ?? needsCategory
        content.threadIdentifier = aps?["thread-id"] as? String ?? ""
        content.sound = .default
        content.userInfo = payload.filter { $0.key == "diri" }
        return content
    }

    static var categories: Set<UNNotificationCategory> {
        let deny = UNNotificationAction(identifier: denyAction, title: "Deny", options: [])
        let approve = UNNotificationAction(identifier: approveAction, title: "Approve", options: [])
        // Destructive: Approve only with the phone unlocked.
        let approveUnlocked = UNNotificationAction(identifier: approveAction, title: "Approve",
                                                   options: [.authenticationRequired, .destructive])
        return [
            UNNotificationCategory(identifier: permissionCategory, actions: [approve, deny], intentIdentifiers: []),
            UNNotificationCategory(identifier: destructiveCategory, actions: [approveUnlocked, deny], intentIdentifiers: []),
            UNNotificationCategory(identifier: needsCategory, actions: [], intentIdentifiers: []),
            UNNotificationCategory(identifier: newsCategory, actions: [], intentIdentifiers: []),
        ]
    }
}
