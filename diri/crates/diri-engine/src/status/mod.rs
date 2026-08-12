//! The per-session status state machine.
//!
//! Everything the daemon learns about a session — hook callbacks, screen
//! observations, PTY activity, process exit, periodic ticks — is funnelled
//! through [`StatusReducer`], which owns the single canonical answer to "what
//! is this session doing". Pure and synchronous: no clock of its own, no IO.
//! The caller passes `now`, which is what makes the debounce behavior testable
//! without sleeping.
//!
//! Ported from the Swift `StatusReducer`. The reducer is where most of the
//! product's hard-won behavior lives — anti-flicker, blocker arbitration,
//! startup grace, subagent isolation — so the port keeps the same structure
//! rather than being rewritten, and the tests below encode the reasons.

mod risk;

pub use risk::classify_risk;

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use diri_proto::{
    ExitInfo, ExitReason, NeedsInputDetail, NeedsInputKind, NeedsInputSource, SessionStatus,
    StatusEvidence, StatusEvidenceSource, StatusFallbackReason,
};

use crate::detect::{ManifestState, ScreenObservation, redact};

/// Which source of truth leads for an agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authority {
    /// Claude: hooks drive state, the screen arbitrates blockers.
    HooksPrimary,
    /// Codex: the screen drives state, notify confirms done.
    ScreenPrimary,
    /// Everything else: starting → working → exited, nothing more.
    ProcessOnly,
}

/// Timing knobs. The defaults are the ones the Swift daemon shipped.
#[derive(Clone, Copy, Debug)]
pub struct ReducerTiming {
    pub idle_confirmations: u32,
    pub recheck_interval: Duration,
    pub idle_confirm_cap: Duration,
    pub startup_grace: Duration,
    pub hook_authority_window: Duration,
    pub blocker_clear_scans: u32,
    pub staleness_timeout: Duration,
}

impl Default for ReducerTiming {
    fn default() -> Self {
        Self {
            idle_confirmations: 3,
            recheck_interval: Duration::from_millis(100),
            idle_confirm_cap: Duration::from_millis(700),
            startup_grace: Duration::from_secs(3),
            hook_authority_window: Duration::from_secs(7),
            blocker_clear_scans: 2,
            staleness_timeout: Duration::from_secs(60),
        }
    }
}

/// A Claude hook event, already parsed out of the raw payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaudeHook {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest {
        tool_name: Option<String>,
        input_summary: Option<String>,
    },
    Notification {
        notification_type: Option<String>,
        message: Option<String>,
    },
    Stop,
    SubagentStart(String),
    SubagentStop(String),
    SessionEnd,
}

/// Everything the reducer can consume.
#[derive(Clone, Debug)]
pub enum StatusSignal {
    /// `is_subagent` is true when the payload carried an agent id. Those events
    /// must never drive the parent session's canonical state.
    ClaudeHook {
        hook: ClaudeHook,
        is_subagent: bool,
    },
    CodexTurnComplete,
    Screen(ScreenObservation),
    PtyOutputActivity,
    UserKeystroke,
    ProcessExit {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// Periodic tick driving the debounce timers.
    Tick,
}

/// What reducing one signal produced.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReducerOutcome {
    /// Set when the canonical status changed.
    pub status_change: Option<SessionStatus>,
    /// A privacy-safe explanation of the canonical status. Emitted only when
    /// its structured meaning changes, so evidence does not turn every screen
    /// scan into a persisted/UI record update.
    pub status_evidence: Option<StatusEvidence>,
    /// Set when a needs-input detail was produced or updated.
    pub needs_input: Option<NeedsInputDetail>,
    /// Set when a turn just completed.
    pub turn_completed: bool,
}

/// The mutable belief and debounce tracking for one session.
#[derive(Clone, Debug)]
struct InternalState {
    spawned_at: SystemTime,
    /// When the most recent meaningful signal arrived; drives staleness.
    last_signal_at: SystemTime,

    /// A turn is in flight, which gates `turn_completed`.
    turn_in_flight: bool,
    /// Running subagents. Bookkeeping only, never canonical state.
    active_subagents: HashSet<String>,

    // Working → idle anti-flicker.
    idle_candidate_since: Option<SystemTime>,
    idle_confirms: u32,
    /// A strong idle signal (Claude Stop, codex turn-complete) lowers the
    /// confirmation requirement to one.
    idle_strong: bool,
    /// Fire `turn_completed` exactly once on the next committed working→idle.
    pending_turn_completed: bool,

    // On-screen blocker tracking.
    screen_blocker_active: bool,
    blocker_miss_scans: u32,

    // Screen belief.
    screen_belief: Option<ManifestState>,
    last_screen_seq: Option<u64>,
    last_matched_rule_id: Option<String>,

    /// A `skip` screen (transcript viewer, model picker) is being held.
    skip_active: bool,
    /// The user started typing a response to a needs-input prompt.
    responding_since: Option<SystemTime>,
    /// Last needs-input detail produced, for dedupe upstream.
    pending_needs_input: Option<NeedsInputDetail>,
}

impl InternalState {
    fn new(spawned_at: SystemTime) -> Self {
        Self {
            spawned_at,
            last_signal_at: spawned_at,
            turn_in_flight: false,
            active_subagents: HashSet::new(),
            idle_candidate_since: None,
            idle_confirms: 0,
            idle_strong: false,
            pending_turn_completed: false,
            screen_blocker_active: false,
            blocker_miss_scans: 0,
            screen_belief: None,
            last_screen_seq: None,
            last_matched_rule_id: None,
            skip_active: false,
            responding_since: None,
            pending_needs_input: None,
        }
    }
}

pub struct StatusReducer {
    status: SessionStatus,
    authority: Authority,
    timing: ReducerTiming,
    state: InternalState,
    manifest_id: Option<String>,
    manifest_version: Option<String>,
    evidence: Option<StatusEvidence>,
}

impl StatusReducer {
    pub fn new(authority: Authority, spawned_at: SystemTime) -> Self {
        Self {
            status: SessionStatus::Starting,
            authority,
            timing: ReducerTiming::default(),
            state: InternalState::new(spawned_at),
            manifest_id: None,
            manifest_version: None,
            evidence: None,
        }
    }

    /// Associates the reducer with manifest metadata safe to expose over the
    /// control protocol. Manifest contents and terminal captures stay inside
    /// the detection engine.
    pub fn with_manifest(
        mut self,
        id: impl Into<String>,
        version: Option<impl Into<String>>,
    ) -> Self {
        self.manifest_id = Some(id.into());
        self.manifest_version = version.map(Into::into);
        self
    }

    pub fn with_timing(mut self, timing: ReducerTiming) -> Self {
        self.timing = timing;
        self
    }

    pub fn status(&self) -> &SessionStatus {
        &self.status
    }

    pub fn authority(&self) -> Authority {
        self.authority
    }

    pub fn evidence(&self) -> Option<&StatusEvidence> {
        self.evidence.as_ref()
    }

    pub fn active_subagents(&self) -> usize {
        self.state.active_subagents.len()
    }

    /// A Holder adoption is not a process launch. Its first authoritative
    /// snapshot describes an already-running terminal and must not be delayed
    /// by the new-process startup grace.
    pub fn finish_startup_grace(&mut self, now: SystemTime) {
        self.state.spawned_at = now
            .checked_sub(self.timing.startup_grace)
            .unwrap_or(SystemTime::UNIX_EPOCH);
    }

    /// Folds one signal into the session's status.
    pub fn reduce(&mut self, signal: StatusSignal, now: SystemTime) -> ReducerOutcome {
        let mut outcome = ReducerOutcome::default();

        // Exited is absorbing: once dead, nothing changes it.
        if matches!(self.status, SessionStatus::Exited(_)) {
            return outcome;
        }

        // Process exit is authoritative under every authority mode.
        if let StatusSignal::ProcessExit { code, signal } = &signal {
            let reason = if signal.is_some() {
                ExitReason::Signaled
            } else {
                ExitReason::Exited
            };
            self.set_status(
                SessionStatus::Exited(ExitInfo {
                    reason,
                    code: *code,
                    signal: *signal,
                }),
                &mut outcome,
            );
            self.publish_evidence(
                StatusEvidenceSource::ProcessLiveness,
                None,
                Some(StatusFallbackReason::ProcessExited),
                now,
                &mut outcome,
            );
            return outcome;
        }

        // processOnly: starting → working on first output, then only exit moves it.
        if self.authority == Authority::ProcessOnly {
            if matches!(signal, StatusSignal::PtyOutputActivity) {
                if self.status == SessionStatus::Starting {
                    self.state.turn_in_flight = true;
                    self.set_status(SessionStatus::Working, &mut outcome);
                }
                self.state.last_signal_at = now;
                self.publish_evidence(
                    StatusEvidenceSource::ProcessLiveness,
                    None,
                    Some(StatusFallbackReason::ProcessOnly),
                    now,
                    &mut outcome,
                );
            }
            return outcome;
        }

        let evidence_hint = match &signal {
            StatusSignal::ClaudeHook { .. } => Some((StatusEvidenceSource::Hook, None, None)),
            StatusSignal::CodexTurnComplete => Some((StatusEvidenceSource::Notify, None, None)),
            StatusSignal::Screen(observation) => Some((
                StatusEvidenceSource::ScreenRule,
                Some(observation.matched_rule_id.clone()),
                None,
            )),
            StatusSignal::Tick => None,
            StatusSignal::PtyOutputActivity
            | StatusSignal::UserKeystroke
            | StatusSignal::ProcessExit { .. } => None,
        };

        match signal {
            StatusSignal::ProcessExit { .. } => {} // handled above
            StatusSignal::PtyOutputActivity => {
                // Refreshes the recency of `working`; for full status models it
                // never by itself means work is happening.
                self.state.last_signal_at = now;
            }
            StatusSignal::UserKeystroke => {
                self.state.last_signal_at = now;
                if matches!(self.status, SessionStatus::NeedsInput(_)) {
                    self.state.responding_since = Some(now);
                }
            }
            StatusSignal::ClaudeHook { hook, is_subagent } => {
                self.handle_claude_hook(hook, is_subagent, now, &mut outcome)
            }
            StatusSignal::CodexTurnComplete => {
                self.state.last_signal_at = now;
                self.handle_strong_idle(now, &mut outcome);
            }
            StatusSignal::Screen(observation) => self.handle_screen(observation, now, &mut outcome),
            StatusSignal::Tick => self.handle_tick(now, &mut outcome),
        }

        if self.status == SessionStatus::Unknown {
            self.publish_evidence(
                StatusEvidenceSource::Staleness,
                None,
                Some(StatusFallbackReason::StaleSignals),
                now,
                &mut outcome,
            );
        } else if let Some((source, rule, fallback)) = evidence_hint {
            let source_is_authoritative = outcome.status_change.is_some()
                || (source == StatusEvidenceSource::ScreenRule
                    && self.authority == Authority::ScreenPrimary)
                || matches!(self.status, SessionStatus::NeedsInput(_))
                || self.state.idle_candidate_since.is_some();
            if source_is_authoritative {
                self.publish_evidence(source, rule, fallback, now, &mut outcome);
            }
        } else if self.status == SessionStatus::Starting {
            self.publish_evidence(
                StatusEvidenceSource::ProcessLiveness,
                None,
                Some(StatusFallbackReason::StartupGrace),
                now,
                &mut outcome,
            );
        } else if outcome.status_change.is_some()
            && let Some(previous) = self.evidence.clone()
            && previous.anti_flicker_active
            && matches!(
                previous.source,
                StatusEvidenceSource::Hook | StatusEvidenceSource::Notify
            )
        {
            // The Tick only closes an anti-flicker window opened by this
            // strong signal. A previously matched screen rule may describe
            // the old status, so the initiating hook/notify wins here.
            self.publish_evidence(
                previous.source,
                previous.matched_rule_id,
                previous.fallback_reason,
                now,
                &mut outcome,
            );
        } else if outcome.status_change.is_some()
            && let Some(rule) = self.state.last_matched_rule_id.clone()
        {
            // A Tick can commit an already-observed screen decision after the
            // startup grace or anti-flicker window. Preserve that rule rather
            // than labelling the timer itself as the authority.
            self.publish_evidence(
                StatusEvidenceSource::ScreenRule,
                Some(rule),
                None,
                now,
                &mut outcome,
            );
        } else if outcome.status_change.is_some()
            && let Some(previous) = self.evidence.clone()
        {
            // A delayed hook/notify idle decision is committed by a Tick, but
            // the timer is not the reason for the decision. Carry the source
            // which opened the anti-flicker window into the final status.
            self.publish_evidence(
                previous.source,
                previous.matched_rule_id,
                previous.fallback_reason,
                now,
                &mut outcome,
            );
        }

        outcome
    }

    fn publish_evidence(
        &mut self,
        source: StatusEvidenceSource,
        matched_rule_id: Option<String>,
        fallback_reason: Option<StatusFallbackReason>,
        now: SystemTime,
        outcome: &mut ReducerOutcome,
    ) {
        let startup_grace_active = self.status == SessionStatus::Starting
            && now
                .duration_since(self.state.spawned_at)
                .unwrap_or_default()
                < self.timing.startup_grace;
        let anti_flicker_active = self.state.idle_candidate_since.is_some()
            || (self.state.screen_blocker_active && self.state.blocker_miss_scans > 0);
        let candidate = StatusEvidence {
            status: self.status.clone(),
            source,
            signal_at: now.into(),
            matched_rule_id,
            startup_grace_active,
            anti_flicker_active,
            manifest_id: self.manifest_id.clone(),
            manifest_version: self.manifest_version.clone(),
            fallback_reason,
        };
        let meaning_changed = self.evidence.as_ref().is_none_or(|previous| {
            previous.status != candidate.status
                || previous.source != candidate.source
                || previous.matched_rule_id != candidate.matched_rule_id
                || previous.startup_grace_active != candidate.startup_grace_active
                || previous.anti_flicker_active != candidate.anti_flicker_active
                || previous.manifest_id != candidate.manifest_id
                || previous.manifest_version != candidate.manifest_version
                || previous.fallback_reason != candidate.fallback_reason
        });
        if meaning_changed {
            self.evidence = Some(candidate.clone());
            outcome.status_evidence = Some(candidate);
        }
    }

    fn set_status(&mut self, new: SessionStatus, outcome: &mut ReducerOutcome) {
        if self.status != new {
            self.status = new.clone();
            outcome.status_change = Some(new);
        }
    }

    // MARK: Working and idle

    fn cancel_idle_candidacy(&mut self) {
        self.state.idle_candidate_since = None;
        self.state.idle_confirms = 0;
        self.state.idle_strong = false;
        self.state.pending_turn_completed = false;
    }

    /// Enter or refresh `working` from a positive work signal. For
    /// hooks-primary agents a work hook also clears a stale on-screen blocker.
    fn go_working(
        &mut self,
        now: SystemTime,
        clear_screen_blocker: bool,
        outcome: &mut ReducerOutcome,
    ) {
        self.cancel_idle_candidacy();
        if clear_screen_blocker {
            self.state.screen_blocker_active = false;
            self.state.blocker_miss_scans = 0;
        }
        self.state.turn_in_flight = true;
        self.state.last_signal_at = now;
        self.set_status(SessionStatus::Working, outcome);
    }

    /// A strong idle signal. Commits immediately when the screen already reads
    /// idle, otherwise waits for one further confirmation.
    fn handle_strong_idle(&mut self, now: SystemTime, outcome: &mut ReducerOutcome) {
        if self.status == SessionStatus::Starting {
            // A definitive end-of-turn during startup means idle.
            self.set_status(SessionStatus::Idle, outcome);
            return;
        }
        if self.status != SessionStatus::Working {
            return;
        }
        self.state.idle_strong = true;
        self.state.pending_turn_completed = self.state.turn_in_flight;
        if self.state.idle_candidate_since.is_none() {
            self.state.idle_candidate_since = Some(now);
        }
        if self.state.screen_belief == Some(ManifestState::Idle) {
            self.state.idle_confirms += 1;
            self.commit_idle(now, outcome);
        }
    }

    /// Register one idle-confirming observation.
    fn confirm_idle(&mut self, now: SystemTime, outcome: &mut ReducerOutcome) {
        if self.status != SessionStatus::Working {
            return;
        }
        if self.state.idle_candidate_since.is_none() {
            self.state.idle_candidate_since = Some(now);
            self.state.idle_confirms = 0;
        }
        self.state.idle_confirms += 1;
        let required = if self.state.idle_strong {
            1
        } else {
            self.timing.idle_confirmations
        };
        let elapsed = self
            .state
            .idle_candidate_since
            .and_then(|since| now.duration_since(since).ok())
            .unwrap_or_default();
        if self.state.idle_confirms >= required
            || (elapsed >= self.timing.idle_confirm_cap && self.state.idle_confirms >= 1)
        {
            self.commit_idle(now, outcome);
        }
    }

    fn commit_idle(&mut self, now: SystemTime, outcome: &mut ReducerOutcome) {
        let fire = self.state.pending_turn_completed;
        self.set_status(SessionStatus::Idle, outcome);
        self.state.turn_in_flight = false;
        if fire {
            outcome.turn_completed = true;
        }
        self.cancel_idle_candidacy();
        self.state.last_signal_at = now;
    }

    // MARK: Claude hooks

    fn handle_claude_hook(
        &mut self,
        hook: ClaudeHook,
        is_subagent: bool,
        now: SystemTime,
        outcome: &mut ReducerOutcome,
    ) {
        self.state.last_signal_at = now;

        // Subagent lifecycle is bookkeeping only, never canonical state.
        match &hook {
            ClaudeHook::SubagentStart(id) => {
                self.state.active_subagents.insert(id.clone());
                return;
            }
            ClaudeHook::SubagentStop(id) => {
                self.state.active_subagents.remove(id);
                return;
            }
            _ => {}
        }
        // Anything carrying an agent id belongs to a subagent — the parent's
        // state must not move because a child of it did something.
        if is_subagent {
            return;
        }

        match hook {
            ClaudeHook::SessionStart => {
                // Definitive signal ending the startup grace.
                if self.status == SessionStatus::Starting {
                    self.set_status(SessionStatus::Idle, outcome);
                }
            }
            ClaudeHook::UserPromptSubmit => {
                self.state.turn_in_flight = true;
                self.go_working(now, true, outcome);
            }
            ClaudeHook::PreToolUse => self.go_working(now, true, outcome),
            ClaudeHook::PermissionRequest {
                tool_name,
                input_summary,
            } => {
                let detail = permission_detail(tool_name, input_summary, now);
                self.state.pending_needs_input = Some(detail.clone());
                outcome.needs_input = Some(detail);
                self.cancel_idle_candidacy();
                self.set_status(
                    SessionStatus::NeedsInput(NeedsInputKind::Permission),
                    outcome,
                );
            }
            ClaudeHook::Notification {
                notification_type,
                message,
            } => self.handle_notification(notification_type, message, now, outcome),
            ClaudeHook::Stop => self.handle_strong_idle(now, outcome),
            // A hint only.
            ClaudeHook::SessionEnd => {}
            ClaudeHook::SubagentStart(_) | ClaudeHook::SubagentStop(_) => {}
        }
    }

    fn handle_notification(
        &mut self,
        notification_type: Option<String>,
        message: Option<String>,
        now: SystemTime,
        outcome: &mut ReducerOutcome,
    ) {
        match notification_type.as_deref() {
            Some("permission_prompt") => {
                let text = message.unwrap_or_else(|| "Permission required".into());
                let detail = NeedsInputDetail {
                    kind: NeedsInputKind::Permission,
                    source: NeedsInputSource::ClaudeNotificationHook,
                    tool_name: None,
                    summary: redact(&text),
                    prompt_excerpt: None,
                    options: None,
                    risk_hint: classify_risk(&text),
                    occurred_at: now.into(),
                };
                self.state.pending_needs_input = Some(detail.clone());
                outcome.needs_input = Some(detail);
                self.cancel_idle_candidacy();
                self.set_status(
                    SessionStatus::NeedsInput(NeedsInputKind::Permission),
                    outcome,
                );
            }
            Some("idle_prompt") | Some("agent_needs_input") | Some("elicitation_dialog") => {
                let text = message.unwrap_or_else(|| "Waiting for input".into());
                let detail = NeedsInputDetail {
                    kind: NeedsInputKind::Question,
                    source: NeedsInputSource::ClaudeNotificationHook,
                    tool_name: None,
                    summary: redact(&text),
                    prompt_excerpt: None,
                    options: None,
                    risk_hint: classify_risk(&text),
                    occurred_at: now.into(),
                };
                self.state.pending_needs_input = Some(detail.clone());
                outcome.needs_input = Some(detail);
                self.cancel_idle_candidacy();
                self.set_status(SessionStatus::NeedsInput(NeedsInputKind::Question), outcome);
            }
            Some("agent_completed") => self.handle_strong_idle(now, outcome),
            _ => {}
        }
    }

    // MARK: Screen

    fn handle_screen(
        &mut self,
        observation: ScreenObservation,
        now: SystemTime,
        outcome: &mut ReducerOutcome,
    ) {
        self.state.last_signal_at = now;

        // A skip screen holds the current state and suppresses screen-driven
        // transitions entirely.
        if observation.state == ManifestState::Skip {
            self.state.skip_active = true;
            return;
        }
        self.state.skip_active = false;

        // Skip redundant scans when the content has not changed.
        if self.state.last_screen_seq == Some(observation.content_seq) {
            return;
        }
        self.state.last_screen_seq = Some(observation.content_seq);
        self.state.screen_belief = Some(observation.state);
        self.state.last_matched_rule_id = Some(observation.matched_rule_id.clone());

        // A visible blocker beats everything except process exit.
        if let Some(kind) = needs_input_kind(observation.state) {
            self.state.screen_blocker_active = true;
            self.state.blocker_miss_scans = 0;
            let detail = screen_detail(kind, &observation, now);
            self.state.pending_needs_input = Some(detail.clone());
            outcome.needs_input = Some(detail);
            self.cancel_idle_candidacy();
            self.set_status(SessionStatus::NeedsInput(kind), outcome);
            return;
        }

        // A non-blocker observation while a blocker is active only releases it
        // after enough consecutive misses — one stray frame must not clear a
        // prompt the user is still looking at.
        if self.state.screen_blocker_active {
            self.state.blocker_miss_scans += 1;
            if self.state.blocker_miss_scans < self.timing.blocker_clear_scans {
                return;
            }
            self.state.screen_blocker_active = false;
            self.state.blocker_miss_scans = 0;
            self.apply_non_blocker_screen(&observation, now, true, outcome);
            return;
        }

        // Startup grace: hold `starting` unless the signal is definitive. A
        // working screen counts as definitive only for screen-primary agents.
        if self.status == SessionStatus::Starting {
            let elapsed = now
                .duration_since(self.state.spawned_at)
                .unwrap_or_default();
            let grace_active = elapsed < self.timing.startup_grace;
            let definitive = self.authority == Authority::ScreenPrimary
                && observation.state == ManifestState::Working;
            if grace_active && !definitive {
                return;
            }
        }

        self.apply_non_blocker_screen(&observation, now, false, outcome);
    }

    fn apply_non_blocker_screen(
        &mut self,
        observation: &ScreenObservation,
        now: SystemTime,
        cleared_blocker: bool,
        outcome: &mut ReducerOutcome,
    ) {
        match observation.state {
            ManifestState::Working => self.go_working(now, false, outcome),
            ManifestState::Idle => {
                if self.status == SessionStatus::Working {
                    self.confirm_idle(now, outcome);
                } else if self.status == SessionStatus::Starting {
                    self.set_status(SessionStatus::Idle, outcome);
                } else if cleared_blocker && matches!(self.status, SessionStatus::NeedsInput(_)) {
                    // The blocker was released and the screen now reads idle.
                    self.cancel_idle_candidacy();
                    self.set_status(SessionStatus::Idle, outcome);
                }
            }
            // Handled elsewhere.
            ManifestState::BlockedPermission
            | ManifestState::BlockedQuestion
            | ManifestState::Skip => {}
        }
    }

    // MARK: Tick

    fn handle_tick(&mut self, now: SystemTime, outcome: &mut ReducerOutcome) {
        // A reconnect/full snapshot can arrive entirely inside startup grace.
        // `handle_screen` remembers that belief but intentionally does not
        // publish it yet. If the screen then stays unchanged there is no
        // second frame to revisit, so the session used to remain Starting
        // forever. Reconsider the remembered non-blocker once grace expires.
        if self.status == SessionStatus::Starting
            && !self.state.skip_active
            && now
                .duration_since(self.state.spawned_at)
                .unwrap_or_default()
                >= self.timing.startup_grace
        {
            match self.state.screen_belief {
                Some(ManifestState::Working) => self.go_working(now, false, outcome),
                Some(ManifestState::Idle) => self.set_status(SessionStatus::Idle, outcome),
                Some(
                    ManifestState::BlockedPermission
                    | ManifestState::BlockedQuestion
                    | ManifestState::Skip,
                )
                | None => {}
            }
        }

        // Running but unreadable for long enough becomes unknown rather than a
        // confident lie.
        if self.status == SessionStatus::Working {
            let quiet = now
                .duration_since(self.state.last_signal_at)
                .unwrap_or_default();
            if quiet > self.timing.staleness_timeout {
                self.set_status(SessionStatus::Unknown, outcome);
                return;
            }
        }
        // A tick can supply the single confirmation a strong idle still needs.
        if self.status == SessionStatus::Working
            && self.state.idle_strong
            && self.state.idle_candidate_since.is_some()
        {
            self.state.idle_confirms += 1;
            self.commit_idle(now, outcome);
        }
    }
}

fn needs_input_kind(state: ManifestState) -> Option<NeedsInputKind> {
    match state {
        ManifestState::BlockedPermission => Some(NeedsInputKind::Permission),
        ManifestState::BlockedQuestion => Some(NeedsInputKind::Question),
        _ => None,
    }
}

fn permission_detail(
    tool_name: Option<String>,
    input_summary: Option<String>,
    now: SystemTime,
) -> NeedsInputDetail {
    let tool = tool_name.clone().unwrap_or_default();
    let summary = match tool.as_str() {
        "Bash" => format!(
            "wants to run `{}`",
            input_summary.clone().unwrap_or_default()
        ),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => format!(
            "wants to edit {}",
            input_summary.clone().unwrap_or_else(|| "a file".into())
        ),
        "" => input_summary
            .clone()
            .unwrap_or_else(|| "Permission required".into()),
        other => match &input_summary {
            Some(detail) => format!("wants to use {other}: {detail}"),
            None => format!("wants to use {other}"),
        },
    };
    let risk_source = input_summary.clone().unwrap_or(tool);
    NeedsInputDetail {
        kind: NeedsInputKind::Permission,
        source: NeedsInputSource::ClaudePermissionHook,
        tool_name,
        summary: redact(&summary),
        prompt_excerpt: input_summary.as_deref().map(redact),
        options: None,
        risk_hint: classify_risk(&risk_source),
        occurred_at: now.into(),
    }
}

fn screen_detail(
    kind: NeedsInputKind,
    observation: &ScreenObservation,
    now: SystemTime,
) -> NeedsInputDetail {
    let first_line = observation.prompt_excerpt.as_ref().and_then(|excerpt| {
        excerpt
            .split('\n')
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string)
    });
    let summary = first_line.unwrap_or_else(|| "Waiting for input".into());
    let risk_source = observation
        .prompt_excerpt
        .clone()
        .unwrap_or_else(|| summary.clone());
    NeedsInputDetail {
        kind,
        source: NeedsInputSource::ScreenScrape,
        tool_name: None,
        summary: redact(&summary),
        prompt_excerpt: observation.prompt_excerpt.clone(),
        options: observation.options.clone(),
        risk_hint: classify_risk(&risk_source),
        occurred_at: now.into(),
    }
}

#[cfg(test)]
mod tests;
