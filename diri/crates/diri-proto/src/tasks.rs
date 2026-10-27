//! Explicit task acknowledgements are independent of terminal/session status.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum TaskStatus {
    AwaitingAcknowledgement,
    Acknowledged,
    Blocked,
    Completed,
    Failed,
    /// The sender withdrew the task. Terminal, like completion.
    Cancelled,
}
impl TaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// One entry in a task's bounded audit trail: progress notes, blockers,
/// answers from the sender, and the terminal report.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskUpdate {
    /// `progress`, `status`, `answer`, or `cancel`.
    pub kind: String,
    pub by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub revision: u64,
}

/// Oldest audit entries are dropped past this bound; the latest status and
/// result on the record itself are never dropped.
pub const MAX_TASK_UPDATES: usize = 32;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskRecord {
    pub task_id: String,
    pub sender_id: String,
    pub session_id: String,
    pub delivery: String,
    pub status: TaskStatus,
    pub result: Option<String>,
    pub revision: u64,
    /// Additive: absent on receipts written before the audit trail existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub updates: Vec<TaskUpdate>,
    /// Optional JSON Schema the sender expects the completed result to match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_schema: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskSubmitParams {
    pub caller_id: String,
    pub request_id: String,
    pub session_id: String,
    pub text: String,
    #[serde(default)]
    pub result_schema: Option<serde_json::Value>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskGetParams {
    pub caller_id: String,
    pub task_id: Option<String>,
    pub request_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskReportParams {
    pub caller_id: String,
    pub task_id: String,
    pub status: TaskStatus,
    pub result: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskAnswerParams {
    pub caller_id: String,
    pub task_id: String,
    pub text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskCancelParams {
    pub caller_id: String,
    pub task_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskListParams {
    pub caller_id: String,
    /// `sent`, `assigned`, or omitted for both.
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub include_terminal: bool,
    #[serde(default)]
    pub limit: Option<u32>,
}
