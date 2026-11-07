//! Task receipts require explicit acknowledgements. No screen/idle heuristic
//! can complete a task. The journal stays in the local Engine, outside Holders.
use super::message_delivery::{digest, open};
use super::operations::{identity, storage_error};
use diri_proto::tasks::{
    MAX_TASK_UPDATES, TaskAnswerParams, TaskCancelParams, TaskGetParams, TaskListParams,
    TaskRecord, TaskReportParams, TaskStatus, TaskSubmitParams, TaskUpdate,
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
    let raw = raw.ok_or_else(|| {
        ControlError::not_found(format!(
            "no task {id}: task receipts are never expired, so this id was never submitted. Check it for typos, and look a request_id up only from the session that submitted it; list_tasks shows the tasks you sent or were assigned."
        ))
    })?;
    serde_json::from_str(&raw).map_err(|_| ControlError::internal("invalid stored task receipt"))
}
/// The wire spelling of a status, for messages an Agent reads.
fn status_name(status: &TaskStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}
/// A finished task never changes; the reply says how it ended so the
/// caller can stop instead of retrying.
fn terminal_error(record: &TaskRecord, attempt: &str) -> ControlError {
    ControlError::new(
        "task_terminal",
        format!(
            "task {} already ended as {}; its result is final, so it cannot {attempt}. Nothing more is needed for this task.",
            record.task_id,
            status_name(&record.status)
        ),
    )
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
    // Schemaless submissions keep their original fingerprint.
    let fingerprint = match &p.result_schema {
        None => digest(&json!([p.session_id, p.text])),
        Some(schema) => digest(&json!([p.session_id, p.text, schema])),
    };
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
        updates: Vec::new(),
        result_schema: p.result_schema.clone(),
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
    let id = match (&p.task_id, &p.request_id) {
        (Some(id), None) => {
            identity(id)?;
            id.clone()
        }
        (None, Some(request)) => {
            identity(request)?;
            format!("task_{}", digest(&json!([p.caller_id, request])))
        }
        _ => {
            return Err(ControlError::bad_request(
                "provide exactly one of task_id or request_id",
            ));
        }
    };
    let record = load(&database(path)?, &id)?;
    if p.caller_id != record.sender_id && p.caller_id != record.session_id {
        return Err(ControlError::new(
            "forbidden",
            format!(
                "task {id} was sent by session {} to session {}; only those two may inspect it, and you are session {}.",
                record.sender_id, record.session_id, p.caller_id
            ),
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
            format!(
                "task {} is assigned to session {}; only that session may acknowledge or finish it, and you are session {}.",
                record.task_id, record.session_id, p.caller_id
            ),
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
        return Err(terminal_error(&record, "be reported again"));
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
    let kind = if record.status == p.status {
        "progress"
    } else {
        "status"
    };
    record.status = p.status.clone();
    record.result = p.result.clone();
    record.revision += 1;
    push_update(&mut record, kind, &p.caller_id, p.result.clone());
    save(&tx, &record)?;
    tx.commit().map_err(storage_error)?;
    Ok(record)
}

fn push_update(record: &mut TaskRecord, kind: &str, by: &str, text: Option<String>) {
    record.updates.push(TaskUpdate {
        kind: kind.into(),
        by: by.into(),
        status: Some(record.status.clone()),
        text,
        revision: record.revision,
    });
    let excess = record.updates.len().saturating_sub(MAX_TASK_UPDATES);
    record.updates.drain(..excess);
}

/// Sender-side mutation shared by answer and cancel: only the task's sender
/// may act, and a terminal task never changes again.
fn sender_update(
    path: &Path,
    caller: &str,
    task_id: &str,
    apply: impl FnOnce(&mut TaskRecord),
) -> Result<TaskRecord, ControlError> {
    identity(task_id)?;
    let mut db = database(path)?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage_error)?;
    let mut record = load(&tx, task_id)?;
    if caller != record.sender_id {
        return Err(ControlError::new(
            "forbidden",
            format!(
                "task {task_id} was submitted by session {}; only that session may answer or cancel it, and you are session {caller}.",
                record.sender_id
            ),
        ));
    }
    if record.status.is_terminal() {
        return Err(terminal_error(&record, "be answered or cancelled"));
    }
    record.revision += 1;
    apply(&mut record);
    save(&tx, &record)?;
    tx.commit().map_err(storage_error)?;
    Ok(record)
}

fn answer(path: &Path, p: &TaskAnswerParams) -> Result<TaskRecord, ControlError> {
    if p.text.trim().is_empty() || p.text.len() > 65_536 {
        return Err(ControlError::bad_request(
            "a task answer must contain 1–65536 bytes",
        ));
    }
    sender_update(path, &p.caller_id, &p.task_id, |record| {
        // Answering a blocker resumes the work; the Agent still owns the result.
        if record.status == TaskStatus::Blocked {
            record.status = TaskStatus::Acknowledged;
        }
        push_update(record, "answer", &p.caller_id, Some(p.text.clone()));
    })
}

fn cancel(path: &Path, p: &TaskCancelParams) -> Result<TaskRecord, ControlError> {
    if p.reason
        .as_ref()
        .is_some_and(|reason| reason.len() > 16_384)
    {
        return Err(ControlError::bad_request(
            "cancel reason exceeds 16384 bytes",
        ));
    }
    sender_update(path, &p.caller_id, &p.task_id, |record| {
        record.status = TaskStatus::Cancelled;
        record.result = p.reason.clone();
        push_update(record, "cancel", &p.caller_id, p.reason.clone());
    })
}

/// The newest tasks a caller sent or received. Receipts are small; the table
/// is capped, so a bounded newest-first scan stays cheap.
fn list(path: &Path, p: &TaskListParams) -> Result<Vec<TaskRecord>, ControlError> {
    identity(&p.caller_id)?;
    let (sent, assigned) = match p.role.as_deref() {
        None | Some("all") => (true, true),
        Some("sent") => (true, false),
        Some("assigned") => (false, true),
        Some(_) => {
            return Err(ControlError::bad_request(
                "role must be sent, assigned, or all",
            ));
        }
    };
    let limit = p.limit.unwrap_or(50).clamp(1, 200) as usize;
    let db = database(path)?;
    let mut statement = db
        .prepare("SELECT record FROM tasks_v1 ORDER BY rowid DESC LIMIT 5000")
        .map_err(storage_error)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(storage_error)?;
    let mut tasks = Vec::new();
    for raw in rows {
        let Ok(record) = serde_json::from_str::<TaskRecord>(&raw.map_err(storage_error)?) else {
            continue;
        };
        let mine = (sent && record.sender_id == p.caller_id)
            || (assigned && record.session_id == p.caller_id);
        if mine && (p.include_terminal || !record.status.is_terminal()) {
            tasks.push(record);
            if tasks.len() == limit {
                break;
            }
        }
    }
    Ok(tasks)
}

impl super::ControlServer {
    pub(super) fn tasks_path(&self) -> std::path::PathBuf {
        self.durable_store_path(crate::layout_migration::TASKS_STORE)
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
        self.publish_task(&record);
        super::encode(&record)
    }
    pub(super) fn task_answer(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: TaskAnswerParams = super::decode(params)?;
        let record = answer(&self.tasks_path(), &p)?;
        self.publish_task(&record);
        let receipt = self.notify_task_agent(
            &record,
            "answer",
            &format!(
                "[Diri task {} — answer from session {}]\n{}\n\nContinue the task, then report_task with this task_id.",
                record.task_id, record.sender_id, p.text
            ),
        );
        Ok(json!({"ok": receipt == "sent", "delivery": receipt, "task": record}))
    }
    pub(super) fn task_cancel(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: TaskCancelParams = super::decode(params)?;
        let record = cancel(&self.tasks_path(), &p)?;
        self.publish_task(&record);
        let reason = p
            .reason
            .as_deref()
            .map(|reason| format!("\nReason: {reason}"))
            .unwrap_or_default();
        let receipt = self.notify_task_agent(
            &record,
            "cancel",
            &format!(
                "[Diri task {} cancelled by session {}]{reason}\nStop working on this task. Do not report it again.",
                record.task_id, record.sender_id
            ),
        );
        Ok(json!({"ok": true, "delivery": receipt, "task": record}))
    }
    pub(super) fn task_list(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: TaskListParams = super::decode(params)?;
        Ok(json!({"tasks": list(&self.tasks_path(), &p)?}))
    }
    fn publish_task(&self, record: &TaskRecord) {
        self.events.publish(
            "task.updated",
            json!({"task_id":record.task_id,"revision":record.revision}),
            None,
        );
    }
    /// Best-effort, at-most-once notice to the assigned Agent. The task
    /// receipt is already durable; delivery is reported, never retried here.
    fn notify_task_agent(&self, record: &TaskRecord, kind: &str, text: &str) -> String {
        let message = DeliverMessageParams {
            session_id: SessionId::new(&record.session_id),
            sender_id: record.sender_id.clone(),
            message_id: format!("{}:{kind}:{}", record.task_id, record.revision),
            submit: true,
            text: text.to_owned(),
        };
        self.session_deliver_message(Some(serde_json::to_value(message).unwrap()))
            .ok()
            .and_then(|receipt| receipt["delivery"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".into())
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
            result_schema: None,
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
                    task_id: Some(task.task_id),
                    request_id: None,
                }
            )
            .is_err()
        );
        assert!(!String::from_utf8_lossy(&std::fs::read(path).unwrap()).contains("private task"));
    }

    #[test]
    fn senders_answer_blockers_and_cancel_while_agents_keep_the_result() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tasks.sqlite");
        let (task, _) = reserve(&path, &submission()).unwrap();
        let mut p = TaskReportParams {
            caller_id: "child".into(),
            task_id: task.task_id.clone(),
            status: TaskStatus::Acknowledged,
            result: None,
        };
        report(&path, &p).unwrap();
        p.status = TaskStatus::Blocked;
        p.result = Some("which database?".into());
        report(&path, &p).unwrap();
        let answer_params = |caller: &str| TaskAnswerParams {
            caller_id: caller.into(),
            task_id: task.task_id.clone(),
            text: "sqlite".into(),
        };
        assert_eq!(
            answer(&path, &answer_params("child")).unwrap_err().code,
            "forbidden"
        );
        let answered = answer(&path, &answer_params("parent")).unwrap();
        assert_eq!(answered.status, TaskStatus::Acknowledged);
        assert_eq!(answered.updates.last().unwrap().kind, "answer");

        let listed = list(
            &path,
            &TaskListParams {
                caller_id: "parent".into(),
                role: Some("sent".into()),
                include_terminal: false,
                limit: None,
            },
        )
        .unwrap();
        assert_eq!(listed.len(), 1);

        let cancelled = cancel(
            &path,
            &TaskCancelParams {
                caller_id: "parent".into(),
                task_id: task.task_id.clone(),
                reason: Some("superseded".into()),
            },
        )
        .unwrap();
        assert_eq!(cancelled.status, TaskStatus::Cancelled);
        p.status = TaskStatus::Completed;
        p.result = Some("done anyway".into());
        assert_eq!(report(&path, &p).unwrap_err().code, "task_terminal");
        assert!(
            list(
                &path,
                &TaskListParams {
                    caller_id: "child".into(),
                    role: Some("assigned".into()),
                    include_terminal: false,
                    limit: None,
                },
            )
            .unwrap()
            .is_empty()
        );
    }

    /// Orchestrating Agents ask about tasks that ended, that belong to other
    /// sessions, or that never existed. Each refusal says which, so the
    /// Agent can stop instead of retrying.
    #[test]
    fn refusals_say_what_happened_to_the_task() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tasks.sqlite");
        let (task, _) = reserve(&path, &submission()).unwrap();
        let lookup = |caller: &str, task_id: &str| {
            get(
                &path,
                &TaskGetParams {
                    caller_id: caller.into(),
                    task_id: Some(task_id.into()),
                    request_id: None,
                },
            )
            .unwrap_err()
        };

        let missing = lookup("parent", "task_nope");
        assert_eq!(missing.code, "not_found");
        assert!(missing.message.contains("task_nope"), "{missing:?}");
        assert!(missing.message.contains("never expired"), "{missing:?}");
        assert!(missing.message.contains("list_tasks"), "{missing:?}");

        let foreign = lookup("stranger", &task.task_id);
        assert_eq!(foreign.code, "forbidden");
        for part in ["parent", "child", "stranger"] {
            assert!(foreign.message.contains(part), "{foreign:?}");
        }

        let mut p = TaskReportParams {
            caller_id: "child".into(),
            task_id: task.task_id.clone(),
            status: TaskStatus::Acknowledged,
            result: None,
        };
        report(&path, &p).unwrap();
        p.status = TaskStatus::Completed;
        p.result = Some("tested".into());
        report(&path, &p).unwrap();
        p.result = Some("tested again".into());
        let again = report(&path, &p).unwrap_err();
        assert_eq!(again.code, "task_terminal");
        assert!(
            again.message.contains("already ended as completed"),
            "{again:?}"
        );
        let cancelled = cancel(
            &path,
            &TaskCancelParams {
                caller_id: "parent".into(),
                task_id: task.task_id.clone(),
                reason: None,
            },
        )
        .unwrap_err();
        assert_eq!(cancelled.code, "task_terminal");
        assert!(cancelled.message.contains("completed"), "{cancelled:?}");
        let unknown_cancel = cancel(
            &path,
            &TaskCancelParams {
                caller_id: "parent".into(),
                task_id: "task_gone".into(),
                reason: None,
            },
        )
        .unwrap_err();
        assert_eq!(unknown_cancel.code, "not_found");
        assert!(
            unknown_cancel.message.contains("task_gone"),
            "{unknown_cancel:?}"
        );
    }
}
