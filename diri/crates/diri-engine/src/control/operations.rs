//! Durable cold-path operation identities. Reserve before effects, never replay
//! an uncertain launch. No prompts, argv, or environment are stored here.
use std::path::Path;

use diri_proto::ControlError;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use super::message_delivery::{digest, open};

pub(super) fn identity(value: &str) -> Result<(), ControlError> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(ControlError::bad_request("invalid operation identity"));
    }
    Ok(())
}

pub(super) struct Reservation {
    pub key: String,
    pub resource: String,
    pub outcome: String,
    pub fresh: bool,
}

pub(super) fn reserve(
    path: &Path,
    sender: &str,
    id: &str,
    payload: &Value,
) -> Result<Reservation, ControlError> {
    identity(sender)?;
    identity(id)?;
    let key = digest(&json!([sender, id]));
    let fingerprint = digest(payload);
    let mut db = open(path)?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS spawn_operations_v1 (
        identity TEXT PRIMARY KEY, fingerprint TEXT NOT NULL,
        resource TEXT NOT NULL, outcome TEXT NOT NULL
    );",
    )
    .map_err(storage_error)?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage_error)?;
    let previous: Option<(String, String, String)> = tx
        .query_row(
            "SELECT fingerprint, resource, outcome FROM spawn_operations_v1 WHERE identity=?1",
            [&key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(storage_error)?;
    if let Some((prior, resource, outcome)) = previous {
        if prior != fingerprint {
            return Err(ControlError::new(
                "operation_id_conflict",
                "operation_id already identifies a different spawn; nothing was launched",
            ));
        }
        return Ok(Reservation {
            key,
            resource,
            outcome,
            fresh: false,
        });
    }
    let count: i64 = tx
        .query_row("SELECT COUNT(*) FROM spawn_operations_v1", [], |row| {
            row.get(0)
        })
        .map_err(storage_error)?;
    if count >= 100_000 {
        return Err(ControlError::new(
            "operation_storage_full",
            "operation storage is full; nothing was launched",
        ));
    }
    let resource = super::next_session_id();
    tx.execute(
        "INSERT INTO spawn_operations_v1 VALUES (?1, ?2, ?3, 'unknown')",
        params![key, fingerprint, resource],
    )
    .map_err(storage_error)?;
    tx.commit().map_err(storage_error)?;
    Ok(Reservation {
        key,
        resource,
        outcome: "unknown".into(),
        fresh: true,
    })
}

pub(super) fn finish(path: &Path, key: &str, outcome: &str) -> Result<(), ControlError> {
    open(path)?
        .execute(
            "UPDATE spawn_operations_v1 SET outcome=?1 WHERE identity=?2",
            params![outcome, key],
        )
        .map_err(storage_error)?;
    Ok(())
}

pub(super) fn storage_error(_: rusqlite::Error) -> ControlError {
    ControlError::new(
        "operation_storage_unavailable",
        "operation storage unavailable; inspect the original operation rather than retrying with a new identity",
    )
}

impl super::ControlServer {
    pub(super) fn session_spawn_tracked(
        &self,
        params: Option<Value>,
    ) -> Result<Value, ControlError> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Request {
            #[serde(rename = "senderID")]
            sender_id: String,
            #[serde(rename = "operationID")]
            operation_id: String,
            spawn: Value,
        }
        let mut p: Request = super::decode(params)?;
        let typed: diri_proto::SessionSpawnParams = super::decode(Some(p.spawn.clone()))?;
        if typed.parent.as_ref().map(|id| id.0.as_str()) != Some(p.sender_id.as_str()) {
            return Err(ControlError::bad_request(
                "tracked spawn must belong to its sender",
            ));
        }
        let path = self.socket_path.with_file_name("operations-v1.sqlite");
        let mut reservation = reserve(&path, &p.sender_id, &p.operation_id, &p.spawn)?;
        let worktree_branch = typed.new_worktree.unwrap_or(false).then(|| {
            typed
                .worktree_branch
                .unwrap_or_else(|| format!("diri/mcp-{}", reservation.resource))
        });
        if let Some(branch) = &worktree_branch {
            p.spawn["worktreeBranch"] = json!(branch);
        }
        let mut error_code = None;
        let record = if reservation.fresh {
            match self.session_spawn_identified(Some(p.spawn), Some(reservation.resource.clone())) {
                Ok(record) => {
                    if finish(&path, &reservation.key, "completed").is_ok() {
                        reservation.outcome = "completed".into();
                    }
                    Some(record)
                }
                Err(error) => {
                    error_code = Some(error.code);
                    if finish(&path, &reservation.key, "failed").is_ok() {
                        reservation.outcome = "failed".into();
                    }
                    None
                }
            }
        } else {
            None
        };
        let record = record.or_else(|| {
            self.registry
                .lock()
                .ok()?
                .records()
                .into_iter()
                .find(|r| r.id.0 == reservation.resource)
                .and_then(|r| serde_json::to_value(r).ok())
        });
        let session_present = record.is_some();
        let mut result = record.unwrap_or_else(|| json!({"id":reservation.resource}));
        result["ok"] = json!(reservation.outcome == "completed");
        result["spawn_receipt"] = json!({
            "operation_id":p.operation_id, "session_id":reservation.resource,
            "outcome":reservation.outcome, "duplicate":!reservation.fresh, "session_present":session_present,
            "error_code":error_code, "worktree_branch":worktree_branch,
            "note":"This operation never launches twice. Reuse operation_id on retries. Unknown or failed may have left a session or worktree; inspect session_id before recovery."
        });
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reservation_survives_interruption_and_rejects_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ops.sqlite");
        let first = reserve(&path, "parent", "op1", &json!({"prompt":"private"})).unwrap();
        let retry = reserve(&path, "parent", "op1", &json!({"prompt":"private"})).unwrap();
        assert!(first.fresh && !retry.fresh);
        assert_eq!(first.resource, retry.resource);
        assert_eq!(retry.outcome, "unknown");
        assert!(reserve(&path, "parent", "op1", &json!({"prompt":"changed"})).is_err());
        finish(&path, &first.key, "completed").unwrap();
        assert_eq!(
            reserve(&path, "parent", "op1", &json!({"prompt":"private"}))
                .unwrap()
                .outcome,
            "completed"
        );
        assert!(!String::from_utf8_lossy(&std::fs::read(path).unwrap()).contains("private"));
    }
    #[test]
    fn concurrent_reservations_choose_one_session() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ops.sqlite");
        let _ = reserve(&path, "setup", "setup", &json!({})).unwrap();
        let workers: Vec<_> = (0..10)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || reserve(&path, "parent", "op", &json!({})).unwrap())
            })
            .collect();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|r| r.fresh).count(), 1);
        assert!(results.iter().all(|r| r.resource == results[0].resource));
    }
}
