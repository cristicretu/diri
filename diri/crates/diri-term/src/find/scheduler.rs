//! Single-flight scheduling for asynchronous terminal searches.
//!
//! A search crosses two asynchronous seams: reading daemon history and then
//! scanning it on a blocking executor. [`FindSearchScheduler`] admits one
//! pipeline at a time and retains only the newest pending request. Callers do
//! not need queues, task cancellation, or knowledge of which intermediate
//! snapshots are still useful.

use super::SearchRequest;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchPhase {
    Reading,
    Scanning,
}

#[derive(Clone, Debug)]
struct ActiveSearch {
    request: SearchRequest,
    phase: SearchPhase,
    cancelled: bool,
}

/// What to do when an asynchronous history read completes.
#[must_use]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadCompletion {
    /// The completion did not belong to the active read.
    Ignore,
    /// No usable snapshot or pending request remains.
    Idle,
    /// Scan the snapshot that just completed.
    Scan,
    /// Discard that snapshot and read again for this newer request.
    Read(SearchRequest),
}

/// A recognized scan completion and the single next read, if one was dirtied
/// while the scan was active.
#[must_use]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanCompletion {
    apply_result: bool,
    next_request: Option<SearchRequest>,
}

impl ScanCompletion {
    #[must_use]
    pub const fn should_apply_result(&self) -> bool {
        self.apply_result
    }

    pub fn into_next_request(self) -> Option<SearchRequest> {
        self.next_request
    }
}

/// Per-terminal single-flight state for the history-read and blocking-scan
/// pipeline.
#[derive(Clone, Debug, Default)]
pub struct FindSearchScheduler {
    active: Option<ActiveSearch>,
    pending: Option<SearchRequest>,
}

impl FindSearchScheduler {
    /// Schedules the newest due request. Returns a request only when the caller
    /// should start a history read now; otherwise it replaces the one pending
    /// follow-up without growing a queue.
    pub fn schedule(&mut self, request: SearchRequest) -> Option<SearchRequest> {
        if self.active.is_some() {
            self.pending = Some(request);
            return None;
        }
        self.start_read(request.clone());
        Some(request)
    }

    /// Advances a matching read. A newer pending request makes the completed
    /// snapshot stale before it ever consumes blocking-pool capacity.
    pub fn finish_read(
        &mut self,
        request: &SearchRequest,
        snapshot_available: bool,
    ) -> ReadCompletion {
        let Some(active) = self.active.as_ref() else {
            return ReadCompletion::Ignore;
        };
        if active.phase != SearchPhase::Reading || active.request != *request {
            return ReadCompletion::Ignore;
        }

        if let Some(next) = self.pending.take() {
            self.start_read(next.clone());
            return ReadCompletion::Read(next);
        }
        if active.cancelled || !snapshot_available {
            self.active = None;
            return ReadCompletion::Idle;
        }
        if let Some(active) = self.active.as_mut() {
            active.phase = SearchPhase::Scanning;
        }
        ReadCompletion::Scan
    }

    /// Completes the matching blocking scan and atomically promotes only the
    /// newest pending request. `None` means the result was not the active scan.
    pub fn finish_scan(&mut self, request: &SearchRequest) -> Option<ScanCompletion> {
        let active = self.active.as_ref()?;
        if active.phase != SearchPhase::Scanning || active.request != *request {
            return None;
        }
        let apply_result = !active.cancelled;
        let next_request = self.pending.take();
        if let Some(next) = next_request.as_ref() {
            self.start_read(next.clone());
        } else {
            self.active = None;
        }
        Some(ScanCompletion {
            apply_result,
            next_request,
        })
    }

    /// Stops pending work and suppresses the active result without pretending
    /// a blocking task was cancelled. A later schedule remains single-flight
    /// behind that task and starts as soon as it really completes.
    pub fn cancel(&mut self) {
        self.pending = None;
        if let Some(active) = self.active.as_mut() {
            active.cancelled = true;
        }
    }

    fn start_read(&mut self, request: SearchRequest) {
        self.active = Some(ActiveSearch {
            request,
            phase: SearchPhase::Reading,
            cancelled: false,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::find::{SEARCH_DEBOUNCE, TerminalFindModel};

    fn next_request(model: &mut TerminalFindModel, query: &str, at: Duration) -> SearchRequest {
        model.set_query(query, at);
        model
            .take_due_search(at.saturating_add(SEARCH_DEBOUNCE))
            .expect("query search should be due")
    }

    #[test]
    fn one_active_scan_coalesces_many_invalidations_into_one_latest_follow_up() {
        let mut model = TerminalFindModel::default();
        let first = next_request(&mut model, "first", Duration::ZERO);
        let second = next_request(&mut model, "second", Duration::from_secs(1));
        let latest = next_request(&mut model, "latest", Duration::from_secs(2));
        let mut scheduler = FindSearchScheduler::default();

        assert_eq!(scheduler.schedule(first.clone()), Some(first.clone()));
        assert_eq!(scheduler.finish_read(&first, true), ReadCompletion::Scan);

        // Hold the first blocking job while multiple query/output generations
        // arrive. There is still exactly one active scan and one replaceable
        // dirty slot, never a queue of snapshots or blocking jobs.
        assert_eq!(scheduler.schedule(second), None);
        assert_eq!(scheduler.schedule(latest.clone()), None);

        let completion = scheduler
            .finish_scan(&first)
            .expect("first scan should be active");
        assert!(completion.should_apply_result());
        assert_eq!(completion.into_next_request(), Some(latest.clone()));
        assert_eq!(scheduler.finish_read(&latest, true), ReadCompletion::Scan);
        assert!(scheduler.finish_scan(&latest).is_some());
    }

    #[test]
    fn newer_request_discards_a_completed_snapshot_before_it_can_scan() {
        let mut model = TerminalFindModel::default();
        let first = next_request(&mut model, "first", Duration::ZERO);
        let latest = next_request(&mut model, "latest", Duration::from_secs(1));
        let mut scheduler = FindSearchScheduler::default();

        assert_eq!(scheduler.schedule(first.clone()), Some(first.clone()));
        assert_eq!(scheduler.schedule(latest.clone()), None);
        assert_eq!(
            scheduler.finish_read(&first, true),
            ReadCompletion::Read(latest.clone())
        );
        assert_eq!(scheduler.finish_read(&latest, true), ReadCompletion::Scan);
    }

    #[test]
    fn cancelled_active_result_is_suppressed_but_later_work_stays_single_flight() {
        let mut model = TerminalFindModel::default();
        let first = next_request(&mut model, "first", Duration::ZERO);
        let reopened = next_request(&mut model, "reopened", Duration::from_secs(1));
        let mut scheduler = FindSearchScheduler::default();

        assert_eq!(scheduler.schedule(first.clone()), Some(first.clone()));
        assert_eq!(scheduler.finish_read(&first, true), ReadCompletion::Scan);
        scheduler.cancel();
        assert_eq!(scheduler.schedule(reopened.clone()), None);

        let completion = scheduler
            .finish_scan(&first)
            .expect("cancelled scan is active");
        assert!(!completion.should_apply_result());
        assert_eq!(completion.into_next_request(), Some(reopened));
    }
}
