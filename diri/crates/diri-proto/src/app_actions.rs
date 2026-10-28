//! App actions: the commands a Diri window runs from its menus, keys and
//! command palette, exported so an Agent, a script or the `dirijor` CLI can
//! run them by name through the Engine.
//!
//! The app owns the catalog. When it connects it publishes every action it
//! supports ([`Method::APP_ACTIONS_PUBLISH`]) on its control connection,
//! which makes that connection the Engine's performer until it closes. A
//! caller then asks the Engine ([`Method::APP_PERFORM`]); the Engine checks
//! the name and arguments against the published catalog, writes one
//! [`EventName::APP_PERFORM_REQUESTED`] frame to the performer's connection
//! only (never to the shared event log, so a replay can never run an action
//! twice), and waits for the app's [`Method::APP_REPLY`] up to a timeout.
//!
//! With no app connected the Engine answers [`APP_NOT_CONNECTED`] at once.
//! Additive: an older Engine answers "not implemented", and an older app
//! never publishes, so callers see [`APP_NOT_CONNECTED`].

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::SessionId;

/// No app is connected to the Engine, so nothing can run the action.
pub const APP_NOT_CONNECTED: &str = "app_not_connected";
/// The connected app does not export an action by that name.
pub const UNKNOWN_ACTION: &str = "unknown_action";
/// The arguments do not match the action's schema.
pub const INVALID_ARGS: &str = "invalid_args";
/// The app did not answer in time. The action may still have run.
pub const APP_TIMEOUT: &str = "app_timeout";
/// The action exists but cannot run in the window's current state (for
/// example a terminal command while no terminal has focus).
pub const ACTION_UNAVAILABLE: &str = "action_unavailable";

/// Default and upper bound for how long the Engine waits for the app.
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
pub const MAX_TIMEOUT_MS: u64 = 30_000;
/// Upper bound on a published catalog, so a confused client cannot make the
/// Engine hold an arbitrary amount of memory.
pub const MAX_ACTIONS: usize = 512;

/// How much an action can lose. Gates on the MCP side read it: a
/// destructive action needs an explicit `session_id` that the calling
/// Agent is allowed to manage, or it is reserved for the person.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ActionRisk {
    /// Changes what the window shows: focus, splits, zoom, sidebar, theme.
    #[default]
    Safe,
    /// Closes, quits, archives or removes something.
    Destructive,
}

/// One exported action.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AppActionSpec {
    /// Stable id, the same one Settings › Shortcuts stores (`split-pane-right`).
    pub name: String,
    /// English title, as the menu and palette show it.
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub category: String,
    /// JSON Schema (an object schema) for the action's arguments.
    pub args: Value,
    #[serde(default)]
    pub risk: ActionRisk,
    /// The default key binding's label, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shortcut: Option<String>,
}

impl AppActionSpec {
    /// The schema for an action that takes no arguments.
    pub fn no_args() -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }

    /// Whether the action can be aimed at one session with `session_id`.
    pub fn takes_session(&self) -> bool {
        self.args
            .pointer("/properties/session_id")
            .is_some_and(Value::is_object)
    }

    /// Checks `args` against this action's schema.
    pub fn validate(&self, args: &Value) -> Result<(), String> {
        crate::json_schema::validate(args, &self.args, "args")
    }
}

/// `app.actions.publish`: the app's catalog. Replaces any earlier one and
/// makes this connection the performer.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AppActionsPublishParams {
    /// The app build, shown by `app.actions.list`.
    #[serde(default)]
    pub build: String,
    pub actions: Vec<AppActionSpec>,
}

/// `app.actions.list`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppActionsListResult {
    /// Whether an app is connected and can perform actions now.
    pub connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    #[serde(default)]
    pub actions: Vec<AppActionSpec>,
}

/// `app.perform`: run `action` in the app with `args`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AppPerformParams {
    pub action: String,
    #[serde(default = "empty_object")]
    pub args: Value,
    /// The Agent session asking ([`crate::agent_action::ACTOR_FIELD`]). Set
    /// by `dirijor-mcp` and the CLI inside an Agent, never by the app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

fn empty_object() -> Value {
    json!({})
}

/// What the app answered for a performed action.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppPerformResult {
    pub action: String,
    #[serde(default)]
    pub result: Value,
}

/// `app.perform_requested`, written only to the performer's connection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppPerformRequest {
    pub request_id: u64,
    pub action: String,
    #[serde(default = "empty_object")]
    pub args: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<SessionId>,
}

/// `app.reply`: the app's answer to one [`AppPerformRequest`]. Exactly one
/// of `result` and `error`; an `error` is passed to the caller unchanged.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AppReplyParams {
    pub request_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<crate::ControlError>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perform_params_default_to_no_arguments_and_no_actor() {
        let params: AppPerformParams =
            serde_json::from_value(json!({"action": "toggle-sidebar"})).unwrap();
        assert_eq!(params.args, json!({}));
        assert_eq!(params.actor, None);
        let stamped: AppPerformParams = serde_json::from_value(
            json!({"action": "toggle-sidebar", crate::agent_action::ACTOR_FIELD: "s-1"}),
        )
        .unwrap();
        assert_eq!(stamped.actor, Some(SessionId::new("s-1")));
    }

    #[test]
    fn an_action_validates_its_own_arguments() {
        let spec = AppActionSpec {
            name: "select-tab".into(),
            title: "Select Tab".into(),
            description: String::new(),
            category: String::new(),
            args: json!({"type":"object","properties":{"index":{"type":"integer","minimum":-1}},"required":["index"],"additionalProperties":false}),
            risk: ActionRisk::Safe,
            shortcut: None,
        };
        assert!(spec.validate(&json!({"index": 2})).is_ok());
        assert!(spec.validate(&json!({})).is_err());
        assert!(spec.validate(&json!({"index": 2, "x": 1})).is_err());
        assert!(!spec.takes_session());
        let none = AppActionSpec {
            args: AppActionSpec::no_args(),
            ..spec
        };
        assert!(none.validate(&json!({})).is_ok());
        assert!(none.validate(&json!({"index": 1})).is_err());
    }
}
