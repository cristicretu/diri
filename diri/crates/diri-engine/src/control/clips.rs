//! Context Ring methods: save, list, read and delete clips, rank the
//! Sessions a clip can go to, and dispatch it. Content never reaches logs
//! or telemetry; only counts do.

use std::time::{SystemTime, UNIX_EPOCH};

use diri_proto::clip::{
    ClipCaptureParams, ClipDestination, ClipDispatchParams, ClipDispatchResult, ClipIdParams,
    ClipListParams, ClipListResult, ClipRecord, ClipSaveParams, ClipSummary, ClipTarget,
    ClipTargetReason, ClipTargetsResult, ConversationPreview, MAX_PROMPT_BYTES,
};
use diri_proto::{ControlError, EventName, SessionId, SessionRecord};
use serde_json::{Value, json};

use super::{decode, encode, io_control_error, poisoned};
use crate::clip::{ClipStore, prompt, tab_key};

/// Targets that get a transcript preview; the rest are listed bare so the
/// HUD's picker stays fast with many Sessions open.
const PREVIEWED_TARGETS: usize = 8;
/// The most targets answered.
const MAX_TARGETS: usize = 50;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Whether a Session can receive a clip: open, with a terminal, not a note.
fn receives_clips(record: &SessionRecord) -> bool {
    !record.is_archived() && !record.is_note()
}

/// Ranks open Sessions for a clip: the Session remembered for its page,
/// then Sessions serving its `localhost` port (the Agent that owns a dev
/// server shell, when there is one), then the rest by recent activity.
pub(crate) fn rank_targets(
    records: &[SessionRecord],
    remembered: Option<&str>,
    port: Option<u16>,
) -> Vec<(SessionRecord, ClipTargetReason)> {
    let open: Vec<&SessionRecord> = records.iter().filter(|r| receives_clips(r)).collect();
    let mut ranked: Vec<(SessionRecord, ClipTargetReason)> = Vec::new();
    let mut push = |record: &SessionRecord, reason| {
        if !ranked.iter().any(|(r, _)| r.id == record.id) {
            ranked.push((record.clone(), reason));
        }
    };
    if let Some(id) = remembered
        && let Some(record) = open.iter().find(|r| r.id.0 == id)
    {
        push(record, ClipTargetReason::Remembered);
    }
    if let Some(port) = port {
        for record in &open {
            let serves = record
                .listening_ports
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|p| p.port == i64::from(port));
            if !serves {
                continue;
            }
            // A dev server usually runs in an Agent's pane shell: feedback
            // belongs to the Agent.
            let owner = record
                .parent
                .as_ref()
                .filter(|_| record.kind.is_terminal())
                .and_then(|parent| open.iter().find(|r| &r.id == parent))
                .copied()
                .unwrap_or(record);
            push(owner, ClipTargetReason::ServesPort);
        }
    }
    let mut rest: Vec<&&SessionRecord> = open.iter().collect();
    // Agents before plain shells, then most recently active first.
    rest.sort_by(|a, b| {
        a.kind
            .is_terminal()
            .cmp(&b.kind.is_terminal())
            .then_with(|| b.updated_at.0.total_cmp(&a.updated_at.0))
            .then_with(|| a.id.0.cmp(&b.id.0))
    });
    for record in rest {
        push(record, ClipTargetReason::Recent);
    }
    ranked.truncate(MAX_TARGETS);
    ranked
}

impl super::ControlServer {
    fn clip_store(&self) -> ClipStore {
        ClipStore::new(self.durable_store_path(crate::clip::CLIPS_DIR))
    }

    fn publish_clip(&self, id: &str) {
        self.events
            .publish(EventName::CLIP_UPDATED, json!({ "id": id }), None);
    }

    fn find_clip(&self, id: &str) -> Result<ClipRecord, ControlError> {
        self.clip_store()
            .get(id, now_ms())
            .map_err(io_control_error)?
            .ok_or_else(|| ControlError::not_found(format!("no clip {id}")))
    }

    pub(super) fn clip_save(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: ClipSaveParams = decode(params)?;
        let record = self
            .clip_store()
            .save(p, now_ms())
            .map_err(io_control_error)?;
        diri_telemetry::event!(
            "clip.save",
            png = record.png_path.is_some(),
            text_chars = record.text.as_ref().map_or(0, |t| t.chars().count()),
            keyed = record.tab_key.is_some(),
        );
        self.publish_clip(&record.id);
        encode(&record)
    }

    pub(super) fn clip_list(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: ClipListParams = decode(params.or_else(|| Some(json!({}))))?;
        let limit = p.limit.unwrap_or(20).clamp(1, 200) as usize;
        let clips = self.clip_store().list(now_ms()).map_err(io_control_error)?;
        encode(&ClipListResult {
            clips: clips.iter().take(limit).map(ClipSummary::from).collect(),
        })
    }

    pub(super) fn clip_get(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: ClipIdParams = decode(params)?;
        encode(&self.find_clip(&p.id)?)
    }

    pub(super) fn clip_delete(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: ClipIdParams = decode(params)?;
        let removed = self
            .clip_store()
            .delete(&p.id, now_ms())
            .map_err(io_control_error)?
            .ok_or_else(|| ControlError::not_found(format!("no clip {}", p.id)))?;
        self.publish_clip(&removed);
        Ok(json!({}))
    }

    pub(super) fn clip_targets(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: ClipIdParams = decode(params)?;
        let clip = self.find_clip(&p.id)?;
        let records = self.registry.lock().map_err(poisoned)?.records();
        let remembered = match &clip.tab_key {
            Some(key) => {
                let affinity = self.clip_store().affinity().map_err(io_control_error)?;
                affinity
                    .lookup(key, |id| {
                        records.iter().any(|r| r.id.0 == id && receives_clips(r))
                    })
                    .map(str::to_owned)
            }
            None => None,
        };
        let port = clip.tab_key.as_deref().and_then(tab_key::localhost_port);
        let ranked = rank_targets(&records, remembered.as_deref(), port);
        let targets = ranked
            .into_iter()
            .enumerate()
            .map(|(index, (record, reason))| {
                let preview = if index < PREVIEWED_TARGETS {
                    session_preview(&record)
                } else {
                    ConversationPreview::default()
                };
                let project = record
                    .worktree_path
                    .as_deref()
                    .unwrap_or(&record.cwd)
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .map(str::to_owned);
                ClipTarget {
                    session_id: record.id.clone(),
                    title: record.title.clone(),
                    kind: record.kind.clone(),
                    status: record.status.clone(),
                    project,
                    reason,
                    preview,
                }
            })
            .collect();
        encode(&ClipTargetsResult { targets })
    }

    pub(super) fn clip_dispatch(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: ClipDispatchParams = decode(params)?;
        if p.prompt.len() > MAX_PROMPT_BYTES {
            return Err(ControlError::bad_request("the prompt is too long"));
        }
        let clip = self.find_clip(&p.id)?;
        let (session_id, delivered) = match p.destination {
            ClipDestination::Ring => (None, false),
            ClipDestination::Session { session_id } => {
                let record = self
                    .registry
                    .lock()
                    .map_err(poisoned)?
                    .record(&session_id.0);
                let record = record
                    .filter(receives_clips)
                    .ok_or_else(|| ControlError::not_found(session_id.0.clone()))?;
                let text = prompt::compose(&p.prompt, &clip, record.host.is_some());
                self.session_deliver_message(Some(json!({
                    "sessionID": session_id.0,
                    "senderID": "diri-clip",
                    "messageID": format!("clip-{}-{}", clip.id, now_ms()),
                    "text": text,
                    "submit": true,
                })))?;
                (Some(session_id), true)
            }
            ClipDestination::New { kind, cwd } => {
                let text = prompt::compose(&p.prompt, &clip, false);
                let _permit = self.launch_permit()?;
                let spawned = self.session_spawn(Some(json!({
                    "kind": kind,
                    "cwd": cwd,
                    "initialPrompt": text,
                })))?;
                let id = spawned
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ControlError::bad_request("the new session has no id"))?;
                (Some(SessionId::new(id)), true)
            }
        };
        let remembered_for = match &session_id {
            Some(session) => self
                .clip_store()
                .mark_sent(&clip.id, &session.0, now_ms())
                .map_err(io_control_error)?,
            None => None,
        };
        diri_telemetry::event!(
            "clip.dispatch",
            destination = match (&session_id, delivered) {
                (None, _) => "ring",
                (Some(_), true) => "session",
                _ => "unknown",
            },
            remembered = remembered_for.is_some(),
        );
        self.publish_clip(&clip.id);
        encode(&ClipDispatchResult {
            clip_id: clip.id,
            session_id,
            delivered,
            remembered_for,
        })
    }

    /// Republishes a capture request for the app; nothing is captured here.
    pub(super) fn clip_capture(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: ClipCaptureParams = decode(params.or_else(|| Some(json!({}))))?;
        if let Some(mode) = p.mode.as_deref()
            && !matches!(mode, "region" | "window")
        {
            return Err(ControlError::bad_request("mode is region or window"));
        }
        self.events
            .publish(EventName::CLIP_CAPTURE_REQUESTED, encode(&p)?, None);
        Ok(json!({}))
    }
}

/// The latest exchange from a local Claude Code or Codex transcript; empty
/// for anything without one.
fn session_preview(record: &SessionRecord) -> ConversationPreview {
    match super::orchestration::transcript_turns(record) {
        Ok(turns) => prompt::preview(
            turns
                .iter()
                .rev()
                .take(12)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|t| (t.role.as_str(), t.text.as_str())),
        ),
        Err(_) => ConversationPreview::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::{AgentKind, DateMillis, PortInfo};

    fn record(id: &str, kind: AgentKind, updated: f64) -> SessionRecord {
        let mut record = crate::control::tests::test_record(id);
        record.kind = kind;
        record.updated_at = DateMillis(updated);
        record
    }

    fn ids(ranked: &[(SessionRecord, ClipTargetReason)]) -> Vec<(&str, ClipTargetReason)> {
        ranked
            .iter()
            .map(|(r, why)| (r.id.0.as_str(), *why))
            .collect()
    }

    #[test]
    fn remembered_session_ranks_first_then_port_then_recency() {
        let mut shell = record("shell", AgentKind::SHELL, 50.0);
        shell.parent = Some(SessionId::new("agent"));
        shell.listening_ports = Some(vec![PortInfo {
            port: 3000,
            process_name: "node".into(),
        }]);
        let records = vec![
            record("old", AgentKind::CLAUDE_CODE, 1.0),
            record("agent", AgentKind::CODEX, 2.0),
            record("fresh", AgentKind::CLAUDE_CODE, 99.0),
            shell,
        ];
        let ranked = rank_targets(&records, Some("old"), Some(3000));
        assert_eq!(
            ids(&ranked),
            [
                ("old", ClipTargetReason::Remembered),
                ("agent", ClipTargetReason::ServesPort),
                ("fresh", ClipTargetReason::Recent),
                ("shell", ClipTargetReason::Recent),
            ]
        );
        let ranked = rank_targets(&records, None, None);
        assert_eq!(ranked[0].0.id.0, "fresh");
        assert_eq!(
            ranked.last().unwrap().0.id.0,
            "shell",
            "shells after agents"
        );
    }

    #[test]
    fn closed_notes_and_unknown_remembered_sessions_are_skipped() {
        let mut archived = record("archived", AgentKind::CLAUDE_CODE, 5.0);
        archived.archived_at = Some(DateMillis(1.0));
        let records = vec![
            archived,
            record("note", AgentKind::NOTE, 6.0),
            record("live", AgentKind::CLAUDE_CODE, 1.0),
        ];
        let ranked = rank_targets(&records, Some("archived"), None);
        assert_eq!(ids(&ranked), [("live", ClipTargetReason::Recent)]);
    }
}
