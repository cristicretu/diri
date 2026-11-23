//! Session records across Macs: what travels, and the idempotent merge
//! (`merge_idempotent` in `formal/MigrationSync.lean`).

use std::collections::HashSet;

use diri_proto::{AgentKind, Resumability, SessionRecord, SessionStatus};
use serde_json::{Map, Value};

use crate::pathmap::to_portable;

/// Record members that travel. Everything else is live state, a machine
/// binding (host, account profile), or recomputed on import.
const KEPT: [&str; 19] = [
    "id",
    "kind",
    "cwd",
    "projectID",
    "worktreePath",
    "gitBranch",
    "title",
    "titleSource",
    "originatingPrompt",
    "agentSessionID",
    "parent",
    "createdAt",
    "updatedAt",
    "lastTurnCompletedAt",
    "lastSeenAt",
    "pinned",
    "archivedAt",
    "noteId",
    "resumability",
];

/// Whether a record can travel: local sessions only. A remote session's
/// Holder lives on its host and is reached from any Mac that adds the host.
pub fn exportable(record: &SessionRecord) -> bool {
    record.host.is_none()
}

/// The exported form: allowlisted members, `~/` paths, status ended.
pub fn sanitize(record: &SessionRecord, home: &str) -> Option<Value> {
    let Value::Object(raw) = serde_json::to_value(record).ok()? else {
        return None;
    };
    let mut kept: Map<String, Value> = raw
        .into_iter()
        .filter(|(key, _)| KEPT.contains(&key.as_str()))
        .collect();
    for key in ["cwd", "worktreePath"] {
        if let Some(Value::String(path)) = kept.get_mut(key) {
            *path = to_portable(home, path);
        }
    }
    kept.insert("status".into(), ended_status());
    Some(Value::Object(kept))
}

/// `exited` because the machine changed: clients offer Resume, as after a
/// reboot.
pub fn ended_status() -> Value {
    serde_json::json!({"exited": {"_0": {"reason": "daemonRestart", "systemRestart": true}}})
}

pub fn ended() -> SessionStatus {
    serde_json::from_value(ended_status()).expect("ended status decodes")
}

/// Kinds whose conversation travels with a transcript.
pub fn transcript_kind(kind: &AgentKind) -> Option<&'static str> {
    match kind.id() {
        AgentKind::CLAUDE_CODE_ID => Some("claude"),
        AgentKind::CODEX_ID => Some("codex"),
        _ => None,
    }
}

/// What to do with one incoming record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Add,
    Update,
    Keep,
}

/// The merge rule: absent here -> add; here and (at least as new, or live)
/// -> keep this Mac's; otherwise -> update from the newer exported copy.
/// Importing the same record again always keeps.
pub fn decide(local: Option<&SessionRecord>, live: bool, incoming: &SessionRecord) -> Decision {
    match local {
        None => Decision::Add,
        Some(local) if live || local.updated_at.0 >= incoming.updated_at.0 => Decision::Keep,
        Some(_) => Decision::Update,
    }
}

/// Merges `incoming` into `local` by the rule above (used by tests and the
/// formal model's counterpart; the Engine applies decisions record by
/// record under its Registry lock).
pub fn merge_sessions(
    local: &[SessionRecord],
    live: &HashSet<String>,
    incoming: &[SessionRecord],
) -> Vec<SessionRecord> {
    let mut out = local.to_vec();
    for record in incoming {
        let position = out.iter().position(|r| r.id == record.id);
        let decision = decide(
            position.map(|i| &out[i]),
            live.contains(&record.id.0),
            record,
        );
        match (decision, position) {
            (Decision::Add, _) => out.push(record.clone()),
            (Decision::Update, Some(i)) => out[i] = record.clone(),
            _ => {}
        }
    }
    out
}

/// The resumability an imported record gets.
pub fn imported_resumability(kind: &AgentKind, transcript_placed: bool) -> Resumability {
    if transcript_placed {
        Resumability::Resumable
    } else if transcript_kind(kind).is_some() {
        Resumability::TranscriptMissing
    } else {
        // A shell or note reopens fresh in its folder.
        Resumability::Resumable
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn record(id: &str, updated: f64, cwd: &str) -> SessionRecord {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "kind": {"claudeCode": {}},
            "cwd": cwd,
            "projectID": "p",
            "title": id,
            "titleSource": 3,
            "agentSessionID": format!("00000000-0000-4000-8000-{:012}", id.len()),
            "status": {"idle": {}},
            "resumability": "resumable",
            "createdAt": 1.0,
            "updatedAt": updated,
            "pinned": false,
            "memoryBytes": 123,
            "accountProfile": {"id": "work", "label": "Work", "agent": "claude-code", "configHome": "/Users/alice/.claude-work"},
            "terminalCwd": "/tmp",
        }))
        .unwrap()
    }

    #[test]
    fn sanitize_keeps_identity_and_drops_machine_state() {
        let value = sanitize(&record("s1", 5.0, "/Users/alice/work/repo"), "/Users/alice").unwrap();
        assert_eq!(value["cwd"], "~/work/repo");
        assert!(value.get("memoryBytes").is_none());
        assert!(value.get("accountProfile").is_none());
        assert!(value.get("terminalCwd").is_none());
        let back: SessionRecord = serde_json::from_value(value).unwrap();
        assert!(matches!(back.status, SessionStatus::Exited(_)));
        assert_eq!(
            back.agent_session_id,
            record("s1", 5.0, "/").agent_session_id
        );
    }

    #[test]
    fn merge_is_idempotent_and_never_regresses_local_state() {
        let local = vec![
            record("a", 10.0, "/x"),
            record("b", 1.0, "/x"),
            record("live", 1.0, "/x"),
        ];
        let live: HashSet<String> = ["live".to_owned()].into();
        let incoming = vec![
            record("a", 5.0, "/old"),
            record("b", 7.0, "/new"),
            record("live", 9.0, "/new"),
            record("c", 3.0, "/new"),
        ];
        let once = merge_sessions(&local, &live, &incoming);
        assert_eq!(once.len(), 4);
        assert_eq!(once[0].cwd, "/x");
        assert_eq!(once[1].cwd, "/new");
        assert_eq!(once[2].cwd, "/x");
        let twice = merge_sessions(&once, &live, &incoming);
        assert_eq!(
            serde_json::to_value(&once).unwrap(),
            serde_json::to_value(&twice).unwrap()
        );
    }
}
