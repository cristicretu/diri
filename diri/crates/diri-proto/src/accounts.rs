//! Engine-owned launch profiles. Credentials remain in the provider's config directory.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAccountProfile {
    pub id: String,
    pub label: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub config_home: String,
    #[serde(default)]
    pub is_default: bool,
    /// Credentials-only store for a local Claude profile that shares `~/.claude`.
    /// Claude Code keys its Keychain item (macOS) or `.credentials.json` (Linux)
    /// by this path, so conversations, MCP setup and settings stay in one home
    /// while each profile signs in separately. `None` keeps the legacy
    /// isolated-home behaviour driven by `config_home`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_store: Option<String>,
}

impl AgentAccountProfile {
    pub fn environment_key(&self) -> Option<&'static str> {
        match self.agent.as_str() {
            "codex" => Some("CODEX_HOME"),
            "claude-code" => Some("CLAUDE_CONFIG_DIR"),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct AgentAccountCatalog {
    pub profiles: Vec<AgentAccountProfile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentAccountId {
    pub id: String,
}

/// Who a local login belongs to, read from the login itself. Display only:
/// never a credential, and never stored in the catalog file.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAccountIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// The plan as the provider names it: "max", "pro", "plus", "team", …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

/// The saved login of one local profile that shares its Agent's home.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAccountLogin {
    pub profile_id: String,
    #[serde(default)]
    pub identity: AgentAccountIdentity,
    /// A login is saved for this profile.
    pub signed_in: bool,
    /// Its sign-in tab is still open.
    #[serde(default)]
    pub signing_in: bool,
    /// The file holding this profile's current Codex login, for reading plan
    /// limits. Claude logins are found through `login_store`. Never the secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_file: Option<String>,
}

/// The login that new local tabs of an Agent use right now.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLiveLogin {
    pub agent: String,
    /// The saved profile holding this login; `None` while it is not saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default)]
    pub identity: AgentAccountIdentity,
}

/// `account.overview`: the catalog, who each local login belongs to, and
/// which login each Agent uses now. Derived on every call; nothing here is
/// persisted or carries credentials.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAccountOverview {
    pub catalog: AgentAccountCatalog,
    #[serde(default)]
    pub logins: Vec<AgentAccountLogin>,
    #[serde(default)]
    pub live: Vec<AgentLiveLogin>,
}

impl AgentAccountOverview {
    pub fn login(&self, profile_id: &str) -> Option<&AgentAccountLogin> {
        self.logins.iter().find(|l| l.profile_id == profile_id)
    }

    pub fn live(&self, agent: &str) -> Option<&AgentLiveLogin> {
        self.live.iter().find(|l| l.agent == agent)
    }

    /// Whether new local tabs of the profile's Agent use this profile.
    pub fn is_live(&self, profile: &AgentAccountProfile) -> bool {
        profile.host.is_none()
            && self
                .live(&profile.agent)
                .is_some_and(|l| l.profile_id.as_deref() == Some(profile.id.as_str()))
    }
}

/// `account.adopt` and `account.add` name the Agent: `codex` or `claude-code`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentAccountAgent {
    pub agent: String,
}

/// Switch conversations open in Diri tabs for this profile's Agent and execution host.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchAccountParams {
    pub account_profile_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchAccountResult {
    pub switched: Vec<crate::SessionRecord>,
    pub failures: Vec<AccountSwitchFailure>,
    pub unchanged: Vec<crate::SessionId>,
    /// Open tabs left running on the previous login because their provider
    /// conversation could not be identified; they pick up the new login when
    /// restarted. Never a reason to refuse the switch.
    #[serde(default)]
    pub deferred: Vec<AccountSwitchFailure>,
    pub default_changed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSwitchFailure {
    #[serde(rename = "sessionID")]
    pub session_id: crate::SessionId,
    pub message: String,
}
