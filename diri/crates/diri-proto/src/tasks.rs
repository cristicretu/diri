//! Explicit task acknowledgements are independent of terminal/session status.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    AwaitingAcknowledgement,
    Acknowledged,
    Blocked,
    Completed,
    Failed,
}
impl TaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskRecord {
    pub task_id: String,
    pub sender_id: String,
    pub session_id: String,
    pub delivery: String,
    pub status: TaskStatus,
    pub result: Option<String>,
    pub revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSubmitParams {
    pub caller_id: String,
    pub request_id: String,
    pub session_id: String,
    pub text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGetParams {
    pub caller_id: String,
    pub task_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskReportParams {
    pub caller_id: String,
    pub task_id: String,
    pub status: TaskStatus,
    pub result: Option<String>,
}
