//! `computer.act`: one Cua Driver call made for a Session.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::SessionId;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ComputerActParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    /// A driver tool name, or `tools` for the catalog.
    pub tool: String,
    /// That tool's arguments; the Engine adds the session's cursor badge.
    #[serde(default)]
    pub args: Value,
}
