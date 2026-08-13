//! Durable, explicit agent-turn orchestration layered over terminal status.
//!
//! A terminal being `Idle` is ambiguous: it may not have begun a turn yet or
//! it may just have completed one. This module owns the deeper contract used
//! by MCP callers: monotonic run identities, terminal outcomes, stale-event
//! rejection, and an Engine-owned durable FIFO that waits for a safe composer.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use diri_proto::{
    AgentRun, AgentRunState, DateMillis, NeedsInputDetail, NeedsInputKind, NeedsInputSource,
    SessionRecord, SessionStatus,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const TERMINAL_HISTORY_LIMIT: usize = 32;
const REQUEST_REPLAY_LIMIT: usize = 64;
const REQUEST_ID_LIMIT: usize = 256;

#[derive(Clone, Default)]
pub struct StateChanges {
    inner: Arc<StateChangesInner>,
}

#[derive(Default)]
struct StateChangesInner {
    revision: Mutex<u64>,
    changed: Condvar,
}

impl StateChanges {
    pub fn observe(&self) -> u64 {
        *self.inner.revision.lock().expect("state changes")
    }

    pub fn notify(&self) {
        let mut revision = self.inner.revision.lock().expect("state changes");
        *revision = revision.saturating_add(1);
        self.inner.changed.notify_all();
    }

    /// Waits without polling. Returns the latest revision; a different value
    /// means at least one session changed.
    pub fn wait_after(&self, observed: u64, timeout: Duration) -> u64 {
        let revision = self.inner.revision.lock().expect("state changes");
        if *revision != observed {
            return *revision;
        }
        let (revision, _) = self
            .inner
            .changed
            .wait_timeout_while(revision, timeout, |revision| *revision == observed)
            .expect("state changes");
        *revision
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingDelivery {
    pub text: String,
    pub submit: bool,
    /// A submitted delivery owns its future identity from the moment the
    /// Engine acknowledges it, even while an older run is still active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_run: Option<AgentRun>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BlockerIdentity {
    kind: NeedsInputKind,
    source: NeedsInputSource,
    tool_name: Option<String>,
    /// The summary is the stable human-visible question. Prompt excerpts are
    /// intentionally excluded: screen echoes and selection cursors make them
    /// change while the same blocker remains visible.
    summary: String,
    options: Option<Vec<String>>,
}

impl From<&NeedsInputDetail> for BlockerIdentity {
    fn from(detail: &NeedsInputDetail) -> Self {
        Self {
            kind: detail.kind,
            source: detail.source,
            tool_name: detail.tool_name.clone(),
            summary: detail
                .summary
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            options: detail.options.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct DispatchIntent {
    #[serde(flatten)]
    delivery: PendingDelivery,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run_id: Option<u64>,
    #[serde(default)]
    prepared_output_offset: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prepared_blocker: Option<BlockerIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_fingerprint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AwaitingWork {
    run_id: u64,
    /// Output tail at the accepted PTY write. A holder can advance this while
    /// the daemon is absent, giving adoption durable evidence that work ran.
    #[serde(default)]
    accepted_output_offset: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    answered_blocker: Option<BlockerIdentity>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct UncertainDelivery {
    run_id: u64,
    prepared_output_offset: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prepared_blocker: Option<BlockerIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_fingerprint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct InterruptIntent {
    run_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_fingerprint: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ReplayOperation {
    Delivery,
    Interrupt,
    Report,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum ReplayOutcome {
    Delivery {
        queued: bool,
        run: Option<AgentRun>,
    },
    Interrupt {
        run: AgentRun,
    },
    Report {
        parent: String,
        queued: bool,
        run: Option<AgentRun>,
    },
    OutcomeUncertain {
        detail: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplayEntry {
    operation: ReplayOperation,
    request_id: String,
    fingerprint: String,
    outcome: ReplayOutcome,
}

#[derive(Clone, Debug)]
pub(crate) struct WaitDecision {
    pub reached: bool,
    pub superseded: bool,
    pub resolved: bool,
    pub run: Option<AgentRun>,
}

#[derive(Clone, Debug)]
pub(crate) struct DeliveryPlan {
    pub queued: bool,
    pub run: Option<AgentRun>,
}

pub(crate) struct DeliveryRequest<'a> {
    pub status: &'a SessionStatus,
    pub needs_input: Option<&'a NeedsInputDetail>,
    pub completion_seq: u64,
    pub output_offset: u64,
    pub expected_run_id: Option<u64>,
    pub text: String,
    pub submit: bool,
    pub allow_needs_input: bool,
    pub request_id: Option<&'a str>,
    pub request_fingerprint: Option<&'a str>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RunError {
    Stale { expected: u64, current: u64 },
    Exited,
    AlreadyTerminal { current: u64 },
    InvalidRequestId,
    RequestConflict { request_id: String },
}

/// Only durable fields are serialized. Completion counters and the last
/// observed low-level status belong to one Engine process and are rebound when
/// a Holder is adopted.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Orchestration {
    #[serde(skip)]
    completion_baselines: HashMap<String, u64>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pending: HashMap<String, VecDeque<PendingDelivery>>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    awaiting_work: HashMap<String, AwaitingWork>,
    /// Dispatch is a two-phase outbox. It is persisted before touching the
    /// PTY. A crash in the tiny send/commit window is surfaced as uncertain,
    /// never silently replayed into an agent composer.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    dispatching: HashMap<String, DispatchIntent>,
    /// Dispatches found in the pre-send phase after a restart remain tied to
    /// their original generation until holder output proves what happened.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    uncertain_deliveries: HashMap<String, UncertainDelivery>,
    /// Interrupt is also a two-phase operation: intent lands before Ctrl-C.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    interrupting: HashMap<String, InterruptIntent>,
    /// Bounded exact outcomes make FIFO advancement lossless to pinned waits.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    terminal_runs: HashMap<String, VecDeque<AgentRun>>,
    /// Bounded idempotency results survive daemon and MCP reconnects.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    request_replays: HashMap<String, VecDeque<ReplayEntry>>,
    #[serde(skip)]
    last_status: HashMap<String, SessionStatus>,
    /// Set only while rebinding holder sessions after load. It permits durable
    /// output offsets to close an awaiting run without weakening live Idle
    /// semantics immediately after a prompt echo.
    #[serde(skip)]
    adoption_pending: HashSet<String>,
}

impl Orchestration {
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
            && self.awaiting_work.is_empty()
            && self.dispatching.is_empty()
            && self.uncertain_deliveries.is_empty()
            && self.interrupting.is_empty()
            && self.terminal_runs.is_empty()
            && self.request_replays.is_empty()
    }

    pub fn register(
        &mut self,
        record: &SessionRecord,
        completion_seq: u64,
        status: &SessionStatus,
    ) {
        if record.run.is_some() {
            self.completion_baselines
                .insert(record.id.0.clone(), completion_seq);
            self.last_status.insert(record.id.0.clone(), status.clone());
        }
    }

    pub fn register_adopted(
        &mut self,
        record: &SessionRecord,
        completion_seq: u64,
        status: &SessionStatus,
    ) {
        self.register(record, completion_seq, status);
        let id = &record.id.0;
        if self.awaiting_work.contains_key(id) || self.uncertain_deliveries.contains_key(id) {
            self.adoption_pending.insert(id.clone());
        }
    }

    pub fn forget(&mut self, id: &str) {
        self.completion_baselines.remove(id);
        self.pending.remove(id);
        self.awaiting_work.remove(id);
        self.dispatching.remove(id);
        self.uncertain_deliveries.remove(id);
        self.interrupting.remove(id);
        self.terminal_runs.remove(id);
        self.request_replays.remove(id);
        self.last_status.remove(id);
        self.adoption_pending.remove(id);
    }

    /// A persisted dispatch means the Engine crashed after durably reserving
    /// the delivery but before durably committing its outcome. Retrying could
    /// duplicate a prompt; forgetting could hide loss. Fail closed and expose
    /// the uncertainty on the exact run instead.
    pub fn recover_inflight(&mut self, records: &mut HashMap<String, SessionRecord>) -> bool {
        let mut recovered = false;
        let uncertain: Vec<String> = self.dispatching.keys().cloned().collect();
        for id in uncertain {
            let Some(intent) = self.dispatching.remove(&id) else {
                continue;
            };
            if let Some(record) = records.get_mut(&id)
                && let Some(run) = record.run.as_mut()
            {
                run.state = AgentRunState::Failed;
                run.finished_at = Some(now());
                run.terminal_outcome = Some("delivery_outcome_uncertain".into());
                record.updated_at = now();
                record.last_turn_completed_at = run.finished_at;
                let terminal = run.clone();
                self.remember_terminal(&id, terminal);
                self.uncertain_deliveries.insert(
                    id.clone(),
                    UncertainDelivery {
                        run_id: intent.run_id.unwrap_or(run.id),
                        prepared_output_offset: intent.prepared_output_offset,
                        prepared_blocker: intent.prepared_blocker,
                        request_id: intent.request_id.clone(),
                        request_fingerprint: intent.request_fingerprint.clone(),
                    },
                );
                if let (Some(request_id), Some(fingerprint)) =
                    (intent.request_id, intent.request_fingerprint)
                {
                    self.remember_replay_unchecked(
                        &id,
                        ReplayEntry {
                            operation: ReplayOperation::Delivery,
                            request_id,
                            fingerprint,
                            outcome: ReplayOutcome::OutcomeUncertain {
                                detail: "daemon restarted during delivery".into(),
                            },
                        },
                    );
                }
            }
            self.awaiting_work.remove(&id);
            recovered = true;
        }

        let interrupts: Vec<String> = self.interrupting.keys().cloned().collect();
        for id in interrupts {
            let Some(intent) = self.interrupting.remove(&id) else {
                continue;
            };
            if let Some(record) = records.get_mut(&id)
                && let Some(run) = record.run.as_mut()
                && run.id == intent.run_id
                && !run.state.is_terminal()
            {
                run.state = AgentRunState::Aborted;
                run.finished_at = Some(now());
                run.terminal_outcome = Some("interrupt_outcome_uncertain".into());
                record.updated_at = now();
                record.last_turn_completed_at = run.finished_at;
                self.remember_terminal(&id, run.clone());
            }
            if let (Some(request_id), Some(fingerprint)) =
                (intent.request_id, intent.request_fingerprint)
            {
                self.remember_replay_unchecked(
                    &id,
                    ReplayEntry {
                        operation: ReplayOperation::Interrupt,
                        request_id,
                        fingerprint,
                        outcome: ReplayOutcome::OutcomeUncertain {
                            detail: "daemon restarted during interrupt".into(),
                        },
                    },
                );
            }
            recovered = true;
        }
        recovered
    }

    /// Folds a low-level status snapshot into the explicit run state. The
    /// completion sequence prevents a short Working→Idle edge from being lost
    /// when both transitions occur between registry/event snapshots.
    pub fn observe(
        &mut self,
        record: &mut SessionRecord,
        status: &SessionStatus,
        needs_input: Option<&NeedsInputDetail>,
        completion_seq: u64,
        output_offset: u64,
    ) -> bool {
        let next_run_id = self.next_run_id(record);
        let Some(current_run) = record.run.as_ref() else {
            return false;
        };
        let id = record.id.0.clone();
        let previous_status = self.last_status.insert(id.clone(), status.clone());
        let baseline = *self
            .completion_baselines
            .entry(id.clone())
            .or_insert(completion_seq);

        // Intent is already durable and the caller has not yet committed its
        // PTY side effect. Persistence itself must never consume that marker.
        if self.dispatching.contains_key(&id) || self.interrupting.contains_key(&id) {
            return false;
        }

        let uncertain = self.uncertain_deliveries.get(&id).cloned();
        if current_run.state.is_terminal()
            && let Some(uncertain) = uncertain
            && current_run.id == uncertain.run_id
        {
            let output_advanced = output_offset > uncertain.prepared_output_offset;
            let blocker_changed = needs_input.map(BlockerIdentity::from).as_ref()
                != uncertain.prepared_blocker.as_ref();
            let accepted = output_advanced
                && (matches!(status, SessionStatus::Working)
                    || matches!(status, SessionStatus::NeedsInput(_)) && blocker_changed);
            if accepted {
                let run = record.run.as_mut().expect("checked above");
                run.state = if matches!(status, SessionStatus::NeedsInput(_)) {
                    AgentRunState::NeedsInput
                } else {
                    AgentRunState::Running
                };
                run.finished_at = None;
                run.terminal_outcome = None;
                let accepted_run = run.clone();
                self.remove_terminal(&id, accepted_run.id);
                self.uncertain_deliveries.remove(&id);
                self.adoption_pending.remove(&id);
                self.completion_baselines.insert(id.clone(), completion_seq);
                if let (Some(request_id), Some(fingerprint)) =
                    (uncertain.request_id, uncertain.request_fingerprint)
                {
                    self.remember_replay_unchecked(
                        &id,
                        ReplayEntry {
                            operation: ReplayOperation::Delivery,
                            request_id,
                            fingerprint,
                            outcome: ReplayOutcome::Delivery {
                                queued: false,
                                run: Some(accepted_run),
                            },
                        },
                    );
                }
                return true;
            }
        }

        // A raw terminal submission (the app's attached PTY path) is still a
        // real turn. Allocate it only on a new transition into Working, not on
        // the stale Working frame that can coexist with a completion hook.
        if current_run.state.is_terminal() {
            if matches!(status, SessionStatus::Working)
                && !matches!(previous_status, Some(SessionStatus::Working))
            {
                self.remember_terminal(&id, current_run.clone());
                record.run = Some(AgentRun {
                    id: next_run_id,
                    state: AgentRunState::Running,
                    started_at: now(),
                    finished_at: None,
                    terminal_outcome: None,
                });
                self.completion_baselines.insert(id.clone(), completion_seq);
                return true;
            }
            return false;
        }

        let awaiting = self.awaiting_work.get(&id).cloned();
        let awaiting = match awaiting {
            Some(awaiting) if awaiting.run_id == current_run.id => Some(awaiting),
            Some(_) => {
                self.awaiting_work.remove(&id);
                self.adoption_pending.remove(&id);
                None
            }
            None => None,
        };
        let next = if matches!(status, SessionStatus::Exited(_)) {
            AgentRunState::Failed
        } else if completion_seq > baseline {
            self.completion_baselines.insert(id.clone(), completion_seq);
            AgentRunState::Completed
        } else if let Some(awaiting) = awaiting {
            let output_advanced = output_offset > awaiting.accepted_output_offset;
            match status {
                SessionStatus::Working => {
                    self.awaiting_work.remove(&id);
                    self.adoption_pending.remove(&id);
                    AgentRunState::Running
                }
                SessionStatus::NeedsInput(_)
                    if awaiting.answered_blocker.as_ref()
                        == needs_input.map(BlockerIdentity::from).as_ref() =>
                {
                    // The reducer has not yet observed the answer. Keep the
                    // guard and never reopen the same dialog to a duplicate.
                    AgentRunState::Running
                }
                SessionStatus::NeedsInput(_) => {
                    self.awaiting_work.remove(&id);
                    self.adoption_pending.remove(&id);
                    AgentRunState::NeedsInput
                }
                SessionStatus::Idle
                    if matches!(previous_status, Some(SessionStatus::Working))
                        || self.adoption_pending.contains(&id) && output_advanced =>
                {
                    self.awaiting_work.remove(&id);
                    self.adoption_pending.remove(&id);
                    AgentRunState::Completed
                }
                SessionStatus::Idle | SessionStatus::Starting | SessionStatus::Unknown => {
                    AgentRunState::Running
                }
                SessionStatus::Exited(_) => unreachable!(),
            }
        } else {
            match status {
                SessionStatus::Working => AgentRunState::Running,
                SessionStatus::NeedsInput(_) => AgentRunState::NeedsInput,
                // On adoption the reducer's completion counter starts at
                // zero. A persisted Running→authoritative Idle transition is
                // therefore the missing completion edge.
                SessionStatus::Idle
                    if matches!(current_run.state, AgentRunState::Running)
                        && matches!(previous_status, Some(SessionStatus::Working)) =>
                {
                    AgentRunState::Completed
                }
                SessionStatus::Starting | SessionStatus::Idle | SessionStatus::Unknown => {
                    current_run.state
                }
                SessionStatus::Exited(_) => unreachable!(),
            }
        };

        if next == current_run.state {
            return false;
        }
        let run = record.run.as_mut().expect("checked above");
        run.state = next;
        if next.is_terminal() {
            self.awaiting_work.remove(&id);
            run.finished_at = Some(now());
            if run.terminal_outcome.is_none() {
                run.terminal_outcome = Some(
                    match next {
                        AgentRunState::Completed => "completed",
                        AgentRunState::Failed => "process_exited",
                        AgentRunState::Aborted => "interrupted",
                        _ => unreachable!(),
                    }
                    .into(),
                );
            }
            self.remember_terminal(&id, run.clone());
        }
        true
    }

    pub fn validate_expected(
        record: &SessionRecord,
        expected: Option<u64>,
    ) -> Result<(), RunError> {
        let Some(expected) = expected else {
            return Ok(());
        };
        let current = record.run.as_ref().map_or(0, |run| run.id);
        if expected == current {
            Ok(())
        } else {
            Err(RunError::Stale { expected, current })
        }
    }

    pub fn lookup_replay(
        &self,
        scope: &str,
        operation: ReplayOperation,
        request_id: Option<&str>,
        fingerprint: Option<&str>,
    ) -> Result<Option<ReplayOutcome>, RunError> {
        let Some(request_id) = request_id else {
            return Ok(None);
        };
        validate_request_id(request_id)?;
        let Some(entry) = self
            .request_replays
            .get(scope)
            .into_iter()
            .flatten()
            .find(|entry| entry.operation == operation && entry.request_id == request_id)
        else {
            return Ok(None);
        };
        if Some(entry.fingerprint.as_str()) != fingerprint {
            return Err(RunError::RequestConflict {
                request_id: request_id.to_owned(),
            });
        }
        Ok(Some(entry.outcome.clone()))
    }

    pub fn remember_replay(
        &mut self,
        scope: &str,
        operation: ReplayOperation,
        request_id: Option<&str>,
        fingerprint: Option<&str>,
        outcome: ReplayOutcome,
    ) -> Result<(), RunError> {
        let Some(request_id) = request_id else {
            return Ok(());
        };
        validate_request_id(request_id)?;
        let fingerprint = fingerprint.ok_or(RunError::InvalidRequestId)?;
        self.remember_replay_unchecked(
            scope,
            ReplayEntry {
                operation,
                request_id: request_id.to_owned(),
                fingerprint: fingerprint.to_owned(),
                outcome,
            },
        );
        Ok(())
    }

    fn remember_replay_unchecked(&mut self, scope: &str, entry: ReplayEntry) {
        let entries = self.request_replays.entry(scope.to_owned()).or_default();
        if let Some(existing) = entries.iter_mut().find(|existing| {
            existing.operation == entry.operation && existing.request_id == entry.request_id
        }) {
            *existing = entry;
            return;
        }
        entries.push_back(entry);
        while entries.len() > REQUEST_REPLAY_LIMIT {
            entries.pop_front();
        }
    }

    pub fn remember_current_terminal(&mut self, record: &SessionRecord) {
        if let Some(run) = record.run.as_ref().filter(|run| run.state.is_terminal()) {
            self.remember_terminal(&record.id.0, run.clone());
        }
    }

    pub fn remember_terminal(&mut self, id: &str, run: AgentRun) {
        if !run.state.is_terminal() {
            return;
        }
        let history = self.terminal_runs.entry(id.to_owned()).or_default();
        if let Some(existing) = history.iter_mut().find(|existing| existing.id == run.id) {
            *existing = run;
        } else {
            history.push_back(run);
        }
        while history.len() > TERMINAL_HISTORY_LIMIT {
            history.pop_front();
        }
    }

    fn remove_terminal(&mut self, id: &str, run_id: u64) {
        if let Some(history) = self.terminal_runs.get_mut(id) {
            history.retain(|run| run.id != run_id);
            if history.is_empty() {
                self.terminal_runs.remove(id);
            }
        }
    }

    pub fn wait_decision(
        &self,
        record: &SessionRecord,
        run_id: Option<u64>,
        targets: &[String],
    ) -> WaitDecision {
        let current = record.run.as_ref();
        let current_id = current.map_or(0, |run| run.id);
        if let Some(expected) = run_id {
            if let Some(run) = self
                .terminal_runs
                .get(&record.id.0)
                .into_iter()
                .flatten()
                .find(|run| run.id == expected && expected != current_id)
            {
                return WaitDecision {
                    reached: targets
                        .iter()
                        .any(|target| historical_run_satisfies_target(run, target)),
                    superseded: false,
                    resolved: true,
                    run: Some(run.clone()),
                };
            }
            if expected < current_id {
                return WaitDecision {
                    reached: false,
                    superseded: true,
                    resolved: true,
                    run: current.cloned(),
                };
            }
            if expected > current_id {
                return WaitDecision {
                    reached: false,
                    superseded: false,
                    resolved: matches!(record.status, SessionStatus::Exited(_)),
                    run: None,
                };
            }
        }

        let run = current.cloned();
        let reached = targets.iter().any(|target| {
            run.as_ref().map_or_else(
                || status_satisfies_target(&record.status, target),
                |run| run_satisfies_target(run, &record.status, target),
            )
        });
        let resolved = reached
            || matches!(record.status, SessionStatus::Exited(_))
            || run.as_ref().is_some_and(|run| run.state.is_terminal());
        WaitDecision {
            reached,
            superseded: false,
            resolved,
            run,
        }
    }

    /// Highest generation the Engine has durably accepted for this target,
    /// including a queued future turn not yet installed on the live record.
    #[cfg(test)]
    pub fn latest_run_id(&self, record: &SessionRecord) -> u64 {
        self.next_run_id(record).saturating_sub(1)
    }

    /// Prepares an MCP prompt. A queued submitted prompt receives its future
    /// run id now, so callers can immediately pin a wait to that exact turn.
    pub fn prepare_delivery(
        &mut self,
        record: &mut SessionRecord,
        request: DeliveryRequest<'_>,
    ) -> Result<DeliveryPlan, RunError> {
        let DeliveryRequest {
            status,
            needs_input,
            completion_seq,
            output_offset,
            expected_run_id,
            text,
            submit,
            allow_needs_input,
            request_id,
            request_fingerprint,
        } = request;
        if let Some(request_id) = request_id {
            validate_request_id(request_id)?;
        }
        Self::validate_expected(record, expected_run_id)?;
        if matches!(status, SessionStatus::Exited(_)) {
            return Err(RunError::Exited);
        }
        let _ = self.observe(record, status, needs_input, completion_seq, output_offset);
        let next_run_id = self.next_run_id(record);
        let Some(run) = record.run.as_mut() else {
            return Ok(DeliveryPlan {
                queued: false,
                run: None,
            });
        };
        let id = record.id.0.clone();
        let queue_occupied = self.pending.get(&id).is_some_and(|queue| !queue.is_empty())
            || self.dispatching.contains_key(&id);
        let safe = !queue_occupied
            && match run.state {
                AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Aborted => {
                    matches!(status, SessionStatus::Idle)
                }
                AgentRunState::NeedsInput if allow_needs_input => {
                    !self.awaiting_work.contains_key(&id)
                }
                AgentRunState::Starting => matches!(status, SessionStatus::Idle),
                AgentRunState::Running | AgentRunState::NeedsInput => false,
            };

        if safe {
            if run.state.is_terminal() {
                let previous = run.clone();
                *run = AgentRun::starting(next_run_id, now());
                self.remember_terminal(&id, previous);
                self.uncertain_deliveries.remove(&id);
                self.adoption_pending.remove(&id);
                self.completion_baselines.insert(id.clone(), completion_seq);
            } else if matches!(run.state, AgentRunState::NeedsInput) {
                run.state = AgentRunState::Running;
            }
            let response_run = Some(run.clone());
            self.dispatching.insert(
                id,
                DispatchIntent {
                    delivery: PendingDelivery {
                        text,
                        submit,
                        assigned_run: submit.then(|| run.clone()),
                    },
                    run_id: Some(run.id),
                    prepared_output_offset: output_offset,
                    prepared_blocker: needs_input.map(BlockerIdentity::from),
                    request_id: request_id.map(str::to_owned),
                    request_fingerprint: request_fingerprint.map(str::to_owned),
                },
            );
            return Ok(DeliveryPlan {
                queued: false,
                run: response_run,
            });
        }

        let assigned_run = submit.then(|| AgentRun::starting(next_run_id, now()));
        let response_run = assigned_run.clone().or_else(|| Some(run.clone()));
        self.pending
            .entry(id)
            .or_default()
            .push_back(PendingDelivery {
                text,
                submit,
                assigned_run,
            });
        Ok(DeliveryPlan {
            queued: true,
            run: response_run,
        })
    }

    /// Moves one ready item into the durable dispatch phase. The caller must
    /// persist this state before touching the PTY.
    pub fn begin_ready(
        &mut self,
        record: &mut SessionRecord,
        status: &SessionStatus,
        needs_input: Option<&NeedsInputDetail>,
        completion_seq: u64,
        output_offset: u64,
    ) -> Option<PendingDelivery> {
        let _ = self.observe(record, status, needs_input, completion_seq, output_offset);
        let id = record.id.0.clone();
        if !matches!(status, SessionStatus::Idle)
            || self.awaiting_work.contains_key(&id)
            || self.dispatching.contains_key(&id)
        {
            return None;
        }
        let delivery = self.pending.get_mut(&id)?.pop_front()?;
        if self.pending.get(&id).is_some_and(VecDeque::is_empty) {
            self.pending.remove(&id);
        }
        if let Some(assigned) = &delivery.assigned_run {
            if let Some(previous) = record
                .run
                .as_ref()
                .filter(|run| run.state.is_terminal())
                .cloned()
            {
                self.remember_terminal(&id, previous);
            }
            record.run = Some(assigned.clone());
            self.uncertain_deliveries.remove(&id);
            self.adoption_pending.remove(&id);
            self.completion_baselines.insert(id.clone(), completion_seq);
        }
        self.dispatching.insert(
            id,
            DispatchIntent {
                run_id: record.run.as_ref().map(|run| run.id),
                prepared_output_offset: output_offset,
                prepared_blocker: needs_input.map(BlockerIdentity::from),
                request_id: None,
                request_fingerprint: None,
                delivery: delivery.clone(),
            },
        );
        Some(delivery)
    }

    pub fn finish_dispatch(
        &mut self,
        id: &str,
        run_id: Option<u64>,
        submit: bool,
        answered_blocker: Option<NeedsInputDetail>,
        accepted_output_offset: u64,
    ) {
        self.dispatching.remove(id);
        if submit {
            self.awaiting_work.insert(
                id.to_owned(),
                AwaitingWork {
                    run_id: run_id.unwrap_or(0),
                    accepted_output_offset,
                    answered_blocker: answered_blocker.as_ref().map(BlockerIdentity::from),
                },
            );
        }
    }

    pub fn prepare_interrupt(
        &mut self,
        record: &mut SessionRecord,
        expected: Option<u64>,
        request_id: Option<&str>,
        request_fingerprint: Option<&str>,
    ) -> Result<AgentRun, RunError> {
        if let Some(request_id) = request_id {
            validate_request_id(request_id)?;
        }
        Self::validate_expected(record, expected)?;
        let run = record
            .run
            .get_or_insert_with(|| AgentRun::starting(1, now()));
        if run.state.is_terminal() {
            return Err(RunError::AlreadyTerminal { current: run.id });
        }
        self.interrupting.insert(
            record.id.0.clone(),
            InterruptIntent {
                run_id: run.id,
                request_id: request_id.map(str::to_owned),
                request_fingerprint: request_fingerprint.map(str::to_owned),
            },
        );
        Ok(run.clone())
    }

    pub fn finish_interrupt(&mut self, record: &mut SessionRecord) -> AgentRun {
        let run = record
            .run
            .get_or_insert_with(|| AgentRun::starting(1, now()));
        run.state = AgentRunState::Aborted;
        run.finished_at = Some(now());
        run.terminal_outcome = Some("interrupted".into());
        // Future acknowledged turns stay queued. Only the current turn's
        // acceptance guard is retired by Ctrl-C.
        self.awaiting_work.remove(&record.id.0);
        self.interrupting.remove(&record.id.0);
        self.remember_terminal(&record.id.0, run.clone());
        run.clone()
    }

    fn next_run_id(&self, record: &SessionRecord) -> u64 {
        let id = &record.id.0;
        let current = record.run.as_ref().map_or(0, |run| run.id);
        self.pending
            .get(id)
            .into_iter()
            .flatten()
            .filter_map(|delivery| delivery.assigned_run.as_ref().map(|run| run.id))
            .chain(
                self.dispatching
                    .get(id)
                    .and_then(|intent| intent.delivery.assigned_run.as_ref())
                    .map(|run| run.id),
            )
            .chain(
                self.terminal_runs
                    .get(id)
                    .into_iter()
                    .flatten()
                    .map(|run| run.id),
            )
            .fold(current, u64::max)
            .saturating_add(1)
    }
}

pub(crate) fn request_fingerprint(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).expect("request fingerprint serialization");
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn validate_request_id(request_id: &str) -> Result<(), RunError> {
    if request_id.is_empty() || request_id.len() > REQUEST_ID_LIMIT {
        Err(RunError::InvalidRequestId)
    } else {
        Ok(())
    }
}

fn run_satisfies_target(run: &AgentRun, status: &SessionStatus, target: &str) -> bool {
    match target {
        "done" | "settled" => run.state.is_terminal(),
        "completed" => matches!(run.state, AgentRunState::Completed),
        "running" | "working" => matches!(run.state, AgentRunState::Running),
        "starting" => matches!(run.state, AgentRunState::Starting),
        "idle" => matches!(status, SessionStatus::Idle),
        "needsInput" | "needs_input" | "needs-input" | "needs_me" | "blocked" => {
            matches!(run.state, AgentRunState::NeedsInput)
        }
        "failed" => matches!(run.state, AgentRunState::Failed),
        "aborted" => matches!(run.state, AgentRunState::Aborted),
        "exited" | "dead" => matches!(status, SessionStatus::Exited(_)),
        "any" => !matches!(run.state, AgentRunState::Starting),
        _ => false,
    }
}

fn historical_run_satisfies_target(run: &AgentRun, target: &str) -> bool {
    match target {
        "done" | "settled" | "any" => run.state.is_terminal(),
        "completed" => matches!(run.state, AgentRunState::Completed),
        "failed" => matches!(run.state, AgentRunState::Failed),
        "aborted" => matches!(run.state, AgentRunState::Aborted),
        _ => false,
    }
}

fn status_satisfies_target(status: &SessionStatus, target: &str) -> bool {
    match target {
        "idle" | "done" => matches!(status, SessionStatus::Idle),
        "working" | "running" => matches!(status, SessionStatus::Working),
        "starting" => matches!(status, SessionStatus::Starting),
        "unknown" => matches!(status, SessionStatus::Unknown),
        "needsInput" | "needs_input" | "needs-input" | "needs_me" | "blocked" => {
            matches!(status, SessionStatus::NeedsInput(_))
        }
        "exited" | "dead" => matches!(status, SessionStatus::Exited(_)),
        "any" => !matches!(status, SessionStatus::Starting),
        _ => false,
    }
}

fn now() -> DateMillis {
    std::time::SystemTime::now().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::{AgentKind, ProjectId, Resumability, SessionId, TitleSource};

    fn record() -> SessionRecord {
        let now = now();
        SessionRecord {
            id: SessionId::new("child"),
            kind: AgentKind::CODEX,
            cwd: "/tmp".into(),
            project_id: ProjectId::new("p"),
            worktree_path: None,
            git_branch: None,
            title: "child".into(),
            title_source: TitleSource::Placeholder,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status: SessionStatus::Starting,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::Live,
            parent: Some(SessionId::new("parent")),
            created_at: now,
            updated_at: now,
            last_turn_completed_at: None,
            last_seen_at: None,
            pinned: false,
            archived_at: None,
            host: None,
            remote_persistence: None,
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
            run: Some(AgentRun::starting(1, now)),
        }
    }

    #[test]
    fn idle_before_work_is_not_completed_but_completion_edge_is_lossless() {
        let mut lifecycle = Orchestration::default();
        let mut record = record();
        lifecycle.register(&record, 0, &SessionStatus::Starting);
        assert!(!lifecycle.observe(&mut record, &SessionStatus::Idle, None, 0, 0));
        assert_eq!(record.run.as_ref().unwrap().state, AgentRunState::Starting);
        assert!(lifecycle.observe(&mut record, &SessionStatus::Idle, None, 1, 0));
        assert_eq!(record.run.as_ref().unwrap().state, AgentRunState::Completed);
    }

    #[test]
    fn queued_turns_receive_stable_future_ids_and_release_fifo() {
        let mut lifecycle = Orchestration::default();
        let mut record = record();
        lifecycle.register(&record, 0, &SessionStatus::Working);
        lifecycle.observe(&mut record, &SessionStatus::Working, None, 0, 0);
        let first = lifecycle
            .prepare_delivery(
                &mut record,
                DeliveryRequest {
                    status: &SessionStatus::Working,
                    needs_input: None,
                    completion_seq: 0,
                    output_offset: 0,
                    expected_run_id: Some(1),
                    text: "first".into(),
                    submit: true,
                    allow_needs_input: true,
                    request_id: None,
                    request_fingerprint: None,
                },
            )
            .unwrap();
        let second = lifecycle
            .prepare_delivery(
                &mut record,
                DeliveryRequest {
                    status: &SessionStatus::Working,
                    needs_input: None,
                    completion_seq: 0,
                    output_offset: 0,
                    expected_run_id: Some(1),
                    text: "second".into(),
                    submit: true,
                    allow_needs_input: true,
                    request_id: None,
                    request_fingerprint: None,
                },
            )
            .unwrap();
        assert_eq!(first.run.unwrap().id, 2);
        assert_eq!(second.run.unwrap().id, 3);
        assert_eq!(record.run.as_ref().unwrap().id, 1);

        let first = lifecycle
            .begin_ready(&mut record, &SessionStatus::Idle, None, 1, 0)
            .unwrap();
        assert_eq!(first.text, "first");
        assert_eq!(record.run.as_ref().unwrap().id, 2);
        lifecycle.finish_dispatch("child", Some(2), true, None, 0);
        assert!(
            lifecycle
                .begin_ready(&mut record, &SessionStatus::Idle, None, 1, 0)
                .is_none(),
            "one submitted prompt must begin work before the next is released"
        );
        lifecycle.observe(&mut record, &SessionStatus::Working, None, 1, 0);
        lifecycle.observe(&mut record, &SessionStatus::Idle, None, 2, 0);
        let second = lifecycle
            .begin_ready(&mut record, &SessionStatus::Idle, None, 2, 0)
            .unwrap();
        assert_eq!(second.text, "second");
        assert_eq!(record.run.as_ref().unwrap().id, 3);
    }

    #[test]
    fn answered_blocker_cannot_regress_and_accept_a_duplicate() {
        let mut lifecycle = Orchestration::default();
        let mut record = record();
        let detail = NeedsInputDetail {
            kind: diri_proto::NeedsInputKind::Question,
            source: diri_proto::NeedsInputSource::ScreenScrape,
            tool_name: None,
            summary: "choose".into(),
            prompt_excerpt: None,
            options: None,
            risk_hint: diri_proto::RiskHint::Neutral,
            occurred_at: now(),
        };
        record.run.as_mut().unwrap().state = AgentRunState::NeedsInput;
        lifecycle.register(&record, 0, &SessionStatus::NeedsInput(detail.kind));
        let answer = lifecycle
            .prepare_delivery(
                &mut record,
                DeliveryRequest {
                    status: &SessionStatus::NeedsInput(detail.kind),
                    needs_input: Some(&detail),
                    completion_seq: 0,
                    output_offset: 0,
                    expected_run_id: Some(1),
                    text: "yes".into(),
                    submit: true,
                    allow_needs_input: true,
                    request_id: None,
                    request_fingerprint: None,
                },
            )
            .unwrap();
        lifecycle.finish_dispatch("child", Some(1), true, Some(detail.clone()), 0);
        assert!(!answer.queued);
        lifecycle.observe(
            &mut record,
            &SessionStatus::NeedsInput(detail.kind),
            Some(&detail),
            0,
            0,
        );
        assert_eq!(record.run.as_ref().unwrap().state, AgentRunState::Running);
        let duplicate = lifecycle
            .prepare_delivery(
                &mut record,
                DeliveryRequest {
                    status: &SessionStatus::NeedsInput(detail.kind),
                    needs_input: Some(&detail),
                    completion_seq: 0,
                    output_offset: 0,
                    expected_run_id: Some(1),
                    text: "yes again".into(),
                    submit: true,
                    allow_needs_input: true,
                    request_id: None,
                    request_fingerprint: None,
                },
            )
            .unwrap();
        assert!(duplicate.queued);
        assert_eq!(duplicate.run.unwrap().id, 2);
    }

    #[test]
    fn interrupt_preserves_acknowledged_future_turns_and_rejects_terminal_runs() {
        let mut lifecycle = Orchestration::default();
        let mut record = record();
        lifecycle.register(&record, 0, &SessionStatus::Working);
        lifecycle.observe(&mut record, &SessionStatus::Working, None, 0, 0);
        let queued = lifecycle
            .prepare_delivery(
                &mut record,
                DeliveryRequest {
                    status: &SessionStatus::Working,
                    needs_input: None,
                    completion_seq: 0,
                    output_offset: 0,
                    expected_run_id: Some(1),
                    text: "next".into(),
                    submit: true,
                    allow_needs_input: true,
                    request_id: None,
                    request_fingerprint: None,
                },
            )
            .unwrap();
        assert_eq!(queued.run.unwrap().id, 2);
        assert_eq!(
            lifecycle
                .prepare_interrupt(&mut record, Some(1), None, None)
                .unwrap()
                .id,
            1
        );
        assert_eq!(lifecycle.finish_interrupt(&mut record).id, 1);
        assert_eq!(
            lifecycle.prepare_interrupt(&mut record, Some(1), None, None),
            Err(RunError::AlreadyTerminal { current: 1 })
        );
        let next = lifecycle
            .begin_ready(&mut record, &SessionStatus::Idle, None, 1, 0)
            .unwrap();
        assert_eq!(next.assigned_run.unwrap().id, 2);
    }

    #[test]
    fn adopted_working_run_completes_on_authoritative_idle_without_an_old_counter() {
        let mut lifecycle = Orchestration::default();
        let mut record = record();
        record.run.as_mut().unwrap().state = AgentRunState::Running;
        lifecycle.register(&record, 0, &SessionStatus::Working);

        assert!(lifecycle.observe(&mut record, &SessionStatus::Idle, None, 0, 0));
        assert_eq!(record.run.as_ref().unwrap().state, AgentRunState::Completed);
    }

    #[test]
    fn state_wait_is_edge_triggered_and_cannot_miss_a_pre_wait_change() {
        let changes = StateChanges::default();
        let observed = changes.observe();
        changes.notify();
        let start = std::time::Instant::now();
        let latest = changes.wait_after(observed, Duration::from_secs(5));
        assert_ne!(latest, observed);
        assert!(
            start.elapsed() < Duration::from_millis(20),
            "a buffered edge must not wait for a polling interval"
        );
    }

    #[test]
    fn fifo_advancement_preserves_the_exact_terminal_run_for_waiters() {
        let mut lifecycle = Orchestration::default();
        let mut record = record();
        record.run.as_mut().unwrap().state = AgentRunState::Running;
        lifecycle.register(&record, 0, &SessionStatus::Working);
        let queued = lifecycle
            .prepare_delivery(
                &mut record,
                DeliveryRequest {
                    status: &SessionStatus::Working,
                    needs_input: None,
                    completion_seq: 0,
                    output_offset: 10,
                    expected_run_id: Some(1),
                    text: "next".into(),
                    submit: true,
                    allow_needs_input: true,
                    request_id: None,
                    request_fingerprint: None,
                },
            )
            .unwrap();
        assert_eq!(queued.run.unwrap().id, 2);

        assert!(lifecycle.observe(&mut record, &SessionStatus::Idle, None, 1, 20));
        lifecycle
            .begin_ready(&mut record, &SessionStatus::Idle, None, 1, 20)
            .expect("queued run advances");
        assert_eq!(record.run.as_ref().unwrap().id, 2);

        let decision = lifecycle.wait_decision(&record, Some(1), &["completed".into()]);
        assert!(decision.reached);
        assert!(decision.resolved);
        assert!(!decision.superseded);
        let completed = decision.run.expect("historical run");
        assert_eq!(completed.id, 1);
        assert_eq!(completed.state, AgentRunState::Completed);
    }

    #[test]
    fn same_needs_input_snapshot_cannot_consume_the_pre_send_intent() {
        let mut lifecycle = Orchestration::default();
        let mut record = record();
        let detail = NeedsInputDetail {
            kind: diri_proto::NeedsInputKind::Question,
            source: diri_proto::NeedsInputSource::ScreenScrape,
            tool_name: None,
            summary: "choose".into(),
            prompt_excerpt: Some("choose\n> yes".into()),
            options: Some(vec!["yes".into(), "no".into()]),
            risk_hint: diri_proto::RiskHint::Neutral,
            occurred_at: now(),
        };
        record.run.as_mut().unwrap().state = AgentRunState::NeedsInput;
        lifecycle.register(&record, 0, &SessionStatus::NeedsInput(detail.kind));
        lifecycle
            .prepare_delivery(
                &mut record,
                DeliveryRequest {
                    status: &SessionStatus::NeedsInput(detail.kind),
                    needs_input: Some(&detail),
                    completion_seq: 0,
                    output_offset: 50,
                    expected_run_id: Some(1),
                    text: "yes".into(),
                    submit: true,
                    allow_needs_input: true,
                    request_id: Some("answer-1"),
                    request_fingerprint: Some("fingerprint"),
                },
            )
            .unwrap();

        assert!(!lifecycle.observe(
            &mut record,
            &SessionStatus::NeedsInput(detail.kind),
            Some(&detail),
            0,
            50,
        ));
        let encoded = serde_json::to_value(&lifecycle).unwrap();
        assert!(encoded["dispatching"]["child"].is_object());

        let mut restored: Orchestration = serde_json::from_value(encoded).unwrap();
        let mut records = HashMap::from([("child".into(), record)]);
        assert!(restored.recover_inflight(&mut records));
        assert_eq!(
            records["child"]
                .run
                .as_ref()
                .unwrap()
                .terminal_outcome
                .as_deref(),
            Some("delivery_outcome_uncertain")
        );
    }

    #[test]
    fn blocker_identity_ignores_timestamp_and_volatile_screen_echo() {
        let mut lifecycle = Orchestration::default();
        let mut record = record();
        let detail = NeedsInputDetail {
            kind: diri_proto::NeedsInputKind::Question,
            source: diri_proto::NeedsInputSource::ScreenScrape,
            tool_name: None,
            summary: "Choose one".into(),
            prompt_excerpt: Some("Choose one\n> A".into()),
            options: Some(vec!["A".into(), "B".into()]),
            risk_hint: diri_proto::RiskHint::Neutral,
            occurred_at: DateMillis(1.0),
        };
        record.run.as_mut().unwrap().state = AgentRunState::NeedsInput;
        lifecycle.register(&record, 0, &SessionStatus::NeedsInput(detail.kind));
        lifecycle
            .prepare_delivery(
                &mut record,
                DeliveryRequest {
                    status: &SessionStatus::NeedsInput(detail.kind),
                    needs_input: Some(&detail),
                    completion_seq: 0,
                    output_offset: 10,
                    expected_run_id: Some(1),
                    text: "A".into(),
                    submit: true,
                    allow_needs_input: true,
                    request_id: None,
                    request_fingerprint: None,
                },
            )
            .unwrap();
        lifecycle.finish_dispatch("child", Some(1), true, Some(detail.clone()), 10);

        let mut refreshed = detail.clone();
        refreshed.occurred_at = DateMillis(2.0);
        refreshed.prompt_excerpt = Some("Choose one\nA█".into());
        assert!(!lifecycle.observe(
            &mut record,
            &SessionStatus::NeedsInput(detail.kind),
            Some(&refreshed),
            0,
            11,
        ));
        assert_eq!(record.run.as_ref().unwrap().state, AgentRunState::Running);
        let duplicate = lifecycle
            .prepare_delivery(
                &mut record,
                DeliveryRequest {
                    status: &SessionStatus::NeedsInput(detail.kind),
                    needs_input: Some(&refreshed),
                    completion_seq: 0,
                    output_offset: 11,
                    expected_run_id: Some(1),
                    text: "A".into(),
                    submit: true,
                    allow_needs_input: true,
                    request_id: None,
                    request_fingerprint: None,
                },
            )
            .unwrap();
        assert!(duplicate.queued);
    }

    #[test]
    fn adoption_uses_durable_output_provenance_to_close_awaiting_work() {
        let mut lifecycle = Orchestration::default();
        let mut record = record();
        record.run.as_mut().unwrap().state = AgentRunState::Starting;
        lifecycle.register(&record, 0, &SessionStatus::Idle);
        lifecycle
            .prepare_delivery(
                &mut record,
                DeliveryRequest {
                    status: &SessionStatus::Idle,
                    needs_input: None,
                    completion_seq: 0,
                    output_offset: 100,
                    expected_run_id: Some(1),
                    text: "work".into(),
                    submit: true,
                    allow_needs_input: true,
                    request_id: None,
                    request_fingerprint: None,
                },
            )
            .unwrap();
        lifecycle.finish_dispatch("child", Some(1), true, None, 100);
        let encoded = serde_json::to_value(&lifecycle).unwrap();
        let mut restored: Orchestration = serde_json::from_value(encoded).unwrap();
        restored.register_adopted(&record, 0, &SessionStatus::Idle);

        assert!(restored.observe(&mut record, &SessionStatus::Idle, None, 0, 120));
        assert_eq!(record.run.as_ref().unwrap().state, AgentRunState::Completed);
    }

    #[test]
    fn adoption_closes_an_answered_blocker_that_finished_while_daemon_was_down() {
        let mut lifecycle = Orchestration::default();
        let mut record = record();
        let detail = NeedsInputDetail {
            kind: diri_proto::NeedsInputKind::Question,
            source: diri_proto::NeedsInputSource::ScreenScrape,
            tool_name: None,
            summary: "Continue?".into(),
            prompt_excerpt: None,
            options: Some(vec!["yes".into(), "no".into()]),
            risk_hint: diri_proto::RiskHint::Neutral,
            occurred_at: DateMillis(1.0),
        };
        record.run.as_mut().unwrap().state = AgentRunState::NeedsInput;
        lifecycle.register(&record, 0, &SessionStatus::NeedsInput(detail.kind));
        lifecycle
            .prepare_delivery(
                &mut record,
                DeliveryRequest {
                    status: &SessionStatus::NeedsInput(detail.kind),
                    needs_input: Some(&detail),
                    completion_seq: 0,
                    output_offset: 100,
                    expected_run_id: Some(1),
                    text: "yes".into(),
                    submit: true,
                    allow_needs_input: true,
                    request_id: None,
                    request_fingerprint: None,
                },
            )
            .unwrap();
        lifecycle.finish_dispatch("child", Some(1), true, Some(detail), 100);
        let mut restored: Orchestration =
            serde_json::from_value(serde_json::to_value(&lifecycle).unwrap()).unwrap();
        restored.register_adopted(
            &record,
            0,
            &SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Question),
        );

        assert!(restored.observe(&mut record, &SessionStatus::Idle, None, 0, 120));
        assert_eq!(record.run.as_ref().unwrap().state, AgentRunState::Completed);
    }

    #[test]
    fn interrupt_intent_recovers_uncertain_without_losing_future_fifo() {
        let mut lifecycle = Orchestration::default();
        let mut record = record();
        record.run.as_mut().unwrap().state = AgentRunState::Running;
        lifecycle.register(&record, 0, &SessionStatus::Working);
        lifecycle
            .prepare_delivery(
                &mut record,
                DeliveryRequest {
                    status: &SessionStatus::Working,
                    needs_input: None,
                    completion_seq: 0,
                    output_offset: 10,
                    expected_run_id: Some(1),
                    text: "future".into(),
                    submit: true,
                    allow_needs_input: true,
                    request_id: None,
                    request_fingerprint: None,
                },
            )
            .unwrap();
        lifecycle
            .prepare_interrupt(
                &mut record,
                Some(1),
                Some("interrupt-1"),
                Some("fingerprint"),
            )
            .unwrap();

        let mut restored: Orchestration =
            serde_json::from_value(serde_json::to_value(&lifecycle).unwrap()).unwrap();
        let mut records = HashMap::from([("child".into(), record)]);
        assert!(restored.recover_inflight(&mut records));
        let run = records["child"].run.as_ref().unwrap();
        assert_eq!(run.state, AgentRunState::Aborted);
        assert_eq!(
            run.terminal_outcome.as_deref(),
            Some("interrupt_outcome_uncertain")
        );
        assert_eq!(restored.latest_run_id(&records["child"]), 2);
        assert!(matches!(
            restored
                .lookup_replay(
                    "child",
                    ReplayOperation::Interrupt,
                    Some("interrupt-1"),
                    Some("fingerprint")
                )
                .unwrap(),
            Some(ReplayOutcome::OutcomeUncertain { .. })
        ));
    }

    #[test]
    fn uncertain_delivery_reconciles_activity_to_the_original_generation() {
        let mut lifecycle = Orchestration::default();
        let mut record = record();
        lifecycle.register(&record, 0, &SessionStatus::Idle);
        lifecycle
            .prepare_delivery(
                &mut record,
                DeliveryRequest {
                    status: &SessionStatus::Idle,
                    needs_input: None,
                    completion_seq: 0,
                    output_offset: 10,
                    expected_run_id: Some(1),
                    text: "maybe".into(),
                    submit: true,
                    allow_needs_input: true,
                    request_id: None,
                    request_fingerprint: None,
                },
            )
            .unwrap();
        let mut records = HashMap::from([("child".into(), record)]);
        assert!(lifecycle.recover_inflight(&mut records));
        let mut record = records.remove("child").unwrap();
        assert_eq!(record.run.as_ref().unwrap().state, AgentRunState::Failed);
        lifecycle.register_adopted(&record, 0, &SessionStatus::Idle);

        assert!(lifecycle.observe(&mut record, &SessionStatus::Working, None, 0, 20,));
        let run = record.run.as_ref().unwrap();
        assert_eq!(run.id, 1);
        assert_eq!(run.state, AgentRunState::Running);
        assert!(run.terminal_outcome.is_none());
    }

    #[test]
    fn dead_future_wait_resolves_without_timeout_or_false_supersession() {
        let mut lifecycle = Orchestration::default();
        let mut record = record();
        record.status = SessionStatus::Exited(diri_proto::ExitInfo {
            reason: diri_proto::ExitReason::Exited,
            code: Some(0),
            signal: None,
        });
        let run = record.run.as_mut().unwrap();
        run.state = AgentRunState::Failed;
        run.finished_at = Some(now());
        run.terminal_outcome = Some("process_exited".into());
        lifecycle.remember_current_terminal(&record);

        let decision = lifecycle.wait_decision(&record, Some(2), &["done".into()]);
        assert!(decision.resolved);
        assert!(!decision.reached);
        assert!(!decision.superseded);
        assert!(decision.run.is_none());
    }

    #[test]
    fn durable_journals_are_bounded() {
        let mut lifecycle = Orchestration::default();
        for id in 1..=40 {
            lifecycle.remember_terminal(
                "child",
                AgentRun {
                    id,
                    state: AgentRunState::Completed,
                    started_at: DateMillis(id as f64),
                    finished_at: Some(DateMillis(id as f64)),
                    terminal_outcome: Some("completed".into()),
                },
            );
        }
        assert_eq!(
            lifecycle.terminal_runs["child"].len(),
            TERMINAL_HISTORY_LIMIT
        );
        assert_eq!(lifecycle.terminal_runs["child"].front().unwrap().id, 9);

        for id in 1..=70 {
            let request_id = format!("request-{id}");
            lifecycle
                .remember_replay(
                    "child",
                    ReplayOperation::Delivery,
                    Some(&request_id),
                    Some("fingerprint"),
                    ReplayOutcome::Delivery {
                        queued: true,
                        run: None,
                    },
                )
                .unwrap();
        }
        assert_eq!(
            lifecycle.request_replays["child"].len(),
            REQUEST_REPLAY_LIMIT
        );
        let restored: Orchestration =
            serde_json::from_value(serde_json::to_value(&lifecycle).unwrap()).unwrap();
        assert!(matches!(
            restored
                .lookup_replay(
                    "child",
                    ReplayOperation::Delivery,
                    Some("request-70"),
                    Some("fingerprint")
                )
                .unwrap(),
            Some(ReplayOutcome::Delivery { queued: true, .. })
        ));
        assert_eq!(
            restored.lookup_replay(
                "child",
                ReplayOperation::Delivery,
                Some("request-70"),
                Some("different")
            ),
            Err(RunError::RequestConflict {
                request_id: "request-70".into()
            })
        );
    }
}
