//! Polls GitHub (via the `gh` CLI) for the state of every PR URL captured as
//! a session artifact: open/merged/closed, draft, review decision,
//! mergeability, CI checks, comment counts, and +/- line stats.
//!
//! Ported from `PullRequestMonitor`. Results land on
//! `SessionRecord.pullRequests`; one shared per-URL cache dedupes PRs that
//! appear in several sessions; fetches per sweep are capped so a screen full
//! of PR links can't turn one sweep into a minute of serial gh calls.
//! Silently inert when `gh` isn't installed.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use diri_proto::{ArtifactKind, DateMillis, PrCheck, PrDiscussionItem, PullRequestStatus};
use serde_json::Value;

use crate::attach::AttachHub;
use crate::events::EventBus;
use crate::registry::Registry;

/// Match the GitHub Pull Requests extension: active PR UI refreshes once a
/// minute, while background state starts at five minutes and backs off.
const FOREGROUND_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const BACKGROUND_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const MAX_BACKGROUND_REFRESH_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Review-thread resolution costs a separate GraphQL subprocess; refresh it
/// much less often than the main PR state.
const THREAD_REFRESH_TTL: Duration = Duration::from_secs(1800);
/// Bound each batch, but immediately drain any remaining due work rather than
/// making it wait for another global sweep.
const MAX_FETCHES_PER_BATCH: usize = 2;
/// Recently-seen window: records viewed within this qualify for polling even
/// when no client is attached right now.
const RECENTLY_SEEN: Duration = Duration::from_secs(600);
/// A missed wake cannot strand work forever; this is a local reconciliation
/// only and does not imply a GitHub request when nothing is due.
const IDLE_RECONCILE_INTERVAL: Duration = Duration::from_secs(30 * 60);
const STOP_CHECK_INTERVAL: Duration = Duration::from_secs(1);

fn initial_sweep_delay() -> Duration {
    Duration::ZERO
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PollInterest {
    Background,
    Foreground,
}

fn poll_interest(attached: bool, foreground_active: bool) -> PollInterest {
    if attached && foreground_active {
        PollInterest::Foreground
    } else {
        PollInterest::Background
    }
}

#[derive(Debug)]
struct RefreshState {
    last_attempt: Option<Instant>,
    background_interval: Duration,
}

impl Default for RefreshState {
    fn default() -> Self {
        Self {
            last_attempt: None,
            background_interval: BACKGROUND_REFRESH_INTERVAL,
        }
    }
}

impl RefreshState {
    fn interval(&self, interest: PollInterest) -> Duration {
        match interest {
            PollInterest::Foreground => FOREGROUND_REFRESH_INTERVAL,
            PollInterest::Background => self.background_interval,
        }
    }

    fn record_result(&mut self, interest: PollInterest, changed: bool) {
        if changed {
            self.background_interval = BACKGROUND_REFRESH_INTERVAL;
        } else if interest == PollInterest::Background {
            self.background_interval = std::cmp::min(
                MAX_BACKGROUND_REFRESH_INTERVAL,
                self.background_interval.saturating_mul(2),
            );
        }
    }
}

#[derive(Default)]
struct PendingWake {
    reconcile: bool,
    foreground: bool,
    sessions: HashSet<String>,
}

impl PendingWake {
    fn is_empty(&self) -> bool {
        !self.reconcile && !self.foreground && self.sessions.is_empty()
    }
}

struct WakeInner {
    pending: Mutex<PendingWake>,
    ready: Condvar,
    foreground_active: AtomicBool,
}

/// Event-driven invalidation for the PR monitor. The control server signals
/// focus/selection and the governor signals newly discovered artifacts.
#[derive(Clone)]
pub struct PrMonitorWake {
    inner: Arc<WakeInner>,
}

impl Default for PrMonitorWake {
    fn default() -> Self {
        Self {
            inner: Arc::new(WakeInner {
                pending: Mutex::new(PendingWake::default()),
                ready: Condvar::new(),
                // The desktop store starts active and only emits a transition
                // when that changes, so the daemon must share that default.
                foreground_active: AtomicBool::new(true),
            }),
        }
    }
}

impl PrMonitorWake {
    pub fn wake_session(&self, session_id: impl Into<String>) {
        let mut pending = self.inner.pending.lock().expect("PR monitor wake");
        pending.sessions.insert(session_id.into());
        drop(pending);
        self.inner.ready.notify_one();
    }

    /// A foreground transition refreshes only attached sessions, not every
    /// record that happened to be viewed in the recent window.
    pub fn set_foreground_active(&self, active: bool) {
        self.inner.foreground_active.store(active, Ordering::SeqCst);
        let mut pending = self.inner.pending.lock().expect("PR monitor wake");
        pending.reconcile = true;
        pending.foreground |= active;
        drop(pending);
        self.inner.ready.notify_one();
    }

    pub(crate) fn foreground_active(&self) -> bool {
        self.inner.foreground_active.load(Ordering::SeqCst)
    }

    fn wait(&self, timeout: Duration, stop: &AtomicBool) -> PendingWake {
        if timeout.is_zero() {
            return PendingWake::default();
        }
        let deadline = Instant::now() + timeout;
        let mut pending = self.inner.pending.lock().expect("PR monitor wake");
        loop {
            if !pending.is_empty() || stop.load(Ordering::SeqCst) {
                return std::mem::take(&mut *pending);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return PendingWake::default();
            };
            let wait_for = remaining.min(STOP_CHECK_INTERVAL);
            let (next, wait) = self
                .inner
                .ready
                .wait_timeout(pending, wait_for)
                .expect("PR monitor wake");
            pending = next;
            if wait.timed_out() && remaining <= STOP_CHECK_INTERVAL {
                return PendingWake::default();
            }
        }
    }
}

pub fn spawn_pr_monitor(
    registry: Arc<Mutex<Registry>>,
    events: EventBus,
    attach: AttachHub,
    wake: PrMonitorWake,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("diri-pr-monitor".into())
        .spawn(move || {
            let Some(gh) = resolve_gh() else {
                eprintln!("dirijord-rs: pull-request monitor idle: gh not on PATH");
                return;
            };
            let mut cache: HashMap<String, PullRequestStatus> = HashMap::new();
            let mut refresh: HashMap<String, RefreshState> = HashMap::new();
            let mut last_thread_attempt: HashMap<String, Instant> = HashMap::new();
            let mut forced_urls = HashSet::new();
            let mut delay = initial_sweep_delay();
            loop {
                let pending = wake.wait(delay, &stop);
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                delay = sweep(
                    &registry,
                    &events,
                    &attach,
                    &gh,
                    &mut cache,
                    &mut refresh,
                    &mut last_thread_attempt,
                    &mut forced_urls,
                    pending,
                    wake.foreground_active(),
                );
            }
        })
        .expect("spawn pr monitor")
}

#[allow(clippy::too_many_arguments)]
fn sweep(
    registry: &Arc<Mutex<Registry>>,
    events: &EventBus,
    attach: &AttachHub,
    gh: &str,
    cache: &mut HashMap<String, PullRequestStatus>,
    refresh: &mut HashMap<String, RefreshState>,
    last_thread_attempt: &mut HashMap<String, Instant>,
    forced_urls: &mut HashSet<String>,
    pending: PendingWake,
    foreground_active: bool,
) -> Duration {
    // PR URLs worth polling: sessions currently attached, or viewed recently.
    // Merely being a live restored process is not evidence anyone is looking
    // at its PR pill.
    let records = {
        let Ok(guard) = registry.lock() else {
            return IDLE_RECONCILE_INTERVAL;
        };
        guard.records()
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as f64;
    let mut wanted: Vec<(String, Vec<String>)> = Vec::new();
    let mut targets: HashMap<String, PollInterest> = HashMap::new();
    for record in &records {
        let recently_seen = record
            .last_seen_at
            .as_ref()
            .is_some_and(|seen| now_ms - seen.0 < RECENTLY_SEEN.as_millis() as f64);
        let attached = attach.has_sinks(&record.id.0);
        if !(attached || recently_seen) {
            continue;
        }
        let urls: Vec<String> = record
            .artifacts
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|artifact| artifact.kind == ArtifactKind::PullRequest)
            .map(|artifact| artifact.url.clone())
            .collect();
        if !urls.is_empty() {
            let interest = poll_interest(attached, foreground_active);
            for url in &urls {
                targets
                    .entry(url.clone())
                    .and_modify(|current| *current = (*current).max(interest))
                    .or_insert(interest);
            }
            if pending.sessions.contains(&record.id.0) || (pending.foreground && attached) {
                forced_urls.extend(urls.iter().cloned());
            }
            if let Some(statuses) = &record.pull_requests {
                for status in statuses {
                    let replace = cache
                        .get(&status.url)
                        .is_none_or(|cached| cached.fetched_at < status.fetched_at);
                    if replace {
                        cache.insert(status.url.clone(), status.clone());
                    }
                }
            }
            wanted.push((record.id.0.clone(), urls));
        }
    }
    if wanted.is_empty() {
        forced_urls.clear();
        return IDLE_RECONCILE_INTERVAL;
    }
    forced_urls.retain(|url| targets.contains_key(url));

    // Foreground first, then never-attempted/oldest. The batch cap protects
    // serial gh subprocesses; remaining due URLs make the returned delay zero
    // and are drained immediately by the next iteration.
    let now = Instant::now();
    let mut due: Vec<(String, PollInterest, Option<Instant>)> = targets
        .iter()
        .filter_map(|(url, interest)| {
            let state = refresh.entry(url.clone()).or_default();
            let forced = forced_urls.contains(url);
            let is_due = forced
                || state.last_attempt.is_none_or(|at| {
                    now.saturating_duration_since(at) >= state.interval(*interest)
                });
            is_due.then_some((url.clone(), *interest, state.last_attempt))
        })
        .collect();
    due.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
    for (url, interest, _) in due.into_iter().take(MAX_FETCHES_PER_BATCH) {
        forced_urls.remove(&url);
        refresh.entry(url.clone()).or_default().last_attempt = Some(Instant::now());
        let refresh_threads = last_thread_attempt
            .get(&url)
            .is_none_or(|at| at.elapsed() >= THREAD_REFRESH_TTL);
        if refresh_threads {
            last_thread_attempt.insert(url.clone(), Instant::now());
        }
        let mut changed = false;
        if let Some(mut status) = fetch(&url, gh, refresh_threads) {
            if !refresh_threads && let Some(previous) = cache.get(&url) {
                status.resolved_threads = previous.resolved_threads;
                status.total_threads = previous.total_threads;
            }
            changed = cache
                .get(&url)
                .is_none_or(|previous| !status_materially_same(previous, &status));
            cache.insert(url.clone(), status);
        }
        refresh
            .entry(url)
            .or_default()
            .record_result(interest, changed);
    }

    for (id, urls) in wanted {
        let statuses: Vec<PullRequestStatus> = urls
            .iter()
            .filter_map(|url| cache.get(url).cloned())
            .collect();
        let record = {
            let Ok(mut guard) = registry.lock() else {
                return IDLE_RECONCILE_INTERVAL;
            };
            let changed = guard.apply_pull_request_statuses(&id, statuses);
            if changed {
                let _ = guard.persist();
                guard.records().into_iter().find(|record| record.id.0 == id)
            } else {
                None
            }
        };
        if let Some(record) = record {
            events.publish_encoded(diri_proto::EventName::SESSION_UPDATED, &record, Some(&id));
        }
    }

    next_refresh_delay(&targets, refresh, forced_urls, Instant::now())
}

fn next_refresh_delay(
    targets: &HashMap<String, PollInterest>,
    refresh: &HashMap<String, RefreshState>,
    forced_urls: &HashSet<String>,
    now: Instant,
) -> Duration {
    targets
        .iter()
        .map(|(url, interest)| {
            if forced_urls.contains(url) {
                return Duration::ZERO;
            }
            let Some(state) = refresh.get(url) else {
                return Duration::ZERO;
            };
            let Some(last_attempt) = state.last_attempt else {
                return Duration::ZERO;
            };
            state
                .interval(*interest)
                .saturating_sub(now.saturating_duration_since(last_attempt))
        })
        .min()
        .unwrap_or(IDLE_RECONCILE_INTERVAL)
}

fn status_materially_same(a: &PullRequestStatus, b: &PullRequestStatus) -> bool {
    let mut b_pinned = b.clone();
    b_pinned.fetched_at = a.fetched_at;
    *a == b_pinned
}

fn resolve_gh() -> Option<String> {
    let path = std::env::var("PATH").ok()?;
    path.split(':')
        .map(|dir| std::path::Path::new(dir).join("gh"))
        .find(|candidate| candidate.is_file())
        .map(|path| path.to_string_lossy().into_owned())
}

/// `gh pr view <url> --json …` plus a GraphQL round trip for review-thread
/// resolution (which `pr view` can't report). None on any failure — the last
/// cached status stays in effect.
pub fn fetch(url: &str, gh: &str, include_threads: bool) -> Option<PullRequestStatus> {
    const FIELDS: &str = "number,title,author,body,baseRefName,headRefName,state,isDraft,\
        reviewDecision,mergeable,mergeStateStatus,additions,deletions,changedFiles,\
        comments,reviews,statusCheckRollup";
    let data = run_gh(
        gh,
        &["pr", "view", url, "--json", FIELDS],
        Duration::from_secs(15),
    )?;
    let mut status = parse(&data, url, now())?;

    if include_threads && let Some((owner, repo, number)) = pr_coordinates(url) {
        let query = "query=query($owner:String!,$name:String!,$number:Int!){\
            repository(owner:$owner,name:$name){pullRequest(number:$number){\
            reviewThreads(first:100){totalCount nodes{isResolved}}}}}";
        if let Some(thread_data) = run_gh(
            gh,
            &[
                "api",
                "graphql",
                "-f",
                query,
                "-f",
                &format!("owner={owner}"),
                "-f",
                &format!("name={repo}"),
                "-F",
                &format!("number={number}"),
            ],
            Duration::from_secs(15),
        ) && let Some((resolved, total)) = parse_threads(&thread_data)
        {
            status.resolved_threads = Some(resolved);
            status.total_threads = Some(total);
        }
    }
    Some(status)
}

/// Runs gh with a watchdog so a hung network call can't wedge the sweep.
fn run_gh(gh: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let mut child = std::process::Command::new(gh)
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(exit)) if exit.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => return None,
        }
    }
    let mut output = Vec::new();
    use std::io::Read;
    child.stdout.take()?.read_to_end(&mut output).ok()?;
    Some(output)
}

/// `github.com/owner/repo/pull/123` → (owner, repo, 123).
pub fn pr_coordinates(url: &str) -> Option<(String, String, i64)> {
    let parts: Vec<&str> = url.split('/').collect();
    let pull = parts.iter().position(|part| *part == "pull")?;
    if pull < 2 || pull + 1 >= parts.len() {
        return None;
    }
    let number: i64 = parts[pull + 1]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    Some((
        parts[pull - 2].to_string(),
        parts[pull - 1].to_string(),
        number,
    ))
}

/// Decodes the reviewThreads GraphQL response into (resolved, total).
pub fn parse_threads(data: &[u8]) -> Option<(i64, i64)> {
    let value: Value = serde_json::from_slice(data).ok()?;
    let threads = &value["data"]["repository"]["pullRequest"]["reviewThreads"];
    let total = threads["totalCount"].as_i64()?;
    let resolved = threads["nodes"]
        .as_array()?
        .iter()
        .filter(|node| node["isResolved"].as_bool() == Some(true))
        .count() as i64;
    Some((resolved, total))
}

/// Decodes one `gh pr view --json` payload. Input-driven so tests feed
/// canned JSON without a subprocess.
pub fn parse(data: &[u8], url: &str, fetched_at: DateMillis) -> Option<PullRequestStatus> {
    let view: Value = serde_json::from_slice(data).ok()?;
    let number = view["number"].as_i64()?;
    let string = |value: &Value| value.as_str().map(str::to_string);
    let nonempty = |value: &Value| value.as_str().filter(|s| !s.is_empty()).map(str::to_string);

    let checks: Vec<PrCheck> = view["statusCheckRollup"]
        .as_array()
        .map(|rollup| {
            rollup
                .iter()
                .map(|check| {
                    // CheckRun reports conclusion once COMPLETED; StatusContext
                    // only has state. One word decides the bucket; an empty
                    // conclusion means still running.
                    let verdict = ["conclusion", "state", "status"]
                        .iter()
                        .filter_map(|key| check[*key].as_str())
                        .find(|value| !value.is_empty())
                        .unwrap_or("");
                    let result = match verdict {
                        "SUCCESS" | "NEUTRAL" | "SKIPPED" => "pass",
                        "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
                        | "STARTUP_FAILURE" => "fail",
                        _ => "pending",
                    };
                    let base = check["name"]
                        .as_str()
                        .or_else(|| check["context"].as_str())
                        .unwrap_or("check");
                    let name = match check["workflowName"].as_str() {
                        Some(workflow) => format!("{workflow} / {base}"),
                        None => base.to_string(),
                    };
                    PrCheck {
                        name,
                        result: result.to_string(),
                        detail: (!verdict.is_empty()).then(|| verdict.to_string()),
                        url: string(&check["detailsUrl"]).or_else(|| string(&check["targetUrl"])),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let count = |result: &str| checks.iter().filter(|check| check.result == result).count() as i64;

    let discussion_item = |kind: &str, entry: &Value, created_key: &str| PrDiscussionItem {
        kind: kind.to_string(),
        author: entry["author"]["login"]
            .as_str()
            .unwrap_or("ghost")
            .to_string(),
        body: entry["body"].as_str().unwrap_or("").to_string(),
        state: if kind == "review" {
            string(&entry["state"])
        } else {
            None
        },
        created_at: entry[created_key].as_str().and_then(parse_github_date),
        url: string(&entry["url"]),
    };
    let comments: Vec<PrDiscussionItem> = view["comments"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|comment| discussion_item("comment", comment, "createdAt"))
                .collect()
        })
        .unwrap_or_default();
    let reviews: Vec<PrDiscussionItem> = view["reviews"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|review| discussion_item("review", review, "submittedAt"))
                .collect()
        })
        .unwrap_or_default();
    let comment_count = comments.len() as i64;
    let review_count = reviews.len() as i64;
    let mut discussion: Vec<PrDiscussionItem> = comments.into_iter().chain(reviews).collect();
    discussion.sort_by(|a, b| {
        let time = |item: &PrDiscussionItem| item.created_at.as_ref().map_or(f64::MIN, |at| at.0);
        time(a)
            .partial_cmp(&time(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Some(PullRequestStatus {
        url: url.to_string(),
        number,
        title: string(&view["title"]),
        author: string(&view["author"]["login"]),
        body: string(&view["body"]),
        base_ref_name: string(&view["baseRefName"]),
        head_ref_name: string(&view["headRefName"]),
        state: view["state"].as_str().unwrap_or("OPEN").to_string(),
        is_draft: view["isDraft"].as_bool().unwrap_or(false),
        review_decision: nonempty(&view["reviewDecision"]),
        mergeable: string(&view["mergeable"]),
        merge_state_status: string(&view["mergeStateStatus"]),
        additions: view["additions"].as_i64().unwrap_or(0),
        deletions: view["deletions"].as_i64().unwrap_or(0),
        changed_files: view["changedFiles"].as_i64().unwrap_or(0),
        comment_count,
        review_count,
        resolved_threads: None,
        total_threads: None,
        checks_passed: count("pass"),
        checks_failed: count("fail"),
        checks_pending: count("pending"),
        checks: (!checks.is_empty()).then_some(checks),
        discussion: (!discussion.is_empty()).then_some(discussion),
        fetched_at,
    })
}

fn parse_github_date(value: &str) -> Option<DateMillis> {
    // ISO 8601 `2026-08-07T12:34:56Z`; a hand parser avoids a chrono
    // dependency for one field the client only sorts and displays by.
    let bytes = value.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> { value.get(range)?.parse().ok() };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    // Days since epoch via the civil-days algorithm.
    let years = if month <= 2 { year - 1 } else { year };
    let era = years.div_euclid(400);
    let year_of_era = years - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(DateMillis(
        ((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000) as f64,
    ))
}

fn now() -> DateMillis {
    DateMillis::from(std::time::SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinates_come_out_of_a_pr_url() {
        assert_eq!(
            pr_coordinates("https://github.com/cristicretu/diri/pull/7"),
            Some(("cristicretu".into(), "diri".into(), 7))
        );
        assert_eq!(pr_coordinates("https://github.com/x/pull"), None);
    }

    #[test]
    fn monitor_refreshes_immediately_on_start() {
        assert_eq!(initial_sweep_delay(), Duration::ZERO);
    }

    #[test]
    fn foreground_and_background_cadences_match_visible_pr_ui() {
        let mut state = RefreshState::default();
        assert_eq!(
            state.interval(PollInterest::Foreground),
            Duration::from_secs(60)
        );
        assert_eq!(
            state.interval(PollInterest::Background),
            Duration::from_secs(5 * 60)
        );

        state.record_result(PollInterest::Background, false);
        assert_eq!(
            state.interval(PollInterest::Background),
            Duration::from_secs(10 * 60)
        );
        state.record_result(PollInterest::Background, false);
        state.record_result(PollInterest::Background, false);
        state.record_result(PollInterest::Background, false);
        assert_eq!(
            state.interval(PollInterest::Background),
            Duration::from_secs(30 * 60),
            "background polling caps at thirty minutes"
        );

        state.record_result(PollInterest::Background, true);
        assert_eq!(
            state.interval(PollInterest::Background),
            Duration::from_secs(5 * 60),
            "activity resets the backoff"
        );
    }

    #[test]
    fn attached_prs_become_background_when_the_app_is_inactive() {
        assert_eq!(poll_interest(true, true), PollInterest::Foreground);
        assert_eq!(poll_interest(true, false), PollInterest::Background);
        assert_eq!(poll_interest(false, true), PollInterest::Background);
    }

    #[test]
    fn forced_or_never_fetched_prs_are_due_now() {
        let url = "https://github.com/o/r/pull/1".to_owned();
        let targets = HashMap::from([(url.clone(), PollInterest::Foreground)]);
        assert_eq!(
            next_refresh_delay(&targets, &HashMap::new(), &HashSet::new(), Instant::now()),
            Duration::ZERO
        );

        let refresh = HashMap::from([(
            url.clone(),
            RefreshState {
                last_attempt: Some(Instant::now()),
                ..RefreshState::default()
            },
        )]);
        assert_eq!(
            next_refresh_delay(&targets, &refresh, &HashSet::from([url]), Instant::now(),),
            Duration::ZERO
        );
    }

    #[test]
    fn visibility_wakes_are_delivered_without_waiting_for_the_timer() {
        let wake = PrMonitorWake::default();
        wake.wake_session("s_selected");
        wake.set_foreground_active(true);
        let stop = AtomicBool::new(false);

        let pending = wake.wait(Duration::from_secs(60), &stop);
        assert!(pending.reconcile);
        assert!(pending.foreground);
        assert!(pending.sessions.contains("s_selected"));
    }

    #[test]
    fn deactivation_wakes_reconciliation_without_forcing_a_network_refresh() {
        let wake = PrMonitorWake::default();
        wake.set_foreground_active(false);
        let stop = AtomicBool::new(false);

        let pending = wake.wait(Duration::from_secs(60), &stop);
        assert!(pending.reconcile);
        assert!(!pending.foreground);
        assert!(!wake.foreground_active());
    }

    #[test]
    fn a_gh_view_payload_parses_into_the_wire_status() {
        let payload = serde_json::json!({
            "number": 12,
            "title": "Add the thing",
            "author": {"login": "shawn"},
            "state": "OPEN",
            "isDraft": false,
            "reviewDecision": "",
            "additions": 10, "deletions": 2, "changedFiles": 3,
            "comments": [{"author": {"login": "giga"}, "body": "nice", "createdAt": "2026-08-07T10:00:00Z"}],
            "reviews": [{"author": {"login": "bot"}, "body": "lgtm", "state": "APPROVED", "submittedAt": "2026-08-07T11:00:00Z"}],
            "statusCheckRollup": [
                {"name": "test", "workflowName": "CI", "status": "COMPLETED", "conclusion": "SUCCESS", "detailsUrl": "https://x"},
                {"context": "lint", "state": "FAILURE"},
                {"name": "build", "status": "IN_PROGRESS", "conclusion": ""}
            ],
        });
        let status = parse(
            payload.to_string().as_bytes(),
            "https://github.com/o/r/pull/12",
            DateMillis(0.0),
        )
        .expect("parse");
        assert_eq!(status.number, 12);
        assert_eq!(status.author.as_deref(), Some("shawn"));
        assert_eq!(status.review_decision, None, "empty string means none");
        assert_eq!(
            (
                status.checks_passed,
                status.checks_failed,
                status.checks_pending
            ),
            (1, 1, 1)
        );
        let checks = status.checks.expect("checks");
        assert_eq!(checks[0].name, "CI / test");
        assert_eq!(checks[1].name, "lint");
        let discussion = status.discussion.expect("discussion");
        assert_eq!(discussion.len(), 2);
        assert_eq!(discussion[0].kind, "comment", "sorted by time");
        assert_eq!(discussion[1].state.as_deref(), Some("APPROVED"));
        assert!(
            discussion[0].created_at.expect("date").0 > 1.7e12,
            "the date parser lands in the right epoch decade"
        );
    }

    #[test]
    fn thread_counts_decode_from_graphql() {
        let payload = serde_json::json!({
            "data": {"repository": {"pullRequest": {"reviewThreads": {
                "totalCount": 5,
                "nodes": [{"isResolved": true}, {"isResolved": false}, {"isResolved": true}]
            }}}}
        });
        assert_eq!(parse_threads(payload.to_string().as_bytes()), Some((2, 5)));
    }
}
