//! First-party node management protocol.
//!
//! The node protocol is deliberately separate from terminal attachment. It is
//! the stable management seam for identities, provider accounts, fleet usage,
//! and transactional handoff. SSH remains a recovery/data-plane fallback.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::control::{JsonValue, WIRE_VERSION};

pub const NODE_PROTOCOL_VERSION: u32 = 1;

pub struct NodeMethod;

impl NodeMethod {
    pub const HELLO: &'static str = "node.hello";
    pub const STATUS: &'static str = "node.status";
    pub const ACCOUNT_LIST: &'static str = "account.list";
    pub const ACCOUNT_UPSERT: &'static str = "account.upsert";
    pub const ACCOUNT_SET_DEFAULT: &'static str = "account.default.set";
    pub const ACCOUNT_STATUS: &'static str = "account.status";
    pub const ACCOUNT_LOGIN_START: &'static str = "account.login.start";
    pub const ACCOUNT_LOGIN_POLL: &'static str = "account.login.poll";
    pub const ACCOUNT_LOGIN_INPUT: &'static str = "account.login.input";
    pub const ACCOUNT_LOGIN_CANCEL: &'static str = "account.login.cancel";
    pub const ACCOUNT_PORTABLE_CONFIG_EXPORT: &'static str = "account.portable-config.export";
    pub const ACCOUNT_PORTABLE_CONFIG_APPLY: &'static str = "account.portable-config.apply";
    pub const PROVIDER_CALL: &'static str = "provider.call";
    pub const USAGE_RECORD: &'static str = "usage.record";
    pub const USAGE_QUERY: &'static str = "usage.query";
    pub const USAGE_REFRESH: &'static str = "usage.refresh";
    pub const CHECKPOINT_PREPARE: &'static str = "checkpoint.prepare";
    pub const CHECKPOINT_MANIFEST_PUT: &'static str = "checkpoint.manifest.put";
    pub const CHECKPOINT_BLOB_HAS: &'static str = "checkpoint.blob.has";
    pub const CHECKPOINT_BLOB_READ: &'static str = "checkpoint.blob.read";
    pub const CHECKPOINT_BLOB_PUT: &'static str = "checkpoint.blob.put";
    pub const CHECKPOINT_STAGE: &'static str = "checkpoint.stage";
    pub const MOVE_COMMIT: &'static str = "move.commit";
    pub const MOVE_ABORT: &'static str = "move.abort";
}

pub struct NodeCapability;

impl NodeCapability {
    pub const ACCOUNTS: &'static str = "accounts.v1";
    pub const CODEX_APP_SERVER: &'static str = "codex.app-server.v1";
    pub const CLAUDE_SUPERVISOR: &'static str = "claude.supervisor.v1";
    pub const FLEET_USAGE: &'static str = "usage-ledger.v1";
    pub const CHECKPOINTS: &'static str = "checkpoints.v1";
    pub const MOVE_LEASES: &'static str = "move-leases.v1";
    pub const PORTABLE_CONFIG: &'static str = "portable-config.v1";
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Claude,
    Codex,
}

impl ProviderKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeHelloParams {
    pub proto: u32,
    pub build: String,
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_node_id: Option<String>,
}

impl NodeHelloParams {
    pub fn new(build: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            proto: NODE_PROTOCOL_VERSION,
            build: build.into(),
            token: token.into(),
            client_node_id: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeHelloResult {
    pub proto: u32,
    pub control_proto: u32,
    pub build: String,
    pub node_id: String,
    pub display_name: String,
    pub os: String,
    pub arch: String,
    pub capabilities: Vec<String>,
}

impl NodeHelloResult {
    pub fn control_wire_version() -> u32 {
        WIRE_VERSION
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeStatusResult {
    pub node: NodeHelloResult,
    pub started_at: i64,
    pub accounts: usize,
    pub active_logins: usize,
    pub pending_moves: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountProfile {
    pub id: String,
    pub provider: ProviderKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InstallationStatus {
    Missing,
    SignedOut,
    Authenticating,
    Ready,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountInstallation {
    pub profile_id: String,
    pub provider: ProviderKind,
    pub node_id: String,
    pub status: InstallationStatus,
    pub config_home: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountCatalogResult {
    pub profiles: Vec<AccountProfile>,
    pub installations: Vec<AccountInstallation>,
    #[serde(default)]
    pub defaults: BTreeMap<ProviderKind, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountUpsertParams {
    pub id: String,
    pub provider: ProviderKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountProfileParams {
    pub profile_id: String,
}

/// A bounded snapshot of the settings that make one provider profile behave
/// consistently across nodes. Provider authentication and MCP OAuth state are
/// never part of this structure; known inline credential fields are reported
/// as omissions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableConfigBundle {
    pub version: u32,
    pub provider: ProviderKind,
    pub profile_id: String,
    pub files: Vec<PortableConfigFile>,
    #[serde(default)]
    pub omitted: Vec<PortableConfigOmission>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableConfigFile {
    /// Provider-home-relative path from a fixed allowlist.
    pub path: String,
    pub sha256: String,
    /// Portable config is deliberately text-only and bounded below the control
    /// protocol's message limit.
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PortableConfigOmission {
    pub path: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PortableConfigApplyParams {
    pub bundle: PortableConfigBundle,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableConfigApplyResult {
    pub provider: ProviderKind,
    pub profile_id: String,
    /// New files or MCP server definitions installed on the target.
    pub installed: Vec<String>,
    /// Files or definitions already byte-identical on the target.
    pub unchanged: Vec<String>,
    /// Target values kept because source and target differ. Sync never
    /// overwrites a conflict.
    pub conflicts: Vec<String>,
    #[serde(default)]
    pub omitted: Vec<PortableConfigOmission>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSetDefaultParams {
    pub provider: ProviderKind,
    pub profile_id: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LoginMode {
    #[default]
    DeviceCode,
    Browser,
    Interactive,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountLoginStartParams {
    pub profile_id: String,
    #[serde(default)]
    pub mode: LoginMode,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginChallenge {
    pub login_id: String,
    pub profile_id: String,
    pub kind: LoginMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
    #[serde(default)]
    pub output: String,
    pub complete: bool,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginSessionParams {
    pub login_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginInputParams {
    pub login_id: String,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCallParams {
    pub profile_id: String,
    pub method: String,
    #[serde(default)]
    pub params: JsonValue,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCallResult {
    pub provider: ProviderKind,
    pub method: String,
    pub result: JsonValue,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UsageValueKind {
    SubscriptionQuota,
    EstimatedApiEquivalent,
    AuthoritativeBilled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageSource {
    AppServer,
    Otel,
    Transcript,
    Manual,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageEvent {
    pub id: String,
    pub occurred_at: i64,
    pub provider: ProviderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub cache_read_tokens: i64,
    #[serde(default)]
    pub cache_write_tokens: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billed_usd: Option<f64>,
    pub value_kind: UsageValueKind,
    pub source: UsageSource,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct UsageRecordParams {
    pub event: UsageEvent,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageQueryParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTotals {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub estimated_usd: f64,
    pub billed_usd: f64,
    pub events: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageQueryResult {
    pub node_id: String,
    pub totals: UsageTotals,
    pub by_provider: BTreeMap<ProviderKind, UsageTotals>,
    pub authoritative_billing_available: bool,
    pub updated_at: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferMode {
    Move,
    Fork,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointPrepareParams {
    pub session_id: String,
    pub provider: ProviderKind,
    pub profile_id: String,
    pub workspace_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    pub mode: TransferMode,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointFile {
    pub path: String,
    pub digest: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unix_mode: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointManifest {
    pub version: u32,
    pub checkpoint_id: String,
    pub source_node_id: String,
    pub session_id: String,
    pub provider: ProviderKind,
    pub profile_id: String,
    pub workspace_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    pub mode: TransferMode,
    pub created_at: i64,
    pub files: Vec<CheckpointFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<CheckpointFile>,
    /// Paths intentionally omitted by the default secret/derived-data policy.
    #[serde(default)]
    pub excluded: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointManifestParams {
    pub manifest: CheckpointManifest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointIdParams {
    pub checkpoint_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlobHasParams {
    pub digests: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlobHasResult {
    pub missing: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobReadParams {
    pub digest: String,
    #[serde(default)]
    pub offset: u64,
    #[serde(default)]
    pub max_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobChunk {
    pub digest: String,
    pub offset: u64,
    /// Hex is intentionally boring and universally debuggable. Chunks are
    /// bounded to keep control messages below the 4 MiB wire limit.
    pub hex: String,
    pub eof: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobPutParams {
    pub digest: String,
    pub offset: u64,
    pub hex: String,
    pub eof: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointStageResult {
    pub checkpoint_id: String,
    pub quarantine_path: String,
    pub ready: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MovePhase {
    Prepared,
    Transferring,
    Staged,
    TargetReady,
    Committed,
    Aborted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveCommitParams {
    pub checkpoint_id: String,
    pub target_node_id: String,
    pub lease_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveAbortParams {
    pub checkpoint_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveRecord {
    pub checkpoint_id: String,
    pub session_id: String,
    pub source_node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_node_id: Option<String>,
    pub phase: MovePhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHandoffResult {
    pub checkpoint: CheckpointManifest,
    pub staged: CheckpointStageResult,
    /// Absent when either side predates `portable-config.v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portable_config: Option<PortableConfigApplyResult>,
    pub provider_result: JsonValue,
    pub target_commit: MoveRecord,
    pub source_commit: MoveRecord,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_hello_never_serializes_a_credential_reference() {
        let hello = NodeHelloResult {
            proto: NODE_PROTOCOL_VERSION,
            control_proto: NodeHelloResult::control_wire_version(),
            build: "test".into(),
            node_id: "forge".into(),
            display_name: "Forge".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            capabilities: vec![NodeCapability::ACCOUNTS.into()],
        };
        let json = serde_json::to_string(&hello).expect("serializes");
        assert!(!json.contains("token"));
        assert!(!json.contains("secret"));
        assert!(json.contains("controlProto"));
    }

    #[test]
    fn provider_kind_is_a_stable_map_key() {
        let mut defaults = BTreeMap::new();
        defaults.insert(ProviderKind::Claude, "work".to_owned());
        defaults.insert(ProviderKind::Codex, "personal".to_owned());
        let json = serde_json::to_string(&defaults).expect("serializes");
        assert_eq!(json, r#"{"claude":"work","codex":"personal"}"#);
    }
}
