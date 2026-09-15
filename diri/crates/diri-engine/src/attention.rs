//! One owner for inferred wait episodes and native request identities.
//! The reducer supplies evidence; repaint text and wall-clock time never rearm.
use std::path::Path;
use std::time::SystemTime;

use diri_proto::SessionStatus;
use diri_proto::attention::{
    ATTENTION_VERSION, AttentionEvent, AttentionKind, AttentionState, EVENT_LIMIT,
};

use crate::status::{ClaudeHook, ReducerOutcome, StatusSignal};
use rusqlite::{Connection, OptionalExtension};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SignalIdentity {
    pub request: Option<String>,
    pub completion: Option<String>,
    pub resolved_request: Option<String>,
    pub started_tool: Option<(String, String)>,
}

impl SignalIdentity {
    fn receipts(&self) -> [(&'static str, Option<&String>); 4] {
        [
            ("request", self.request.as_ref()),
            ("completion", self.completion.as_ref()),
            ("tool", self.started_tool.as_ref().map(|(id, _)| id)),
            ("resolution", self.resolved_request.as_ref()),
        ]
    }
}

#[derive(Default)]
pub(crate) struct Evidence {
    pub responding: bool,
    pub submitted: bool,
    pub continuation: bool,
    pub completion: bool,
    pub queued: bool,
    pub subagent: bool,
}
impl From<&StatusSignal> for Evidence {
    fn from(signal: &StatusSignal) -> Self {
        Self {
            responding: matches!(signal, StatusSignal::UserSubmission),
            submitted: matches!(
                signal,
                StatusSignal::ClaudeHook {
                    hook: ClaudeHook::UserPromptSubmit,
                    ..
                }
            ),
            continuation: matches!(
                signal,
                StatusSignal::ClaudeHook {
                    hook: ClaudeHook::PreToolUse | ClaudeHook::PostToolUse,
                    ..
                } | StatusSignal::CursorTranscriptWorking
            ),
            completion: matches!(
                signal,
                StatusSignal::CodexTurnComplete
                    | StatusSignal::CursorTranscriptIdle
                    | StatusSignal::ClaudeHook {
                        hook: ClaudeHook::Stop,
                        pending_work: None | Some(false),
                        ..
                    }
            ),
            queued: matches!(signal, StatusSignal::Screen(observation) if observation.matched_rule_id == "queued-follow-up-question"),
            subagent: matches!(
                signal,
                StatusSignal::ClaudeHook {
                    is_subagent: true,
                    ..
                }
            ),
        }
    }
}

pub struct AttentionLifecycle {
    state: AttentionState,
    storage: Option<Connection>,
    healthy: bool,
    /// A key only permits rearming after observed continuation, never alone.
    responding: bool,
    ephemeral_receipts: std::collections::HashSet<(String, String)>,
}

impl Default for AttentionLifecycle {
    fn default() -> Self {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).expect("OS randomness for attention identity");
        Self {
            state: AttentionState {
                version: ATTENTION_VERSION,
                epoch: bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
                sequence: 0,
                turn: 0,
                working: false,
                last_native_completion: None,
                observed_at: None,
                active_tools: Default::default(),
                events: Vec::new(),
                native_requests: Default::default(),
                native_completions: Default::default(),
            },
            storage: None,
            healthy: true,
            responding: false,
            ephemeral_receipts: Default::default(),
        }
    }
}

impl AttentionLifecycle {
    /// Only an actual process launch creates a new namespace. Adoption retains it.
    /// Keep native receipts so delayed hooks from a previous process cannot replay.
    pub fn start_incarnation(&mut self) {
        self.state = Self::default().state;
        if !self.healthy {
            return;
        }
        if let Some(db) = &mut self.storage {
            let result = serde_json::to_string(&self.state)
                .map_err(std::io::Error::other)
                .and_then(|body| {
                    db.execute("INSERT OR REPLACE INTO state VALUES (1, ?1)", [body])
                        .map_err(std::io::Error::other)
                });
            if result.is_err() {
                self.healthy = false;
            }
        }
    }
    pub fn open(path: &Path) -> Self {
        let mut lifecycle = Self::default();
        let path = path.with_extension("sqlite");
        let result = (|| -> Result<_, Box<dyn std::error::Error>> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(&path)?;
            let db = Connection::open(path)?;
            db.busy_timeout(std::time::Duration::from_secs(1))?;
            let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if version > 1 {
                return Err("unsupported attention database".into());
            }
            db.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS state(id INTEGER PRIMARY KEY CHECK(id=1), body TEXT NOT NULL); CREATE TABLE IF NOT EXISTS identity(kind TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY(kind,id)); PRAGMA user_version=1;")?;
            let body: Option<String> = db
                .query_row("SELECT body FROM state WHERE id=1", [], |row| row.get(0))
                .optional()?;
            if let Some(body) = body {
                if body.len() > 1024 * 1024 {
                    return Err("attention state too large".into());
                }
                let state: AttentionState = serde_json::from_str(&body)?;
                if state.version != ATTENTION_VERSION {
                    return Err("unsupported attention state".into());
                }
                lifecycle.state = state;
            }
            Ok(db)
        })();
        match result {
            Ok(db) => lifecycle.storage = Some(db),
            Err(_) => {
                lifecycle.healthy = false;
                eprintln!("diri: attention storage unavailable; session alerts suppressed");
            }
        }
        lifecycle
    }

    pub fn snapshot(&self) -> Option<&AttentionState> {
        self.healthy.then_some(&self.state)
    }

    /// Claude PermissionRequest currently omits tool_use_id. Correlate only
    /// when exactly one unfinished native tool of that name exists. Parallel
    /// ambiguous tools keep the conservative inferred wait identity.
    pub fn correlate_request(&self, identity: &mut SignalIdentity, tool: Option<&str>) {
        if identity.request.is_some() {
            return;
        }
        let Some(tool) = tool else {
            return;
        };
        let mut candidates = self
            .state
            .active_tools
            .iter()
            .filter(|(_, name)| name.as_str() == tool);
        if let Some((id, _)) = candidates.next()
            && candidates.next().is_none()
        {
            identity.request = Some(id.clone());
        }
    }

    pub fn duplicate(&mut self, identity: &SignalIdentity) -> bool {
        for (kind, id) in identity.receipts() {
            if let Some(id) = id {
                if let Some(db) = &self.storage {
                    match db.query_row(
                        "SELECT EXISTS(SELECT 1 FROM identity WHERE kind=?1 AND id=?2)",
                        [kind, id.as_str()],
                        |row| row.get::<_, bool>(0),
                    ) {
                        Ok(true) => return true,
                        Ok(false) => {}
                        Err(_) => {
                            self.healthy = false;
                            return true;
                        }
                    }
                } else if self
                    .ephemeral_receipts
                    .contains(&(kind.to_owned(), id.clone()))
                {
                    return true;
                }
            }
        }
        false
    }

    fn resolve(&mut self, sequence: Option<u64>) {
        for event in &mut self.state.events {
            if event.kind == AttentionKind::Request
                && sequence.is_none_or(|seq| seq == event.sequence)
            {
                event.resolved = true;
            }
        }
    }

    fn resolve_inferred(&mut self) {
        for event in &mut self.state.events {
            if event.kind == AttentionKind::Request
                && !self
                    .state
                    .native_requests
                    .values()
                    .any(|seq| *seq == event.sequence)
            {
                event.resolved = true;
            }
        }
    }

    fn append(&mut self, kind: AttentionKind, outcome: &ReducerOutcome, now: SystemTime) -> u64 {
        self.state.sequence = self
            .state
            .sequence
            .checked_add(1)
            .expect("attention sequence overflow");
        self.state.events.push(AttentionEvent {
            sequence: self.state.sequence,
            turn: self.state.turn,
            kind,
            occurred_at: now.into(),
            resolved: false,
            blocking: true,
            detail: outcome.needs_input.clone(),
        });
        while self.state.events.len() > EVENT_LIMIT {
            let Some(index) = self
                .state
                .events
                .iter()
                .position(|event| event.resolved || event.kind != AttentionKind::Request)
            else {
                break;
            };
            self.state.events.remove(index);
        }
        self.state.sequence
    }

    /// Returns whether durable semantic state changed. Storage failure disables
    /// event publication, but never stops the Agent or PTY draining.
    pub(crate) fn observe(
        &mut self,
        evidence: &Evidence,
        identity: &SignalIdentity,
        outcome: &ReducerOutcome,
        now: SystemTime,
    ) -> bool {
        if !self.healthy {
            return false;
        }
        if evidence.subagent {
            return false;
        }
        if evidence.responding {
            self.responding = true;
        }
        let submitted = evidence.submitted;
        let continuation = evidence.continuation;
        if !submitted
            && !continuation
            && !evidence.queued
            && identity.request.is_none()
            && outcome.needs_input.is_some()
            && self.state.active_requests().any(|event| event.blocking)
            && identity.resolved_request.is_none()
            && identity.started_tool.is_none()
            && outcome.status_change.is_none()
        {
            return false;
        }
        if !submitted
            && !continuation
            && !evidence.queued
            && outcome.needs_input.is_none()
            && outcome.status_change.is_none()
            && !outcome.turn_completed
            && !evidence.completion
            && identity.completion.is_none()
            && identity.resolved_request.is_none()
            && identity.started_tool.is_none()
        {
            return false;
        }
        let before = self.state.clone();
        if submitted {
            self.resolve(None);
            self.state.active_tools.clear();
            self.responding = false;
        }
        if continuation && self.responding {
            self.resolve_inferred();
            self.responding = false;
        }
        if let Some((id, name)) = &identity.started_tool
            && self.state.active_tools.len() < 32
        {
            self.state.active_tools.insert(id.clone(), name.clone());
        }
        if let Some(id) = &identity.resolved_request
            && let Some(sequence) = self.state.native_requests.get(id).copied()
        {
            self.resolve(Some(sequence));
        }
        if let Some(id) = &identity.resolved_request {
            self.state.active_tools.remove(id);
        }
        // Screen-only misses cannot resolve a wait. Require user response and
        // the reducer's confirmed continuation, or an explicit lifecycle fact.
        if self.responding && outcome.status_change == Some(SessionStatus::Working) {
            self.resolve_inferred();
            self.responding = false;
        }
        if submitted
            || (!self.state.working
                && outcome.status_change == Some(SessionStatus::Working)
                && self.state.active_requests().next().is_none())
        {
            self.state.turn += 1;
            self.state.working = true;
            self.state.last_native_completion = None;
            for event in &mut self.state.events {
                if event.kind == AttentionKind::Completion {
                    event.resolved = true;
                }
            }
        }
        if outcome.needs_input.is_some() {
            let active = self
                .state
                .active_requests()
                .next()
                .map(|event| event.sequence);
            let needs_new = identity.request.is_some() || active.is_none();
            if needs_new && self.state.active_requests().count() < 32 {
                // A native request can enrich an already-observed screen wait.
                let inferred = identity.request.as_ref().and_then(|_| {
                    self.state
                        .active_requests()
                        .find(|event| {
                            !self
                                .state
                                .native_requests
                                .values()
                                .any(|seq| *seq == event.sequence)
                        })
                        .map(|event| event.sequence)
                });
                let seq =
                    inferred.unwrap_or_else(|| self.append(AttentionKind::Request, outcome, now));
                if let Some(id) = &identity.request {
                    self.state.native_requests.insert(id.clone(), seq);
                }
            }
        }
        if evidence.queued {
            self.state.working = true;
            if self.state.active_requests().next().is_none() {
                let detail = diri_proto::NeedsInputDetail {
                    kind: diri_proto::NeedsInputKind::Question,
                    source: diri_proto::NeedsInputSource::ScreenScrape,
                    tool_name: None,
                    summary: "Question queued; agent is still working".into(),
                    prompt_excerpt: None,
                    options: None,
                    risk_hint: diri_proto::RiskHint::Neutral,
                    occurred_at: now.into(),
                };
                let queued = ReducerOutcome {
                    needs_input: Some(detail),
                    ..Default::default()
                };
                self.append(AttentionKind::Request, &queued, now);
            }
            for event in &mut self.state.events {
                if event.kind == AttentionKind::Request
                    && !event.resolved
                    && !self
                        .state
                        .native_requests
                        .values()
                        .any(|seq| *seq == event.sequence)
                {
                    event.blocking = false;
                }
            }
        } else if outcome.needs_input.is_some() {
            for event in &mut self.state.events {
                if event.kind == AttentionKind::Request && !event.resolved {
                    event.blocking = true;
                }
            }
        }
        if (outcome.turn_completed || evidence.completion)
            && (evidence.completion
                || self.responding
                || self.state.active_requests().next().is_none())
        {
            self.resolve(None);
            if identity.completion.is_some()
                && self.state.last_native_completion.is_some()
                && !self.state.working
            {
                self.state.turn += 1;
                for event in &mut self.state.events {
                    if event.kind == AttentionKind::Completion {
                        event.resolved = true;
                    }
                }
            }
            if self.state.working
                || !self.state.events.iter().any(|event| {
                    event.kind == AttentionKind::Completion && event.turn == self.state.turn
                })
            {
                self.append(AttentionKind::Completion, outcome, now);
            }
            self.state.working = false;
            self.state.active_tools.clear();
        }
        if let Some(id) = &identity.completion {
            self.state.native_completions.insert(id.clone());
            self.state.last_native_completion = Some(id.clone());
        }
        if let Some(SessionStatus::Exited(exit)) = &outcome.status_change {
            self.resolve(None);
            self.state.working = false;
            self.state.active_tools.clear();
            if exit.code != Some(0) || exit.signal.is_some() {
                self.append(AttentionKind::Failure, outcome, now);
            }
        }
        if before == self.state && identity.receipts().iter().all(|(_, id)| id.is_none()) {
            return false;
        }
        self.state.observed_at = Some(now.into());
        if let Some(storage) = &mut self.storage {
            // Disk receipts retain replay protection after bounded display history
            // and in-memory native mappings are compacted.
            self.state.native_requests.retain(|_, seq| {
                self.state
                    .events
                    .iter()
                    .any(|event| event.sequence == *seq && !event.resolved)
            });
            self.state.native_completions.clear();
            let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                let tx = storage.transaction()?;
                for (kind, id) in identity.receipts() {
                    if let Some(id) = id {
                        tx.execute(
                            "INSERT OR IGNORE INTO identity VALUES (?1, ?2)",
                            [kind, id.as_str()],
                        )?;
                    }
                }
                tx.execute(
                    "INSERT OR REPLACE INTO state VALUES (1, ?1)",
                    [serde_json::to_string(&self.state)?],
                )?;
                tx.commit()?;
                Ok(())
            })();
            if result.is_err() {
                self.healthy = false;
                eprintln!("diri: attention persistence unavailable; session alerts suppressed");
            }
        }
        for (kind, id) in identity.receipts() {
            if self.storage.is_none()
                && let Some(id) = id
            {
                self.ephemeral_receipts
                    .insert((kind.to_owned(), id.clone()));
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{Authority, StatusReducer};
    use serde_json::json;
    use std::time::{Duration, UNIX_EPOCH};

    fn hook(reducer: &mut StatusReducer, event: &str, payload: serde_json::Value) {
        let (signal, metadata) =
            crate::hooks::parse_claude_hook(event, &payload, UNIX_EPOCH + Duration::from_secs(100))
                .unwrap();
        reducer.reduce_identified(
            signal,
            metadata.identity,
            UNIX_EPOCH + Duration::from_secs(100),
        );
    }
    fn request(reducer: &mut StatusReducer, id: Option<&str>) {
        hook(
            reducer,
            "PermissionRequest",
            json!({"session_id":"conversation", "tool_use_id":id,"tool_name":"Bash", "tool_input":{"command":"cargo test"}}),
        );
    }
    fn count(reducer: &StatusReducer, kind: AttentionKind) -> usize {
        reducer
            .attention_state()
            .unwrap()
            .events
            .iter()
            .filter(|event| event.kind == kind)
            .count()
    }

    #[test]
    fn native_replays_survive_restart_and_requests_resolve_independently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attention.sqlite");
        let mut reducer =
            StatusReducer::new(Authority::HooksPrimary, UNIX_EPOCH).with_attention_path(&path);
        request(&mut reducer, Some("one"));
        request(&mut reducer, Some("two"));
        assert_eq!(
            reducer.attention_state().unwrap().active_requests().count(),
            2
        );
        let identity = reducer.attention_state().unwrap().epoch.clone();
        drop(reducer);
        let mut reducer =
            StatusReducer::new(Authority::HooksPrimary, UNIX_EPOCH).with_attention_path(&path);
        request(&mut reducer, Some("one"));
        assert_eq!(count(&reducer, AttentionKind::Request), 2);
        assert_eq!(reducer.attention_state().unwrap().epoch, identity);
        hook(
            &mut reducer,
            "PostToolUse",
            json!({"session_id":"conversation", "tool_use_id":"one"}),
        );
        assert_eq!(
            reducer.attention_state().unwrap().active_requests().count(),
            1
        );
        assert_eq!(
            reducer
                .attention_state()
                .unwrap()
                .active_requests()
                .next()
                .unwrap()
                .sequence,
            2
        );
        drop(reducer);
        let mut reducer =
            StatusReducer::new(Authority::HooksPrimary, UNIX_EPOCH).with_attention_path(&path);
        request(&mut reducer, Some("one"));
        assert_eq!(
            count(&reducer, AttentionKind::Request),
            2,
            "resolved native request is still receipted"
        );
    }

    #[test]
    fn uncorrelated_hooks_share_a_wait_until_real_continuation() {
        let mut reducer = StatusReducer::new(Authority::HooksPrimary, UNIX_EPOCH);
        request(&mut reducer, None);
        for _ in 0..20 {
            request(&mut reducer, None);
        }
        assert_eq!(count(&reducer, AttentionKind::Request), 1);
        reducer.reduce(StatusSignal::UserSubmission, UNIX_EPOCH);
        hook(
            &mut reducer,
            "PostToolUse",
            json!({"session_id":"conversation", "tool_use_id":"one"}),
        );
        request(&mut reducer, None);
        assert_eq!(
            count(&reducer, AttentionKind::Request),
            2,
            "identical wording after continuation is a new request"
        );
    }

    #[test]
    fn storage_failure_disables_publication_without_stopping_reduction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attention.sqlite");
        std::fs::write(&path, b"not sqlite").unwrap();
        let mut reducer =
            StatusReducer::new(Authority::HooksPrimary, UNIX_EPOCH).with_attention_path(&path);
        request(&mut reducer, None);
        assert!(matches!(reducer.status(), SessionStatus::NeedsInput(_)));
        assert!(reducer.attention_state().is_none());
    }

    #[test]
    fn a_native_request_enriches_an_existing_inferred_wait() {
        let mut reducer = StatusReducer::new(Authority::HooksPrimary, UNIX_EPOCH);
        request(&mut reducer, None);
        request(&mut reducer, Some("one"));
        assert_eq!(count(&reducer, AttentionKind::Request), 1);
        hook(
            &mut reducer,
            "PostToolUse",
            json!({"session_id":"conversation", "tool_use_id":"one"}),
        );
        assert_eq!(
            reducer.attention_state().unwrap().active_requests().count(),
            0
        );
    }

    #[test]
    fn child_and_pending_background_work_cannot_announce_parent_completion() {
        let mut reducer = StatusReducer::new(Authority::HooksPrimary, UNIX_EPOCH);
        hook(&mut reducer, "UserPromptSubmit", json!({}));
        hook(&mut reducer, "Stop", json!({"agent_id":"child"}));
        hook(
            &mut reducer,
            "Stop",
            json!({"background_tasks":[{"status":"running"}]}),
        );
        assert_eq!(count(&reducer, AttentionKind::Completion), 0);
        hook(
            &mut reducer,
            "Stop",
            json!({"background_tasks":[], "session_crons":[]}),
        );
        assert_eq!(count(&reducer, AttentionKind::Completion), 1);
        hook(&mut reducer, "Stop", json!({}));
        assert_eq!(count(&reducer, AttentionKind::Completion), 1);
    }

    #[test]
    fn codex_turn_ids_are_conversation_scoped_and_survive_recovery() {
        let mut reducer = StatusReducer::new(Authority::ScreenPrimary, UNIX_EPOCH);
        for thread in ["one", "one", "two"] {
            let (signal, meta) = crate::hooks::parse_codex_notify(
                &json!({"type":"agent-turn-complete", "thread-id":thread,"turn-id":"turn"}),
            )
            .unwrap();
            reducer.reduce_identified(signal, meta.identity, UNIX_EPOCH);
        }
        // Distinct native turns can finish without a captured screen transition.
        assert_eq!(count(&reducer, AttentionKind::Completion), 2);
    }
    #[test]
    fn adoption_retains_an_episode_but_a_process_launch_gets_a_new_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attention.sqlite");
        let mut reducer =
            StatusReducer::new(Authority::HooksPrimary, UNIX_EPOCH).with_attention_path(&path);
        request(&mut reducer, None);
        let epoch = reducer.attention_state().unwrap().epoch.clone();
        drop(reducer);
        let mut reducer =
            StatusReducer::new(Authority::HooksPrimary, UNIX_EPOCH).with_attention_path(&path);
        request(&mut reducer, None);
        assert_eq!(reducer.attention_state().unwrap().epoch, epoch);
        assert_eq!(count(&reducer, AttentionKind::Request), 1);
        drop(reducer);
        let mut reducer = StatusReducer::new(Authority::HooksPrimary, UNIX_EPOCH)
            .with_attention_storage(&path, true);
        request(&mut reducer, None);
        assert_ne!(reducer.attention_state().unwrap().epoch, epoch);
        assert_eq!(count(&reducer, AttentionKind::Request), 1);
    }

    #[test]
    fn mere_keystrokes_and_screen_flaps_do_not_rearm_the_same_wait() {
        let mut lifecycle = AttentionLifecycle::default();
        let detail = crate::hooks::parse_claude_hook(
            "PermissionRequest",
            &json!({"tool_name":"Bash"}),
            UNIX_EPOCH,
        )
        .unwrap()
        .1
        .needs_input
        .unwrap();
        let blocked = ReducerOutcome {
            needs_input: Some(detail),
            ..Default::default()
        };
        lifecycle.observe(
            &Evidence::default(),
            &SignalIdentity::default(),
            &blocked,
            UNIX_EPOCH,
        );
        let key = Evidence::from(&StatusSignal::UserKeystroke);
        lifecycle.observe(
            &key,
            &SignalIdentity::default(),
            &ReducerOutcome::default(),
            UNIX_EPOCH,
        );
        let working = ReducerOutcome {
            status_change: Some(SessionStatus::Working),
            ..Default::default()
        };
        lifecycle.observe(
            &Evidence::default(),
            &SignalIdentity::default(),
            &working,
            UNIX_EPOCH,
        );
        lifecycle.observe(
            &Evidence::default(),
            &SignalIdentity::default(),
            &blocked,
            UNIX_EPOCH,
        );
        assert_eq!(lifecycle.snapshot().unwrap().events.len(), 1);
        let submitted = Evidence::from(&StatusSignal::UserSubmission);
        lifecycle.observe(
            &submitted,
            &SignalIdentity::default(),
            &ReducerOutcome::default(),
            UNIX_EPOCH,
        );
        lifecycle.observe(
            &Evidence::default(),
            &SignalIdentity::default(),
            &working,
            UNIX_EPOCH,
        );
        lifecycle.observe(
            &Evidence::default(),
            &SignalIdentity::default(),
            &blocked,
            UNIX_EPOCH,
        );
        assert_eq!(lifecycle.snapshot().unwrap().events.len(), 2);
    }
    #[test]
    fn real_claude_permission_payload_correlates_only_an_unambiguous_tool() {
        let mut reducer = StatusReducer::new(Authority::HooksPrimary, UNIX_EPOCH);
        hook(
            &mut reducer,
            "PreToolUse",
            json!({"session_id":"conversation", "tool_use_id":"one", "tool_name":"Bash"}),
        );
        request(&mut reducer, None);
        hook(
            &mut reducer,
            "PostToolUse",
            json!({"session_id":"conversation", "tool_use_id":"unrelated"}),
        );
        assert_eq!(
            reducer.attention_state().unwrap().active_requests().count(),
            1
        );
        hook(
            &mut reducer,
            "PostToolUse",
            json!({"session_id":"conversation", "tool_use_id":"one"}),
        );
        assert_eq!(
            reducer.attention_state().unwrap().active_requests().count(),
            0
        );
        hook(&mut reducer, "Stop", json!({}));
        hook(
            &mut reducer,
            "PreToolUse",
            json!({"session_id":"conversation", "tool_use_id":"one", "tool_name":"Bash"}),
        );
        assert!(
            !reducer.attention_state().unwrap().working,
            "replayed work must not reopen a finished turn"
        );
        hook(
            &mut reducer,
            "PreToolUse",
            json!({"session_id":"conversation", "tool_use_id":"two", "tool_name":"Bash"}),
        );
        hook(
            &mut reducer,
            "PreToolUse",
            json!({"session_id":"conversation", "tool_use_id":"three", "tool_name":"Bash"}),
        );
        request(&mut reducer, None);
        assert!(
            !reducer
                .attention_state()
                .unwrap()
                .native_requests
                .values()
                .any(|seq| reducer
                    .attention_state()
                    .unwrap()
                    .active_requests()
                    .any(|event| event.sequence == *seq))
        );
        hook(
            &mut reducer,
            "PostToolUse",
            json!({"session_id":"conversation", "tool_use_id":"two"}),
        );
        assert_eq!(
            reducer.attention_state().unwrap().active_requests().count(),
            1,
            "parallel tool completion cannot resolve an ambiguous wait"
        );
    }
}
