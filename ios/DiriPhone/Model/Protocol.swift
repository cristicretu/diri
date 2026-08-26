import Foundation

/// The daemon's wire types.
///
/// The control protocol was designed against a Swift client, so its enums are
/// still encoded the way `Codable` encoded them: a single-key object whose key
/// is the case name and whose value carries the payload (`{"working":{}}`,
/// `{"needsInput":{"_0":"permission"}}`). Nothing here is a plain string, and
/// decoding it as one is the mistake that produces a list of blank rows.

// MARK: - Agent kind

/// `AgentKind`. Five legacy cases encode under their old Swift names, manifest
/// agents under `{"agent":{"id":…}}`, and `generic` carries its command line.
struct AgentKind: Codable, Hashable, Sendable {
    let id: String
    let command: String?

    static let claudeCode = AgentKind(id: "claude-code", command: nil)
    static let codex = AgentKind(id: "codex", command: nil)
    static let cursor = AgentKind(id: "cursor", command: nil)
    static let gemini = AgentKind(id: "gemini", command: nil)
    static let shell = AgentKind(id: "shell", command: nil)
    static let unknown = AgentKind(id: "unknown", command: nil)

    /// Legacy Swift case name ⇄ manifest id. Frozen: a state file written by
    /// any build must decode in any other.
    private static let legacy: [String: String] = [
        "claudeCode": "claude-code",
        "codex": "codex",
        "cursor": "cursor",
        "gemini": "gemini",
        "shell": "shell",
    ]
    private static let legacyReversed: [String: String] = [
        "claude-code": "claudeCode",
        "codex": "codex",
        "cursor": "cursor",
        "gemini": "gemini",
        "shell": "shell",
    ]

    init(id: String, command: String? = nil) {
        self.id = id
        self.command = command
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: AnyKey.self)
        guard let key = container.allKeys.first else {
            self = .unknown
            return
        }
        let payload = try? container.decode(Payload.self, forKey: key)
        switch key.stringValue {
        case let name where Self.legacy[name] != nil:
            self.init(id: Self.legacy[name]!)
        case "generic":
            self.init(id: "generic", command: payload?.command)
        case "agent":
            self.init(id: payload?.id ?? "unknown")
        default:
            // An unrecognised case is still a real agent on a newer daemon;
            // keeping its key beats collapsing every one of them to "unknown".
            self.init(id: key.stringValue)
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: AnyKey.self)
        if let legacyName = Self.legacyReversed[id] {
            try container.encode(Payload(), forKey: AnyKey(legacyName))
        } else if id == "generic" {
            try container.encode(Payload(command: command), forKey: AnyKey("generic"))
        } else {
            try container.encode(Payload(id: id), forKey: AnyKey("agent"))
        }
    }

    private struct Payload: Codable {
        var id: String?
        var command: String?
    }

    /// Which brand mark, if any, stands for this kind.
    var brandMark: BrandMarkKind? {
        switch id {
        case "claude-code": .claude
        case "codex": .openAI
        case "cursor": .cursor
        case "gemini": .gemini
        default: nil
        }
    }

    /// A short human label for chips.
    var displayName: String {
        switch id {
        case "claude-code": "claude"
        case "generic": command.map { String($0.prefix(24)) } ?? "generic"
        default: id
        }
    }
}

// MARK: - Status

enum RiskHint: String, Codable, Sendable {
    case destructive
    case network
    case fileWrite
    case neutral
}

enum NeedsInputKind: String, Codable, Sendable {
    case permission
    case question
}

struct NeedsInputDetail: Codable, Hashable, Sendable {
    var kind: NeedsInputKind?
    var summary: String?
    var promptExcerpt: String?
    var options: [String]?
    var riskHint: RiskHint?
    var occurredAt: Double?
}

struct ExitInfo: Codable, Hashable, Sendable {
    var code: Int?
    var reason: String?
}

/// `SessionStatus`, in the same single-key encoding.
enum SessionStatus: Codable, Hashable, Sendable {
    case starting
    case idle
    case working
    case needsInput(NeedsInputKind)
    case exited(ExitInfo)
    case unknown

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: AnyKey.self)
        guard let key = container.allKeys.first else {
            self = .unknown
            return
        }
        switch key.stringValue {
        case "starting": self = .starting
        case "idle": self = .idle
        case "working": self = .working
        case "needsInput":
            let payload = try? container.decode(Unnamed<NeedsInputKind>.self, forKey: key)
            self = .needsInput(payload?._0 ?? .question)
        case "exited":
            let payload = try? container.decode(Unnamed<ExitInfo>.self, forKey: key)
            self = .exited(payload?._0 ?? ExitInfo())
        default: self = .unknown
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: AnyKey.self)
        switch self {
        case .starting: try container.encode(Empty(), forKey: AnyKey("starting"))
        case .idle: try container.encode(Empty(), forKey: AnyKey("idle"))
        case .working: try container.encode(Empty(), forKey: AnyKey("working"))
        case let .needsInput(kind):
            try container.encode(Unnamed(_0: kind), forKey: AnyKey("needsInput"))
        case let .exited(info):
            try container.encode(Unnamed(_0: info), forKey: AnyKey("exited"))
        case .unknown: try container.encode(Empty(), forKey: AnyKey("unknown"))
        }
    }

    /// Swift's `Codable` spells an enum's unlabelled payload `_0`.
    private struct Unnamed<Value: Codable>: Codable {
        let _0: Value
    }

    private struct Empty: Codable {}
}

/// `AttentionLevel` — how loudly a session is asking for a human.
enum AttentionLevel: Int, Comparable, Sendable {
    case needsInput = 0
    case doneUnseen = 1
    case working = 2
    case idleSeen = 3
    case none = 4

    static func < (lhs: Self, rhs: Self) -> Bool { lhs.rawValue < rhs.rawValue }
}

// MARK: - Session

struct SessionRecord: Codable, Identifiable, Hashable, Sendable {
    var id: String
    var kind: AgentKind
    var foregroundAgent: AgentKind?
    var cwd: String
    var projectID: String
    var worktreePath: String?
    var gitBranch: String?
    var title: String
    var status: SessionStatus
    var needsInput: NeedsInputDetail?
    var parent: String?
    var createdAt: Double
    var updatedAt: Double
    var lastSeenAt: Double?
    var pinned: Bool
    var archivedAt: Double?
    var host: String?
    var hibernation: Hibernation?

    struct Hibernation: Codable, Hashable, Sendable {
        var reason: String?
    }

    /// The kind actually running, which is what the row's mark shows.
    var effectiveKind: AgentKind { foregroundAgent ?? kind }

    var isArchived: Bool { archivedAt != nil }
    var isHibernated: Bool { hibernation != nil }

    var hasEnded: Bool {
        if case .exited = status { return !isArchived }
        return false
    }

    /// Mirrors the desktop's `SessionRecord::attention`.
    var attention: AttentionLevel {
        if case .needsInput = status { return .needsInput }
        if case .working = status { return .working }
        if case .starting = status { return .working }
        if case .exited = status { return .none }
        // Idle: whether it has been seen since it last finished is what
        // separates "done, look at me" from "done, you already looked".
        let seen = lastSeenAt ?? 0
        return seen >= updatedAt ? .idleSeen : .doneUnseen
    }

    /// `status_state` — what the glyph should show.
    var statusState: StatusState {
        if isHibernated { return .hibernated }
        switch attention {
        case .needsInput:
            return .needsInput(destructive: needsInput?.riskHint == .destructive)
        case .doneUnseen: return .doneUnseen
        case .working: return .working
        case .idleSeen: return .idleSeen
        case .none: return .none
        }
    }
}

/// `StatusState`.
enum StatusState: Hashable, Sendable {
    case working
    case needsInput(destructive: Bool)
    case doneUnseen
    case idleSeen
    case none
    case hibernated

    var label: String {
        switch self {
        case .working: "Working"
        case let .needsInput(destructive): destructive ? "Needs input · destructive" : "Needs input"
        case .doneUnseen: "Done · unseen"
        case .idleSeen: "Idle · seen"
        case .none: "Ended"
        case .hibernated: "Hibernated"
        }
    }
}

struct Project: Codable, Hashable, Sendable {
    var id: String
    var name: String?
    var root: String?

    var displayName: String {
        name ?? root.map { ($0 as NSString).lastPathComponent } ?? id
    }
}

// MARK: - Coding helpers

/// A `CodingKey` for objects whose single key is data, not schema.
struct AnyKey: CodingKey {
    var stringValue: String
    var intValue: Int? { nil }

    init(stringValue: String) { self.stringValue = stringValue }
    init(_ stringValue: String) { self.stringValue = stringValue }
    init?(intValue: Int) { nil }
}
