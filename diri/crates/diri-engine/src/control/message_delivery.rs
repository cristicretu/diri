//! Durable at-most-once receipts for MCP messages. Reserve before touching the
//! PTY: a crash between reservation and completion is unknown, never retryable.
//! No prompts are stored here, and this is outside the terminal hot path.
//!
//! Identities are kept for [`RECEIPT_TTL`], then pruned: a retry inside that
//! window is deduplicated, and the tables never fill permanently. A send that
//! fails before any input could reach the Agent releases its reservation so
//! the same identity stays deliverable.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use diri_proto::{ControlError, DeliverMessageParams};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAX_RECEIPTS: i64 = 100_000;
/// How long a message or operation identity stays reserved. Far longer than
/// any agent retry loop, short enough that the tables stay bounded.
pub(super) const RECEIPT_TTL_SECS: i64 = 7 * 24 * 60 * 60;
/// Expired identities are swept at most this often per table (or whenever the
/// table is at its cap), so a send normally pays no scan.
const PRUNE_INTERVAL_SECS: i64 = 60 * 60;

/// Why a send closure failed.
pub(super) enum SendError {
    /// Nothing reached the Agent (the session is gone, or waking it failed
    /// before any byte was written). The reservation is released.
    NotSent(ControlError),
    /// Input may have been partially written. The reservation stays.
    Uncertain,
}

pub(super) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

pub(super) fn deliver(
    path: &Path,
    message: &DeliverMessageParams,
    send: impl FnOnce() -> Result<(), SendError>,
) -> Result<Value, ControlError> {
    deliver_at(path, message, now_secs(), send)
}

fn deliver_at(
    path: &Path,
    message: &DeliverMessageParams,
    now: i64,
    send: impl FnOnce() -> Result<(), SendError>,
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
    if reserved_count(&transaction, RECEIPTS_TABLE, MAX_RECEIPTS, now).map_err(storage_error)?
        >= MAX_RECEIPTS
    {
        return Err(ControlError::new(
            "message_receipts_full",
            "message receipt storage is full; nothing was sent",
        ));
    }
    transaction.execute(
        "INSERT INTO message_receipts_v1 (identity, fingerprint, outcome) VALUES (?1, ?2, 'unknown')",
        params![key, fingerprint],
    ).map_err(storage_error)?;
    record_age(&transaction, RECEIPTS_TABLE, &key, now).map_err(storage_error)?;
    transaction.commit().map_err(storage_error)?;

    match send() {
        Ok(()) => {}
        // Nothing was written: release the identity so a retry with the same
        // message_id delivers instead of reporting `unknown` forever. If the
        // release itself fails the receipt stays `unknown`, which is safe.
        Err(SendError::NotSent(error)) => {
            return match release(&db, RECEIPTS_TABLE, &key) {
                Ok(()) => Err(error),
                Err(_) => Ok(receipt(message, "unknown", false)),
            };
        }
        // Even a partial paste or failed Enter may have affected the agent.
        // Retain the reservation; transport uncertainty cannot undo it.
        Err(SendError::Uncertain) => return Ok(receipt(message, "unknown", false)),
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

const RECEIPTS_TABLE: &str = "message_receipts_v1";

/// Records when `identity` was reserved in `table`. Ages live in a companion
/// table so the reservation tables keep the exact shape older Engines insert
/// into (`spawn_operations_v1` is written positionally).
pub(super) fn record_age(
    db: &Connection,
    table: &str,
    identity: &str,
    now: i64,
) -> rusqlite::Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO receipt_ages_v1 (tbl, identity, created_at) VALUES (?1, ?2, ?3)",
        params![table, identity, now],
    )
    .map(drop)
}

/// Deletes a reservation that provably had no effect.
pub(super) fn release(db: &Connection, table: &str, identity: &str) -> rusqlite::Result<()> {
    db.execute(
        &format!("DELETE FROM {table} WHERE identity = ?1"),
        [identity],
    )?;
    db.execute(
        "DELETE FROM receipt_ages_v1 WHERE tbl = ?1 AND identity = ?2",
        params![table, identity],
    )
    .map(drop)
}

/// Prunes identities older than [`RECEIPT_TTL_SECS`] (hourly, or whenever the
/// table is at `cap`) and returns how many reservations remain. `table` is
/// always one of this module's fixed table names.
pub(super) fn reserved_count(
    db: &Connection,
    table: &str,
    cap: i64,
    now: i64,
) -> rusqlite::Result<i64> {
    let count = |db: &Connection| {
        db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })
    };
    let last: Option<i64> = db
        .query_row(
            "SELECT pruned_at FROM receipt_prunes_v1 WHERE tbl = ?1",
            [table],
            |row| row.get(0),
        )
        .optional()?;
    // A clock that went backwards also sweeps; that only ever keeps more.
    let due = last.is_none_or(|last| !(last..last + PRUNE_INTERVAL_SECS).contains(&now));
    if !due {
        let current = count(db)?;
        if current < cap {
            return Ok(current);
        }
    }
    // Rows written by older Engines carry no age: their window starts now.
    db.execute(
        &format!(
            "INSERT OR IGNORE INTO receipt_ages_v1 (tbl, identity, created_at)
             SELECT ?1, identity, ?2 FROM {table}"
        ),
        params![table, now],
    )?;
    let cutoff = now - RECEIPT_TTL_SECS;
    db.execute(
        &format!(
            "DELETE FROM {table} WHERE identity IN
             (SELECT identity FROM receipt_ages_v1 WHERE tbl = ?1 AND created_at <= ?2)"
        ),
        params![table, cutoff],
    )?;
    db.execute(
        &format!(
            "DELETE FROM receipt_ages_v1 WHERE tbl = ?1
             AND (created_at <= ?2 OR identity NOT IN (SELECT identity FROM {table}))"
        ),
        params![table, cutoff],
    )?;
    db.execute(
        "INSERT OR REPLACE INTO receipt_prunes_v1 (tbl, pruned_at) VALUES (?1, ?2)",
        params![table, now],
    )?;
    count(db)
}

pub(super) fn digest(value: &Value) -> String {
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

pub(super) fn open(path: &Path) -> Result<Connection, ControlError> {
    let unavailable = || {
        ControlError::new(
            "message_receipts_unavailable",
            "cannot safely open message receipt storage; nothing was sent",
        )
    };
    let _file = diri_platform::security::open_private_rw(path).map_err(|_| unavailable())?;
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
        );
        CREATE TABLE IF NOT EXISTS receipt_ages_v1 (
            tbl TEXT NOT NULL,
            identity TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            PRIMARY KEY (tbl, identity)
        );
        CREATE INDEX IF NOT EXISTS receipt_ages_v1_created
            ON receipt_ages_v1 (tbl, created_at);
        CREATE TABLE IF NOT EXISTS receipt_prunes_v1 (
            tbl TEXT PRIMARY KEY,
            pruned_at INTEGER NOT NULL
        );",
    )
    .map_err(storage_error)?;
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|_| unavailable())?;
    Ok(db)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
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

    #[cfg(unix)]
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
                    Err(SendError::Uncertain)
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

    #[cfg(unix)]
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
    fn pre_effect_failures_release_the_identity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("receipts.sqlite");
        let error = deliver(&path, &message(), || {
            Err(SendError::NotSent(ControlError::not_found("s_target")))
        })
        .unwrap_err();
        assert_eq!(error.code, "not_found");
        // The same identity (e.g. the MCP auto id of identical text) is
        // still deliverable once the session is back.
        let retry = deliver(&path, &message(), || Ok(())).unwrap();
        assert_eq!(retry["delivery"], "sent");
        assert_eq!(retry["duplicate"], false);
        let ages: i64 = open(&path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM receipt_ages_v1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(ages, 1);
    }

    #[test]
    fn expired_receipts_are_pruned_and_live_ones_kept() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("receipts.sqlite");
        let t0 = 1_000_000;
        deliver_at(&path, &message(), t0, || Ok(())).unwrap();
        // A receipt an older Engine wrote has no age; its window starts at
        // the first sweep that sees it.
        open(&path)
            .unwrap()
            .execute(
                "INSERT INTO message_receipts_v1 (identity, fingerprint, outcome) VALUES ('legacy', 'f', 'sent')",
                [],
            )
            .unwrap();
        let mut other = message();
        other.message_id = "task-2".into();
        let swept = t0 + PRUNE_INTERVAL_SECS;
        deliver_at(&path, &other, swept, || Ok(())).unwrap();
        // Inside the window the identity is still deduplicated.
        let within = deliver_at(&path, &message(), t0 + RECEIPT_TTL_SECS - 1, || {
            panic!("replayed inside the at-most-once window")
        })
        .unwrap();
        assert_eq!(within["duplicate"], true);
        let mut third = message();
        third.message_id = "task-3".into();
        deliver_at(&path, &third, t0 + RECEIPT_TTL_SECS + 1, || Ok(())).unwrap();
        let identities: Vec<String> = {
            let db = open(&path).unwrap();
            let mut statement = db
                .prepare("SELECT identity FROM message_receipts_v1")
                .unwrap();
            statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(identities.len(), 3, "{identities:?}");
        assert!(identities.contains(&"legacy".to_string()));
        // After the window the original identity is new again.
        let after = deliver_at(&path, &message(), t0 + RECEIPT_TTL_SECS + 2, || Ok(())).unwrap();
        assert_eq!(after["duplicate"], false);
        // The legacy row expires one TTL after it was first seen.
        let mut fourth = message();
        fourth.message_id = "task-4".into();
        deliver_at(&path, &fourth, swept + RECEIPT_TTL_SECS + 1, || Ok(())).unwrap();
        let legacy: i64 = open(&path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM message_receipts_v1 WHERE identity = 'legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy, 0);
    }

    #[test]
    fn full_storage_recovers_once_receipts_expire() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("receipts.sqlite");
        let t0 = 1_000_000;
        deliver_at(&path, &message(), t0, || Ok(())).unwrap();
        open(&path).unwrap().execute(
            "WITH RECURSIVE entries(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM entries WHERE n < ?1)
             INSERT INTO message_receipts_v1 SELECT CAST(n AS TEXT), 'fixture', 'sent' FROM entries",
            [MAX_RECEIPTS - 1],
        ).unwrap();
        let mut next = message();
        next.message_id = "new task".into();
        assert_eq!(
            deliver_at(&path, &next, t0 + 1, || panic!("untracked new input"))
                .unwrap_err()
                .code,
            "message_receipts_full"
        );
        let later = t0 + 1 + RECEIPT_TTL_SECS + 1;
        assert_eq!(
            deliver_at(&path, &next, later, || Ok(())).unwrap()["delivery"],
            "sent"
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
