//! Durable at-most-once receipts for MCP messages. Reserve before touching the
//! PTY: a crash between reservation and completion is unknown, never retryable.
//! No prompts are stored here, and this is outside the terminal hot path.

use std::fs::OpenOptions;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use diri_proto::{ControlError, DeliverMessageParams};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAX_RECEIPTS: i64 = 100_000;

pub(super) fn deliver(
    path: &Path,
    message: &DeliverMessageParams,
    send: impl FnOnce() -> Result<(), ControlError>,
) -> Result<Value, ControlError> {
    for value in [
        &message.sender_id,
        &message.message_id,
        &message.session_id.0,
    ] {
        if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            return Err(ControlError::bad_request("invalid message identity"));
        }
    }
    let key = digest(&json!([
        message.sender_id,
        message.session_id,
        message.message_id
    ]));
    let fingerprint = digest(&json!([message.text, message.submit]));
    let mut db = open(path)?;
    let transaction = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage_error)?;
    let prior: Option<(String, String)> = transaction
        .query_row(
            "SELECT fingerprint, outcome FROM message_receipts_v1 WHERE identity = ?1",
            [&key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(storage_error)?;
    if let Some((previous, outcome)) = prior {
        if previous != fingerprint {
            return Err(ControlError::new(
                "message_id_conflict",
                "message_id was already used for different text or submit mode; nothing was sent",
            ));
        }
        return Ok(receipt(message, &outcome, true));
    }
    let count: i64 = transaction
        .query_row("SELECT COUNT(*) FROM message_receipts_v1", [], |row| {
            row.get(0)
        })
        .map_err(storage_error)?;
    if count >= MAX_RECEIPTS {
        return Err(ControlError::new(
            "message_receipts_full",
            "message receipt storage is full; nothing was sent",
        ));
    }
    transaction.execute(
        "INSERT INTO message_receipts_v1 (identity, fingerprint, outcome) VALUES (?1, ?2, 'unknown')",
        params![key, fingerprint],
    ).map_err(storage_error)?;
    transaction.commit().map_err(storage_error)?;

    // Even a partial paste or failed Enter may have affected the agent. Retain
    // the reservation on every error; transport uncertainty cannot undo it.
    if send().is_err() {
        return Ok(receipt(message, "unknown", false));
    }
    if db
        .execute(
            "UPDATE message_receipts_v1 SET outcome = 'sent' WHERE identity = ?1",
            [&key],
        )
        .is_err()
    {
        return Ok(receipt(message, "unknown", false));
    }
    Ok(receipt(message, "sent", false))
}

fn receipt(message: &DeliverMessageParams, outcome: &str, duplicate: bool) -> Value {
    json!({
        "message_id": message.message_id,
        "delivery": outcome,
        "duplicate": duplicate,
        "note": if outcome == "sent" {
            "Input was sent once. This is not an acknowledgement that the agent has started or finished."
        } else {
            "Input may have reached the agent. Do not resend with a new message_id; inspect the target session."
        },
    })
}

fn digest(value: &Value) -> String {
    Sha256::digest(value.to_string().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn storage_error(_: rusqlite::Error) -> ControlError {
    ControlError::new(
        "message_receipts_unavailable",
        "message receipt storage is unavailable; delivery must not be retried with a new identity",
    )
}

fn open(path: &Path) -> Result<Connection, ControlError> {
    let unavailable = || {
        ControlError::new(
            "message_receipts_unavailable",
            "cannot safely open message receipt storage; nothing was sent",
        )
    };
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| unavailable())?;
    let metadata = file.metadata().map_err(|_| unavailable())?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(unavailable());
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|_| unavailable())?;
    // macOS /var and /tmp are parent aliases. Resolve the directory only;
    // SQLite NOFOLLOW must still reject a symlink at the database itself.
    let parent = path
        .parent()
        .ok_or_else(unavailable)?
        .canonicalize()
        .map_err(|_| unavailable())?;
    let db_path = parent.join(path.file_name().ok_or_else(unavailable)?);
    let db = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(storage_error)?;
    db.busy_timeout(Duration::from_secs(2))
        .map_err(storage_error)?;
    db.execute_batch(
        "PRAGMA synchronous=FULL;
        CREATE TABLE IF NOT EXISTS message_receipts_v1 (
            identity TEXT PRIMARY KEY,
            fingerprint TEXT NOT NULL,
            outcome TEXT NOT NULL CHECK (outcome IN ('unknown', 'sent'))
        );",
    )
    .map_err(storage_error)?;
    std::fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|_| unavailable())?;
    Ok(db)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn message() -> DeliverMessageParams {
        DeliverMessageParams {
            session_id: diri_proto::SessionId::new("s_target"),
            sender_id: "s_sender".into(),
            message_id: "task-1".into(),
            text: "a private task".into(),
            submit: true,
        }
    }

    #[test]
    fn receipt_survives_reopen_and_rejects_conflicting_payloads() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("receipts.sqlite");
        let mut message = message();
        assert_eq!(
            deliver(&path, &message, || Ok(())).unwrap()["delivery"],
            "sent"
        );
        let duplicate = deliver(&path, &message, || panic!("replayed after reopen")).unwrap();
        assert_eq!(duplicate["duplicate"], true);
        assert_eq!(duplicate["delivery"], "sent");
        message.text.push('!');
        assert_eq!(
            deliver(&path, &message, || panic!("conflicting send"))
                .unwrap_err()
                .code,
            "message_id_conflict"
        );
        message.text.pop();
        message.submit = false;
        assert_eq!(
            deliver(&path, &message, || panic!("conflicting submit"))
                .unwrap_err()
                .code,
            "message_id_conflict"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!String::from_utf8_lossy(&std::fs::read(path).unwrap()).contains("a private task"));
    }

    #[test]
    fn interrupted_and_failed_sends_never_replay() {
        for crash in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("receipts.sqlite");
            let message = message();
            let _ = std::panic::catch_unwind(|| {
                deliver(&path, &message, || {
                    if crash {
                        panic!("simulate Engine failure after reservation");
                    }
                    Err(ControlError::internal("lost transport acknowledgement"))
                })
            });
            let retry = deliver(&path, &message, || panic!("replayed uncertain input")).unwrap();
            assert_eq!(retry["delivery"], "unknown");
            assert_eq!(retry["duplicate"], true);
        }
    }

    #[test]
    fn concurrent_duplicate_does_not_enter_the_send_closure() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("receipts.sqlite");
        let barrier = Arc::new(Barrier::new(2));
        let child_barrier = barrier.clone();
        let child_path = path.clone();
        let worker = std::thread::spawn(move || {
            deliver(&child_path, &message(), || {
                child_barrier.wait();
                child_barrier.wait();
                Ok(())
            })
            .unwrap()
        });
        barrier.wait();
        let duplicate = deliver(&path, &message(), || panic!("concurrent duplicate")).unwrap();
        barrier.wait();
        assert_eq!(worker.join().unwrap()["delivery"], "sent");
        assert_eq!(duplicate["delivery"], "unknown");
        assert_eq!(duplicate["duplicate"], true);
    }

    #[test]
    fn intentional_repeats_and_other_senders_or_targets_have_distinct_identities() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("receipts.sqlite");
        let mut messages = vec![message(); 4];
        messages[1].message_id = "task-2".into();
        messages[2].sender_id = "s_other_sender".into();
        messages[3].session_id = diri_proto::SessionId::new("s_other_target");
        for message in messages {
            assert_eq!(
                deliver(&path, &message, || Ok(())).unwrap()["duplicate"],
                false
            );
        }
    }

    #[test]
    fn corrupt_or_symlinked_receipts_fail_before_input() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("corrupt.sqlite");
        std::fs::write(&path, b"corrupt receipt storage").unwrap();
        assert!(deliver(&path, &message(), || panic!("untracked send")).is_err());
        let link = temp.path().join("link.sqlite");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(deliver(&link, &message(), || panic!("followed symlink")).is_err());
    }

    #[test]
    fn full_storage_preserves_existing_identities_and_rejects_new_sends() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("receipts.sqlite");
        deliver(&path, &message(), || Ok(())).unwrap();
        let db = open(&path).unwrap();
        db.execute(
            "WITH RECURSIVE entries(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM entries WHERE n < ?1)
             INSERT INTO message_receipts_v1 SELECT CAST(n AS TEXT), 'fixture', 'sent' FROM entries",
            [MAX_RECEIPTS - 1],
        ).unwrap();
        let prior = deliver(&path, &message(), || panic!("forgot old receipt")).unwrap();
        assert_eq!(prior["duplicate"], true);
        let mut next = message();
        next.message_id = "new task".into();
        assert_eq!(
            deliver(&path, &next, || panic!("untracked new input"))
                .unwrap_err()
                .code,
            "message_receipts_full"
        );
    }

    #[test]
    fn receipt_storage_cost_is_measured() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("receipts.sqlite");
        let mut timings = Vec::new();
        for index in 0..100 {
            let mut message = message();
            message.message_id = index.to_string();
            let start = std::time::Instant::now();
            deliver(&path, &message, || Ok(())).unwrap();
            timings.push(start.elapsed());
        }
        timings.sort();
        eprintln!(
            "100 durable message receipts: median {:?}, p95 {:?}, database {} bytes",
            timings[50],
            timings[95],
            std::fs::metadata(path).unwrap().len()
        );
    }
}
