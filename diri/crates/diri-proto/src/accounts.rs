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
