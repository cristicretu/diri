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
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use diri_proto::{
    ExitInfo, ExitReason, NeedsInputDetail, NeedsInputKind, NeedsInputSource, SessionStatus,
    StatusEvidence, StatusEvidenceSource, StatusFallbackReason,
};

use crate::detect::{ManifestState, ScreenObservation, redact};
use diri_terminal_state::{BlockedKind, ProgramRecord, ProgramRemoval, ProgramState};

/// Which source of truth leads for an agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authority {
    /// Claude: hooks drive state, the screen arbitrates blockers.
    HooksPrimary,
    /// Codex: the screen drives state, notify confirms done.
    ScreenPrimary,
    /// Everything else: starting → working → exited, nothing more.
    /// Shell is the exception: working follows the PTY foreground group, not
    /// process liveness, so an idle login prompt is idle.
    ProcessOnly,
}

/// Timing knobs. The defaults are the ones the Swift daemon shipped.
#[derive(Clone, Copy, Debug)]
pub struct ReducerTiming {
    pub idle_confirmations: u32,
    pub recheck_interval: Duration,
    pub idle_confirm_cap: Duration,
    pub startup_grace: Duration,
    pub blocker_clear_scans: u32,
    pub staleness_timeout: Duration,
    /// How long a shell job's terminal must have been still before a line
    /// read counts as a question. A prompt is printed and then waits; a job
    /// that reads between bursts of output is not asking anything.
    pub line_prompt_settle: Duration,
    /// How long after a submit a `stale_after_submit` screen is still taken
    /// for the previous turn's composer. Past it, that screen can end the turn
    /// again, so a turn too fast to show any working evidence still finishes.
    pub submit_chrome_hold: Duration,
}

impl Default for ReducerTiming {
    fn default() -> Self {
        Self {
            idle_confirmations: 3,
            recheck_interval: Duration::from_millis(100),
            idle_confirm_cap: Duration::from_millis(700),
            startup_grace: Duration::from_secs(3),
            blocker_clear_scans: 2,
            staleness_timeout: Duration::from_secs(60),
            line_prompt_settle: Duration::from_millis(750),
            submit_chrome_hold: Duration::from_secs(10),
        }
    }
}

/// A Claude hook event, already parsed out of the raw payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaudeHook {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PermissionRequest {
        tool_name: Option<String>,
        input_summary: Option<String>,
    },
    Notification {
        notification_type: Option<String>,
        message: Option<String>,
    },
    Stop,
    /// A Stop whose final message ends on a question to the user.
    StopAsking {
        question: String,
    },
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
        /// Optional aggregate from the payload or the durable recovery seed.
        pending_work: Option<bool>,
    },
    CodexTurnComplete,
    /// Cursor jsonl tail: last object is a user prompt or `tool_use`.
    /// `bytes` is the transcript's size, which tells turns apart.
    CursorTranscriptWorking {
        bytes: u64,
    },
    /// Cursor jsonl tail: last object is assistant text or `turn_ended`.
    CursorTranscriptIdle {
        bytes: u64,
    },
    Screen(ScreenObservation),
    PtyOutputActivity,
    UserKeystroke,
    /// Input accepted by the transport that can submit or dismiss a prompt.
    UserSubmission,
    /// The PTY foreground process group is, or is not, the session child.
    /// Shell sessions use this to show work only while a foreground job runs.
    ForegroundJob {
        running: bool,
    },
    ProcessExit {
        code: Option<i32>,
        signal: Option<i32>,
        /// Diri did not ask for this exit and an outside kill caused it; see
        /// [`diri_proto::ExitInfo::interrupted`].
        interrupted: bool,
    },
    /// Transport failed without evidence that the Agent process exited.
    TransportUnavailable,
    /// Whether a shell's foreground job is blocked reading a line from the
    /// terminal (`Proceed? [y/N]`, `Password:`, a script's `read`), sampled
    /// from the PTY owner. `None` when it is not.
    TerminalLine(Option<TerminalPrompt>),
    /// The `OSC 7501` record that best describes the program in the
    /// terminal, sent whenever it changes; `None` once nothing reports.
    ProgramStatus(Option<ProgramRecord>),
    /// Nothing reports any more, and why: the program cleared its records,
    /// the terminal was reset, or the program ended. Read as
    /// `ProgramStatus(None)`, with the reason on the evidence.
    ProgramStatusRemoved(ProgramRemoval),
    /// Periodic tick driving the debounce timers.
    Tick,
}

/// What a shell job waiting on a line shows, for the needs-input detail.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalPrompt {
    /// The cursor's row up to the cursor: the question as printed. Always
    /// `None` while echo is off.
    pub line: Option<String>,
    /// Echo is off: a password is being typed.
    pub secret: bool,
}

/// What Agents mean by their own `OSC 7501` reports, where it goes beyond the
/// specification, read from each manifest's `agent.programStatus`.
///
/// The specification has `done` end a turn and `idle` mean "waiting, with no
/// result": a cancelled or denied turn. Some programs report `idle` at the
/// end of every turn and never `done` (Codex 0.163), so for them `idle` after
/// work is the turn's result.
#[derive(Clone, Debug, Default)]
pub struct ProgramReporters {
    /// Values of the report's `app` key whose `idle` ends a turn.
    idle_ends_turn_apps: HashSet<String>,
    /// Manifests whose `idle` ends a turn, for reports that name no `app`.
    idle_ends_turn_manifests: HashSet<String>,
}

impl ProgramReporters {
    /// Declares that manifest `manifest_id`, whose program reports as `app`,
    /// ends its turns with `idle`.
    pub fn idle_ends_turn(&mut self, manifest_id: &str, app: Option<&str>) {
        self.idle_ends_turn_manifests.insert(manifest_id.to_owned());
        if let Some(app) = app {
            self.idle_ends_turn_apps.insert(app.to_owned());
        }
    }

    /// A record's own `app` decides; one without it is read with the manifest
    /// the session's foreground program runs under.
    fn idle_ends_turn_for(&self, record: &ProgramRecord, manifest_id: Option<&str>) -> bool {
        match record.app.as_deref() {
            Some(app) => self.idle_ends_turn_apps.contains(app),
            None => manifest_id.is_some_and(|id| self.idle_ends_turn_manifests.contains(id)),
        }
    }
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
    /// The turn that just completed failed: the program reported
    /// `OSC 7501` `error`. Only set together with `turn_completed`.
    pub turn_failed: bool,
    /// The turn ended without a result: the program reported `OSC 7501`
    /// `idle` mid-turn, which is how a cancelled or denied turn reads. Its
    /// open requests are settled, and nothing is announced as finished.
    pub turn_abandoned: bool,
    pub attention_changed: bool,
    /// A shell job stopped waiting on its line: answered, interrupted or
    /// gone. Settles the request it raised, which no turn completion will.
    pub line_prompt_ended: bool,
}

/// The mutable belief and debounce tracking for one session.
#[derive(Clone, Debug)]
struct InternalState {
    transport_unavailable: bool,
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
    /// A parent work hook owns its turn until a strong completion signal.
    /// Tool calls and thinking have no time limit; an idle-looking input box
    /// must not expire hook authority and announce completion mid-turn.
    hook_turn_in_flight: bool,
    /// Retained across idle reminders, which omit background task metadata.
    claude_pending_work: bool,

    // On-screen blocker tracking.
    screen_blocker_active: bool,
    blocker_miss_scans: u32,
    blocker_miss_since: Option<SystemTime>,

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
    /// Cursor jsonl said the turn ended. Ignore leftover Working OSC until
    /// the transcript shows work again — cursor-agent does not update the
    /// title to Ready at end of turn.
    hold_idle_against_screen: bool,
    /// The last submit, while a `stale_after_submit` screen may still be the
    /// previous turn's composer. Cleared by evidence that this turn started
    /// or ended, or after [`ReducerTiming::submit_chrome_hold`].
    submitted_at: Option<SystemTime>,
    /// Flags of the rule behind [`Self::screen_belief`].
    last_stale_after_submit: bool,
    last_turn_started: bool,
    /// Size of the Cursor transcript at its last poll.
    cursor_transcript_bytes: Option<u64>,
    /// The transcript size at submit. An idle tail of exactly this size is
    /// the previous turn's; any growth means this turn reached the file.
    cursor_stale_transcript_bytes: Option<u64>,
    /// The last output or keystroke on a shell's terminal, which a line
    /// read must outlast by [`ReducerTiming::line_prompt_settle`].
    terminal_active_at: SystemTime,
}

impl InternalState {
    fn new(spawned_at: SystemTime) -> Self {
        Self {
            transport_unavailable: false,
            spawned_at,
            last_signal_at: spawned_at,
            turn_in_flight: false,
            active_subagents: HashSet::new(),
            idle_candidate_since: None,
            idle_confirms: 0,
            idle_strong: false,
            pending_turn_completed: false,
            hook_turn_in_flight: false,
            claude_pending_work: false,
            screen_blocker_active: false,
            blocker_miss_scans: 0,
            blocker_miss_since: None,
            screen_belief: None,
            last_screen_seq: None,
            last_matched_rule_id: None,
            skip_active: false,
            responding_since: None,
            pending_needs_input: None,
            hold_idle_against_screen: false,
            submitted_at: None,
            last_stale_after_submit: false,
            last_turn_started: false,
            cursor_transcript_bytes: None,
            cursor_stale_transcript_bytes: None,
            terminal_active_at: spawned_at,
        }
    }
}

pub struct StatusReducer {
    attention: crate::attention::AttentionLifecycle,
    status: SessionStatus,
    authority: Authority,
    timing: ReducerTiming,
    state: InternalState,
    manifest_id: Option<String>,
    manifest_version: Option<String>,
    evidence: Option<StatusEvidence>,
    /// The shell's own authority and manifest while an Agent it runs in the
    /// foreground has borrowed the reducer. See [`Self::lend_to_agent`].
    lent_from: Option<(Authority, Option<String>, Option<String>)>,
    /// The program reports its own status with `OSC 7501`. Until it clears
    /// its records or ends, that outranks everything inferred from the screen
    /// or the shell's job; hooks, input and exit still apply.
    program_active: bool,
    /// The choices the screen last showed for a blocker while a program
    /// reports, which its `OSC 7501` record does not carry. See
    /// [`Self::note_screen_while_program_reports`].
    program_screen_choices: Option<(NeedsInputKind, Vec<String>)>,
    /// Set while the reducer is lent to an Agent known by name alone, whose
    /// status follows the terminal's output. See [`Self::lend_to_activity`].
    activity: Option<ActivityRead>,
    /// Shared by every session; see [`ProgramReporters`].
    program_reporters: Option<Arc<ProgramReporters>>,
}

/// How long output must keep coming before an Agent known only by name is
/// Working. One redraw (a resize, a clock, a short reply) is not work.
const ACTIVITY_WORKING_AFTER: Duration = Duration::from_millis(500);

/// A pause in output longer than this starts a new burst.
const ACTIVITY_BURST_GAP: Duration = Duration::from_secs(1);

/// How long its terminal must be still before such an Agent is Idle. Long
/// enough to ride out a model's silent wait between streamed chunks.
const ACTIVITY_IDLE_AFTER: Duration = Duration::from_secs(3);

/// Output this soon after a keystroke is its echo, not the Agent's work.
const ACTIVITY_ECHO: Duration = Duration::from_millis(250);

/// What output-activity reading remembers. See [`StatusReducer::lend_to_activity`].
#[derive(Debug, Default)]
struct ActivityRead {
    burst_since: Option<SystemTime>,
    output_at: Option<SystemTime>,
    typed_at: Option<SystemTime>,
    /// The user submitted something since the last turn: the next stretch
    /// of work is a turn whose end is worth announcing.
    asked: bool,
}

fn elapsed(since: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(since).unwrap_or_default()
}

impl StatusReducer {
    pub fn new(authority: Authority, spawned_at: SystemTime) -> Self {
        Self {
            attention: Default::default(),
            status: SessionStatus::Starting,
            authority,
            timing: ReducerTiming::default(),
            state: InternalState::new(spawned_at),
            manifest_id: None,
            manifest_version: None,
            evidence: None,
            lent_from: None,
            program_active: false,
            program_screen_choices: None,
            activity: None,
            program_reporters: None,
        }
    }

    /// What the catalog's Agents mean by their `OSC 7501` reports.
    pub fn with_program_reporters(mut self, reporters: Arc<ProgramReporters>) -> Self {
        self.program_reporters = Some(reporters);
        self
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

    /// The Agent manifest a shell's foreground program is being read with.
    pub fn foreground_agent(&self) -> Option<&str> {
        self.lent_from.as_ref().and(self.manifest_id.as_deref())
    }

    /// Hands a shell's status to an Agent the user started inside it.
    ///
    /// An Agent typed at a shell prompt gets none of the launch-time wiring
    /// (no hooks, no notify command), so only its screen can say what it is
    /// doing: while it holds the foreground the reducer reads that Agent's
    /// screen rules as a screen-primary Agent would. It starts Idle and with
    /// no turn in flight, so recognising an Agent never announces a finished
    /// turn it did not run.
    pub fn lend_to_agent(
        &mut self,
        manifest_id: &str,
        manifest_version: Option<&str>,
        now: SystemTime,
    ) -> ReducerOutcome {
        let mut outcome = ReducerOutcome::default();
        if matches!(self.status, SessionStatus::Exited(_))
            || (self.lent_from.is_some() && self.manifest_id.as_deref() == Some(manifest_id))
        {
            return outcome;
        }
        if self.lent_from.is_none() {
            self.lent_from = Some((
                self.authority,
                self.manifest_id.take(),
                self.manifest_version.take(),
            ));
        }
        // A shell question the Agent's arrival interrupted is over.
        outcome.line_prompt_ended = matches!(self.status, SessionStatus::NeedsInput(_));
        self.authority = Authority::ScreenPrimary;
        self.manifest_id = Some(manifest_id.to_owned());
        self.manifest_version = manifest_version.map(str::to_owned);
        self.activity = None;
        self.forget_screen(now);
        self.set_status(SessionStatus::Idle, &mut outcome);
        self.publish_evidence(
            StatusEvidenceSource::ProcessLiveness,
            None,
            None,
            now,
            &mut outcome,
        );
        outcome
    }

    /// Hands a shell's status to an Agent recognised by its program name
    /// alone, with no manifest whose screen rules could read it.
    ///
    /// Its status follows the terminal's output instead: output that keeps
    /// coming for [`ACTIVITY_WORKING_AFTER`] is Working, and a terminal still
    /// for [`ACTIVITY_IDLE_AFTER`] is Idle. The echo of typing is not work,
    /// and only work that follows a submission is a turn whose end is
    /// announced. It never reads a question or a permission prompt.
    pub fn lend_to_activity(&mut self, agent_id: &str, now: SystemTime) -> ReducerOutcome {
        if self.activity.is_some() && self.foreground_agent() == Some(agent_id) {
            return ReducerOutcome::default();
        }
        let outcome = self.lend_to_agent(agent_id, None, now);
        if self.foreground_agent() == Some(agent_id) {
            self.activity = Some(ActivityRead::default());
        }
        outcome
    }

    /// Returns a lent reducer to its shell, whose job state `running` is.
    pub fn return_from_agent(&mut self, running: bool, now: SystemTime) -> ReducerOutcome {
        let mut outcome = ReducerOutcome::default();
        let Some((authority, manifest_id, manifest_version)) = self.lent_from.take() else {
            return outcome;
        };
        self.activity = None;
        self.authority = authority;
        self.manifest_id = manifest_id;
        self.manifest_version = manifest_version;
        if matches!(self.status, SessionStatus::Exited(_)) {
            return outcome;
        }
        self.forget_screen(now);
        self.state.pending_needs_input = None;
        self.apply_shell_job(running, now, &mut outcome);
        outcome
    }

    /// Drops every belief read from a screen, keeping the session's clock.
    fn forget_screen(&mut self, now: SystemTime) {
        let spawned_at = now
            .checked_sub(self.timing.startup_grace)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut state = InternalState::new(spawned_at);
        state.transport_unavailable = self.state.transport_unavailable;
        state.last_signal_at = now;
        self.state = state;
    }

    pub fn with_attention_path(self, path: &std::path::Path) -> Self {
        self.with_attention_storage(path, false)
    }

    pub(crate) fn with_attention_storage(mut self, path: &std::path::Path, fresh: bool) -> Self {
        self.attention = crate::attention::AttentionLifecycle::open(path);
        if fresh {
            self.attention.start_incarnation();
        }
        if let Some(state) = self.attention.snapshot() {
            if let Some(request) = state.active_requests().find(|event| event.blocking) {
                if let Some(detail) = &request.detail {
                    self.status = SessionStatus::NeedsInput(detail.kind);
                    self.state.pending_needs_input = Some(detail.clone());
                }
            } else if state.working {
                self.status = SessionStatus::Working;
                self.state.turn_in_flight = true;
                self.state.hook_turn_in_flight = self.authority == Authority::HooksPrimary;
            } else if state.sequence > 0 {
                self.status = SessionStatus::Idle;
            }
        }
        self
    }

    pub fn attention_state(&self) -> Option<&diri_proto::attention::AttentionState> {
        self.attention.snapshot()
    }

    pub fn reduce_identified(
        &mut self,
        signal: StatusSignal,
        mut identity: crate::attention::SignalIdentity,
        now: SystemTime,
    ) -> ReducerOutcome {
        if let StatusSignal::ClaudeHook {
            hook: ClaudeHook::PermissionRequest { tool_name, .. },
            is_subagent: false,
            ..
        } = &signal
        {
            self.attention
                .correlate_request(&mut identity, tool_name.as_deref());
        }
        if self.attention.duplicate(&identity) {
            return ReducerOutcome {
                attention_changed: self.attention.snapshot().is_none(),
                ..Default::default()
            };
        }
        let mut evidence = crate::attention::Evidence::from(&signal);
        let mut outcome = self.reduce_status(signal, now);
        evidence.completion &=
            self.state.idle_strong || outcome.turn_completed || self.status == SessionStatus::Idle;
        let mut identity = identity;
        if !evidence.completion && !outcome.turn_completed {
            identity.completion = None;
        }
        outcome.attention_changed = self.attention.observe(&evidence, &identity, &outcome, now);
        outcome
    }

    /// Folds one signal into the session's status.
    pub fn reduce(&mut self, signal: StatusSignal, now: SystemTime) -> ReducerOutcome {
        self.reduce_identified(signal, Default::default(), now)
    }

    fn reduce_status(&mut self, signal: StatusSignal, now: SystemTime) -> ReducerOutcome {
        let mut outcome = ReducerOutcome::default();

        // Exited is absorbing: once dead, nothing changes it.
        if matches!(self.status, SessionStatus::Exited(_)) {
            return outcome;
        }

        // Process exit is authoritative under every authority mode.
        if let StatusSignal::ProcessExit {
            code,
            signal,
            interrupted,
        } = &signal
        {
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
                    system_restart: false,
                    interrupted: *interrupted,
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

        if matches!(signal, StatusSignal::TransportUnavailable) {
            self.state.transport_unavailable = true;
            self.cancel_idle_candidacy();
            self.set_status(SessionStatus::Unknown, &mut outcome);
            self.publish_evidence(
                StatusEvidenceSource::Transport,
                None,
                Some(StatusFallbackReason::TransportUnavailable),
                now,
                &mut outcome,
            );
            return outcome;
        }
        if self.state.transport_unavailable {
            return outcome;
        }

        if let StatusSignal::ProgramStatus(report) = signal {
            self.apply_program_status(report, None, now, &mut outcome);
            return outcome;
        }
        if let StatusSignal::ProgramStatusRemoved(reason) = signal {
            self.apply_program_status(None, Some(reason), now, &mut outcome);
            return outcome;
        }
        if self.program_active {
            // Inference, and the staleness that doubts it, wait while the
            // program says what it is doing. A long quiet build is working.
            //
            // So do an agent's own hooks and notify events where they would
            // only move it between working and idle: a CLI that reports
            // program status and still runs hooks would otherwise race the
            // two, and the report is the one the program keeps current. A
            // hook that raises a blocker still applies, for it names the
            // tool and command a report's message may not, and a report that
            // follows keeps that detail.
            match &signal {
                StatusSignal::Screen(observation) => {
                    self.note_screen_while_program_reports(observation, &mut outcome);
                    self.state.last_signal_at = now;
                    return outcome;
                }
                StatusSignal::ForegroundJob { .. }
                | StatusSignal::TerminalLine(_)
                | StatusSignal::Tick
                | StatusSignal::CodexTurnComplete
                | StatusSignal::CursorTranscriptWorking { .. }
                | StatusSignal::CursorTranscriptIdle { .. } => {
                    self.state.last_signal_at = now;
                    return outcome;
                }
                StatusSignal::ClaudeHook {
                    hook,
                    is_subagent,
                    pending_work,
                } if *is_subagent
                    || !self.hook_applies_while_program_reports(hook, *pending_work) =>
                {
                    self.state.last_signal_at = now;
                    match hook {
                        ClaudeHook::SubagentStart(id) => {
                            self.state.active_subagents.insert(id.clone());
                        }
                        ClaudeHook::SubagentStop(id) => {
                            self.state.active_subagents.remove(id);
                        }
                        _ if !is_subagent => {
                            if let Some(pending) = pending_work {
                                self.state.claude_pending_work = *pending;
                            }
                        }
                        _ => {}
                    }
                    return outcome;
                }
                _ => {}
            }
        }

        if self.activity.is_some() {
            self.reduce_activity(signal, now, &mut outcome);
            return outcome;
        }

        // processOnly: starting → working on first output, then only exit
        // moves it. A shell is still process-only, but an idle login prompt
        // is not work: Working follows the foreground process group.
        if self.authority == Authority::ProcessOnly {
            self.reduce_process_only(signal, now, &mut outcome);
            return outcome;
        }

        let evidence_hint = match &signal {
            StatusSignal::ClaudeHook { .. } => Some((StatusEvidenceSource::Hook, None, None)),
            StatusSignal::CodexTurnComplete | StatusSignal::CursorTranscriptIdle { .. } => {
                Some((StatusEvidenceSource::Notify, None, None))
            }
            StatusSignal::CursorTranscriptWorking { .. } => {
                Some((StatusEvidenceSource::Hook, None, None))
            }
            StatusSignal::Screen(observation) => Some((
                StatusEvidenceSource::ScreenRule,
                Some(observation.matched_rule_id.clone()),
                None,
            )),
            StatusSignal::Tick => None,
            StatusSignal::PtyOutputActivity
            | StatusSignal::UserKeystroke
            | StatusSignal::UserSubmission
            | StatusSignal::ForegroundJob { .. }
            | StatusSignal::TerminalLine(_)
            | StatusSignal::ProcessExit { .. }
            | StatusSignal::TransportUnavailable
            | StatusSignal::ProgramStatus(_)
            | StatusSignal::ProgramStatusRemoved(_) => None,
        };

        match signal {
            StatusSignal::ProcessExit { .. }
            | StatusSignal::TransportUnavailable
            | StatusSignal::ProgramStatus(_)
            | StatusSignal::ProgramStatusRemoved(_) => {} // handled above
            StatusSignal::ForegroundJob { .. } | StatusSignal::TerminalLine(_) => {}
            StatusSignal::PtyOutputActivity => {
                // Bytes alone do not establish work: late terminal repaints,
                // title updates and status lines continue after a turn ends.
                self.state.last_signal_at = now;
            }
            StatusSignal::UserKeystroke | StatusSignal::UserSubmission => {
                self.state.last_signal_at = now;
                self.state.hold_idle_against_screen = false;
                if matches!(signal, StatusSignal::UserSubmission) {
                    // The composer and transcript tail on screen still belong
                    // to the previous turn. Neither may finish this one.
                    self.state.submitted_at = Some(now);
                    self.state.cursor_stale_transcript_bytes = self.state.cursor_transcript_bytes;
                    if self.state.last_stale_after_submit {
                        self.cancel_idle_candidacy();
                    }
                }
                if matches!(self.status, SessionStatus::NeedsInput(_)) {
                    self.state.responding_since = Some(now);
                }
            }
            StatusSignal::ClaudeHook {
                hook,
                is_subagent,
                pending_work,
            } => self.handle_claude_hook(hook, is_subagent, pending_work, now, &mut outcome),
            StatusSignal::CodexTurnComplete => {
                self.state.last_signal_at = now;
                self.handle_strong_idle(now, &mut outcome);
            }
            StatusSignal::CursorTranscriptWorking { bytes } => {
                self.note_cursor_transcript(bytes);
                if matches!(self.status, SessionStatus::NeedsInput(_)) {
                    return outcome;
                }
                self.go_working(now, false, &mut outcome);
            }
            StatusSignal::CursorTranscriptIdle { bytes } => {
                self.state.last_signal_at = now;
                self.note_cursor_transcript(bytes);
                // A submit can race the previous turn's tail. Until the file
                // grows, that idle belongs to the turn that just ended.
                let previous_turn = self.state.cursor_stale_transcript_bytes == Some(bytes);
                // The poll can still see the preceding turn's transcript while
                // Cursor streams the next one. Its live spinner is stronger
                // evidence than that tail. Keep the transcript fallback for OSC
                // titles, which older Cursor versions can leave stale.
                let live_spinner = self.state.screen_belief == Some(ManifestState::Working)
                    && self.state.last_turn_started;
                if self.status == SessionStatus::Working && !previous_turn && !live_spinner {
                    self.state.submitted_at = None;
                    self.state.hold_idle_against_screen = true;
                    self.handle_strong_idle(now, &mut outcome);
                }
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

    fn reduce_activity(
        &mut self,
        signal: StatusSignal,
        now: SystemTime,
        outcome: &mut ReducerOutcome,
    ) {
        let Some(read) = self.activity.as_mut() else {
            return;
        };
        match signal {
            StatusSignal::UserKeystroke => read.typed_at = Some(now),
            StatusSignal::UserSubmission => {
                read.typed_at = Some(now);
                read.asked = true;
            }
            StatusSignal::PtyOutputActivity => {
                if read
                    .typed_at
                    .is_some_and(|typed_at| elapsed(typed_at, now) < ACTIVITY_ECHO)
                {
                    return;
                }
                if read
                    .output_at
                    .is_none_or(|output_at| elapsed(output_at, now) >= ACTIVITY_BURST_GAP)
                {
                    read.burst_since = Some(now);
                }
                read.output_at = Some(now);
                let sustained = read
                    .burst_since
                    .is_some_and(|since| elapsed(since, now) >= ACTIVITY_WORKING_AFTER);
                if sustained && self.status != SessionStatus::Working {
                    self.state.turn_in_flight = std::mem::take(&mut read.asked);
                    self.state.last_signal_at = now;
                    self.set_status(SessionStatus::Working, outcome);
                }
            }
            StatusSignal::Tick => {
                let still = read
                    .output_at
                    .is_none_or(|output_at| elapsed(output_at, now) >= ACTIVITY_IDLE_AFTER);
                if self.status == SessionStatus::Working && still {
                    self.state.pending_turn_completed = self.state.turn_in_flight;
                    self.commit_idle(now, outcome);
                }
            }
            _ => return,
        }
        self.publish_evidence(
            StatusEvidenceSource::ProcessLiveness,
            None,
            Some(StatusFallbackReason::ProcessOnly),
            now,
            outcome,
        );
    }

    fn reduce_process_only(
        &mut self,
        signal: StatusSignal,
        now: SystemTime,
        outcome: &mut ReducerOutcome,
    ) {
        match signal {
            StatusSignal::ForegroundJob { running } if self.tracks_shell_jobs() => {
                self.state.last_signal_at = now;
                if running && matches!(self.status, SessionStatus::NeedsInput(_)) {
                    // The job is still there, still waiting on its line.
                    return;
                }
                outcome.line_prompt_ended = matches!(self.status, SessionStatus::NeedsInput(_));
                self.apply_shell_job(running, now, outcome);
            }
            StatusSignal::TerminalLine(prompt) if self.tracks_shell_jobs() => {
                self.apply_line_prompt(prompt, now, outcome);
            }
            StatusSignal::UserKeystroke | StatusSignal::UserSubmission
                if self.tracks_shell_jobs() =>
            {
                self.state.terminal_active_at = now;
                // The user is answering. The request it raised stays open
                // until the job stops reading (or Enter is pressed, which the
                // attention lifecycle sees), so a pause mid-answer marks the
                // terminal again without announcing a second question.
                if matches!(self.status, SessionStatus::NeedsInput(_)) {
                    self.state.pending_needs_input = None;
                    self.set_status(SessionStatus::Working, outcome);
                    self.publish_evidence(
                        StatusEvidenceSource::ProcessLiveness,
                        None,
                        Some(StatusFallbackReason::ProcessOnly),
                        now,
                        outcome,
                    );
                }
            }
            StatusSignal::PtyOutputActivity => {
                self.state.last_signal_at = now;
                self.state.terminal_active_at = now;
                if self.tracks_shell_jobs() {
                    // Prompt output is not a job. Drop Starting so an older
                    // Helper that never sends ForegroundJob cannot sit on
                    // Loading forever; a later job sample still wins.
                    if self.status == SessionStatus::Starting {
                        self.set_status(SessionStatus::Idle, outcome);
                    }
                } else if self.status == SessionStatus::Starting {
                    self.state.turn_in_flight = true;
                    self.set_status(SessionStatus::Working, outcome);
                }
                self.publish_evidence(
                    StatusEvidenceSource::ProcessLiveness,
                    None,
                    Some(StatusFallbackReason::ProcessOnly),
                    now,
                    outcome,
                );
            }
            _ => {}
        }
    }

    /// Follows an `OSC 7501` report. A result (`done`, `error`) ends the turn
    /// once. `idle` ends it without a result, unless the program's manifest
    /// says its `idle` is its result ([`ProgramReporters`]). `None` hands the
    /// session back to inference; `removal` says why, when known.
    fn apply_program_status(
        &mut self,
        report: Option<ProgramRecord>,
        removal: Option<ProgramRemoval>,
        now: SystemTime,
        outcome: &mut ReducerOutcome,
    ) {
        let was_active = std::mem::replace(&mut self.program_active, report.is_some());
        if report.is_none() {
            self.program_screen_choices = None;
        }
        self.state.last_signal_at = now;
        let evidence_rule = match (&report, removal) {
            (Some(record), _) => Some(program_state_name(record.state).to_owned()),
            (None, Some(reason)) => Some(format!("removed:{}", reason.as_str())),
            (None, None) => Some("removed".to_owned()),
        };
        match report {
            Some(record) if record.state == ProgramState::Blocked => {
                let kind = match record.kind {
                    Some(BlockedKind::Permission) => NeedsInputKind::Permission,
                    Some(BlockedKind::Auth) => NeedsInputKind::Auth,
                    Some(BlockedKind::Question) | None => NeedsInputKind::Question,
                };
                let hook_detail = self
                    .state
                    .pending_needs_input
                    .as_ref()
                    .filter(|pending| {
                        self.status == SessionStatus::NeedsInput(kind)
                            && pending.kind == kind
                            && matches!(
                                pending.source,
                                NeedsInputSource::ClaudePermissionHook
                                    | NeedsInputSource::ClaudeNotificationHook
                                    | NeedsInputSource::CodexNotify
                            )
                    })
                    .cloned();
                let mut detail = hook_detail.unwrap_or_else(|| program_detail(&record, kind, now));
                if detail.options.is_none()
                    && let Some((shown, options)) = &self.program_screen_choices
                    && *shown == kind
                {
                    detail.options = Some(options.clone());
                }
                self.cancel_idle_candidacy();
                self.state.hold_idle_against_screen = false;
                self.state.turn_in_flight = true;
                let unchanged = self.status == SessionStatus::NeedsInput(kind)
                    && self
                        .state
                        .pending_needs_input
                        .as_ref()
                        .is_some_and(|pending| {
                            pending.source == detail.source
                                && pending.summary == detail.summary
                                && pending.prompt_excerpt == detail.prompt_excerpt
                                && pending.options == detail.options
                        });
                if !unchanged {
                    self.state.pending_needs_input = Some(detail.clone());
                    outcome.needs_input = Some(detail);
                }
                self.set_status(SessionStatus::NeedsInput(kind), outcome);
            }
            Some(record) if record.state == ProgramState::Working => {
                if self.asking_at_turn_end() {
                    // The turn ended on a question; the work the program
                    // still reports runs in the background. Only a new turn,
                    // a result or another blocker ends the question.
                    return;
                }
                self.state.pending_needs_input = None;
                self.go_working(now, false, outcome);
            }
            Some(record) => {
                // `done` and `error` are the turn's result. `idle` is the
                // program waiting with none, after the user cancelled or
                // denied, unless this program reports `idle` for every
                // finished turn.
                let result = match record.state {
                    ProgramState::Done | ProgramState::Error => true,
                    _ => self.program_reporters.as_ref().is_some_and(|reporters| {
                        reporters.idle_ends_turn_for(&record, self.manifest_id.as_deref())
                    }),
                };
                self.state.pending_needs_input = None;
                self.state.screen_blocker_active = false;
                self.state.blocker_miss_scans = 0;
                self.state.blocker_miss_since = None;
                match self.status {
                    SessionStatus::Working
                    | SessionStatus::NeedsInput(_)
                    | SessionStatus::Unknown => {
                        // A spinner frame still on screen must not undo it.
                        self.state.hold_idle_against_screen = true;
                        let in_flight = self.state.turn_in_flight;
                        if result {
                            self.state.pending_turn_completed = in_flight;
                            outcome.turn_failed = in_flight && record.state == ProgramState::Error;
                        } else {
                            self.state.pending_turn_completed = false;
                            outcome.turn_abandoned = in_flight;
                        }
                        self.commit_idle(now, outcome);
                    }
                    SessionStatus::Starting => self.set_status(SessionStatus::Idle, outcome),
                    SessionStatus::Idle | SessionStatus::Exited(_) => {}
                }
            }
            None if was_active => {
                // Cleared, or its program ended. Whatever turn it reported is
                // over without a result, and inference takes over from here.
                self.state.pending_needs_input = None;
                self.cancel_idle_candidacy();
                self.state.last_screen_seq = None;
                let next = if self.authority == Authority::ProcessOnly && !self.tracks_shell_jobs()
                {
                    SessionStatus::Working
                } else {
                    self.state.turn_in_flight = false;
                    SessionStatus::Idle
                };
                self.set_status(next, outcome);
            }
            None => return,
        }
        self.publish_evidence(
            StatusEvidenceSource::ProgramStatus,
            evidence_rule,
            None,
            now,
            outcome,
        );
    }

    /// While a program reports, the screen only lends a blocker its
    /// choices: the record says the program is waiting, the screen what it
    /// offers. Remembered when they render before the report arrives.
    fn note_screen_while_program_reports(
        &mut self,
        observation: &ScreenObservation,
        outcome: &mut ReducerOutcome,
    ) {
        self.program_screen_choices = needs_input_kind(observation.state)
            .zip(observation.options.clone())
            .filter(|(_, options)| !options.is_empty());
        let (SessionStatus::NeedsInput(kind), Some(pending), Some((shown, options))) = (
            &self.status,
            self.state.pending_needs_input.as_mut(),
            &self.program_screen_choices,
        ) else {
            return;
        };
        if kind == shown && pending.kind == *kind && pending.options.is_none() {
            pending.options = Some(options.clone());
            outcome.needs_input = Some(pending.clone());
        }
    }

    fn tracks_shell_jobs(&self) -> bool {
        self.manifest_id.as_deref() == Some("shell")
    }

    /// Whether a [`StatusSignal::Tick`] could change anything now. When it
    /// cannot, a quiet session's pump may sleep until the next real signal:
    /// only a turn in progress, startup grace, a submit's chrome hold or a
    /// fading blocker run on the clock.
    #[must_use]
    pub fn wants_tick(&self) -> bool {
        matches!(
            self.status,
            SessionStatus::Working | SessionStatus::Starting
        ) || self.state.submitted_at.is_some()
            || (self.state.screen_blocker_active && self.state.blocker_miss_since.is_some())
    }

    /// Whether the session should ask its PTY owner if the shell's job is
    /// waiting on a line: a job has run with its terminal still for the
    /// settle, or the question it asked is still up. Asking costs a walk of
    /// the job's processes, so a streaming or idle terminal never asks.
    pub fn wants_line_probe(&self, now: SystemTime) -> bool {
        if !self.tracks_shell_jobs() || self.lent_from.is_some() {
            return false;
        }
        match self.status {
            SessionStatus::Working => {
                now.duration_since(self.state.terminal_active_at)
                    .unwrap_or_default()
                    >= self.timing.line_prompt_settle
            }
            SessionStatus::NeedsInput(_) => true,
            _ => false,
        }
    }

    fn apply_line_prompt(
        &mut self,
        prompt: Option<TerminalPrompt>,
        now: SystemTime,
        outcome: &mut ReducerOutcome,
    ) {
        let waiting = matches!(self.status, SessionStatus::NeedsInput(_));
        let Some(prompt) = prompt else {
            if waiting {
                // Read, interrupted, or timed out without a keystroke here.
                outcome.line_prompt_ended = true;
                self.state.pending_needs_input = None;
                self.set_status(SessionStatus::Working, outcome);
                self.publish_evidence(
                    StatusEvidenceSource::ProcessLiveness,
                    None,
                    Some(StatusFallbackReason::ProcessOnly),
                    now,
                    outcome,
                );
            }
            return;
        };
        let settled = now
            .duration_since(self.state.terminal_active_at)
            .unwrap_or_default()
            >= self.timing.line_prompt_settle;
        let asking = self.status == SessionStatus::Working && settled;
        if !(waiting || asking) {
            return;
        }
        let detail = line_prompt_detail(&prompt, now);
        // Occurrence time is not news: re-sampling the same question must
        // not rewrite the record every tick.
        let unchanged = self
            .state
            .pending_needs_input
            .as_ref()
            .is_some_and(|pending| {
                pending.summary == detail.summary
                    && pending.prompt_excerpt == detail.prompt_excerpt
                    && pending.secret == detail.secret
            });
        if waiting && unchanged {
            return;
        }
        self.state.pending_needs_input = Some(detail.clone());
        outcome.needs_input = Some(detail);
        self.set_status(SessionStatus::NeedsInput(NeedsInputKind::Question), outcome);
        self.publish_evidence(
            StatusEvidenceSource::ProcessLiveness,
            None,
            Some(StatusFallbackReason::ProcessOnly),
            now,
            outcome,
        );
    }

    fn apply_shell_job(&mut self, running: bool, now: SystemTime, outcome: &mut ReducerOutcome) {
        let next = if running {
            self.state.turn_in_flight = true;
            SessionStatus::Working
        } else {
            self.state.turn_in_flight = false;
            SessionStatus::Idle
        };
        self.set_status(next, outcome);
        self.publish_evidence(
            StatusEvidenceSource::ProcessLiveness,
            None,
            Some(StatusFallbackReason::ProcessOnly),
            now,
            outcome,
        );
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

    fn note_cursor_transcript(&mut self, bytes: u64) {
        self.state.cursor_transcript_bytes = Some(bytes);
        if self.state.cursor_stale_transcript_bytes != Some(bytes) {
            self.state.cursor_stale_transcript_bytes = None;
        }
    }

    /// Whether the screen on display may be the composer left over from
    /// before the last submit.
    fn showing_submit_chrome(&self, now: SystemTime) -> bool {
        self.state.last_stale_after_submit
            && self.state.submitted_at.is_some_and(|at| {
                now.duration_since(at).unwrap_or_default() < self.timing.submit_chrome_hold
            })
    }

    /// Enter or refresh `working` from a positive work signal. For
    /// hooks-primary agents a work hook also clears a stale on-screen blocker.
    fn go_working(
        &mut self,
        now: SystemTime,
        clear_screen_blocker: bool,
        outcome: &mut ReducerOutcome,
    ) {
        self.state.hold_idle_against_screen = false;
        self.cancel_idle_candidacy();
        if clear_screen_blocker {
            self.state.screen_blocker_active = false;
            self.state.blocker_miss_scans = 0;
            self.state.blocker_miss_since = None;
        }
        self.state.turn_in_flight = true;
        if clear_screen_blocker {
            self.state.hook_turn_in_flight = true;
        }
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
        if !matches!(
            self.status,
            SessionStatus::Working | SessionStatus::NeedsInput(_) | SessionStatus::Unknown
        ) {
            return;
        }
        self.state.screen_blocker_active = false;
        self.state.blocker_miss_scans = 0;
        self.state.blocker_miss_since = None;
        self.state.pending_needs_input = None;
        self.state.hold_idle_against_screen = true;
        self.state.idle_strong = true;
        self.state.pending_turn_completed = self.state.turn_in_flight;
        if self.state.idle_candidate_since.is_none() {
            self.state.idle_candidate_since = Some(now);
        }
        if self.state.screen_belief == Some(ManifestState::Idle)
            || self.status != SessionStatus::Working
        {
            self.state.idle_confirms += 1;
            self.commit_idle(now, outcome);
        }
    }

    /// Register one idle-confirming observation.
    fn confirm_idle(&mut self, now: SystemTime, outcome: &mut ReducerOutcome) {
        if self.status != SessionStatus::Working
            || (self.authority == Authority::HooksPrimary
                && self.state.hook_turn_in_flight
                && !self.state.idle_strong)
        {
            return;
        }
        if self.state.idle_candidate_since.is_none() {
            self.state.idle_candidate_since = Some(now);
            self.state.idle_confirms = 0;
        }
        self.state.pending_turn_completed = self.state.turn_in_flight;
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
        self.state.hook_turn_in_flight = false;
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
        pending_work: Option<bool>,
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
        if let Some(pending) = pending_work {
            self.state.claude_pending_work = pending;
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
            ClaudeHook::PreToolUse | ClaudeHook::PostToolUse => self.go_working(now, true, outcome),
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
            } => self.handle_notification(notification_type, message, pending_work, now, outcome),
            ClaudeHook::Stop => {
                self.handle_claude_completion(pending_work.unwrap_or(false), now, outcome)
            }
            ClaudeHook::StopAsking { question } => {
                let pending = pending_work.unwrap_or(false);
                if pending && matches!(self.status, SessionStatus::Working | SessionStatus::Unknown)
                {
                    self.ask_at_turn_end(&question, now, outcome);
                } else {
                    self.handle_claude_completion(pending, now, outcome);
                }
            }
            // A hint only.
            ClaudeHook::SessionEnd => {}
            ClaudeHook::SubagentStart(_) | ClaudeHook::SubagentStop(_) => {}
        }
    }

    /// Whether the session waits on a question its last turn ended with.
    fn asking_at_turn_end(&self) -> bool {
        self.status == SessionStatus::NeedsInput(NeedsInputKind::Question)
            && self
                .state
                .pending_needs_input
                .as_ref()
                .is_some_and(|detail| detail.source == NeedsInputSource::ClaudeStopHook)
    }

    /// A hook that moves status even while a program reports its own: one
    /// raising a blocker, a turn ending on a question while background work
    /// keeps the program busy, and the next turn that ends such a question.
    fn hook_applies_while_program_reports(
        &self,
        hook: &ClaudeHook,
        pending_work: Option<bool>,
    ) -> bool {
        match hook {
            ClaudeHook::StopAsking { .. } => {
                pending_work == Some(true) && self.status == SessionStatus::Working
            }
            ClaudeHook::UserPromptSubmit | ClaudeHook::PreToolUse | ClaudeHook::PostToolUse => {
                self.asking_at_turn_end()
            }
            hook => raises_blocker(hook),
        }
    }

    /// The turn handed back to the user with a question, while background
    /// work it started keeps running. That work is why the session would
    /// otherwise read Working, hiding that the user is the one being waited
    /// on. A turn with nothing left in the background reads Done instead,
    /// as any finished turn does.
    fn ask_at_turn_end(&mut self, question: &str, now: SystemTime, outcome: &mut ReducerOutcome) {
        let detail = NeedsInputDetail {
            kind: NeedsInputKind::Question,
            source: NeedsInputSource::ClaudeStopHook,
            tool_name: None,
            summary: question.to_owned(),
            prompt_excerpt: Some(question.to_owned()),
            options: None,
            risk_hint: classify_risk(question),
            secret: false,
            occurred_at: now.into(),
        };
        self.state.claude_pending_work = true;
        self.cancel_idle_candidacy();
        self.state.hold_idle_against_screen = false;
        self.state.screen_blocker_active = false;
        self.state.blocker_miss_scans = 0;
        self.state.blocker_miss_since = None;
        self.state.pending_needs_input = Some(detail.clone());
        outcome.needs_input = Some(detail);
        self.set_status(SessionStatus::NeedsInput(NeedsInputKind::Question), outcome);
    }

    fn handle_claude_completion(
        &mut self,
        pending_work: bool,
        now: SystemTime,
        outcome: &mut ReducerOutcome,
    ) {
        self.state.claude_pending_work = pending_work;
        if pending_work && self.asking_at_turn_end() {
            // A reminder that background work is still running does not
            // answer the question the turn ended on.
            return;
        }
        if pending_work {
            // The foreground response ended, but Claude still has live work.
            // Keep screen idle and subsequent metadata-free reminders from
            // announcing completion until a fresh completion says it drained.
            self.go_working(now, true, outcome);
        } else {
            self.handle_strong_idle(now, outcome);
        }
    }

    fn handle_notification(
        &mut self,
        notification_type: Option<String>,
        message: Option<String>,
        pending_work: Option<bool>,
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
                    secret: false,
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
            // An idle reminder is not a question or an approval request.
            // It must not overwrite a completed turn or an actual blocker.
            // A guard would send an unmatched reminder to the arms below.
            #[allow(clippy::collapsible_match)]
            Some("idle_prompt") => {
                if pending_work == Some(true)
                    && !matches!(self.status, SessionStatus::NeedsInput(_))
                {
                    // A recovered reminder can be the first signal after a
                    // daemon restart. Its retained work fact still outranks
                    // the word "idle", without dismissing a live blocker.
                    self.handle_claude_completion(true, now, outcome);
                }
            }
            Some("agent_needs_input") | Some("elicitation_dialog") => {
                let text = message.unwrap_or_else(|| "Waiting for input".into());
                let detail = NeedsInputDetail {
                    kind: NeedsInputKind::Question,
                    source: NeedsInputSource::ClaudeNotificationHook,
                    tool_name: None,
                    summary: redact(&text),
                    prompt_excerpt: None,
                    options: None,
                    risk_hint: classify_risk(&text),
                    secret: false,
                    occurred_at: now.into(),
                };
                self.state.pending_needs_input = Some(detail.clone());
                outcome.needs_input = Some(detail);
                self.cancel_idle_candidacy();
                self.set_status(SessionStatus::NeedsInput(NeedsInputKind::Question), outcome);
            }
            Some("agent_completed") => self.handle_claude_completion(
                pending_work.unwrap_or(self.state.claude_pending_work),
                now,
                outcome,
            ),
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
            self.state.blocker_miss_scans = 0;
            self.state.blocker_miss_since = None;
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
        self.state.last_stale_after_submit = observation.stale_after_submit;
        self.state.last_turn_started = observation.turn_started;

        // A visible blocker beats everything except process exit.
        if let Some(kind) = needs_input_kind(observation.state) {
            // A blocker is this turn's own screen, so later idle is real.
            self.state.submitted_at = None;
            self.state.screen_blocker_active = true;
            self.state.blocker_miss_scans = 0;
            self.state.blocker_miss_since = None;
            let detail = screen_detail(kind, &observation, now);
            self.state.pending_needs_input = Some(detail.clone());
            outcome.needs_input = Some(detail);
            self.state.hold_idle_against_screen = false;
            self.cancel_idle_candidacy();
            self.set_status(SessionStatus::NeedsInput(kind), outcome);
            return;
        }

        // "Waiting for background agents" and the free composer are what a
        // turn that ended on a question looks like; neither answers it.
        if self.asking_at_turn_end() {
            return;
        }

        // A non-blocker observation while a blocker is active only releases it
        // after enough consecutive misses — one stray frame must not clear a
        // prompt the user is still looking at.
        if self.state.screen_blocker_active {
            self.state.blocker_miss_scans += 1;
            self.state.blocker_miss_since.get_or_insert(now);
            if self.state.blocker_miss_scans < self.timing.blocker_clear_scans {
                return;
            }
            self.state.screen_blocker_active = false;
            self.state.blocker_miss_scans = 0;
            self.state.blocker_miss_since = None;
            self.apply_non_blocker_screen(observation.state, now, true, outcome);
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

        self.apply_non_blocker_screen(observation.state, now, false, outcome);
    }

    fn apply_non_blocker_screen(
        &mut self,
        state: ManifestState,
        now: SystemTime,
        cleared_blocker: bool,
        outcome: &mut ReducerOutcome,
    ) {
        match state {
            ManifestState::Working => {
                if self.state.hold_idle_against_screen {
                    return;
                }
                // A `turn_started` screen (Cursor's Generating/Thinking line)
                // proves the submitted turn is running. A stale Working OSC
                // title does not: the composer below can still be the
                // previous turn.
                if self.state.last_turn_started {
                    self.state.submitted_at = None;
                }
                self.go_working(now, false, outcome);
            }
            ManifestState::Idle => {
                if self.status == SessionStatus::Working && self.showing_submit_chrome(now) {
                    return;
                }
                if !self.state.last_stale_after_submit {
                    // An idle screen the submit could not have left behind,
                    // such as a Ready title, is this turn ending.
                    self.state.submitted_at = None;
                }
                if self.status == SessionStatus::Working {
                    self.confirm_idle(now, outcome);
                } else if self.status == SessionStatus::Starting {
                    self.set_status(SessionStatus::Idle, outcome);
                } else if cleared_blocker && matches!(self.status, SessionStatus::NeedsInput(_)) {
                    if self.authority == Authority::HooksPrimary && self.state.hook_turn_in_flight {
                        // Dismissing a permission/question does not finish the
                        // turn whose tool call was waiting for that answer.
                        self.go_working(now, false, outcome);
                    } else {
                        self.cancel_idle_candidacy();
                        self.set_status(SessionStatus::Idle, outcome);
                    }
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

        // Like idle confirmation, blocker dismissal must not require an
        // extra redraw. Kimi can paint its composer once after trust and stay
        // quiet forever. Preserve the two-frame fast path, but let a stable
        // non-blocker belief clear after the existing debounce cap. A fresh
        // blocker or a skip screen cancels this timer.
        if self.state.screen_blocker_active
            && !self.state.skip_active
            && self.state.blocker_miss_since.is_some_and(|since| {
                now.duration_since(since).unwrap_or_default() >= self.timing.idle_confirm_cap
            })
            && let Some(state @ (ManifestState::Idle | ManifestState::Working)) =
                self.state.screen_belief
        {
            self.state.screen_blocker_active = false;
            self.state.blocker_miss_scans = 0;
            self.state.blocker_miss_since = None;
            self.apply_non_blocker_screen(state, now, true, outcome);
        }

        // Leftover composer chrome held back after a submit counts once the
        // hold expires, so a turn that showed no working screen still ends.
        if let Some(at) = self.state.submitted_at
            && now.duration_since(at).unwrap_or_default() >= self.timing.submit_chrome_hold
        {
            self.state.submitted_at = None;
            if self.status == SessionStatus::Working
                && !self.state.screen_blocker_active
                && !self.state.skip_active
                && self.state.last_stale_after_submit
                && self.state.screen_belief == Some(ManifestState::Idle)
            {
                self.confirm_idle(now, outcome);
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
        // A settled screen often stops emitting new content sequences. One
        // idle observation held for the debounce cap is enough; requiring
        // more redraws leaves quiet agents stuck Working forever.
        if self.status == SessionStatus::Working
            && self.state.screen_belief == Some(ManifestState::Idle)
            && !self.state.idle_strong
            && self.state.idle_candidate_since.is_some_and(|since| {
                now.duration_since(since).unwrap_or_default() >= self.timing.idle_confirm_cap
            })
        {
            self.commit_idle(now, outcome);
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

/// Whether the PTY foreground group is a job other than the session child.
/// `None` until both pids are known.
#[must_use]
pub fn foreground_job_running(child_pid: i32, foreground_pgid: Option<i32>) -> Option<bool> {
    if child_pid <= 1 {
        return None;
    }
    let pgid = foreground_pgid.filter(|pgid| *pgid > 0)?;
    Some(pgid != child_pid)
}

/// A hook that says the agent is waiting on the user.
fn raises_blocker(hook: &ClaudeHook) -> bool {
    match hook {
        ClaudeHook::PermissionRequest { .. } => true,
        ClaudeHook::Notification {
            notification_type, ..
        } => matches!(
            notification_type.as_deref(),
            Some("permission_prompt" | "agent_needs_input" | "elicitation_dialog")
        ),
        _ => false,
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
        secret: false,
        occurred_at: now.into(),
    }
}

/// The longest question a detail carries; the rest of a very long prompt
/// line is not what a notification needs to show.
const LINE_PROMPT_MAX_CHARS: usize = 160;

fn line_prompt_detail(prompt: &TerminalPrompt, now: SystemTime) -> NeedsInputDetail {
    let line = prompt
        .line
        .as_deref()
        .filter(|_| !prompt.secret)
        .map(|line| {
            let line = redact(line.trim());
            match line.char_indices().nth(LINE_PROMPT_MAX_CHARS) {
                Some((end, _)) => format!("{}…", &line[..end]),
                None => line,
            }
        })
        .filter(|line| !line.is_empty());
    let summary = if prompt.secret {
        "Waiting for a password".to_owned()
    } else {
        line.clone()
            .unwrap_or_else(|| "Waiting for input".to_owned())
    };
    NeedsInputDetail {
        kind: NeedsInputKind::Question,
        source: NeedsInputSource::TerminalLine,
        tool_name: None,
        risk_hint: line
            .as_deref()
            .map_or(diri_proto::RiskHint::Neutral, classify_risk),
        summary,
        prompt_excerpt: line,
        options: None,
        occurred_at: now.into(),
        secret: prompt.secret,
    }
}

/// The most a needs-input detail shows of a program's message.
const PROGRAM_MESSAGE_MAX_CHARS: usize = 400;

/// A report's state as the evidence names it, in the protocol's own words.
fn program_state_name(state: ProgramState) -> &'static str {
    match state {
        ProgramState::Idle => "idle",
        ProgramState::Done => "done",
        ProgramState::Error => "error",
        ProgramState::Working => "working",
        ProgramState::Blocked => "blocked",
    }
}

fn program_detail(
    record: &ProgramRecord,
    kind: NeedsInputKind,
    now: SystemTime,
) -> NeedsInputDetail {
    let message = record
        .msg
        .as_deref()
        .or(record.title.as_deref())
        .map(|text| {
            redact(text.trim())
                .chars()
                .take(PROGRAM_MESSAGE_MAX_CHARS)
                .collect::<String>()
        })
        .filter(|text| !text.is_empty());
    let summary = message.clone().unwrap_or_else(|| {
        match record.kind {
            Some(BlockedKind::Permission) => "Waiting for permission",
            Some(BlockedKind::Auth) => "Waiting for sign-in",
            Some(BlockedKind::Question) | None => "Waiting for input",
        }
        .to_owned()
    });
    NeedsInputDetail {
        kind,
        source: NeedsInputSource::ProgramStatus,
        // `app` names the program asking (`claude-code`), not the tool it
        // wants to run; the report has no field for that.
        tool_name: None,
        risk_hint: message
            .as_deref()
            .map_or(diri_proto::RiskHint::Neutral, classify_risk),
        summary,
        prompt_excerpt: message,
        options: None,
        secret: false,
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
        secret: false,
        occurred_at: now.into(),
    }
}

#[cfg(test)]
mod program_replay_tests;
#[cfg(test)]
mod tests;
