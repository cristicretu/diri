//! `session.find_history`: Find over a local terminal's whole retained
//! history, one bounded page per request.
//!
//! The Engine keeps no search state: a page is a pure function of the
//! terminal and the request, so a search costs nothing between pages, and a
//! client cancels one by not asking for the next page. Within a page the
//! screen lock is taken for one run of [`FIND_HISTORY_CHUNK_ROWS`] rows at a
//! time, so output and input never wait for more than one run.

use std::ops::DerefMut;
use std::time::{Duration, Instant};

use diri_proto::ControlError;
use diri_proto::find::{
    FIND_HISTORY_CHUNK_ROWS, FIND_HISTORY_MAX_BUDGET_MS, FIND_HISTORY_MAX_PAGE_MATCHES,
    FindHistoryParams, FindHistoryResult,
};
use diri_terminal_state::{HeadlessScreen, HistorySearch};

/// Pause between runs; see the loop.
const RUN_GAP_MICROS: u64 = 20;

/// Runs one page. `lock` is called once per run and its guard dropped before
/// the next, so other screen users interleave with the search.
pub fn find_history_page<G>(
    params: &FindHistoryParams,
    mut lock: impl FnMut() -> G,
) -> Result<FindHistoryResult, ControlError>
where
    G: DerefMut<Target = HeadlessScreen>,
{
    let mut search = HistorySearch::new(&params.query)
        .ok_or_else(|| ControlError::new("invalid_params", "Find needs a query"))?;
    let max_matches = params.max_matches.clamp(1, FIND_HISTORY_MAX_PAGE_MATCHES) as usize;
    let budget = Duration::from_millis(u64::from(
        params.budget_ms.clamp(1, FIND_HISTORY_MAX_BUDGET_MS),
    ));
    let started = Instant::now();
    let mut absolute = Vec::new();
    let mut matches = Vec::new();
    let mut before = params.before_row;
    let mut scanned = 0_i64;
    loop {
        let mut screen = lock();
        let (cols, _) = screen.size();
        let cols = cols as i64;
        if params.cols.is_some_and(|expected| expected != cols) {
            return Err(ControlError::new(
                "find_history_resized",
                "The terminal changed width; search again",
            ));
        }
        let origin = screen.history_origin();
        let total = screen.total_rows() as i64;
        // Rows evicted since the last page are gone: resume at the oldest
        // row still held.
        let from = before.map_or(total, |stable| (stable - origin).clamp(0, total));
        let next = if from > 0 {
            absolute.clear();
            let next = screen.find_rows_before(
                from as usize,
                FIND_HISTORY_CHUNK_ROWS,
                &mut search,
                max_matches.saturating_sub(matches.len()),
                &mut absolute,
            ) as i64;
            matches.extend(
                absolute
                    .iter()
                    .map(|[row, start, end]| [row + origin, *start, *end]),
            );
            scanned += from - next;
            next
        } else {
            0
        };
        let done = next == 0;
        before = Some(origin + next);
        if done || matches.len() >= max_matches || started.elapsed() >= budget {
            let history = screen.history_rows() as i64;
            return Ok(FindHistoryResult {
                history_origin: origin,
                cols,
                live_start_row: history,
                total_rows: total,
                content_seq: screen.content_seq(),
                matches,
                next_before_row: (!done).then_some(origin + next),
                scanned_rows: scanned,
            });
        }
        drop(screen);
        // Let a writer waiting on the lock in before the next run. The lock
        // is not fair: a yield alone lets this thread take it straight back
        // before a woken waiter runs, which kept a keystroke's echo waiting
        // for dozens of runs (p99 5 ms). A short sleep hands it over.
        std::thread::sleep(Duration::from_micros(RUN_GAP_MICROS));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn params(query: &str, before_row: Option<i64>, max_matches: u32) -> FindHistoryParams {
        FindHistoryParams {
            session_id: diri_proto::SessionId("s".into()),
            query: query.into(),
            before_row,
            cols: None,
            max_matches,
            budget_ms: FIND_HISTORY_MAX_BUDGET_MS,
        }
    }

    fn screen(lines: usize) -> Mutex<HeadlessScreen> {
        let mut screen = HeadlessScreen::new(80, 24);
        let mut bytes = Vec::new();
        for line in 0..lines {
            let word = if line % 997 == 0 { "needle" } else { "hay" };
            bytes.extend_from_slice(format!("{line} {word}\r\n").as_bytes());
        }
        screen.feed(&bytes);
        Mutex::new(screen)
    }

    #[test]
    fn pages_walk_older_and_cover_the_whole_history() {
        let screen = screen(100_000);
        let lock = || screen.lock().unwrap();
        let mut before = None;
        let mut found = Vec::new();
        let mut pages = 0;
        loop {
            let page = find_history_page(&params("needle", before, 16), lock).unwrap();
            pages += 1;
            found.extend(page.matches);
            match page.next_before_row {
                Some(next) => before = Some(next),
                None => break,
            }
        }
        assert_eq!(found.len(), 100_000_usize.div_ceil(997));
        assert!(pages > 1);
        assert!(found.windows(2).all(|pair| pair[0][0] > pair[1][0]));
        assert_eq!(found.last().unwrap()[0], 0, "the first line printed");
    }

    /// Pages from `before` until `want` matches or the oldest row.
    fn drain(
        screen: &Mutex<HeadlessScreen>,
        query: &str,
        mut before: Option<i64>,
        want: usize,
        locks: &mut usize,
    ) -> (Vec<[i64; 3]>, Option<i64>, i64) {
        let mut found = Vec::new();
        let mut origin = 0;
        loop {
            let page = find_history_page(&params(query, before, 16), || {
                *locks += 1;
                screen.lock().unwrap()
            })
            .unwrap();
            origin = origin.max(page.history_origin);
            found.extend(page.matches);
            before = page.next_before_row;
            if before.is_none() || found.len() >= want {
                return (found, before, origin);
            }
        }
    }

    #[test]
    fn a_page_holds_the_lock_one_run_at_a_time() {
        let screen = screen(20_000);
        let total = screen.lock().unwrap().total_rows();
        let mut locks = 0;
        let (found, next, _) = drain(&screen, "absent", None, 1, &mut locks);
        assert_eq!(next, None);
        assert!(found.is_empty());
        assert_eq!(locks, total.div_ceil(FIND_HISTORY_CHUNK_ROWS));
    }

    #[test]
    fn a_resumed_page_follows_stable_rows_across_eviction() {
        let screen = Mutex::new(HeadlessScreen::new(80, 4));
        let mut noise = Vec::new();
        for i in 0..200_000 {
            noise.extend_from_slice(format!("{i:x} {}\r\n", "q".repeat(i % 60)).as_bytes());
        }
        screen.lock().unwrap().feed(&noise);
        screen.lock().unwrap().feed(b"older marker\r\n");
        screen.lock().unwrap().feed(&noise[..noise.len() / 8]);
        screen.lock().unwrap().feed(b"newer marker\r\n");
        let mut locks = 0;
        let (first, next, origin) = drain(&screen, "marker", None, 1, &mut locks);
        assert_eq!(first.len(), 1);
        // The terminal moves on and evicts between pages.
        screen.lock().unwrap().feed(&noise[..noise.len() / 16]);
        let (second, _, later) = drain(&screen, "marker", next, 1, &mut locks);
        assert!(later > origin);
        assert_eq!(second.len(), 1, "{second:?}");
        assert!(second[0][0] < first[0][0]);
    }

    #[test]
    fn width_change_between_pages_fails_instead_of_mixing_layouts() {
        let screen = screen(100);
        let mut request = params("needle", None, 16);
        request.cols = Some(120);
        let error = find_history_page(&request, || screen.lock().unwrap()).unwrap_err();
        assert_eq!(error.code, "find_history_resized");
    }
}
