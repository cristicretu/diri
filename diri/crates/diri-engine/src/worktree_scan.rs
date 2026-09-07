//! One on-demand worker shared by all windows. Polling transfers bounded deltas,
//! not the inventory again. Jobs finish even when no view is polling; results
//! remain available until an explicit refresh. No idle worker or timer remains.
use std::sync::{Arc, Mutex};

use diri_proto::{ControlError, WorktreeOverviewEntry, WorktreeScanParams, WorktreeScanResult};

const PAGE_SIZE: usize = 32;

pub(crate) type Emit<'a> = dyn FnMut(Option<WorktreeOverviewEntry>, bool) -> bool + 'a;

#[derive(Default)]
pub(crate) struct ScanStore(Arc<Mutex<State>>);
#[derive(Default)]
struct State {
    generation: u64,
    running: bool,
    // At most two updates per discovered checkout. Replaced on a new scan.
    updates: Vec<WorktreeOverviewEntry>,
    total: usize,
    checked: usize,
    error: Option<String>,
}
impl ScanStore {
    pub(crate) fn request(
        &self,
        p: WorktreeScanParams,
        scan: impl FnOnce(bool, &mut Emit<'_>) -> Result<(), String> + Send + 'static,
    ) -> Result<WorktreeScanResult, ControlError> {
        let mut state = self
            .0
            .lock()
            .map_err(|e| ControlError::internal(e.to_string()))?;
        // A refresh while another window is scanning joins the same generation.
        if !state.running && (p.refresh || state.generation == 0) {
            *state = State {
                generation: state.generation + 1,
                running: true,
                ..Default::default()
            };
            let shared = Arc::clone(&self.0);
            if let Err(error) = std::thread::Builder::new()
                .name("diri-worktree-scan".into())
                .spawn(move || {
                    let result = scan(p.measure_disk, &mut |entry, checked| {
                        let mut state = shared.lock().expect("worktree scan state");
                        if let Some(entry) = entry {
                            state.updates.push(entry);
                            if checked {
                                state.checked += 1;
                            } else {
                                state.total += 1;
                            }
                        }
                        true
                    });
                    let mut state = shared.lock().expect("worktree scan state");
                    state.running = false;
                    state.error = result.err();
                })
            {
                state.running = false;
                state.error = Some(format!("Could not start worktree scan: {error}"));
            }
        }
        let cursor = if p.generation == Some(state.generation) {
            p.cursor.min(state.updates.len())
        } else {
            0
        };
        let end = (cursor + PAGE_SIZE).min(state.updates.len());
        Ok(WorktreeScanResult {
            generation: state.generation,
            cursor: end,
            entries: state.updates[cursor..end].to_vec(),
            total: state.total,
            checked: state.checked,
            running: state.running,
            has_more: end < state.updates.len(),
            error: state.error.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn entry(n: usize) -> WorktreeOverviewEntry {
        WorktreeOverviewEntry {
            path: format!("/repo/tree-{n}"),
            branch: Some(format!("branch-{n}")),
            project_root: "/repo".into(),
            session_id: None,
            session_status: None,
            dirty: true,
            merged: false,
            age_days: 0,
            stale_suggestion: false,
            health: Default::default(),
        }
    }
    #[test]
    fn worktree_scan_10000_entries_are_incremental_and_refreshes_share_one_worker() {
        let store = ScanStore::default();
        let (ready, started) = mpsc::channel();
        let (release, waiting) = mpsc::channel();
        let first = store
            .request(
                WorktreeScanParams {
                    refresh: true,
                    ..Default::default()
                },
                move |disk, emit| {
                    assert!(!disk);
                    for n in 0..10_000 {
                        assert!(emit(Some(entry(n)), false));
                    }
                    ready.send(()).unwrap();
                    waiting.recv().unwrap();
                    Ok(())
                },
            )
            .unwrap();
        assert!(first.running);
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let start = Instant::now();
        let mut p = WorktreeScanParams {
            refresh: true,
            generation: Some(first.generation),
            cursor: first.cursor,
            ..Default::default()
        };
        let mut count = first.entries.len();
        loop {
            let result = store
                .request(p.clone(), |_, _| panic!("refresh must join existing work"))
                .unwrap();
            assert_eq!(result.generation, first.generation);
            assert_eq!(result.total, 10_000);
            assert!(result.entries.len() <= PAGE_SIZE);
            count += result.entries.len();
            p.cursor = result.cursor;
            if !result.has_more {
                break;
            }
        }
        let elapsed = start.elapsed();
        release.send(()).unwrap();
        assert_eq!(count, 10_000);
        eprintln!(
            "10,000 incremental entries in {elapsed:?}; maximum {PAGE_SIZE} entries per reply; one worker"
        );
        assert!(elapsed < Duration::from_secs(1));
        // A client from a different generation receives the beginning, never a
        // cursor accidentally applied to a different result set.
        let reset = store
            .request(
                WorktreeScanParams {
                    generation: Some(0),
                    cursor: 9999,
                    ..Default::default()
                },
                |_, _| unreachable!(),
            )
            .unwrap();
        assert_eq!(reset.entries[0].path, "/repo/tree-0");
        assert_eq!(reset.cursor, PAGE_SIZE);
    }

    #[test]
    fn worktree_scan_continues_when_the_last_view_stops_polling() {
        let store = ScanStore::default();
        let first = store
            .request(Default::default(), move |_, emit| {
                assert!(emit(Some(entry(0)), false));
                // Outlive the former ten-second view lease without any polls.
                std::thread::sleep(Duration::from_secs(11));
                if !emit(Some(entry(0)), true) {
                    return Err("closing Worktrees cancelled the engine job".into());
                }
                Ok(())
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        // Observe completion directly so the test cannot keep a view lease alive.
        while store.0.lock().unwrap().running {
            assert!(Instant::now() < deadline, "background scan did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
        let reopened = store
            .request(Default::default(), |_, _| {
                panic!("reopening must reuse the scan")
            })
            .unwrap();
        assert_eq!(reopened.generation, first.generation);
        assert_eq!(reopened.total, 1);
        assert_eq!(reopened.checked, 1);
        assert_eq!(reopened.entries.len(), 2);
        assert!(!reopened.running);
        assert_eq!(reopened.error, None);

        let refreshed = store
            .request(
                WorktreeScanParams {
                    refresh: true,
                    ..Default::default()
                },
                |_, _| Ok(()),
            )
            .unwrap();
        assert_eq!(refreshed.generation, first.generation + 1);
    }
}
