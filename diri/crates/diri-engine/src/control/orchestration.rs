//! Read/write seams used by MCP orchestrators: an Agent's final answer from
//! its transcript, and folding a child's committed branch into the parent.
use std::path::{Path, PathBuf};

use diri_proto::{
    AgentKind, ControlError, ReadTranscriptParams, ReadTranscriptResult, SessionRecord,
    TranscriptTurnRecord, WorktreeIntegrateParams,
};
use serde_json::Value;

use super::{decode, encode, io_control_error, poisoned};

const DEFAULT_TURNS: u32 = 1;
const MAX_TURNS: u32 = 100;

impl super::ControlServer {
    fn record_for(&self, id: &str) -> Result<SessionRecord, ControlError> {
        self.registry
            .lock()
            .map_err(poisoned)?
            .records()
            .into_iter()
            .find(|record| record.id.0 == id)
            .ok_or_else(|| ControlError::not_found(id.to_owned()))
    }

    pub(super) fn session_read_transcript(
        &self,
        params: Option<Value>,
    ) -> Result<Value, ControlError> {
        let p: ReadTranscriptParams = decode(params)?;
        let record = self.record_for(&p.session_id.0)?;
        let wanted = p.turns.unwrap_or(DEFAULT_TURNS).clamp(1, MAX_TURNS) as usize;
        encode(&match transcript_turns(&record) {
            Ok(mut turns) => {
                let excess = turns.len().saturating_sub(wanted);
                turns.drain(..excess);
                ReadTranscriptResult {
                    available: true,
                    reason: None,
                    turns,
                }
            }
            Err(reason) => ReadTranscriptResult {
                available: false,
                reason: Some(reason),
                turns: Vec::new(),
            },
        })
    }

    pub(super) fn worktree_integrate(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: WorktreeIntegrateParams = decode(params)?;
        let source = self.record_for(&p.source_session_id.0)?;
        let target = self.record_for(&p.target_session_id.0)?;
        if source.host.is_some() || target.host.is_some() {
            return Err(ControlError::bad_request(
                "integration is available for local sessions only",
            ));
        }
        if source.project_id != target.project_id {
            return Err(ControlError::bad_request(
                "source and target sessions belong to different projects",
            ));
        }
        let branch = crate::git::branch(Path::new(&source.cwd))
            .or(source.git_branch.clone())
            .ok_or_else(|| {
                ControlError::bad_request("the source session is not on a named branch")
            })?;
        let result = crate::git::integrate(
            Path::new(&target.cwd),
            Path::new(&source.cwd),
            &branch,
            p.strategy,
            p.message.as_deref(),
        )
        .map_err(io_control_error)?;
        encode(&result)
    }
}

/// Only local Claude Code and Codex sessions keep a transcript Diri can
/// validate. Everything else reports why, so callers fall back to the screen.
pub(super) fn transcript_turns(
    record: &SessionRecord,
) -> Result<Vec<TranscriptTurnRecord>, String> {
    if record.host.is_some() {
        return Err("remote session transcripts are not readable locally".into());
    }
    let kind = record.effective_kind();
    if !matches!(kind.id(), AgentKind::CLAUDE_CODE_ID | AgentKind::CODEX_ID) {
        return Err(format!(
            "{} sessions have no readable transcript",
            kind.id()
        ));
    }
    let path = record
        .transcript_path
        .as_deref()
        .ok_or("this session has not reported a transcript yet")?;
    let agent_id = record
        .agent_session_id
        .as_deref()
        .ok_or("this session has no provider conversation identity yet")?;
    let home = diri_platform::home_dir()
        .map(|p| p.into_os_string())
        .map(PathBuf::from)
        .ok_or("HOME is unset")?;
    let snapshot =
        crate::transcript::load(&home, Path::new(path), kind, agent_id, &record.cwd, None)
            .map_err(|error| format!("transcript unavailable: {error}"))?
            .ok_or("transcript unavailable")?;
    let mut turns: Vec<TranscriptTurnRecord> = snapshot
        .document
        .turns
        .into_iter()
        .map(|turn| TranscriptTurnRecord {
            role: if turn.role == "You" { "user" } else { "agent" }.into(),
            text: turn.text,
            turn: None,
        })
        .collect();
    mark_finished_turn(&mut turns, record.attention_state.as_ref());
    Ok(turns)
}

/// Tags the closing agent message with the attention turn it finished, so a
/// reader can match it to that turn's completion. Only while the Agent is
/// idle: a working Agent's last message is not the end of anything yet.
fn mark_finished_turn(
    turns: &mut [TranscriptTurnRecord],
    attention: Option<&diri_proto::attention::AttentionState>,
) {
    let Some(state) = attention.filter(|state| !state.working) else {
        return;
    };
    if let Some(last) = turns.last_mut()
        && last.role == "agent"
    {
        last.turn = Some(state.turn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::attention::AttentionState;

    fn turn(role: &str) -> TranscriptTurnRecord {
        TranscriptTurnRecord {
            role: role.into(),
            text: "text".into(),
            turn: None,
        }
    }

    fn attention(turn: u64, working: bool) -> AttentionState {
        AttentionState {
            version: diri_proto::attention::ATTENTION_VERSION,
            epoch: "e".into(),
            sequence: 1,
            turn,
            working,
            observed_at: None,
            last_native_completion: None,
            active_tools: Default::default(),
            events: Vec::new(),
            native_requests: Default::default(),
            native_completions: Default::default(),
        }
    }

    #[test]
    fn only_an_idle_agents_closing_message_carries_its_turn() {
        let mut turns = vec![turn("user"), turn("agent")];
        mark_finished_turn(&mut turns, Some(&attention(4, false)));
        assert_eq!(turns[0].turn, None);
        assert_eq!(turns[1].turn, Some(4));

        let mut working = vec![turn("user"), turn("agent")];
        mark_finished_turn(&mut working, Some(&attention(4, true)));
        assert!(working.iter().all(|turn| turn.turn.is_none()));

        let mut asked = vec![turn("agent"), turn("user")];
        mark_finished_turn(&mut asked, Some(&attention(4, false)));
        assert!(asked.iter().all(|turn| turn.turn.is_none()));

        let mut unknown = vec![turn("agent")];
        mark_finished_turn(&mut unknown, None);
        assert_eq!(unknown[0].turn, None);
    }
}
