//! Task receipts require explicit acknowledgements. No screen/idle heuristic
//! can complete a task. The journal stays in the local Engine, outside Holders.
use super::message_delivery::{digest, open};
use super::operations::{identity, storage_error};
use diri_proto::tasks::{
    TaskGetParams, TaskRecord, TaskReportParams, TaskStatus, TaskSubmitParams,
};
use diri_proto::{ControlError, DeliverMessageParams, SessionId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::path::Path;

fn database(path: &Path) -> Result<Connection, ControlError> {
    let db = open(path)?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS tasks_v1 (
        id TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, record TEXT NOT NULL
    );",
    )
    .map_err(storage_error)?;
    Ok(db)
}
fn load(db: &Connection, id: &str) -> Result<TaskRecord, ControlError> {
    let raw: Option<String> = db
        .query_row("SELECT record FROM tasks_v1 WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .optional()
        .map_err(storage_error)?;
    serde_json::from_str(&raw.ok_or_else(|| ControlError::not_found("task"))?)
        .map_err(|_| ControlError::internal("invalid stored task receipt"))
}
fn save(db: &Connection, record: &TaskRecord) -> Result<(), ControlError> {
    let raw =
        serde_json::to_string(record).map_err(|_| ControlError::internal("cannot encode task"))?;
    db.execute(
        "UPDATE tasks_v1 SET record=?1 WHERE id=?2",
        params![raw, record.task_id],
    )
    .map_err(storage_error)?;
    Ok(())
}
fn reserve(path: &Path, p: &TaskSubmitParams) -> Result<(TaskRecord, bool), ControlError> {
    for field in [&p.caller_id, &p.request_id, &p.session_id] {
        identity(field)?;
    }
    if p.text.is_empty() || p.text.len() > 1_048_576 {
        return Err(ControlError::bad_request(
            "task text must contain 1–1048576 bytes",
        ));
    }
    let id = format!("task_{}", digest(&json!([p.caller_id, p.request_id])));
    let fingerprint = digest(&json!([p.session_id, p.text]));
    let mut db = database(path)?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage_error)?;
    let prior: Option<String> = tx
        .query_row(
            "SELECT fingerprint FROM tasks_v1 WHERE id=?1",
            [&id],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    if let Some(prior) = prior {
        if prior != fingerprint {
            return Err(ControlError::new(
                "task_id_conflict",
                "request_id identifies a different task; nothing was sent",
            ));
        }
        return Ok((load(&tx, &id)?, false));
    }
    let count: i64 = tx
        .query_row("SELECT COUNT(*) FROM tasks_v1", [], |row| row.get(0))
        .map_err(storage_error)?;
    if count >= 100_000 {
        return Err(ControlError::new(
            "task_storage_full",
            "task storage is full; nothing was sent",
        ));
    }
    let record = TaskRecord {
        task_id: id,
        sender_id: p.caller_id.clone(),
        session_id: p.session_id.clone(),
        delivery: "unknown".into(),
        status: TaskStatus::AwaitingAcknowledgement,
        result: None,
        revision: 0,
    };
    tx.execute(
        "INSERT INTO tasks_v1 VALUES (?1, ?2, ?3)",
        params![
            record.task_id,
            fingerprint,
            serde_json::to_string(&record).unwrap()
        ],
    )
    .map_err(storage_error)?;
    tx.commit().map_err(storage_error)?;
    Ok((record, true))
}
fn get(path: &Path, p: &TaskGetParams) -> Result<TaskRecord, ControlError> {
    let record = load(&database(path)?, &p.task_id)?;
    if p.caller_id != record.sender_id && p.caller_id != record.session_id {
        return Err(ControlError::new(
            "forbidden",
            "only task participants may inspect this task",
        ));
    }
    Ok(record)
}
fn report(path: &Path, p: &TaskReportParams) -> Result<TaskRecord, ControlError> {
    if p.result.as_ref().is_some_and(|r| r.len() > 16_384) {
        return Err(ControlError::bad_request("task result exceeds 16384 bytes"));
    }
    let mut db = database(path)?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage_error)?;
    let mut record = load(&tx, &p.task_id)?;
    if p.caller_id != record.session_id {
        return Err(ControlError::new(
            "forbidden",
            "only the assigned Agent may acknowledge or finish this task",
        ));
    }
    if p.status == TaskStatus::AwaitingAcknowledgement {
        return Err(ControlError::bad_request(
            "a task cannot return to unacknowledged",
        ));
    }
    if record.status == p.status && record.result == p.result {
        return Ok(record);
    }
    if record.status.is_terminal() {
        return Err(ControlError::new(
            "task_terminal",
            "a terminal task result is immutable",
        ));
    }
    if record.status == TaskStatus::AwaitingAcknowledgement && p.status != TaskStatus::Acknowledged
    {
        return Err(ControlError::new(
            "task_not_acknowledged",
            "acknowledge this task before reporting progress or completion",
        ));
    }
    if p.status.is_terminal()
        && p.result
            .as_deref()
            .is_none_or(|result| result.trim().is_empty())
    {
        return Err(ControlError::bad_request(
            "a terminal task report requires result evidence",
        ));
    }
    record.status = p.status.clone();
    record.result = p.result.clone();
    record.revision += 1;
    save(&tx, &record)?;
    tx.commit().map_err(storage_error)?;
    Ok(record)
}

impl super::ControlServer {
    fn tasks_path(&self) -> std::path::PathBuf {
        self.socket_path.with_file_name("tasks-v1.sqlite")
    }
    pub(super) fn task_submit(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: TaskSubmitParams = super::decode(params)?;
        // Check existence before reservation, but never infer task state from it.
        if self
            .registry
            .lock()
            .map_err(super::poisoned)?
            .get(&p.session_id)
            .is_none()
        {
            return Err(ControlError::not_found("task target"));
        }
        let path = self.tasks_path();
        let (mut record, fresh) = reserve(&path, &p)?;
        if fresh {
            let message = DeliverMessageParams {
                session_id: SessionId::new(&p.session_id),
                sender_id: p.caller_id,
                message_id: record.task_id.clone(),
                submit: true,
                text: format!(
                    "[Diri task {} from session {}]\nBefore starting, call report_task with task_id=\"{}\" and status=\"acknowledged\". After verifying this task, call report_task with the same task_id, status=\"completed\" (or \"failed\"), and result describing the outcome and evidence. Use status=\"blocked\" for a blocker. Session idle does not complete this task.\n\n{}",
                    record.task_id, record.sender_id, record.task_id, p.text
                ),
            };
            let receipt =
                self.session_deliver_message(Some(serde_json::to_value(message).unwrap()));
            // A target may acknowledge while delivery is returning. Never write
            // a stale status over that acknowledgement.
            let mut db = database(&path)?;
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage_error)?;
            record = load(&tx, &record.task_id)?;
            record.delivery = receipt
                .ok()
                .and_then(|r| r["delivery"].as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".into());
            record.revision += 1;
            save(&tx, &record)?;
            tx.commit().map_err(storage_error)?;
            self.events.publish(
                "task.updated",
                json!({"task_id":record.task_id,"revision":record.revision}),
                None,
            );
        }
        Ok(json!({"ok": record.delivery == "sent", "duplicate":!fresh, "task":record}))
    }
    pub(super) fn task_get(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: TaskGetParams = super::decode(params)?;
        super::encode(&get(&self.tasks_path(), &p)?)
    }
    pub(super) fn task_report(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: TaskReportParams = super::decode(params)?;
        let record = report(&self.tasks_path(), &p)?;
        self.events.publish(
            "task.updated",
            json!({"task_id":record.task_id,"revision":record.revision}),
            None,
        );
        super::encode(&record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn submission() -> TaskSubmitParams {
        TaskSubmitParams {
            caller_id: "parent".into(),
            request_id: "work-1".into(),
            session_id: "child".into(),
            text: "private task".into(),
        }
    }
    #[test]
    fn completion_requires_the_exact_task_acknowledgement_and_survives_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tasks.sqlite");
        let (task, fresh) = reserve(&path, &submission()).unwrap();
        assert!(fresh);
        let mut p = TaskReportParams {
            caller_id: "child".into(),
            task_id: task.task_id.clone(),
            status: TaskStatus::Completed,
            result: Some("tested".into()),
        };
        assert_eq!(report(&path, &p).unwrap_err().code, "task_not_acknowledged");
        p.status = TaskStatus::Acknowledged;
        p.result = None;
        report(&path, &p).unwrap();
        p.status = TaskStatus::Completed;
        p.result = Some("tested".into());
        let finished = report(&path, &p).unwrap();
        assert_eq!(report(&path, &p).unwrap(), finished);
        assert_eq!(reserve(&path, &submission()).unwrap(), (finished, false));
        p.result = Some("different".into());
        assert!(report(&path, &p).is_err());
        p.caller_id = "stranger".into();
        assert_eq!(report(&path, &p).unwrap_err().code, "forbidden");
        assert!(
            get(
                &path,
                &TaskGetParams {
                    caller_id: "stranger".into(),
                    task_id: task.task_id
                }
            )
            .is_err()
        );
        assert!(!String::from_utf8_lossy(&std::fs::read(path).unwrap()).contains("private task"));
    }
}
