//! Ordered, non-blocking application of Agent hook reports.
//!
//! Claude Code runs every hook synchronously, so `hook.report` latency is
//! Agent latency. Applying a report needs the Registry lock, which lifecycle
//! operations hold across slow work: `session.remove` holds it while the
//! Holder's TERM→KILL escalation waits up to 500 ms for the Agent, and a
//! closing Claude waits on its own `SessionEnd` hook, which waited on that same
//! lock. Every close therefore ran into the escalation and lost `SessionEnd`.
//!
//! An uncontended report still applies inline, before the reply. A report
//! that finds the Registry busy, or reports still queued ahead of it, is
//! queued in arrival order to one applier thread and answered at once. Per
//! Agent, arrival order is callback order (the provider waits for each hook
//! process to exit before the next), so a session's reports are never
//! reordered. Before delivery the CLI already wrote the report's lifecycle
//! facts to the session's recovery store, which is what restart recovery
//! reads; the Registry's own write stays on the persistence flusher.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::Instant;

use crate::events::EventBus;
use crate::registry::Registry;

/// Queue depth before a submitter blocks (backpressure, never a drop).
const QUEUE_CAPACITY: usize = 1024;

pub(super) struct HookReport {
    pub(super) session_id: String,
    pub(super) signal: crate::status::StatusSignal,
    pub(super) meta: crate::hooks::HookMetadata,
    pub(super) session_end: bool,
}

/// A report and when it was handed to the applier.
type Queued = (HookReport, Instant);

pub(super) struct HookQueue {
    /// Serializes the inline-or-queue decision so an inline report can never
    /// overtake one that is already queued.
    order: Mutex<Option<SyncSender<Queued>>>,
    /// Reports submitted to the applier and not yet applied.
    pending: Arc<AtomicUsize>,
}

impl HookQueue {
    pub(super) fn new() -> Self {
        Self {
            order: Mutex::new(None),
            pending: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[cfg(all(test, unix))]
    pub(super) fn is_idle(&self) -> bool {
        self.pending.load(Ordering::Acquire) == 0
    }

    /// Applies `report` now if the Registry is free and nothing is queued;
    /// otherwise hands it to the applier. Returns whether it applied inline,
    /// or `Err` for a poisoned Registry (as every other request reports).
    pub(super) fn submit(
        &self,
        registry: &Arc<Mutex<Registry>>,
        events: &EventBus,
        report: HookReport,
    ) -> Result<bool, PoisonedRegistry> {
        let mut sender = self.order.lock().unwrap_or_else(|error| error.into_inner());
        if self.pending.load(Ordering::Acquire) == 0 {
            match registry.try_lock() {
                Ok(mut locked) => {
                    apply(&mut locked, events, report);
                    return Ok(true);
                }
                Err(TryLockError::Poisoned(_)) => return Err(PoisonedRegistry),
                Err(TryLockError::WouldBlock) => {}
            }
        }
        let sender = sender.get_or_insert_with(|| {
            spawn_applier(Arc::clone(registry), events.clone(), &self.pending)
        });
        self.pending.fetch_add(1, Ordering::AcqRel);
        diri_telemetry::count("hook.queued", 1);
        if let Err(error) = sender.send((report, Instant::now())) {
            // The applier is gone (it cannot normally exit): apply here, still
            // in order because `order` is held.
            self.pending.fetch_sub(1, Ordering::AcqRel);
            let mut locked = registry.lock().map_err(|_| PoisonedRegistry)?;
            apply(&mut locked, events, error.0.0);
            return Ok(true);
        }
        Ok(false)
    }
}

#[derive(Debug)]
pub(super) struct PoisonedRegistry;

fn spawn_applier(
    registry: Arc<Mutex<Registry>>,
    events: EventBus,
    pending: &Arc<AtomicUsize>,
) -> SyncSender<Queued> {
    let (sender, receiver) = sync_channel::<Queued>(QUEUE_CAPACITY);
    let pending = Arc::clone(pending);
    let spawned = std::thread::Builder::new()
        .name("diri-hook-applier".into())
        .spawn(move || {
            while let Ok((report, queued_at)) = receiver.recv() {
                // A poisoned Registry fails every request; drop the report
                // rather than fold it into state another thread left torn.
                if let Ok(mut locked) = registry.lock() {
                    // How stale a queued report's status is when it lands:
                    // the wait the Agent no longer pays.
                    diri_telemetry::observe("hook.apply_wait", queued_at.elapsed());
                    apply(&mut locked, &events, report);
                }
                pending.fetch_sub(1, Ordering::AcqRel);
            }
        });
    if spawned.is_err() {
        // No thread: dropping the receiver makes `send` fail, and `submit`
        // then applies inline (the pre-queue behavior).
        eprintln!("diri-engine: hook applier thread did not start; applying hooks inline");
    }
    sender
}

/// Folds one report into the Registry and publishes the record. The disk
/// write goes to the persistence flusher (at most its 500 ms debounce away,
/// the same bound `persist` already allowed) instead of fsyncing under the
/// Registry lock on the Agent's hook path.
fn apply(registry: &mut Registry, events: &EventBus, report: HookReport) {
    if report.session_end
        && let Some(session) = registry.get(&report.session_id)
    {
        session.note_agent_ended();
    }
    if registry.apply_hook_report(&report.session_id, report.signal, &report.meta) {
        registry.persist_deferred();
    }
    if let Some(record) = registry.record(&report.session_id) {
        events.publish_encoded(
            diri_proto::EventName::SESSION_UPDATED,
            &record,
            Some(&report.session_id),
        );
    }
}
