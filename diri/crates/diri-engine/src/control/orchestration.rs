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
fn transcript_turns(record: &SessionRecord) -> Result<Vec<TranscriptTurnRecord>, String> {
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
    Ok(snapshot
        .document
        .turns
        .into_iter()
        .map(|turn| TranscriptTurnRecord {
            role: if turn.role == "You" { "user" } else { "agent" }.into(),
            text: turn.text,
        })
        .collect())
}
