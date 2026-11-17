//! `computer.act`: one Cua Driver call made for a Session.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::SessionId;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ComputerActParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    /// A driver tool name, or `tools` for the catalog.
    pub tool: String,
    /// That tool's arguments; the Engine adds the session's cursor badge.
    #[serde(default)]
    pub args: Value,
}

/// `computer.status`: whether Cua Driver is installed and allowed.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ComputerStatusParams {
    /// Start the driver's daemon if it is not running, so the grants can be
    /// read. Without it a stopped daemon reports them as unknown.
    #[serde(default)]
    pub probe: bool,
}

/// `computer.setup`: one setup step the person asked for.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ComputerSetupAction {
    /// Install (or update to) the tested Cua Driver release, in the
    /// background. Answers at once; `computer.status` reports progress.
    Install,
    /// Restart the driver's daemon, so a grant macOS applies only at launch
    /// (Screen Recording) takes effect.
    Restart,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ComputerSetupParams {
    pub action: ComputerSetupAction,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputerStatus {
    /// Computer use exists on this platform at all.
    pub supported: bool,
    /// The installed driver's version, if one is installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The app macOS charges the driver's grants to (the bundle the Engine
    /// ships in): what the person drags into System Settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_path: Option<String>,
    /// Older than the release Diri was tested with.
    #[serde(default)]
    pub outdated: bool,
    /// The release this Engine installs.
    #[serde(default)]
    pub tested_version: String,
    /// `None` while unknown (no daemon to ask).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessibility: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_recording: Option<bool>,
    /// An install started from Diri is running.
    #[serde(default)]
    pub installing: bool,
    /// Why the last install failed, in words for the person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_error: Option<String>,
    /// The last install's output, for "Show log".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_log: Option<String>,
}

impl ComputerStatus {
    pub fn installed(&self) -> bool {
        self.version.is_some()
    }

    /// Everything an Agent needs: installed, current, and both grants.
    pub fn ready(&self) -> bool {
        self.installed()
            && !self.outdated
            && self.accessibility == Some(true)
            && self.screen_recording == Some(true)
    }
}

/// `computer.setup_needed` event: an Agent tried to use the computer before
/// setup was done. The app offers to finish it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ComputerSetupNeededEvent {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    /// "Claude · signup flow": who asked.
    pub badge: String,
}

/// `computer.skills`: App Skills, tasks computer use learned once in a Mac
/// app and replays without a model. `action` is one of list, learn, trace,
/// save, run, forget, history, rollback; `args` are that action's arguments.
/// Only `list`, `history` and `forget` work without a session.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ComputerSkillsParams {
    #[serde(rename = "sessionID", default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub action: String,
    #[serde(default)]
    pub args: Value,
}

/// How far a saved App Skill can be trusted.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AppSkillStatus {
    /// Replayed cleanly on the app version it was last run on.
    Verified,
    /// Saved without a full replay, or the app changed version since.
    #[default]
    Unverified,
    /// Failed three runs in a row; an Agent has to repair it.
    Broken,
}

/// One App Skill as `computer.skills` `list` reports it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSkillSummary {
    pub name: String,
    pub goal: String,
    #[serde(default)]
    pub params: Vec<String>,
    pub status: AppSkillStatus,
    pub version: u32,
    #[serde(default)]
    pub destructive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    #[serde(default)]
    pub runs: u64,
    /// Unix milliseconds of the last run that succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_at: Option<u64>,
    /// Unix milliseconds of the last run, ok or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<u64>,
}

/// One app with learned skills.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSkillsApp {
    pub bundle_id: String,
    pub name: String,
    pub skills: Vec<AppSkillSummary>,
}

/// `computer.skills` `list`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSkillsList {
    pub apps: Vec<AppSkillsApp>,
}
