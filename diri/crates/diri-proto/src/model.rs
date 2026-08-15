//! Shared wire model ported from `DirijorCore`.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// A Foundation `Date` encoded by Dirijor as milliseconds since 1970.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct DateMillis(pub f64);

impl From<std::time::SystemTime> for DateMillis {
    /// Times before the epoch cannot happen for anything the daemon stamps and
    /// would only arise from a badly wrong clock; they clamp to zero rather
    /// than panicking somewhere far from the cause.
    fn from(time: std::time::SystemTime) -> Self {
        let millis = time
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        Self(millis)
    }
}

macro_rules! string_enum {
    ($(#[$meta:meta])* pub enum $name:ident { $($variant:ident => $wire:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant,)+
            Unknown,
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                let wire = match self {
                    $(Self::$variant => $wire,)+
                    Self::Unknown => "unknown",
                };
                serializer.serialize_str(wire)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let wire = String::deserialize(deserializer)?;
                Ok(match wire.as_str() {
                    $($wire => Self::$variant,)+
                    _ => Self::Unknown,
                })
            }
        }
    };
}

/// What runs inside a session's PTY, identified by its detection-manifest id.
///
/// Mirrors `DirijorCore.AgentKind`, which stopped being a closed enum so that
/// adding an agent is a manifest drop on the daemon rather than a code change on
/// both sides of the wire. The client therefore can't enumerate agents either —
/// it compares ids, and gets human-facing details (name, glyph, approve
/// keystroke) from the descriptors the daemon ships in `agent.readiness`.
///
/// The five ids that predate the rework keep dedicated constants because the
/// client still has hand-tuned brand treatment for them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentKind {
    id: std::borrow::Cow<'static, str>,
    /// Only meaningful for `generic`: the command line the user asked for.
    command: Option<String>,
}

impl AgentKind {
    pub const CLAUDE_CODE_ID: &'static str = "claude-code";
    pub const CODEX_ID: &'static str = "codex";
    pub const CURSOR_ID: &'static str = "cursor";
    pub const GEMINI_ID: &'static str = "gemini";
    pub const SHELL_ID: &'static str = "shell";
    pub const GENERIC_ID: &'static str = "generic";

    pub const CLAUDE_CODE: Self = Self::builtin(Self::CLAUDE_CODE_ID);
    pub const CODEX: Self = Self::builtin(Self::CODEX_ID);
    pub const CURSOR: Self = Self::builtin(Self::CURSOR_ID);
    pub const GEMINI: Self = Self::builtin(Self::GEMINI_ID);
    pub const SHELL: Self = Self::builtin(Self::SHELL_ID);
    /// A kind we could not parse at all. Distinct from a manifest agent the
    /// client simply hasn't heard of, which keeps its real id.
    pub const UNKNOWN: Self = Self::builtin("unknown");

    const fn builtin(id: &'static str) -> Self {
        Self {
            id: std::borrow::Cow::Borrowed(id),
            command: None,
        }
    }

    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: std::borrow::Cow::Owned(id.into()),
            command: None,
        }
    }

    #[must_use]
    pub fn generic(command: impl Into<String>) -> Self {
        Self {
            id: std::borrow::Cow::Borrowed(Self::GENERIC_ID),
            command: Some(command.into()),
        }
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The command line, for `generic` kinds only.
    #[must_use]
    pub fn command(&self) -> Option<&str> {
        self.command.as_deref()
    }

    /// True for the two kinds that are a raw terminal rather than an agent.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.id == Self::SHELL_ID || self.id == Self::GENERIC_ID
    }
}

impl Default for AgentKind {
    fn default() -> Self {
        Self::UNKNOWN
    }
}

/// Legacy Swift enum case name ⇄ manifest id, for the kinds that existed before
/// `AgentKind` became manifest-backed. Their wire encoding is frozen: a state
/// file or session list written by ANY build must decode in any other.
const LEGACY_CASES: [(&str, &str); 5] = [
    ("claudeCode", AgentKind::CLAUDE_CODE_ID),
    ("codex", AgentKind::CODEX_ID),
    ("cursor", AgentKind::CURSOR_ID),
    ("gemini", AgentKind::GEMINI_ID),
    ("shell", AgentKind::SHELL_ID),
];

/// Case key used for every manifest agent without a legacy enum case.
const OPEN_CASE_KEY: &str = "agent";

impl Serialize for AgentKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let legacy = LEGACY_CASES
            .iter()
            .find(|(_, id)| *id == self.id)
            .map(|(case, _)| *case);
        let (case, payload) = if self.id == Self::GENERIC_ID {
            (
                Self::GENERIC_ID,
                Value::Object(Map::from_iter([(
                    "command".into(),
                    Value::String(self.command.clone().unwrap_or_default()),
                )])),
            )
        } else if let Some(case) = legacy {
            (case, Value::Object(Map::new()))
        } else {
            (
                OPEN_CASE_KEY,
                Value::Object(Map::from_iter([(
                    "id".into(),
                    Value::String(self.id.to_string()),
                )])),
            )
        };
        keyed_enum(case, payload).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AgentKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        if let Some(id) = value.as_str() {
            return Ok(Self::new(id));
        }
        let (case, payload) = decode_keyed_enum_value(value).map_err(de::Error::custom)?;
        #[derive(Default, Deserialize)]
        struct Payload {
            id: Option<String>,
            command: Option<String>,
        }
        let payload: Payload = serde_json::from_value(payload).unwrap_or_default();
        if let Some((_, id)) = LEGACY_CASES.iter().find(|(legacy, _)| *legacy == case) {
            return Ok(Self::builtin(id));
        }
        Ok(match case.as_str() {
            AgentKind::GENERIC_ID => Self::generic(payload.command.unwrap_or_default()),
            // A manifest agent this build has no built-in knowledge of. Keeping
            // the id (rather than collapsing to Unknown) is what lets the
            // descriptor catalog name and style it.
            OPEN_CASE_KEY => payload.id.map_or(Self::UNKNOWN, Self::new),
            // A case key from a build newer than the encoding contract: treat
            // the key itself as the manifest id.
            other => Self::new(other),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttentionLevel {
    None,
    IdleSeen,
    Working,
    DoneUnseen,
    NeedsInput,
    Unknown,
}

impl Serialize for AttentionLevel {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i8(match self {
            Self::None => 0,
            Self::IdleSeen => 1,
            Self::Working => 2,
            Self::DoneUnseen => 3,
            Self::NeedsInput => 4,
            Self::Unknown => -1,
        })
    }
}

impl<'de> Deserialize<'de> for AttentionLevel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match i64::deserialize(deserializer)? {
            0 => Self::None,
            1 => Self::IdleSeen,
            2 => Self::Working,
            3 => Self::DoneUnseen,
            4 => Self::NeedsInput,
            _ => Self::Unknown,
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

/// Swift spelling retained for protocol-oriented callers.
pub use SessionId as SessionID;

impl SessionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ProjectId(pub String);

/// Swift spelling retained for protocol-oriented callers.
pub use ProjectId as ProjectID;

impl ProjectId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl std::fmt::Display for ProjectId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

string_enum! {
    pub enum NeedsInputKind {
        Permission => "permission",
        Question => "question",
    }
}

string_enum! {
    pub enum RiskHint {
        Destructive => "destructive",
        Network => "network",
        FileWrite => "fileWrite",
        Neutral => "neutral",
    }
}

string_enum! {
    pub enum NeedsInputSource {
        ClaudePermissionHook => "claudePermissionHook",
        ClaudeNotificationHook => "claudeNotificationHook",
        CodexNotify => "codexNotify",
        ScreenScrape => "screenScrape",
    }
}

string_enum! {
    /// The bounded, structured signal that justified a session status.
    ///
    /// This deliberately describes only the class of signal. Raw hook
    /// payloads, terminal captures, prompts, paths, and environment values do
    /// not belong in status evidence.
    pub enum StatusEvidenceSource {
        Hook => "hook",
        Notify => "notify",
        ScreenRule => "screenRule",
        ProcessLiveness => "processLiveness",
        Staleness => "staleness",
    }
}

string_enum! {
    /// Fixed fallback explanations for decisions without a manifest rule.
    pub enum StatusFallbackReason {
        StartupGrace => "startupGrace",
        ProcessOnly => "processOnly",
        StaleSignals => "staleSignals",
        ProcessExited => "processExited",
    }
}

/// Privacy-safe evidence for one canonical status decision.
///
/// `status` intentionally duplicates the record's status. Clients must ignore
/// evidence whose status does not match the enclosing record, preventing an
/// out-of-order or mixed-version update from explaining a newer contradictory
/// state with stale evidence.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusEvidence {
    pub status: SessionStatus,
    pub source: StatusEvidenceSource,
    pub signal_at: DateMillis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_rule_id: Option<String>,
    #[serde(default)]
    pub startup_grace_active: bool,
    #[serde(default)]
    pub anti_flicker_active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<StatusFallbackReason>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NeedsInputDetail {
    pub kind: NeedsInputKind,
    pub source: NeedsInputSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_excerpt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
    pub risk_hint: RiskHint,
    pub occurred_at: DateMillis,
}

string_enum! {
    pub enum ExitReason {
        Exited => "exited",
        Signaled => "signaled",
        DaemonRestart => "daemonRestart",
        External => "external",
        Archived => "archived",
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExitInfo {
    pub reason: ExitReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionStatus {
    Starting,
    Idle,
    Working,
    NeedsInput(NeedsInputKind),
    Exited(ExitInfo),
    Unknown,
}

impl Serialize for SessionStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let (case, payload) = match self {
            Self::Starting => ("starting", Value::Object(Map::new())),
            Self::Idle => ("idle", Value::Object(Map::new())),
            Self::Working => ("working", Value::Object(Map::new())),
            Self::NeedsInput(kind) => (
                "needsInput",
                Value::Object(Map::from_iter([(
                    "_0".into(),
                    serde_json::to_value(kind).map_err(serde::ser::Error::custom)?,
                )])),
            ),
            Self::Exited(info) => (
                "exited",
                Value::Object(Map::from_iter([(
                    "_0".into(),
                    serde_json::to_value(info).map_err(serde::ser::Error::custom)?,
                )])),
            ),
            Self::Unknown => ("unknown", Value::Object(Map::new())),
        };
        keyed_enum(case, payload).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SessionStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let (case, payload) = decode_keyed_enum(deserializer)?;
        match case.as_str() {
            "starting" => Ok(Self::Starting),
            "idle" => Ok(Self::Idle),
            "working" => Ok(Self::Working),
            "needsInput" => Ok(Self::NeedsInput(decode_unnamed(payload)?)),
            "exited" => Ok(Self::Exited(decode_unnamed(payload)?)),
            "unknown" => Ok(Self::Unknown),
            _ => Ok(Self::Unknown),
        }
    }
}

string_enum! {
    pub enum Resumability {
        Live => "live",
        Resumable => "resumable",
        TranscriptMissing => "transcriptMissing",
        NotResumable => "notResumable",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TitleSource {
    Placeholder,
    FirstPrompt,
    AgentProvided,
    DirijorAssigned,
    UserRename,
    Unknown,
}

impl Serialize for TitleSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i8(match self {
            Self::Placeholder => 0,
            Self::FirstPrompt => 1,
            Self::AgentProvided => 2,
            Self::DirijorAssigned => 3,
            Self::UserRename => 4,
            Self::Unknown => -1,
        })
    }
}

impl<'de> Deserialize<'de> for TitleSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match i64::deserialize(deserializer)? {
            0 => Self::Placeholder,
            1 => Self::FirstPrompt,
            2 => Self::AgentProvided,
            3 => Self::DirijorAssigned,
            4 => Self::UserRename,
            _ => Self::Unknown,
        })
    }
}

string_enum! {
    pub enum HibernationReason {
        Idle => "idle",
        MemoryPressure => "memoryPressure",
        Manual => "manual",
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HibernationInfo {
    pub since: DateMillis,
    pub reason: HibernationReason,
    pub tree_pids: Vec<i32>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "swift_int_keyed_map",
        default
    )]
    pub tree_start_times: Option<BTreeMap<i32, i64>>,
}

/// Swift's Codable encodes integer-keyed dictionaries as a flat JSON array of
/// alternating key/value numbers (`[k1, v1, k2, v2, …]`), not as an object.
/// Accept both forms on decode; emit the Swift form on encode.
mod swift_int_keyed_map {
    use super::BTreeMap;
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Flat(Vec<i64>),
        Map(BTreeMap<i32, i64>),
    }

    pub fn serialize<S: Serializer>(
        value: &Option<BTreeMap<i32, i64>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            None => serializer.serialize_none(),
            Some(map) => {
                let mut flat = Vec::with_capacity(map.len() * 2);
                for (k, v) in map {
                    flat.push(i64::from(*k));
                    flat.push(*v);
                }
                flat.serialize(serializer)
            }
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<BTreeMap<i32, i64>>, D::Error> {
        let Some(repr) = Option::<Repr>::deserialize(deserializer)? else {
            return Ok(None);
        };
        match repr {
            Repr::Map(map) => Ok(Some(map)),
            Repr::Flat(flat) => {
                if flat.len() % 2 != 0 {
                    return Err(D::Error::custom(
                        "flat int-keyed map must have an even number of elements",
                    ));
                }
                let mut map = BTreeMap::new();
                for pair in flat.chunks_exact(2) {
                    let key = i32::try_from(pair[0])
                        .map_err(|_| D::Error::custom("int-keyed map key out of i32 range"))?;
                    map.insert(key, pair[1]);
                }
                Ok(Some(map))
            }
        }
    }
}

string_enum! {
    pub enum ArtifactKind {
        PullRequest => "pullRequest",
        LinearIssue => "linearIssue",
        Preview => "preview",
        Link => "link",
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionArtifact {
    pub kind: ArtifactKind,
    pub url: String,
    pub first_seen_at: DateMillis,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PrCheck {
    pub name: String,
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Swift spells this type `PRCheck`; retain that spelling as an alias.
pub type PRCheck = PrCheck;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrDiscussionItem {
    pub kind: String,
    pub author: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateMillis>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Swift spells this type `PRDiscussionItem`; retain that spelling as an alias.
pub type PRDiscussionItem = PrDiscussionItem;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestStatus {
    pub url: String,
    pub number: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_ref_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_ref_name: Option<String>,
    pub state: String,
    pub is_draft: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mergeable: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_state_status: Option<String>,
    pub additions: i64,
    pub deletions: i64,
    pub changed_files: i64,
    pub comment_count: i64,
    pub review_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_threads: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_threads: Option<i64>,
    pub checks_passed: i64,
    pub checks_failed: i64,
    pub checks_pending: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks: Option<Vec<PrCheck>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discussion: Option<Vec<PrDiscussionItem>>,
    pub fetched_at: DateMillis,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortInfo {
    pub port: i64,
    pub process_name: String,
}

/// Verbs the authoritative Engine has resolved for one concrete session.
///
/// Clients consume this value instead of re-parsing commands or hard-coding
/// Agent names. The field on [`SessionRecord`] is optional so an older Engine
/// still degrades to the established resumability behavior.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCapabilities {
    pub resume: bool,
    pub archive: bool,
    pub send_text: bool,
    pub quick_approve: bool,
    pub reliable_completion: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    pub id: SessionId,
    pub kind: AgentKind,
    pub cwd: String,
    #[serde(rename = "projectID")]
    pub project_id: ProjectId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    pub title: String,
    pub title_source: TitleSource,
    /// The first prompt used to create this session. It is retained so a
    /// sibling proposal can reproduce the user's request without scraping a
    /// live terminal; older records fall back to their transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originating_prompt: Option<String>,
    #[serde(rename = "agentSessionID", skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    pub status: SessionStatus,
    /// Additive decision metadata. Absent for records written by older Engine
    /// builds; clients keep the normal status summary primary either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_evidence: Option<StatusEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_input: Option<NeedsInputDetail>,
    pub resumability: Resumability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<SessionCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<SessionId>,
    pub created_at: DateMillis,
    pub updated_at: DateMillis,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_turn_completed_at: Option<DateMillis>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<DateMillis>,
    pub pinned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<DateMillis>,
    /// `HostEntry.id` when this session runs through a remote PTY Holder;
    /// absent ⇒ local.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Measured logout-survival mode for a remote Holder. It is never inferred
    /// from platform alone; `non-persistent` is surfaced to callers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_persistence: Option<crate::remote_pty::PersistenceCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hibernation: Option<HibernationInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Vec<SessionArtifact>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull_requests: Option<Vec<PullRequestStatus>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listening_ports: Option<Vec<PortInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub foreground_agent: Option<AgentKind>,
}

impl SessionRecord {
    pub fn effective_kind(&self) -> &AgentKind {
        self.foreground_agent.as_ref().unwrap_or(&self.kind)
    }

    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }

    /// Mixed-version-safe answer used by every Resume surface.
    pub fn can_resume(&self) -> bool {
        self.capabilities
            .map_or(self.resumability == Resumability::Resumable, |value| {
                value.resume
            })
    }

    pub fn attention(&self) -> AttentionLevel {
        match self.status {
            SessionStatus::NeedsInput(_) => AttentionLevel::NeedsInput,
            SessionStatus::Working | SessionStatus::Starting => AttentionLevel::Working,
            SessionStatus::Idle => {
                let completed = self.last_turn_completed_at.map(|date| date.0);
                let seen = self.last_seen_at.map_or(f64::NEG_INFINITY, |date| date.0);
                if completed.is_some_and(|date| date > seen) {
                    AttentionLevel::DoneUnseen
                } else {
                    AttentionLevel::IdleSeen
                }
            }
            SessionStatus::Exited(_) | SessionStatus::Unknown => AttentionLevel::None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: ProjectId,
    pub root: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_order: Option<i64>,
    /// `None` means the root is local; a host id means that remote machine
    /// owns it. Absent from records persisted by older daemons — readers fall
    /// back to session-derived hosts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeInfo {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_detached: bool,
    pub is_prunable: bool,
}

fn keyed_enum(case: &str, payload: Value) -> Value {
    Value::Object(Map::from_iter([(case.to_owned(), payload)]))
}

fn decode_keyed_enum<'de, D>(deserializer: D) -> Result<(String, Value), D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    decode_keyed_enum_value(value).map_err(de::Error::custom)
}

fn decode_keyed_enum_value(value: Value) -> Result<(String, Value), &'static str> {
    let object = value
        .as_object()
        .ok_or("Swift enum must be a keyed JSON object")?;
    if object.len() != 1 {
        return Err("Swift enum object must contain exactly one case");
    }
    let (case, payload) = object.iter().next().expect("length checked above");
    Ok((case.clone(), payload.clone()))
}

fn decode_unnamed<T, E>(payload: Value) -> Result<T, E>
where
    T: serde::de::DeserializeOwned,
    E: de::Error,
{
    let object = payload
        .as_object()
        .ok_or_else(|| E::custom("associated-value payload must be an object"))?;
    let value = object
        .get("_0")
        .ok_or_else(|| E::custom("associated-value payload is missing _0"))?;
    serde_json::from_value(value.clone()).map_err(E::custom)
}
