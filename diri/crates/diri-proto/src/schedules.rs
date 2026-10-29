//! Scheduled tasks: Engine-owned timers that start an Agent session with a
//! prompt. A schedule outlives every session and survives Engine restarts; a
//! run the Mac slept through fires once on wake inside its catch-up window,
//! never twice and never silently.
use serde::{Deserialize, Serialize};

use crate::methods::SessionSpawnParams;
use crate::model::{DateMillis, SessionId};

/// Default catch-up window: a run missed by up to 12 hours still fires.
pub const DEFAULT_CATCH_UP_WINDOW_MS: u64 = 12 * 60 * 60 * 1000;
/// Newest runs kept on each record; older entries are dropped.
pub const MAX_SCHEDULE_RUNS: usize = 20;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ScheduleWhen {
    /// Fires once, then the schedule disables itself.
    Once { at: DateMillis },
    /// Standard five-field cron (`minute hour day-of-month month
    /// day-of-week`) evaluated in the Mac's local time zone.
    Cron { expr: String },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScheduleSpec {
    pub title: String,
    pub when: ScheduleWhen,
    /// What each run launches. `initialPrompt` is required.
    pub spawn: SessionSpawnParams,
    /// A run found late by at most this much still fires; older ones are
    /// recorded as missed. Zero never catches up.
    #[serde(default = "default_catch_up_window_ms")]
    pub catch_up_window_ms: u64,
    /// Hold an idle-sleep assertion shortly before each run and while its
    /// session works. Cannot wake a sleeping Mac or stop lid-close sleep.
    #[serde(default)]
    pub keep_awake: bool,
    /// Wake a sleeping Mac shortly before each run through the approved
    /// wake helper, hold it awake while the run works (implies keep-awake),
    /// then let it sleep again if nobody used it meanwhile.
    #[serde(default)]
    pub wake_mac: bool,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_catch_up_window_ms() -> u64 {
    DEFAULT_CATCH_UP_WINDOW_MS
}

fn default_enabled() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ScheduleOutcome {
    OnTime,
    /// Fired after its due time, inside the catch-up window.
    Late,
    /// Found past the catch-up window; nothing was started.
    Missed,
    /// Due and claimed, but the session could not be started.
    Failed,
    /// Started by hand with `schedule.run_now`.
    Manual,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LateReason {
    /// The Engine observed the Mac sleeping across the due time.
    Asleep,
    /// diri was not running at the due time (Mac off, logged out, quit).
    NotRunning,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleRun {
    pub due_at: DateMillis,
    pub fired_at: DateMillis,
    pub outcome: ScheduleOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_reason: Option<LateReason>,
    /// Earlier occurrences collapsed into this one while diri could not run.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub collapsed: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// Stamped on a session a schedule opened, so clients can mark it and say
/// which schedule started it and whether diri woke the Mac for it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledRunInfo {
    pub schedule_id: String,
    pub title: String,
    pub due_at: DateMillis,
    /// The schedule asks to wake the Mac.
    #[serde(default)]
    pub wake_mac: bool,
    /// diri actually woke the Mac for this run.
    #[serde(default)]
    pub woke_mac: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleRecord {
    pub id: String,
    #[serde(flatten)]
    pub spec: ScheduleSpec,
    pub created_at: DateMillis,
    pub revision: u64,
    /// Next occurrence the Engine will fire. Absent once a one-shot ran or
    /// while disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_due: Option<DateMillis>,
    /// Newest last, bounded by [`MAX_SCHEDULE_RUNS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<ScheduleRun>,
}

pub type ScheduleCreateParams = ScheduleSpec;
pub type ScheduleCreateResult = ScheduleRecord;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScheduleUpdateParams {
    pub id: String,
    #[serde(flatten)]
    pub spec: ScheduleSpec,
}

pub type ScheduleUpdateResult = ScheduleRecord;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScheduleIdParams {
    pub id: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleListResult {
    pub schedules: Vec<ScheduleRecord>,
    /// Why the Engine could not reach the wake helper last time a schedule
    /// asked to wake the Mac, usually because it is not approved yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_helper_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AgentKind;

    #[test]
    fn spec_defaults_fill_optional_fields() {
        let spec: ScheduleSpec = serde_json::from_value(serde_json::json!({
            "title": "Morning triage",
            "when": {"kind": "cron", "expr": "0 9 * * 1-5"},
            "spawn": {"kind": AgentKind::CLAUDE_CODE, "cwd": "/tmp", "initialPrompt": "triage"}
        }))
        .unwrap();
        assert_eq!(spec.catch_up_window_ms, DEFAULT_CATCH_UP_WINDOW_MS);
        assert!(spec.enabled);
        assert!(!spec.keep_awake);
        assert!(!spec.wake_mac);
        assert_eq!(spec.spawn.kind, AgentKind::CLAUDE_CODE);
    }

    #[test]
    fn record_round_trips_flattened_spec() {
        let record = ScheduleRecord {
            id: "sched_1".into(),
            spec: ScheduleSpec {
                title: "t".into(),
                when: ScheduleWhen::Once {
                    at: DateMillis(1.0),
                },
                spawn: serde_json::from_value(serde_json::json!({
                    "kind": AgentKind::CLAUDE_CODE, "cwd": "/tmp", "initialPrompt": "p"
                }))
                .unwrap(),
                catch_up_window_ms: 5,
                keep_awake: true,
                wake_mac: true,
                enabled: false,
            },
            created_at: DateMillis(0.0),
            revision: 3,
            next_due: None,
            runs: vec![ScheduleRun {
                due_at: DateMillis(1.0),
                fired_at: DateMillis(2.0),
                outcome: ScheduleOutcome::Late,
                late_reason: Some(LateReason::Asleep),
                collapsed: 2,
                session_id: None,
                error: None,
            }],
        };
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["when"]["kind"], "once");
        assert_eq!(value["runs"][0]["lateReason"], "asleep");
        assert_eq!(value["catchUpWindowMs"], 5);
        let back: ScheduleRecord = serde_json::from_value(value).unwrap();
        assert_eq!(back, record);
    }
}
