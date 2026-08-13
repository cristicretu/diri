//! Caller-scoped idempotency for daemon-owned mutations.
//!
//! The stdio MCP adapter is intentionally disposable. Retry state therefore
//! lives here, in the long-running Engine, and is shared by every control
//! connection. Successful mutations are retained; failures release the key
//! unless their outcome proves or may imply that the mutation already
//! committed.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use diri_proto::ControlError;
use serde_json::Value;

const DEFAULT_ENTRY_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const DEFAULT_MAX_ENTRIES_PER_CALLER: usize = 128;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RequestIdentity {
    caller: String,
    generation: u64,
    tool: &'static str,
    request_key: String,
}

#[derive(Clone, Debug)]
enum EntryState {
    Running,
    Complete {
        result: Result<Value, ControlError>,
        completed_at: Instant,
    },
}

#[derive(Clone, Debug)]
struct Entry {
    fingerprint: String,
    state: EntryState,
}

#[derive(Debug)]
struct State {
    entries: HashMap<RequestIdentity, Entry>,
    caller_generations: HashMap<String, u64>,
    retired_callers: HashMap<String, Instant>,
    next_generation: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            caller_generations: HashMap::new(),
            retired_callers: HashMap::new(),
            next_generation: 1,
        }
    }
}

impl State {
    fn allocate_generation(&mut self, caller: &str) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        self.caller_generations
            .insert(caller.to_owned(), generation);
        generation
    }
}

/// A bounded, process-lifetime registry for successful mutation results.
///
/// Concurrent callers wait behind the first owner of a key. Reusing a key
/// with different normalized arguments is rejected before the mutation runs.
pub struct MutationLedger {
    state: Mutex<State>,
    changed: Condvar,
    entry_ttl: Duration,
    max_entries_per_caller: usize,
}

impl Default for MutationLedger {
    fn default() -> Self {
        Self::new(DEFAULT_ENTRY_TTL, DEFAULT_MAX_ENTRIES_PER_CALLER)
    }
}

impl MutationLedger {
    pub fn new(entry_ttl: Duration, max_entries_per_caller: usize) -> Self {
        Self {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            entry_ttl,
            max_entries_per_caller: max_entries_per_caller.max(1),
        }
    }

    /// Runs `mutation` at most once for a successful identity/fingerprint.
    ///
    /// Pre-commit errors are deliberately not cached. Panics fail closed as a
    /// sticky uncertain outcome because they cannot prove whether the
    /// mutation had already crossed its irreversible commit point.
    pub fn run(
        &self,
        caller: &str,
        tool: &'static str,
        request_key: &str,
        fingerprint: String,
        mutation: impl FnOnce() -> Result<Value, ControlError>,
    ) -> Result<Value, ControlError> {
        let mut state = self.state.lock().map_err(poisoned)?;
        let generation = state
            .caller_generations
            .get(caller)
            .copied()
            .unwrap_or_else(|| state.allocate_generation(caller));
        let identity = RequestIdentity {
            caller: caller.to_owned(),
            generation,
            tool,
            request_key: request_key.to_owned(),
        };
        loop {
            self.prune_expired(&mut state);
            if state.retired_callers.contains_key(caller)
                || state.caller_generations.get(caller).copied() != Some(generation)
            {
                return Err(caller_retired(caller));
            }
            match state.entries.get(&identity) {
                Some(entry) if entry.fingerprint != fingerprint => {
                    return Err(ControlError::new(
                        "idempotency_conflict",
                        "requestKey was already used with different arguments",
                    ));
                }
                Some(Entry {
                    state: EntryState::Complete { result, .. },
                    ..
                }) => return result.clone(),
                Some(Entry {
                    state: EntryState::Running,
                    ..
                }) => {
                    state = self.changed.wait(state).map_err(poisoned)?;
                }
                None => {
                    self.make_room(&mut state, caller, generation)?;
                    state.entries.insert(
                        identity.clone(),
                        Entry {
                            fingerprint: fingerprint.clone(),
                            state: EntryState::Running,
                        },
                    );
                    break;
                }
            }
        }
        drop(state);

        let mut reservation = Reservation {
            ledger: self,
            identity,
            fingerprint,
            finished: false,
        };
        match catch_unwind(AssertUnwindSafe(mutation)) {
            Ok(result) => {
                if match &result {
                    Ok(_) => true,
                    Err(error) => cacheable_error(error),
                } {
                    reservation.complete(result.clone());
                }
                result
            }
            Err(_) => {
                // Once arbitrary mutation code has started, a panic cannot
                // prove whether a process/worktree was already created. Keep
                // the key sticky so recovery can inspect state but can never
                // accidentally repeat the mutation.
                let error = ControlError::new(
                    "idempotency_outcome_uncertain",
                    "the mutation panicked after its outcome became uncertain",
                );
                reservation.complete(Err(error.clone()));
                Err(error)
            }
        }
    }

    /// Retires a caller lifetime without reopening any mutation already in
    /// flight. Completed history is discarded immediately; running owners are
    /// allowed to settle, while all old waiters fail closed.
    pub fn forget_caller(&self, caller: &str) {
        if let Ok(mut state) = self.state.lock() {
            self.prune_expired(&mut state);
            state
                .retired_callers
                .insert(caller.to_owned(), Instant::now());
            state.entries.retain(|identity, entry| {
                identity.caller != caller || matches!(entry.state, EntryState::Running)
            });
            self.changed.notify_all();
        }
    }

    /// Begins a new lifetime for a session ID that was explicitly removed and
    /// later reopened. The generation makes old owners and waiters incapable
    /// of completing or reserving entries in the new lifetime.
    pub fn activate_caller(&self, caller: &str) {
        if let Ok(mut state) = self.state.lock()
            && state.retired_callers.remove(caller).is_some()
        {
            state.allocate_generation(caller);
            self.changed.notify_all();
        }
    }

    fn prune_expired(&self, state: &mut State) {
        let ttl = self.entry_ttl;
        state.entries.retain(|_, entry| match entry.state {
            EntryState::Running => true,
            EntryState::Complete {
                ref result,
                completed_at,
            } => sticky_result(result) || completed_at.elapsed() < ttl,
        });

        // Tombstones only coordinate requests that crossed a caller-removal
        // race. Once their TTL elapsed and no old owner remains, the daemon
        // does not need to retain removed session IDs forever.
        let expired_callers = state
            .retired_callers
            .iter()
            .filter(|(caller, retired_at)| {
                retired_at.elapsed() >= ttl
                    && !state
                        .entries
                        .keys()
                        .any(|identity| &identity.caller == *caller)
            })
            .map(|(caller, _)| caller.clone())
            .collect::<Vec<_>>();
        for caller in expired_callers {
            state.retired_callers.remove(&caller);
            state.caller_generations.remove(&caller);
        }
    }

    fn make_room(
        &self,
        state: &mut State,
        caller: &str,
        generation: u64,
    ) -> Result<(), ControlError> {
        let caller_count = state
            .entries
            .keys()
            .filter(|identity| identity.caller == caller && identity.generation == generation)
            .count();
        if caller_count < self.max_entries_per_caller {
            return Ok(());
        }

        let oldest = state
            .entries
            .iter()
            .filter_map(|(identity, entry)| match entry.state {
                EntryState::Complete {
                    ref result,
                    completed_at,
                } if identity.caller == caller
                    && identity.generation == generation
                    && !sticky_result(result) =>
                {
                    Some((identity.clone(), completed_at))
                }
                _ => None,
            })
            .min_by_key(|(_, completed_at)| *completed_at)
            .map(|(identity, _)| identity);
        if let Some(oldest) = oldest {
            state.entries.remove(&oldest);
            Ok(())
        } else {
            Err(ControlError::new(
                "idempotency_capacity",
                "too many idempotent mutations are already running for this caller",
            ))
        }
    }
}

struct Reservation<'a> {
    ledger: &'a MutationLedger,
    identity: RequestIdentity,
    fingerprint: String,
    finished: bool,
}

impl Reservation<'_> {
    fn complete(&mut self, result: Result<Value, ControlError>) {
        if let Ok(mut state) = self.ledger.state.lock() {
            let is_current = !state.retired_callers.contains_key(&self.identity.caller)
                && state.caller_generations.get(&self.identity.caller).copied()
                    == Some(self.identity.generation);
            let owns_entry = state.entries.get(&self.identity).is_some_and(|entry| {
                entry.fingerprint == self.fingerprint && matches!(entry.state, EntryState::Running)
            });
            if owns_entry {
                if is_current {
                    state
                        .entries
                        .get_mut(&self.identity)
                        .expect("checked")
                        .state = EntryState::Complete {
                        result,
                        completed_at: Instant::now(),
                    };
                } else {
                    state.entries.remove(&self.identity);
                }
                self.finished = true;
                self.ledger.changed.notify_all();
            }
        }
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Ok(mut state) = self.ledger.state.lock() {
            let should_remove = state.entries.get(&self.identity).is_some_and(|entry| {
                entry.fingerprint == self.fingerprint && matches!(entry.state, EntryState::Running)
            });
            if should_remove {
                state.entries.remove(&self.identity);
            }
            self.ledger.changed.notify_all();
        }
    }
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> ControlError {
    ControlError::internal("idempotency registry lock was poisoned")
}

fn caller_retired(caller: &str) -> ControlError {
    ControlError::new(
        "idempotency_caller_retired",
        format!("caller session {caller:?} was removed while the mutation was running"),
    )
}

/// A prompt-delivery failure is reported only after the session exists. It is
/// retained to prevent a retry from creating a second child. Validation and
/// infrastructure failures happen before acknowledgement and remain retryable.
fn cacheable_error(error: &ControlError) -> bool {
    error.code == "initial_prompt_delivery_failed"
}

/// Outcomes that may follow an irreversible side effect are never evicted or
/// expired within a caller lifetime. The per-caller capacity remains the hard
/// memory bound; retirement clears them when that lifetime ends.
fn sticky_result(result: &Result<Value, ControlError>) -> bool {
    result.as_ref().is_err_and(|error| {
        error.code == "initial_prompt_delivery_failed"
            || error.code == "idempotency_outcome_uncertain"
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn concurrent_retries_share_one_success() {
        let ledger = Arc::new(MutationLedger::default());
        let mutations = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(std::sync::Barrier::new(3));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let ledger = Arc::clone(&ledger);
            let mutations = Arc::clone(&mutations);
            let gate = Arc::clone(&gate);
            threads.push(std::thread::spawn(move || {
                gate.wait();
                ledger
                    .run("parent", "spawn_agent", "turn-7", "same".into(), || {
                        mutations.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(30));
                        Ok(serde_json::json!({"id": "s_once"}))
                    })
                    .expect("spawn")
            }));
        }
        gate.wait();
        let values = threads
            .into_iter()
            .map(|thread| thread.join().expect("join"))
            .collect::<Vec<_>>();
        assert_eq!(mutations.load(Ordering::SeqCst), 1);
        assert_eq!(values[0], values[1]);
    }

    #[test]
    fn conflicts_are_scoped_and_failures_are_retryable() {
        let ledger = MutationLedger::default();
        let first = ledger
            .run("one", "spawn_agent", "k", "a".into(), || {
                Ok(serde_json::json!({"id": "s_one"}))
            })
            .expect("first");
        let conflict = ledger
            .run("one", "spawn_agent", "k", "b".into(), || unreachable!())
            .expect_err("conflict");
        assert_eq!(conflict.code, "idempotency_conflict");

        let other = ledger
            .run("two", "spawn_agent", "k", "b".into(), || {
                Ok(serde_json::json!({"id": "s_two"}))
            })
            .expect("other caller");
        assert_ne!(first, other);

        let transient = ledger.run("one", "spawn_agent", "retry", "x".into(), || {
            Err(ControlError::internal("temporary"))
        });
        assert!(transient.is_err());
        let retried = ledger
            .run("one", "spawn_agent", "retry", "x".into(), || {
                Ok(serde_json::json!({"id": "s_retry"}))
            })
            .expect("retry");
        assert_eq!(retried["id"], "s_retry");

        let runs = AtomicUsize::new(0);
        for _ in 0..2 {
            let failure = ledger
                .run("one", "spawn_agent", "partial", "x".into(), || {
                    runs.fetch_add(1, Ordering::SeqCst);
                    Err(ControlError::new(
                        "initial_prompt_delivery_failed",
                        "session s_created already exists",
                    ))
                })
                .expect_err("sticky post-mutation failure");
            assert_eq!(failure.code, "initial_prompt_delivery_failed");
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn caller_removal_and_capacity_bound_retention() {
        let ledger = MutationLedger::new(Duration::from_secs(60), 1);
        let first = ledger
            .run("one", "spawn_agent", "a", "a".into(), || {
                Ok(serde_json::json!({"id": 1}))
            })
            .expect("first");
        let second = ledger
            .run("one", "spawn_agent", "b", "b".into(), || {
                Ok(serde_json::json!({"id": 2}))
            })
            .expect("evicts oldest");
        assert_ne!(first, second);

        ledger.forget_caller("one");
        ledger.activate_caller("one");
        let replayed_as_new = ledger
            .run("one", "spawn_agent", "b", "b".into(), || {
                Ok(serde_json::json!({"id": 3}))
            })
            .expect("new lifetime");
        assert_eq!(replayed_as_new["id"], 3);
    }

    #[test]
    fn retiring_a_caller_never_reopens_its_running_key() {
        let ledger = Arc::new(MutationLedger::default());
        let mutations = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let owner = {
            let ledger = Arc::clone(&ledger);
            let mutations = Arc::clone(&mutations);
            std::thread::spawn(move || {
                ledger.run("parent", "spawn_agent", "same", "same".into(), || {
                    mutations.fetch_add(1, Ordering::SeqCst);
                    started_tx.send(()).expect("announce mutation");
                    finish_rx.recv().expect("finish mutation");
                    Ok(serde_json::json!({"id": "s_only"}))
                })
            })
        };
        started_rx.recv().expect("owner started");
        ledger.forget_caller("parent");

        let retry = ledger
            .run("parent", "spawn_agent", "same", "same".into(), || {
                mutations.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::json!({"id": "s_duplicate"}))
            })
            .expect_err("retired caller must fail closed");
        assert_eq!(retry.code, "idempotency_caller_retired");
        assert_eq!(mutations.load(Ordering::SeqCst), 1);

        finish_tx.send(()).expect("release owner");
        assert_eq!(
            owner.join().expect("owner thread").expect("owner result")["id"],
            "s_only"
        );
        assert_eq!(mutations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn panics_become_sticky_uncertain_outcomes() {
        let ledger = MutationLedger::new(Duration::ZERO, 128);
        let mutations = AtomicUsize::new(0);
        for _ in 0..2 {
            let error = ledger
                .run("parent", "spawn_agent", "panic", "same".into(), || {
                    mutations.fetch_add(1, Ordering::SeqCst);
                    panic!("after a hypothetical process launch")
                })
                .expect_err("panic must fail closed");
            assert_eq!(error.code, "idempotency_outcome_uncertain");
        }
        assert_eq!(mutations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn settled_retirement_tombstones_expire_without_generation_aliasing() {
        let ledger = MutationLedger::new(Duration::ZERO, 8);
        ledger
            .run("old", "spawn_agent", "k", "same".into(), || {
                Ok(serde_json::json!({"id": "s_old"}))
            })
            .expect("old lifetime");
        ledger.forget_caller("old");

        ledger
            .run("other", "spawn_agent", "k", "same".into(), || {
                Ok(serde_json::json!({"id": "s_other"}))
            })
            .expect("trigger pruning");

        let state = ledger.state.lock().expect("ledger");
        assert!(!state.retired_callers.contains_key("old"));
        assert!(!state.caller_generations.contains_key("old"));
    }
}
