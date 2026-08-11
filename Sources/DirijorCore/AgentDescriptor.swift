import Foundation

/// Everything Dirijor needs to know about ONE agent CLI, declared as data.
///
/// This is the `"agent"` block of a detection manifest
/// (`Sources/DirijorCore/Resources/manifests/<id>.json`). It exists so that
/// adding an agent is a file drop rather than a patch across the daemon, the
/// CLI, the protocol and the Rust client — which is what it used to be when
/// `AgentKind` was a closed Swift enum.
///
/// Fields are decoded permissively (`decodeIfPresent` + defaults) so a manifest
/// written against an older schema keeps loading, and a user-authored override
/// only has to spell out the parts it wants to change.
public struct AgentDescriptor: Codable, Hashable, Sendable {
    /// How the status reducer should be driven for this agent. Mirrors
    /// `StatusReducer.Authority`, which lives in DirijorDetection and therefore
    /// can't be referenced from here.
    public enum StatusAuthority: String, Codable, Hashable, Sendable {
        /// Hooks drive state, screen scanning only arbitrates blockers (Claude).
        case hooks
        /// Screen scanning drives state; notify/hook events confirm (Codex, Gemini…).
        case screen
        /// Process liveness only: starting → running → exited (shells, unknown CLIs).
        case process
    }

    /// How to reattach to a previous conversation from the command line.
    public struct Resume: Codable, Hashable, Sendable {
        public enum Style: String, Codable, Hashable, Sendable {
            /// `<binary> --resume <id>` — a flag and the id as separate argv words.
            case flag
            /// `<binary> --resume=<id>` — some CLIs only accept the joined form
            /// (Copilot).
            case flagJoined
            /// `<binary> resume <id>` — a subcommand carries it (Codex).
            case subcommand
            /// `<binary> resume` with no id: the CLI picks its own latest
            /// conversation. Only usable where an id can never be learned, and
            /// deliberately opt-in — "latest" is a guess about user intent.
            case latest
        }

        public var style: Style
        /// The literal token: `--resume`, `resume`, `--session`, `--continue`, …
        public var token: String

        public init(style: Style, token: String) {
            self.style = style
            self.token = token
        }

        /// argv fragment that reattaches to `id`, appended after the binary.
        public func argv(id: String) -> [String] {
            switch style {
            case .flag, .subcommand: [token, id]
            case .flagJoined: ["\(token)=\(id)"]
            case .latest: [token]
            }
        }
    }

    /// A canned keystroke answer to a permission prompt, sent from a
    /// notification action without focusing the session.
    public struct Keystroke: Codable, Hashable, Sendable {
        /// Text to type. Empty means "send nothing, just the Return".
        public var text: String
        /// Whether to append a Return after `text`.
        public var submit: Bool

        public init(text: String, submit: Bool) {
            self.text = text
            self.submit = submit
        }

        /// Escape: the near-universal "cancel this prompt" key.
        public static let escape = Keystroke(text: "\u{1b}", submit: false)
    }

    /// Per-launch config injection. Each flag names a *mechanism* Dirijor
    /// implements, not a file — the daemon owns the file contents.
    public struct Injection: Codable, Hashable, Sendable {
        /// `--settings <hooks.json>`: Claude Code's hook protocol, which is what
        /// makes `statusAuthority: hooks` trustworthy.
        public var claudeHooks: Bool = false
        /// `--mcp-config <servers.json>`: the `dirijor` MCP server (spawn_agent, …).
        public var claudeMCP: Bool = false
        /// `-c notify=[…]`: Codex's turn-complete callback.
        public var codexNotify: Bool = false
        /// `-c mcp_servers.dirijor.…`: Codex's TOML-override MCP wiring.
        public var codexMCP: Bool = false
        /// Cursor has no `--mcp-config`. Launch a session-local `--plugin-dir`
        /// whose `mcp.json` advertises the `dirijor` stdio server.
        public var cursorMCP: Bool = false
        /// Same plugin ships `hooks.json`: Cursor `stop` → `dirijor hook Stop`
        /// (strong idle / done chime), `beforeSubmitPrompt` → working.
        public var cursorHooks: Bool = false

        public init(
            claudeHooks: Bool = false, claudeMCP: Bool = false,
            codexNotify: Bool = false, codexMCP: Bool = false,
            cursorMCP: Bool = false, cursorHooks: Bool = false
        ) {
            self.claudeHooks = claudeHooks
            self.claudeMCP = claudeMCP
            self.codexNotify = codexNotify
            self.codexMCP = codexMCP
            self.cursorMCP = cursorMCP
            self.cursorHooks = cursorHooks
        }

        public var isEmpty: Bool {
            !claudeHooks && !claudeMCP && !codexNotify && !codexMCP
                && !cursorMCP && !cursorHooks
        }

        private enum CodingKeys: String, CodingKey {
            case claudeHooks, claudeMCP, codexNotify, codexMCP, cursorMCP, cursorHooks
        }

        public init(from decoder: any Decoder) throws {
            let c = try decoder.container(keyedBy: CodingKeys.self)
            claudeHooks = try c.decodeIfPresent(Bool.self, forKey: .claudeHooks) ?? false
            claudeMCP = try c.decodeIfPresent(Bool.self, forKey: .claudeMCP) ?? false
            codexNotify = try c.decodeIfPresent(Bool.self, forKey: .codexNotify) ?? false
            codexMCP = try c.decodeIfPresent(Bool.self, forKey: .codexMCP) ?? false
            cursorMCP = try c.decodeIfPresent(Bool.self, forKey: .cursorMCP) ?? false
            cursorHooks = try c.decodeIfPresent(Bool.self, forKey: .cursorHooks) ?? false
        }
    }

    // MARK: Identity

    /// Manifest id — the stable, kebab-case key an `AgentKind` carries.
    public var id: String
    /// "Claude Code", "Amp", … Shown in the sidebar, banners and titles.
    public var displayName: String
    /// Lowercase one-word label for compact output (MCP results, `dirijor ls`).
    public var shortLabel: String
    /// Single character used where a logo can't be drawn (CLI listings).
    public var glyph: String
    /// Extra names accepted for this agent on the MCP `spawn_agent` boundary
    /// and in the CLI, beyond `id` and `shortLabel`.
    public var aliases: [String]

    // MARK: Capability

    /// True when Dirijor has real status detection for the agent (a manifest
    /// with `statusModel: full`). Drives "first-class" affordances everywhere:
    /// PATH readiness checks, needs-input notifications, resume UI.
    public var firstClass: Bool
    public var statusAuthority: StatusAuthority

    // MARK: Launching

    /// argv[0]. nil ⇒ this kind doesn't launch a named CLI (`shell`, `generic`).
    public var binary: String?
    /// Fixed arguments appended after `binary` on every spawn.
    public var spawnArgs: [String]
    /// A flag that accepts a caller-minted conversation UUID
    /// (`--session-id`). When set, Dirijor generates the UUID at spawn so it
    /// can resume the conversation later without waiting for the agent to
    /// report one. nil ⇒ the id (if any) arrives out-of-band via hooks/notify.
    public var sessionIDFlag: String?
    public var resume: Resume?
    /// Run the CLI as the child of an interactive login shell and drop back to
    /// that shell on exit, so a self-update restart or a stray Ctrl-C leaves a
    /// prompt instead of tearing the PTY down into "agent exited".
    public var returnToLoginShell: Bool
    /// Environment forced onto the child (workarounds, feature switches).
    public var env: [String: String]
    /// Env-var prefixes this agent exports to its children to mark nesting.
    /// The daemon strips the union of all agents' prefixes from inherited
    /// environments so a Dirijor started *inside* an agent doesn't hand its
    /// spawned children a "you are a nested session" marker.
    public var envScrubPrefixes: [String]
    public var injection: Injection

    // MARK: Adoption

    /// Path components / basenames that identify this agent's process when it
    /// is typed into a plain shell tab, so the session can adopt it. Matched
    /// against the child's real executable path.
    public var foregroundExecNames: [String]

    // MARK: Answering prompts

    public var approve: Keystroke?
    public var deny: Keystroke

    public init(
        id: String,
        displayName: String,
        shortLabel: String? = nil,
        glyph: String = "▸",
        aliases: [String] = [],
        firstClass: Bool = false,
        statusAuthority: StatusAuthority = .process,
        binary: String? = nil,
        spawnArgs: [String] = [],
        sessionIDFlag: String? = nil,
        resume: Resume? = nil,
        returnToLoginShell: Bool = false,
        env: [String: String] = [:],
        envScrubPrefixes: [String] = [],
        injection: Injection = Injection(),
        foregroundExecNames: [String] = [],
        approve: Keystroke? = nil,
        deny: Keystroke = .escape
    ) {
        self.id = id
        self.displayName = displayName
        self.shortLabel = shortLabel ?? id
        self.glyph = glyph
        self.aliases = aliases
        self.firstClass = firstClass
        self.statusAuthority = statusAuthority
        self.binary = binary
        self.spawnArgs = spawnArgs
        self.sessionIDFlag = sessionIDFlag
        self.resume = resume
        self.returnToLoginShell = returnToLoginShell
        self.env = env
        self.envScrubPrefixes = envScrubPrefixes
        self.injection = injection
        self.foregroundExecNames = foregroundExecNames
        self.approve = approve
        self.deny = deny
    }

    /// Whether Dirijor can actually resume this agent's conversations.
    ///
    /// A resume spec alone isn't enough — most CLIs accept `--resume <id>` but
    /// give us no way to LEARN an id, and a Resume button that can never build
    /// a command line is worse than no button. So this also requires a source
    /// for the conversation id: a caller-minted UUID at spawn, or a hook/notify
    /// channel that reports one.
    ///
    /// `.latest` is the exception, and the reason this isn't a one-line check:
    /// it means "reopen whatever this CLI considers the most recent
    /// conversation", so it needs no id at all and `resumeArgv` already builds
    /// it without one. Gating it behind an id source demoted agents that can
    /// resume perfectly well — Cursor mints its chat ids server-side yet
    /// `cursor-agent resume` reopens the last chat regardless.
    ///
    /// This is only sound because sessions are spawned in their own cwd, which
    /// is the scope these CLIs use for "most recent". Declare `.latest` only
    /// for an agent whose help output actually says so.
    public var canResume: Bool {
        guard let resume else { return false }
        if resume.style == .latest { return true }
        return sessionIDFlag != nil || injection.claudeHooks || injection.codexNotify
    }

    /// The descriptor used for an id no manifest declares: an unknown agent is
    /// a terminal we happen to know the name of. Conservative on purpose — a
    /// wrong resume flag is worse than no resume button.
    public static func fallback(id: String) -> AgentDescriptor {
        AgentDescriptor(
            id: id,
            displayName: id.split(separator: "-").map(\.capitalized).joined(separator: " "),
            shortLabel: id)
    }

    // MARK: Codable

    private enum CodingKeys: String, CodingKey {
        case id, displayName, shortLabel, glyph, aliases
        case firstClass, statusAuthority
        case binary, spawnArgs, sessionIDFlag, resume, returnToLoginShell
        case env, envScrubPrefixes, injection
        case foregroundExecNames
        case approve, deny
    }

    /// Every field but `displayName` defaults, so a manifest only spells out
    /// what makes its agent different from a plain terminal.
    ///
    /// `id` is optional here because inside a manifest the descriptor is nested
    /// under `"agent"` and inherits the manifest's own `id` — a file can't
    /// disagree with itself about which agent it describes. `AgentCatalog`
    /// stamps it after decoding.
    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decodeIfPresent(String.self, forKey: .id) ?? ""
        displayName = try c.decode(String.self, forKey: .displayName)
        shortLabel = try c.decodeIfPresent(String.self, forKey: .shortLabel) ?? id
        glyph = try c.decodeIfPresent(String.self, forKey: .glyph) ?? "▸"
        aliases = try c.decodeIfPresent([String].self, forKey: .aliases) ?? []
        firstClass = try c.decodeIfPresent(Bool.self, forKey: .firstClass) ?? false
        statusAuthority =
            try c.decodeIfPresent(StatusAuthority.self, forKey: .statusAuthority) ?? .process
        binary = try c.decodeIfPresent(String.self, forKey: .binary)
        spawnArgs = try c.decodeIfPresent([String].self, forKey: .spawnArgs) ?? []
        sessionIDFlag = try c.decodeIfPresent(String.self, forKey: .sessionIDFlag)
        resume = try c.decodeIfPresent(Resume.self, forKey: .resume)
        returnToLoginShell = try c.decodeIfPresent(Bool.self, forKey: .returnToLoginShell) ?? false
        env = try c.decodeIfPresent([String: String].self, forKey: .env) ?? [:]
        envScrubPrefixes = try c.decodeIfPresent([String].self, forKey: .envScrubPrefixes) ?? []
        injection = try c.decodeIfPresent(Injection.self, forKey: .injection) ?? Injection()
        foregroundExecNames =
            try c.decodeIfPresent([String].self, forKey: .foregroundExecNames) ?? []
        approve = try c.decodeIfPresent(Keystroke.self, forKey: .approve)
        deny = try c.decodeIfPresent(Keystroke.self, forKey: .deny) ?? .escape
    }
}
