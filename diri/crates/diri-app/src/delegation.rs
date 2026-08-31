//! Pure proposal building for sidebar delegation gestures.
//!
//! Dragging never mutates a session. These helpers turn stable session ids
//! into reviewable proposals from the snapshot the app already holds. The UI
//! decides where to present the proposal and only confirmation emits an
//! effect.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use diri_proto::{
    AgentKind, Project, ProjectId, SessionId, SessionRecord, SessionStatus, WorktreeOverviewEntry,
};
use serde_json::Value;

const TRANSCRIPT_HEAD_BYTES: u64 = 512 * 1024;
const TRANSCRIPT_TAIL_BYTES: u64 = 256 * 1024;
const RESULT_CHAR_LIMIT: usize = 1_200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandoffProposal {
    pub source_id: SessionId,
    pub target_id: SessionId,
    pub source_title: String,
    pub target_title: String,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiblingProposal {
    pub source_id: SessionId,
    pub source_title: String,
    pub kind: AgentKind,
    pub project_id: ProjectId,
    pub cwd: String,
    pub prompt: String,
    pub parent: Option<SessionId>,
    pub host: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeMoveProposal {
    pub source_id: SessionId,
    pub source_title: String,
    pub project_root: String,
    pub worktree_path: String,
    pub branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationRefusal(pub String);

impl std::fmt::Display for DelegationRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

pub fn handoff_proposal(
    sessions: &HashMap<SessionId, Arc<SessionRecord>>,
    source_id: &SessionId,
    target_id: &SessionId,
) -> Result<HandoffProposal, DelegationRefusal> {
    validate_handoff(sessions, source_id, target_id)?;
    let source = sessions
        .get(source_id)
        .ok_or_else(|| DelegationRefusal("The dragged session no longer exists.".to_owned()))?;
    let target = sessions
        .get(target_id)
        .ok_or_else(|| DelegationRefusal("The target session no longer exists.".to_owned()))?;
    Ok(HandoffProposal {
        source_id: source.id.clone(),
        target_id: target.id.clone(),
        source_title: display_title(source),
        target_title: display_title(target),
        summary: assemble_handoff_summary(source),
    })
}

pub fn validate_handoff(
    sessions: &HashMap<SessionId, Arc<SessionRecord>>,
    source_id: &SessionId,
    target_id: &SessionId,
) -> Result<(), DelegationRefusal> {
    let _source = sessions
        .get(source_id)
        .ok_or_else(|| DelegationRefusal("The dragged session no longer exists.".to_owned()))?;
    let target = sessions
        .get(target_id)
        .ok_or_else(|| DelegationRefusal("The target session no longer exists.".to_owned()))?;
    if source_id == target_id {
        return Err(DelegationRefusal(
            "A session cannot delegate work to itself.".to_owned(),
        ));
    }
    if is_descendant(sessions, source_id, target_id) {
        return Err(DelegationRefusal(
            "A session cannot delegate work to one of its descendants.".to_owned(),
        ));
    }
    if target.is_archived() || matches!(target.status, SessionStatus::Exited(_)) {
        return Err(DelegationRefusal(
            "The target session has ended and cannot receive a handoff.".to_owned(),
        ));
    }

    Ok(())
}

pub fn sibling_proposal(
    source: &SessionRecord,
    project: &Project,
) -> Result<SiblingProposal, DelegationRefusal> {
    if source.is_archived() {
        return Err(DelegationRefusal(
            "Archived sessions cannot be fanned out.".to_owned(),
        ));
    }
    let prompt = originating_prompt(source).ok_or_else(|| {
        DelegationRefusal(
            "The originating prompt is unavailable for this older session.".to_owned(),
        )
    })?;
    Ok(SiblingProposal {
        source_id: source.id.clone(),
        source_title: display_title(source),
        kind: source.kind.clone(),
        project_id: source.project_id.clone(),
        cwd: project.root.clone(),
        prompt,
        parent: source.parent.clone(),
        host: source.host.clone(),
    })
}

pub fn worktree_move_proposal(
    source: &SessionRecord,
    source_project: Option<&Project>,
    target: &WorktreeOverviewEntry,
) -> Result<WorktreeMoveProposal, DelegationRefusal> {
    if source.host.is_some() {
        return Err(DelegationRefusal(
            "Remote sessions cannot move into a local worktree.".to_owned(),
        ));
    }
    if !matches!(source.status, SessionStatus::Exited(_)) {
        return Err(DelegationRefusal(
            "Stop or archive the session before moving it to another worktree.".to_owned(),
        ));
    }
    if !source.can_resume() {
        return Err(DelegationRefusal(
            "This session cannot resume after moving to another worktree.".to_owned(),
        ));
    }
    if source.cwd == target.path || source.worktree_path.as_deref() == Some(&target.path) {
        return Err(DelegationRefusal(
            "The session is already attached to this worktree.".to_owned(),
        ));
    }
    if source_project.is_none_or(|project| project.root != target.project_root) {
        return Err(DelegationRefusal(
            "Choose a worktree from the session's current project.".to_owned(),
        ));
    }
    if target
        .session_id
        .as_ref()
        .is_some_and(|id| id != &source.id)
        && target
            .session_status
            .as_ref()
            .is_some_and(|status| !matches!(status, SessionStatus::Exited(_)))
    {
        return Err(DelegationRefusal(
            "Another live session already owns this worktree.".to_owned(),
        ));
    }

    Ok(WorktreeMoveProposal {
        source_id: source.id.clone(),
        source_title: display_title(source),
        project_root: target.project_root.clone(),
        worktree_path: target.path.clone(),
        branch: target.branch.clone(),
    })
}

fn is_descendant(
    sessions: &HashMap<SessionId, Arc<SessionRecord>>,
    ancestor: &SessionId,
    candidate: &SessionId,
) -> bool {
    let mut cursor = sessions
        .get(candidate)
        .and_then(|session| session.parent.clone());
    let mut seen = HashSet::new();
    while let Some(id) = cursor {
        if &id == ancestor {
            return true;
        }
        if !seen.insert(id.clone()) {
            return false;
        }
        cursor = sessions.get(&id).and_then(|session| session.parent.clone());
    }
    false
}

pub fn assemble_handoff_summary(source: &SessionRecord) -> String {
    let title = display_title(source);
    let location = source
        .worktree_path
        .as_deref()
        .unwrap_or(&source.cwd)
        .trim();
    let result = last_transcript_result(source)
        .unwrap_or_else(|| "No transcript result is available yet.".to_owned());
    format!(
        "Please continue the work delegated from the Diri session \"{title}\".\n\n\
         BEGIN DELEGATED SESSION CONTEXT\n\
         Session: {title}\n\
         Worktree: {location}\n\
         Diff stat: {}\n\
         Last transcript result:\n{result}\n\
         END DELEGATED SESSION CONTEXT\n\n\
         Verify the current workspace state, preserve completed work, and continue from here.",
        diff_stat(source)
    )
}

fn display_title(session: &SessionRecord) -> String {
    let title = session
        .title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        session.kind.id().to_owned()
    } else {
        title
    }
}

fn diff_stat(session: &SessionRecord) -> String {
    let Some(latest) = session.pull_requests.as_deref().and_then(|requests| {
        requests.iter().max_by(|left, right| {
            left.fetched_at
                .0
                .total_cmp(&right.fetched_at.0)
                .then_with(|| left.number.cmp(&right.number))
        })
    }) else {
        return "not available".to_owned();
    };
    format!(
        "+{} −{} across {} file{}",
        latest.additions,
        latest.deletions,
        latest.changed_files,
        if latest.changed_files == 1 { "" } else { "s" }
    )
}

pub fn originating_prompt(session: &SessionRecord) -> Option<String> {
    session
        .originating_prompt
        .as_deref()
        .and_then(nonempty)
        .map(str::to_owned)
        .or_else(|| {
            let path = session.transcript_path.as_deref()?;
            let text = read_head(Path::new(path), TRANSCRIPT_HEAD_BYTES).ok()?;
            text.lines().find_map(|line| {
                let object: Value = serde_json::from_str(line).ok()?;
                user_text(&object)
            })
        })
}

fn last_transcript_result(session: &SessionRecord) -> Option<String> {
    let path = session.transcript_path.as_deref()?;
    let text = read_tail(Path::new(path), TRANSCRIPT_TAIL_BYTES).ok()?;
    text.lines().rev().find_map(|line| {
        let object: Value = serde_json::from_str(line).ok()?;
        assistant_text(&object).map(|text| truncate_chars(&text, RESULT_CHAR_LIMIT))
    })
}

fn user_text(object: &Value) -> Option<String> {
    match object.get("type").and_then(Value::as_str) {
        Some("user") => text_content(object.get("message")?.get("content")?),
        Some("event_msg")
            if object.get("payload")?.get("type").and_then(Value::as_str)
                == Some("user_message") =>
        {
            object
                .get("payload")?
                .get("message")?
                .as_str()
                .and_then(nonempty)
                .map(str::to_owned)
        }
        _ => None,
    }
}

fn assistant_text(object: &Value) -> Option<String> {
    match object.get("type").and_then(Value::as_str) {
        Some("assistant") => text_content(object.get("message")?.get("content")?),
        Some("event_msg")
            if object.get("payload")?.get("type").and_then(Value::as_str)
                == Some("agent_message") =>
        {
            object
                .get("payload")?
                .get("message")?
                .as_str()
                .and_then(nonempty)
                .map(str::to_owned)
        }
        Some("response_item") => {
            let payload = object.get("payload")?;
            (payload.get("role").and_then(Value::as_str) == Some("assistant"))
                .then(|| payload.get("content").and_then(text_content))
                .flatten()
        }
        _ => None,
    }
}

fn text_content(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str().and_then(nonempty) {
        return Some(text.to_owned());
    }
    let joined = value
        .as_array()?
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str).and_then(nonempty))
        .collect::<Vec<_>>()
        .join("\n");
    nonempty(&joined).map(str::to_owned)
}

fn nonempty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    value
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>()
        + "…"
}

fn read_head(path: &Path, cap: u64) -> std::io::Result<String> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(cap).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_tail(path: &Path, cap: u64) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let end = file.seek(SeekFrom::End(0))?;
    let start = end.saturating_sub(cap);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    if start == 0 {
        Ok(text.into_owned())
    } else {
        Ok(text
            .split_once('\n')
            .map_or("", |(_, tail)| tail)
            .to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::{
        DateMillis, ExitInfo, ExitReason, PullRequestStatus, Resumability, TitleSource,
    };

    fn session(id: &str, parent: Option<&str>) -> SessionRecord {
        SessionRecord {
            id: SessionId::new(id),
            kind: AgentKind::CODEX,
            cwd: format!("/repo/{id}"),
            project_id: ProjectId::new("project"),
            worktree_path: Some(format!("/repo/worktrees/{id}")),
            git_branch: Some(id.to_owned()),
            title: id.to_owned(),
            title_source: TitleSource::UserRename,
            originating_prompt: Some(format!("Implement {id}")),
            agent_session_id: Some(format!("agent-{id}")),
            transcript_path: None,
            status: SessionStatus::Idle,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::Live,
            capabilities: None,
            parent: parent.map(SessionId::new),
            created_at: DateMillis(1.0),
            updated_at: DateMillis(2.0),
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
        }
    }

    fn sessions(values: Vec<SessionRecord>) -> HashMap<SessionId, Arc<SessionRecord>> {
        values
            .into_iter()
            .map(|session| (session.id.clone(), Arc::new(session)))
            .collect()
    }

    #[test]
    fn row_drop_builds_an_identity_based_handoff() {
        let sessions = sessions(vec![session("source", None), session("target", None)]);
        let proposal = handoff_proposal(
            &sessions,
            &SessionId::new("source"),
            &SessionId::new("target"),
        )
        .unwrap();
        assert_eq!(proposal.source_id, SessionId::new("source"));
        assert_eq!(proposal.target_id, SessionId::new("target"));
        assert!(proposal.summary.contains("Session: source"));
    }

    #[test]
    fn self_and_descendant_targets_are_rejected() {
        let sessions = sessions(vec![
            session("source", None),
            session("child", Some("source")),
            session("grandchild", Some("child")),
        ]);
        let source = SessionId::new("source");
        assert!(handoff_proposal(&sessions, &source, &source).is_err());
        let refusal =
            handoff_proposal(&sessions, &source, &SessionId::new("grandchild")).unwrap_err();
        assert!(refusal.0.contains("descendants"));
    }

    #[test]
    fn empty_space_builds_a_sibling_from_the_originating_prompt() {
        let source = session("source", Some("parent"));
        let project = Project {
            id: source.project_id.clone(),
            root: "/repo".to_owned(),
            name: "repo".to_owned(),
            pinned_order: None,
            host: None,
        };
        let proposal = sibling_proposal(&source, &project).unwrap();
        assert_eq!(proposal.source_id, source.id);
        assert_eq!(proposal.parent, Some(SessionId::new("parent")));
        assert_eq!(proposal.prompt, "Implement source");
        assert_eq!(proposal.cwd, "/repo");
    }

    #[test]
    fn worktree_drop_proposes_only_a_safe_ended_move() {
        let mut source = session("source", None);
        source.status = SessionStatus::Exited(ExitInfo {
            reason: ExitReason::Exited,
            code: Some(0),
            signal: None,
        });
        source.resumability = Resumability::Resumable;
        let project = Project {
            id: source.project_id.clone(),
            root: "/repo".to_owned(),
            name: "repo".to_owned(),
            pinned_order: None,
            host: None,
        };
        let target = WorktreeOverviewEntry {
            path: "/repo-feature".to_owned(),
            branch: Some("feature".to_owned()),
            project_root: "/repo".to_owned(),
            session_id: None,
            session_status: None,
            dirty: false,
            merged: false,
            age_days: 1,
            stale_suggestion: false,
        };
        let proposal = worktree_move_proposal(&source, Some(&project), &target).unwrap();
        assert_eq!(proposal.worktree_path, "/repo-feature");

        source.status = SessionStatus::Working;
        let refusal = worktree_move_proposal(&source, Some(&project), &target).unwrap_err();
        assert!(refusal.0.contains("Stop or archive"));
    }

    #[test]
    fn summary_uses_existing_worktree_diff_and_transcript_data() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            temp.path(),
            concat!(
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"original task\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"Implemented the parser and all tests pass.\"}}\n"
            ),
        )
        .unwrap();
        let mut source = session("Source  session", None);
        source.originating_prompt = None;
        source.transcript_path = Some(temp.path().to_string_lossy().into_owned());
        source.pull_requests = Some(vec![PullRequestStatus {
            url: "https://example.test/pr/1".to_owned(),
            number: 1,
            title: None,
            author: None,
            body: None,
            base_ref_name: None,
            head_ref_name: None,
            state: "OPEN".to_owned(),
            is_draft: false,
            review_decision: None,
            mergeable: None,
            merge_state_status: None,
            additions: 12,
            deletions: 3,
            changed_files: 2,
            comment_count: 0,
            review_count: 0,
            resolved_threads: None,
            total_threads: None,
            checks_passed: 0,
            checks_failed: 0,
            checks_pending: 0,
            checks: None,
            discussion: None,
            fetched_at: DateMillis(3.0),
        }]);
        assert_eq!(
            originating_prompt(&source).as_deref(),
            Some("original task")
        );
        let summary = assemble_handoff_summary(&source);
        assert!(summary.contains("Session: Source session"));
        assert!(summary.contains("Worktree: /repo/worktrees/Source  session"));
        assert!(summary.contains("Diff stat: +12 −3 across 2 files"));
        assert!(summary.contains("Implemented the parser and all tests pass."));
    }
}
