//! Polls GitHub (via the `gh` CLI) for the state of every PR URL captured as
//! a session artifact: open/merged/closed, draft, review decision,
//! mergeability, CI checks, comment counts, and +/- line stats.
//!
//! Ported from `PullRequestMonitor`. Results land on
//! `SessionRecord.pullRequests`; one shared per-URL cache dedupes PRs that
//! appear in several sessions. Due PRs on one host are fetched together by a
//! single aliased `gh api graphql` request (reshaped into exactly what
//! `gh pr view --json` would print); anything the batch can't resolve falls
//! back to per-PR `gh pr view`, a bounded few per iteration.
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
/// Pull requests folded into one GraphQL request. A PR asks for at most four
/// 100-item connections, so 25 PRs stay orders of magnitude below GitHub's
/// 500,000-node ceiling while costing one rate-limit point.
const MAX_PRS_PER_QUERY: usize = 25;
/// Every gh call gets the same watchdog. A sweep iteration runs either one
/// batch query or at most `MAX_FETCHES_PER_BATCH` per-PR fetches, never both,
/// so a slow batch can't stretch an iteration past the per-PR bound.
const GH_TIMEOUT: Duration = Duration::from_secs(15);
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

/// A merged or closed pull request no longer moves on its own: its checks,
/// mergeability and review state are final, and a late comment or a reopen
/// is rare. Polling one every minute because its session is on screen spent a
/// `gh` process and a GitHub API call per PR per minute — 26 a minute for one
/// session that had shipped 26 PRs. Settled PRs refresh on the background
/// ceiling instead; viewing the session still forces an immediate refetch.
fn settled(status: Option<&PullRequestStatus>) -> bool {
    status.is_some_and(|status| {
        status.state.eq_ignore_ascii_case("MERGED") || status.state.eq_ignore_ascii_case("CLOSED")
    })
}

impl RefreshState {
    fn interval(&self, interest: PollInterest, settled: bool) -> Duration {
        if settled {
            return MAX_BACKGROUND_REFRESH_INTERVAL;
        }
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
            let mut fallback = HashSet::new();
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
                    &mut fallback,
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
    fallback: &mut HashSet<String>,
    pending: PendingWake,
    foreground_active: bool,
) -> Duration {
    // Fetch only attached/recently viewed PRs, but propagate their cached
    // status to every record sharing the URL, including archived chats.
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
        let explicitly_viewed = pending.sessions.contains(&record.id.0);
        let eligible = attached || recently_seen;
        let mut seen_urls = HashSet::new();
        let urls: Vec<String> = record
            .artifacts
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|artifact| artifact.kind == ArtifactKind::PullRequest)
            .map(|artifact| artifact.url.clone())
            .chain(
                record
                    .pull_requests
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(|pr| pr.url.clone()),
            )
            .filter(|url| seen_urls.insert(url.clone()))
            .collect();
        if !urls.is_empty() {
            let interest = poll_interest(attached || explicitly_viewed, foreground_active);
            for url in urls.iter().filter(|_| eligible) {
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
    prune_unreferenced(&wanted, cache, refresh, last_thread_attempt);
    if targets.is_empty() {
        forced_urls.clear();
        fallback.clear();
        return IDLE_RECONCILE_INTERVAL;
    }
    forced_urls.retain(|url| targets.contains_key(url));
    fallback.retain(|url| targets.contains_key(url));

    if !fallback.is_empty() {
        // The previous iteration's batch could not resolve these; fetch them
        // the per-PR way, a bounded few at a time.
        let mut owed: Vec<String> = fallback.iter().cloned().collect();
        owed.sort();
        for url in owed.into_iter().take(MAX_FETCHES_PER_BATCH) {
            fallback.remove(&url);
            forced_urls.remove(&url);
            let interest = targets[&url];
            fetch_one(&url, interest, gh, cache, refresh, last_thread_attempt);
        }
    } else {
        // Foreground first, then never-attempted/oldest. Remaining due URLs
        // make the returned delay zero and are drained immediately by the
        // next iteration.
        let now = Instant::now();
        let mut due: Vec<(String, PollInterest, Option<Instant>)> = targets
            .iter()
            .filter_map(|(url, interest)| {
                let state = refresh.entry(url.clone()).or_default();
                let forced = forced_urls.contains(url);
                let settled = settled(cache.get(url));
                let is_due = forced
                    || state.last_attempt.is_none_or(|at| {
                        now.saturating_duration_since(at) >= state.interval(*interest, settled)
                    });
                is_due.then_some((url.clone(), *interest, state.last_attempt))
            })
            .collect();
        due.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| a.0.cmp(&b.0))
        });
        match due.first().and_then(|(url, _, _)| BatchRef::parse(url)) {
            Some(first) => {
                // One GraphQL request for every due PR on this host, up to
                // the per-query cap, instead of one `gh` process per PR.
                let chunk: Vec<(String, PollInterest, BatchRef)> = due
                    .into_iter()
                    .filter_map(|(url, interest, _)| {
                        let target = BatchRef::parse(&url)?;
                        (target.host == first.host).then_some((url, interest, target))
                    })
                    .take(MAX_PRS_PER_QUERY)
                    .collect();
                fetch_chunk(
                    &chunk,
                    gh,
                    cache,
                    refresh,
                    last_thread_attempt,
                    forced_urls,
                    fallback,
                );
            }
            None => {
                // URLs the batch can't address go the per-PR way, as before.
                for (url, interest, _) in due
                    .into_iter()
                    .filter(|(url, _, _)| BatchRef::parse(url).is_none())
                    .take(MAX_FETCHES_PER_BATCH)
                {
                    forced_urls.remove(&url);
                    fetch_one(&url, interest, gh, cache, refresh, last_thread_attempt);
                }
            }
        }
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
                guard.record(&id)
            } else {
                None
            }
        };
        if let Some(record) = record {
            events.publish_encoded(diri_proto::EventName::SESSION_UPDATED, &record, Some(&id));
        }
    }

    if !fallback.is_empty() {
        return Duration::ZERO;
    }
    next_refresh_delay(&targets, refresh, cache, forced_urls, Instant::now())
}

/// Refreshes one pull request the per-PR way: `gh pr view`, plus the thread
/// query when that is due.
fn fetch_one(
    url: &str,
    interest: PollInterest,
    gh: &str,
    cache: &mut HashMap<String, PullRequestStatus>,
    refresh: &mut HashMap<String, RefreshState>,
    last_thread_attempt: &mut HashMap<String, Instant>,
) {
    refresh.entry(url.to_owned()).or_default().last_attempt = Some(Instant::now());
    let refresh_threads = threads_due(last_thread_attempt, url);
    if refresh_threads {
        last_thread_attempt.insert(url.to_owned(), Instant::now());
    }
    let changed = fetch(url, gh, refresh_threads)
        .is_some_and(|status| store_status(cache, url, status, refresh_threads));
    refresh
        .entry(url.to_owned())
        .or_default()
        .record_result(interest, changed);
}

/// Refreshes a chunk of same-host pull requests with one GraphQL request.
/// Anything the batch can't resolve — a failed or timed-out request, a
/// missing PR, a connection longer than one page — is queued for the per-PR
/// path, which the next iteration drains.
fn fetch_chunk(
    chunk: &[(String, PollInterest, BatchRef)],
    gh: &str,
    cache: &mut HashMap<String, PullRequestStatus>,
    refresh: &mut HashMap<String, RefreshState>,
    last_thread_attempt: &mut HashMap<String, Instant>,
    forced_urls: &mut HashSet<String>,
    fallback: &mut HashSet<String>,
) {
    // One attempt time for the whole chunk keeps its PRs due together, so
    // the next refresh is again one request rather than a trickle of them.
    let attempted = Instant::now();
    let requests: Vec<(BatchRef, bool)> = chunk
        .iter()
        .map(|(url, _, target)| {
            forced_urls.remove(url);
            refresh.entry(url.clone()).or_default().last_attempt = Some(attempted);
            (target.clone(), threads_due(last_thread_attempt, url))
        })
        .collect();
    let results = fetch_batch(&requests, gh);
    for ((url, interest, _), ((_, with_threads), result)) in
        chunk.iter().zip(requests.iter().zip(results))
    {
        let Some(status) = result else {
            fallback.insert(url.clone());
            continue;
        };
        if *with_threads {
            last_thread_attempt.insert(url.clone(), attempted);
        }
        let changed = store_status(cache, url, status, *with_threads);
        refresh
            .entry(url.clone())
            .or_default()
            .record_result(*interest, changed);
    }
}

fn threads_due(last_thread_attempt: &HashMap<String, Instant>, url: &str) -> bool {
    last_thread_attempt
        .get(url)
        .is_none_or(|at| at.elapsed() >= THREAD_REFRESH_TTL)
}

/// Caches a fetched status, carrying the previous thread counts forward when
/// this fetch didn't ask for them. True when anything but the fetch time
/// changed.
fn store_status(
    cache: &mut HashMap<String, PullRequestStatus>,
    url: &str,
    mut status: PullRequestStatus,
    refreshed_threads: bool,
) -> bool {
    if !refreshed_threads && let Some(previous) = cache.get(url) {
        status.resolved_threads = previous.resolved_threads;
        status.total_threads = previous.total_threads;
    }
    let changed = cache
        .get(url)
        .is_none_or(|previous| !status_materially_same(previous, &status));
    cache.insert(url.to_owned(), status);
    changed
}

/// Forgets pull requests no record mentions any more.
///
/// The maps are keyed by URL and were only ever added to, so a long-lived
/// Engine kept a status, a backoff state and a timestamp for every pull
/// request any removed session had ever linked. Anything still on a record —
/// an archived one included — is kept: its cached status is what that record
/// is reconciled against, and its backoff is what spares `gh` a refetch.
fn prune_unreferenced(
    wanted: &[(String, Vec<String>)],
    cache: &mut HashMap<String, PullRequestStatus>,
    refresh: &mut HashMap<String, RefreshState>,
    last_thread_attempt: &mut HashMap<String, Instant>,
) {
    let referenced: HashSet<&str> = wanted
        .iter()
        .flat_map(|(_, urls)| urls.iter().map(String::as_str))
        .collect();
    cache.retain(|url, _| referenced.contains(url.as_str()));
    refresh.retain(|url, _| referenced.contains(url.as_str()));
    last_thread_attempt.retain(|url, _| referenced.contains(url.as_str()));
}

fn next_refresh_delay(
    targets: &HashMap<String, PollInterest>,
    refresh: &HashMap<String, RefreshState>,
    cache: &HashMap<String, PullRequestStatus>,
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
                .interval(*interest, settled(cache.get(url)))
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

pub fn resolve_gh() -> Option<String> {
    let path = std::env::var("PATH").ok()?;
    path.split(':')
        .map(|dir| std::path::Path::new(dir).join("gh"))
        .find(|candidate| candidate.is_file())
        .map(|path| path.to_string_lossy().into_owned())
}

/// The `gh pr view --json` fields `parse` reads.
pub const VIEW_FIELDS: &str = "number,title,author,body,baseRefName,headRefName,state,isDraft,\
    reviewDecision,mergeable,mergeStateStatus,additions,deletions,changedFiles,\
    comments,reviews,statusCheckRollup";

/// `gh pr view <url> --json …` plus a GraphQL round trip for review-thread
/// resolution (which `pr view` can't report). None on any failure — the last
/// cached status stays in effect.
pub fn fetch(url: &str, gh: &str, include_threads: bool) -> Option<PullRequestStatus> {
    let data = run_gh(gh, &["pr", "view", url, "--json", VIEW_FIELDS], GH_TIMEOUT)?;
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
            GH_TIMEOUT,
        ) && let Some((resolved, total)) = parse_threads(&thread_data)
        {
            status.resolved_threads = Some(resolved);
            status.total_threads = Some(total);
        }
    }
    Some(status)
}

/// Where a pull request lives, as the batch query addresses it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchRef {
    /// The URL exactly as the record holds it; statuses are keyed by it.
    pub url: String,
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub number: i64,
}

impl BatchRef {
    /// Accepts only the plain `https://host/owner/repo/pull/N[/…]` shape;
    /// anything else keeps the per-PR `gh pr view` path, which knows every
    /// URL form gh does.
    pub fn parse(url: &str) -> Option<Self> {
        let rest = url.strip_prefix("https://")?;
        let mut parts = rest.split('/');
        let host = parts.next()?.to_ascii_lowercase();
        let host = host.strip_prefix("www.").unwrap_or(&host).to_owned();
        let owner = parts.next()?;
        let repo = parts.next()?;
        if parts.next()? != "pull" {
            return None;
        }
        let digits: String = parts
            .next()?
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        let number: i64 = digits.parse().ok().filter(|n| *n > 0)?;
        let plain = |part: &str| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        (plain(&host) && plain(owner) && plain(repo)).then(|| Self {
            url: url.to_owned(),
            host,
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            number,
        })
    }
}

/// The subset of gh's own `pr view --json` query that `parse` reads, with
/// the same connection sizes, so one batched request sees exactly what the
/// per-PR `gh pr view` would. Page info is asked for so a connection longer
/// than one page — which gh would paginate — falls back to gh.
const PR_FRAGMENT: &str = "fragment P on PullRequest{\
    number title author{login ...on User{id}} body baseRefName headRefName \
    state isDraft reviewDecision mergeable mergeStateStatus additions deletions changedFiles \
    comments(first:100){nodes{author{login} body createdAt url} pageInfo{hasNextPage}} \
    reviews(first:100){nodes{author{login} body state submittedAt} pageInfo{hasNextPage}} \
    commits(last:1){nodes{commit{statusCheckRollup{contexts(first:100){nodes{__typename \
    ...on StatusContext{context state targetUrl} \
    ...on CheckRun{name checkSuite{workflowRun{workflow{name}}} status conclusion detailsUrl}} \
    pageInfo{hasNextPage}}}}}}}";
/// The same connection the per-PR thread query reads.
const THREAD_FRAGMENT: &str =
    "fragment T on PullRequest{reviewThreads(first:100){totalCount nodes{isResolved}}}";

/// Builds the aliased query and its variables for one host's chunk. Owner,
/// name and number travel as GraphQL variables, never spliced into the query.
pub fn batch_query(requests: &[(BatchRef, bool)]) -> (String, Vec<(&'static str, String)>) {
    let mut params = Vec::new();
    let mut fields = String::new();
    let mut variables = Vec::new();
    for (index, (target, threads)) in requests.iter().enumerate() {
        params.push(format!(
            "$o{index}:String!,$n{index}:String!,$p{index}:Int!"
        ));
        fields.push_str(&format!(
            "p{index}:repository(owner:$o{index},name:$n{index}){{pullRequest(number:$p{index}){{...P{}}}}}",
            if *threads { " ...T" } else { "" }
        ));
        // `-f` sends a raw string; `-F` makes the number an Int.
        variables.push(("-f", format!("o{index}={}", target.owner)));
        variables.push(("-f", format!("n{index}={}", target.repo)));
        variables.push(("-F", format!("p{index}={}", target.number)));
    }
    let mut query = format!("query({}){{{fields}}}{PR_FRAGMENT}", params.join(","));
    if requests.iter().any(|(_, threads)| *threads) {
        query.push_str(THREAD_FRAGMENT);
    }
    (query, variables)
}

/// One `gh api graphql` request for every pull request in `requests` (all on
/// one host). Each slot is None when the batch couldn't resolve that PR the
/// way `gh pr view` would; the caller then asks gh per PR.
pub fn fetch_batch(requests: &[(BatchRef, bool)], gh: &str) -> Vec<Option<PullRequestStatus>> {
    let Some(host) = requests.first().map(|(target, _)| target.host.clone()) else {
        return Vec::new();
    };
    let (query, variables) = batch_query(requests);
    let mut args = vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "--hostname".to_owned(),
        host,
        "-f".to_owned(),
        format!("query={query}"),
    ];
    for (flag, variable) in variables {
        args.push(flag.to_owned());
        args.push(variable);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // gh exits non-zero when any alias errored (a deleted repo, say) but
    // still prints the other aliases' data, which remains good.
    let data = run_gh_any_status(gh, &args, GH_TIMEOUT);
    parse_batch(data.as_deref().unwrap_or_default(), requests, now())
}

/// Decodes a batch response into per-request statuses. Input-driven so tests
/// feed recorded payloads.
pub fn parse_batch(
    data: &[u8],
    requests: &[(BatchRef, bool)],
    fetched_at: DateMillis,
) -> Vec<Option<PullRequestStatus>> {
    let response: Value = serde_json::from_slice(data).unwrap_or(Value::Null);
    requests
        .iter()
        .enumerate()
        .map(|(index, (target, threads))| {
            let pr = &response["data"][format!("p{index}")]["pullRequest"];
            let view = gh_view_from_graphql(pr)?;
            let mut status = parse_view(&view, &target.url, fetched_at)?;
            if *threads {
                let (resolved, total) = thread_counts(&pr["reviewThreads"])?;
                status.resolved_threads = Some(resolved);
                status.total_threads = Some(total);
            }
            Some(status)
        })
        .collect()
}

/// Reshapes one GraphQL `PullRequest` node into the JSON `gh pr view --json`
/// prints for `VIEW_FIELDS`, following gh's own export (gh `api/export_pr.go`
/// and the Go structs it decodes into): a GraphQL null becomes Go's zero
/// value, a PR author without a User id is `app/<login>`, a check run
/// always carries `workflowName` (empty without a workflow), comments and
/// reviews carry only `author.login`, and an empty comment `url` is omitted.
/// None when a connection has a further page gh would have fetched.
pub fn gh_view_from_graphql(pr: &Value) -> Option<Value> {
    use serde_json::json;
    if !pr.is_object() {
        return None;
    }
    let more = |connection: &Value| connection["pageInfo"]["hasNextPage"].as_bool() != Some(false);
    let text = |value: &Value| value.as_str().unwrap_or("").to_owned();
    let int = |value: &Value| value.as_i64().unwrap_or(0);

    if more(&pr["comments"]) || more(&pr["reviews"]) {
        return None;
    }
    let author = &pr["author"];
    let author_login = text(&author["login"]);
    let author = if text(&author["id"]).is_empty() {
        json!({ "login": format!("app/{author_login}") })
    } else {
        json!({ "login": author_login })
    };
    let comments: Vec<Value> = pr["comments"]["nodes"]
        .as_array()?
        .iter()
        .map(|comment| {
            let mut entry = json!({
                "author": { "login": text(&comment["author"]["login"]) },
                "body": text(&comment["body"]),
                "createdAt": comment["createdAt"].as_str().unwrap_or("0001-01-01T00:00:00Z"),
            });
            let url = text(&comment["url"]);
            if !url.is_empty() {
                entry["url"] = Value::String(url);
            }
            entry
        })
        .collect();
    let reviews: Vec<Value> = pr["reviews"]["nodes"]
        .as_array()?
        .iter()
        .map(|review| {
            json!({
                "author": { "login": text(&review["author"]["login"]) },
                "body": text(&review["body"]),
                "state": text(&review["state"]),
                "submittedAt": review["submittedAt"].as_str(),
            })
        })
        .collect();
    let rollup = match pr["commits"]["nodes"].as_array()?.first() {
        None => Value::Null,
        Some(node) => {
            let contexts = &node["commit"]["statusCheckRollup"]["contexts"];
            if !contexts.is_null() && more(contexts) {
                return None;
            }
            let checks: Vec<Value> = contexts["nodes"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|check| {
                    if check["__typename"].as_str() == Some("CheckRun") {
                        json!({
                            "name": text(&check["name"]),
                            "workflowName": text(&check["checkSuite"]["workflowRun"]["workflow"]["name"]),
                            "status": text(&check["status"]),
                            "conclusion": text(&check["conclusion"]),
                            "detailsUrl": text(&check["detailsUrl"]),
                        })
                    } else {
                        json!({
                            "context": text(&check["context"]),
                            "state": text(&check["state"]),
                            "targetUrl": text(&check["targetUrl"]),
                        })
                    }
                })
                .collect();
            Value::Array(checks)
        }
    };
    Some(json!({
        "number": int(&pr["number"]),
        "title": text(&pr["title"]),
        "author": author,
        "body": text(&pr["body"]),
        "baseRefName": text(&pr["baseRefName"]),
        "headRefName": text(&pr["headRefName"]),
        "state": text(&pr["state"]),
        "isDraft": pr["isDraft"].as_bool().unwrap_or(false),
        "reviewDecision": text(&pr["reviewDecision"]),
        "mergeable": text(&pr["mergeable"]),
        "mergeStateStatus": text(&pr["mergeStateStatus"]),
        "additions": int(&pr["additions"]),
        "deletions": int(&pr["deletions"]),
        "changedFiles": int(&pr["changedFiles"]),
        "comments": comments,
        "reviews": reviews,
        "statusCheckRollup": rollup,
    }))
}

/// Runs gh with a watchdog so a hung network call can't wedge the sweep.
fn run_gh(gh: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let (success, output) = run_gh_with_status(gh, args, timeout)?;
    success.then_some(output)
}

/// Like `run_gh`, but keeps the output of a non-zero exit.
fn run_gh_any_status(gh: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    run_gh_with_status(gh, args, timeout).map(|(_, output)| output)
}

/// Stdout is drained on its own thread while the watchdog polls: a reply
/// larger than the pipe buffer would otherwise block gh on write and read as
/// a timeout. None when gh can't start or outlives `timeout`.
fn run_gh_with_status(gh: &str, args: &[&str], timeout: Duration) -> Option<(bool, Vec<u8>)> {
    let mut child = std::process::Command::new(gh)
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::Builder::new()
        .name("diri-pr-gh-stdout".into())
        .spawn(move || {
            use std::io::Read;
            let mut output = Vec::new();
            stdout.read_to_end(&mut output).map(|_| output)
        });
    let Ok(reader) = reader else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let deadline = Instant::now() + timeout;
    let success = loop {
        match child.try_wait() {
            Ok(Some(exit)) => break Some(exit.success()),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // A grandchild holding the pipe open can't wedge us past the kill: gh
    // doesn't fork one, and the reader only outlives a killed gh if one did.
    let output = reader.join().ok()?.ok()?;
    Some((success?, output))
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
    thread_counts(&value["data"]["repository"]["pullRequest"]["reviewThreads"])
}

fn thread_counts(threads: &Value) -> Option<(i64, i64)> {
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
    parse_view(&view, url, fetched_at)
}

/// `parse` over an already-decoded view, shared with the batch path.
fn parse_view(view: &Value, url: &str, fetched_at: DateMillis) -> Option<PullRequestStatus> {
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
    fn pull_requests_no_record_mentions_are_forgotten() {
        let kept = "https://github.com/o/r/pull/1";
        let gone = "https://github.com/o/r/pull/2";
        let mut cache = HashMap::new();
        let mut refresh: HashMap<String, RefreshState> = HashMap::new();
        let mut last_thread_attempt = HashMap::new();
        for url in [kept, gone] {
            let status = parse(br#"{"number":1,"state":"OPEN"}"#, url, DateMillis(0.0)).unwrap();
            cache.insert(url.to_owned(), status);
            refresh.entry(url.to_owned()).or_default().last_attempt = Some(Instant::now());
            last_thread_attempt.insert(url.to_owned(), Instant::now());
        }

        // The session that linked `gone` was removed; an archived record
        // still links `kept`.
        let wanted = vec![("archived".to_owned(), vec![kept.to_owned()])];
        prune_unreferenced(&wanted, &mut cache, &mut refresh, &mut last_thread_attempt);

        assert_eq!(cache.keys().collect::<Vec<_>>(), [kept]);
        assert_eq!(last_thread_attempt.keys().collect::<Vec<_>>(), [kept]);
        assert!(
            refresh[kept].last_attempt.is_some(),
            "a kept pull request keeps its backoff"
        );
        assert!(!refresh.contains_key(gone));
    }

    #[cfg(unix)]
    /// A registry of records linking pull requests, plus the monitor's state
    /// and a scripted `gh`, so tests drive real sweeps.
    struct Harness {
        temp: tempfile::TempDir,
        registry: Arc<Mutex<Registry>>,
        gh: std::path::PathBuf,
        cache: HashMap<String, PullRequestStatus>,
        refresh: HashMap<String, RefreshState>,
        last_thread_attempt: HashMap<String, Instant>,
        forced_urls: HashSet<String>,
        fallback: HashSet<String>,
    }

    #[cfg(unix)]
    impl Harness {
        #[cfg(unix)]
        /// `records` pairs a session id with whether it was seen just now
        /// and the pull request statuses it already carries.
        fn new(records: &[(&str, bool, Vec<PullRequestStatus>)], gh_script: &str) -> Self {
            #[cfg(unix)]
            use std::os::unix::fs::PermissionsExt;
            let temp = tempfile::tempdir().unwrap();
            let fixture: Value = serde_json::from_str(include_str!(
                "../../diri-proto/tests/fixtures/session_list_response.json"
            ))
            .unwrap();
            let template: diri_proto::SessionRecord =
                serde_json::from_value(fixture["ok"]["sessions"][0].clone()).unwrap();
            let sessions: Vec<diri_proto::SessionRecord> = records
                .iter()
                .map(|(id, seen, statuses)| {
                    let mut record = template.clone();
                    record.id.0 = (*id).into();
                    record.last_seen_at = seen.then(now);
                    record.artifacts = None;
                    record.pull_requests = Some(statuses.clone());
                    record
                })
                .collect();
            let state_file = temp.path().join("state.json");
            std::fs::write(
                &state_file,
                serde_json::to_vec(&serde_json::json!({
                    "version": 1, "projects": [], "sessions": sessions
                }))
                .unwrap(),
            )
            .unwrap();
            let (engine, _) =
                crate::detect::ManifestEngine::load_dir(&crate::detect::bundled_manifest_dir())
                    .unwrap();
            let mut registry = Registry::new(Arc::new(engine), state_file);
            registry.load().unwrap();
            let gh = temp.path().join("gh");
            std::fs::write(
                &gh,
                gh_script.replace("$LOG", &temp.path().join("gh.log").to_string_lossy()),
            )
            .unwrap();
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                temp,
                registry: Arc::new(Mutex::new(registry)),
                gh,
                cache: HashMap::new(),
                refresh: HashMap::new(),
                last_thread_attempt: HashMap::new(),
                forced_urls: HashSet::new(),
                fallback: HashSet::new(),
            }
        }

        #[cfg(unix)]
        fn sweep(&mut self, pending: PendingWake) -> Duration {
            sweep(
                &self.registry,
                &EventBus::new(),
                &AttachHub::new(),
                self.gh.to_str().unwrap(),
                &mut self.cache,
                &mut self.refresh,
                &mut self.last_thread_attempt,
                &mut self.forced_urls,
                &mut self.fallback,
                pending,
                true,
            )
        }

        /// Sweeps until nothing is due right away, as the monitor loop does.
        fn drain(&mut self, pending: PendingWake) -> usize {
            let mut iterations = 1;
            let mut delay = self.sweep(pending);
            while delay.is_zero() {
                assert!(iterations < 50, "the monitor never settles");
                delay = self.sweep(PendingWake::default());
                iterations += 1;
            }
            iterations
        }

        /// One line per gh invocation: its arguments.
        fn gh_calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.temp.path().join("gh.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn states(&self) -> Vec<(String, String)> {
            let mut states: Vec<(String, String)> = self
                .registry
                .lock()
                .unwrap()
                .records()
                .into_iter()
                .flat_map(|record| {
                    record
                        .pull_requests
                        .unwrap_or_default()
                        .into_iter()
                        .map(move |pr| (record.id.0.clone(), format!("{} {}", pr.number, pr.state)))
                })
                .collect();
            states.sort();
            states
        }
    }

    #[cfg(unix)]
    fn open_status(url: &str, number: i64) -> PullRequestStatus {
        parse(
            serde_json::json!({ "number": number, "state": "OPEN" })
                .to_string()
                .as_bytes(),
            url,
            DateMillis(0.0),
        )
        .unwrap()
    }

    /// A GraphQL `PullRequest` node as the batch query returns it.
    fn graphql_node(number: i64, state: &str) -> Value {
        let empty = serde_json::json!({ "nodes": [], "pageInfo": { "hasNextPage": false } });
        serde_json::json!({
            "number": number, "title": "t", "author": { "login": "a", "id": "U_1" },
            "body": "", "baseRefName": "main", "headRefName": "h", "state": state,
            "isDraft": false, "reviewDecision": null, "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN", "additions": 1, "deletions": 0, "changedFiles": 1,
            "comments": empty, "reviews": empty, "commits": { "nodes": [] },
            "reviewThreads": { "totalCount": 0, "nodes": [] }
        })
    }

    #[cfg(unix)]
    #[test]
    fn refresh_updates_status_only_prs_and_every_session_sharing_the_url() {
        let url = "https://github.com/o/r/pull/12";
        // This gh answers every call with a `pr view` payload, so the batch
        // can't be read and the PR takes the per-PR fallback.
        let mut harness = Harness::new(
            &[
                ("visible", true, vec![open_status(url, 12)]),
                ("other", false, vec![open_status(url, 12)]),
            ],
            "#!/bin/sh\nprintf '%s' '{\"number\":12,\"state\":\"MERGED\"}'\n",
        );
        harness.refresh.insert(
            url.to_owned(),
            RefreshState {
                last_attempt: Some(Instant::now()),
                ..Default::default()
            },
        );
        harness.drain(PendingWake {
            sessions: HashSet::from(["visible".into()]),
            ..Default::default()
        });
        assert_eq!(
            harness.states(),
            [
                ("other".to_owned(), "12 MERGED".to_owned()),
                ("visible".to_owned(), "12 MERGED".to_owned()),
            ],
            "every record sharing the URL is refreshed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn one_gh_process_refreshes_every_due_pull_request() {
        let urls: Vec<String> = (1..=5)
            .map(|n| format!("https://github.com/o/r/pull/{n}"))
            .collect();
        // Due order is by URL, so alias pN answers pull/N+1.
        let response = serde_json::json!({
            "data": (0..5)
                .map(|index| (format!("p{index}"), serde_json::json!({ "pullRequest": graphql_node(index + 1, "MERGED") })))
                .collect::<serde_json::Map<String, Value>>()
        });
        let script = format!(
            "#!/bin/sh\necho \"$*\" >> $LOG\nprintf '%s' '{}'\n",
            response
        );
        let mut harness = Harness::new(
            &[(
                "visible",
                true,
                urls.iter()
                    .enumerate()
                    .map(|(index, url)| open_status(url, index as i64 + 1))
                    .collect(),
            )],
            &script,
        );
        harness.drain(PendingWake::default());

        let calls = harness.gh_calls();
        assert_eq!(
            calls.len(),
            1,
            "one batched request, not one per PR: {calls:?}"
        );
        assert!(calls[0].starts_with("api graphql --hostname github.com -f query="));
        assert_eq!(
            harness.states(),
            (1..=5)
                .map(|n| ("visible".to_owned(), format!("{n} MERGED")))
                .collect::<Vec<_>>()
        );
        assert!(
            urls.iter()
                .all(|url| harness.cache[url].total_threads == Some(0)),
            "first fetch also asks for review threads"
        );

        let attempts: HashSet<Option<Instant>> = harness
            .refresh
            .values()
            .map(|state| state.last_attempt)
            .collect();
        assert_eq!(attempts.len(), 1, "the chunk comes due again together");

        // The next due refresh skips threads (their TTL is 30 min) and keeps
        // the counts already known.
        for state in harness.refresh.values_mut() {
            state.last_attempt = Some(Instant::now() - Duration::from_secs(3600));
        }
        harness.drain(PendingWake::default());
        let calls = harness.gh_calls();
        assert_eq!(calls.len(), 2);
        assert!(
            !calls[1].contains("reviewThreads"),
            "threads aren't due again"
        );
        assert!(
            urls.iter()
                .all(|url| harness.cache[url].total_threads == Some(0)),
            "known thread counts are carried forward"
        );
    }

    #[cfg(unix)]
    #[test]
    fn prs_the_batch_cannot_resolve_fall_back_to_gh_pr_view() {
        let url = |n: i64| format!("https://github.com/o/r/pull/{n}");
        // pull/1 resolves; pull/2 is missing from the batch (a deleted
        // repo, say); pull/3 has more comments than one page holds.
        let mut long = graphql_node(3, "OPEN");
        long["comments"]["pageInfo"]["hasNextPage"] = Value::Bool(true);
        let response = serde_json::json!({
            "data": {
                "p0": { "pullRequest": graphql_node(1, "MERGED") },
                "p1": null,
                "p2": { "pullRequest": long },
            },
            "errors": [{ "type": "NOT_FOUND" }]
        });
        let script = format!(
            "#!/bin/sh\necho \"$*\" >> $LOG\n\
             case \"$1 $2\" in\n\
             'pr view') n=${{3##*/}}; printf '{{\"number\":%s,\"state\":\"CLOSED\"}}' \"$n\" ;;\n\
             *) case \"$*\" in *'fragment P'*) printf '%s' '{response}'; exit 1 ;; \
             *) printf '%s' '{{\"data\":{{\"repository\":{{\"pullRequest\":{{\"reviewThreads\":{{\"totalCount\":2,\"nodes\":[{{\"isResolved\":true}}]}}}}}}}}}}' ;; esac ;;\n\
             esac\n"
        );
        let mut harness = Harness::new(
            &[(
                "visible",
                true,
                (1..=3).map(|n| open_status(&url(n), n)).collect(),
            )],
            &script,
        );

        // The batch iteration hands the unresolved PRs to the fallback and
        // asks to be called again at once.
        assert_eq!(harness.sweep(PendingWake::default()), Duration::ZERO);
        assert_eq!(harness.fallback, HashSet::from([url(2), url(3)]));
        assert_eq!(harness.gh_calls().len(), 1);

        harness.drain(PendingWake::default());
        let calls = harness.gh_calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.contains("fragment P"))
                .count(),
            1,
            "the fallback does not retry the batch: {calls:?}"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.starts_with("pr view"))
                .count(),
            2
        );
        assert_eq!(
            harness.states(),
            [
                ("visible".to_owned(), "1 MERGED".to_owned()),
                ("visible".to_owned(), "2 CLOSED".to_owned()),
                ("visible".to_owned(), "3 CLOSED".to_owned()),
            ]
        );
        assert_eq!(
            harness.cache[&url(2)].total_threads,
            Some(2),
            "the per-PR fallback still fetches threads that were due: {calls:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_batch_falls_back_a_bounded_few_per_iteration() {
        let urls: Vec<String> = (1..=5)
            .map(|n| format!("https://github.com/o/r/pull/{n}"))
            .collect();
        let script = "#!/bin/sh\necho \"$*\" >> $LOG\n\
             case \"$1 $2\" in\n\
             'pr view') n=${3##*/}; printf '{\"number\":%s,\"state\":\"MERGED\"}' \"$n\" ;;\n\
             *) exit 1 ;;\n\
             esac\n";
        let mut harness = Harness::new(
            &[(
                "visible",
                true,
                urls.iter()
                    .enumerate()
                    .map(|(index, url)| open_status(url, index as i64 + 1))
                    .collect(),
            )],
            script,
        );
        let iterations = harness.drain(PendingWake::default());
        // One batch, then two per-PR fetches per iteration: never both kinds
        // of work in one iteration, so the per-iteration bound is unchanged.
        assert_eq!(iterations, 1 + 3);
        assert_eq!(
            harness.states(),
            (1..=5)
                .map(|n| ("visible".to_owned(), format!("{n} MERGED")))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn batch_query_passes_names_as_variables_and_asks_threads_only_when_due() {
        let target = |n: i64| BatchRef::parse(&format!("https://github.com/o/r/pull/{n}")).unwrap();
        let (query, variables) = batch_query(&[(target(1), false), (target(2), true)]);
        assert!(query.starts_with(
            "query($o0:String!,$n0:String!,$p0:Int!,$o1:String!,$n1:String!,$p1:Int!){"
        ));
        assert!(query.contains("p0:repository(owner:$o0,name:$n0){pullRequest(number:$p0){...P}}"));
        assert!(
            query.contains("p1:repository(owner:$o1,name:$n1){pullRequest(number:$p1){...P ...T}}")
        );
        assert!(query.contains("fragment T on PullRequest"));
        assert_eq!(
            variables,
            [
                ("-f", "o0=o".to_owned()),
                ("-f", "n0=r".to_owned()),
                ("-F", "p0=1".to_owned()),
                ("-f", "o1=o".to_owned()),
                ("-f", "n1=r".to_owned()),
                ("-F", "p1=2".to_owned()),
            ]
        );
        let (query, _) = batch_query(&[(target(1), false)]);
        assert!(
            !query.contains("fragment T"),
            "GraphQL rejects an unused fragment"
        );
    }

    #[test]
    fn only_plain_pull_request_urls_are_batched() {
        let parsed = BatchRef::parse("https://github.com/cristicretu/diri/pull/7/files").unwrap();
        assert_eq!(
            (
                parsed.host.as_str(),
                parsed.owner.as_str(),
                parsed.repo.as_str(),
                parsed.number
            ),
            ("github.com", "cristicretu", "diri", 7)
        );
        assert_eq!(
            parsed.url,
            "https://github.com/cristicretu/diri/pull/7/files"
        );
        assert_eq!(
            BatchRef::parse("https://ghe.example.com/o/r.js/pull/3")
                .unwrap()
                .host,
            "ghe.example.com"
        );
        for url in [
            "http://github.com/o/r/pull/1",
            "https://github.com/o/r/issues/1",
            "https://github.com/o/r/pull/x",
            "https://github.com/o/r/pull/0",
            "https://github.com/o/r\"){x}/pull/1",
            "https://user@github.com/o/r/pull/1",
            "https://github.com:8443/o/r/pull/1",
            "https://github.com/o/pull/1",
        ] {
            assert_eq!(BatchRef::parse(url), None, "{url}");
        }
    }

    /// Real `gh pr view --json` output and the batch query's node for the same
    /// pull requests, captured back to back (read-only) from
    /// cristicretu/diri, cli/cli and kubernetes/kubernetes with gh 2.101.0;
    /// only comment/review/PR bodies were replaced with placeholders, the
    /// same on both sides. `crates/diri-engine/examples/prbatch.rs capture`
    /// re-records them.
    #[test]
    fn batch_payloads_parse_exactly_like_gh_pr_view() {
        let fixtures: Vec<Value> = serde_json::from_str(include_str!(
            "../tests/fixtures/pr_monitor_batch_equivalence.json"
        ))
        .unwrap();
        let at = DateMillis(1.0);
        let mut seen = HashSet::new();
        for fixture in &fixtures {
            let url = fixture["url"].as_str().unwrap();
            let target = BatchRef::parse(url).unwrap();
            for threads in [true, false] {
                let response = serde_json::json!({
                    "data": { "p0": { "pullRequest": fixture["batch"] } }
                });
                let batched = parse_batch(
                    response.to_string().as_bytes(),
                    &[(target.clone(), threads)],
                    at,
                )
                .remove(0);
                if fixture["view"].is_null() {
                    assert_eq!(batched, None, "{url} needs gh's pagination");
                    seen.insert("fallback");
                    continue;
                }
                let mut expected = parse(fixture["view"].to_string().as_bytes(), url, at).unwrap();
                if threads {
                    let (resolved, total) =
                        parse_threads(fixture["threads"].to_string().as_bytes()).unwrap();
                    expected.resolved_threads = Some(resolved);
                    expected.total_threads = Some(total);
                }
                assert_eq!(batched.as_ref(), Some(&expected), "{url} threads={threads}");

                let status = expected;
                for (covered, label) in [
                    (status.is_draft, "draft"),
                    (status.state == "OPEN", "open"),
                    (status.state == "MERGED", "merged"),
                    (status.state == "CLOSED", "closed"),
                    (status.checks_failed > 0, "failing checks"),
                    (status.checks_pending > 0, "pending checks"),
                    (status.checks.is_none(), "no checks"),
                    (status.review_count > 0, "reviews"),
                    (status.comment_count > 0, "comments"),
                    (status.review_decision.is_some(), "review decision"),
                    (
                        status.resolved_threads.is_some_and(|n| n > 0),
                        "resolved threads",
                    ),
                    (
                        status
                            .author
                            .as_deref()
                            .is_some_and(|a| a.starts_with("app/")),
                        "bot author",
                    ),
                    (
                        status
                            .checks
                            .iter()
                            .flatten()
                            .any(|c| c.name.starts_with(" / ")),
                        "check run without a workflow",
                    ),
                    (
                        fixture["view"]["statusCheckRollup"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|c| c["__typename"] == "StatusContext"),
                        "status context",
                    ),
                    (
                        status.mergeable.as_deref() == Some("CONFLICTING"),
                        "conflicting",
                    ),
                ] {
                    if covered {
                        seen.insert(label);
                    }
                }
            }
        }
        for label in [
            "draft",
            "open",
            "merged",
            "closed",
            "failing checks",
            "pending checks",
            "no checks",
            "reviews",
            "comments",
            "review decision",
            "resolved threads",
            "bot author",
            "check run without a workflow",
            "status context",
            "conflicting",
            "fallback",
        ] {
            assert!(seen.contains(label), "the fixtures no longer cover {label}");
        }
    }

    /// GraphQL nulls the recorded PRs don't exercise, against the JSON gh's
    /// Go structs would export for them (`api/export_pr.go`, gh 2.101.0).
    #[test]
    fn graphql_nulls_become_what_gh_exports() {
        let mut node = graphql_node(9, "OPEN");
        node["author"] = Value::Null;
        node["reviewDecision"] = Value::Null;
        node["mergeable"] = Value::Null;
        node["comments"]["nodes"] = serde_json::json!([
            { "author": null, "body": "gone", "createdAt": "2026-08-07T10:00:00Z", "url": null }
        ]);
        node["reviews"]["nodes"] = serde_json::json!([
            { "author": { "login": "copilot" }, "body": "", "state": "PENDING", "submittedAt": null }
        ]);
        node["commits"]["nodes"] = serde_json::json!([{ "commit": { "statusCheckRollup": { "contexts": {
            "nodes": [
                { "__typename": "CheckRun", "name": "ext", "checkSuite": { "workflowRun": null },
                  "status": "QUEUED", "conclusion": null, "detailsUrl": null },
                { "__typename": "StatusContext", "context": "ci/x", "state": "PENDING", "targetUrl": null }
            ],
            "pageInfo": { "hasNextPage": false }
        } } } }]);
        let gh_view = serde_json::json!({
            "number": 9, "title": "t", "author": { "is_bot": true, "login": "app/" },
            "body": "", "baseRefName": "main", "headRefName": "h", "state": "OPEN",
            "isDraft": false, "reviewDecision": "", "mergeable": "", "mergeStateStatus": "CLEAN",
            "additions": 1, "deletions": 0, "changedFiles": 1,
            "comments": [{ "author": { "login": "" }, "body": "gone", "createdAt": "2026-08-07T10:00:00Z" }],
            "reviews": [{ "author": { "login": "copilot" }, "body": "", "state": "PENDING", "submittedAt": null }],
            "statusCheckRollup": [
                { "__typename": "CheckRun", "name": "ext", "workflowName": "", "status": "QUEUED",
                  "conclusion": "", "detailsUrl": "" },
                { "__typename": "StatusContext", "context": "ci/x", "state": "PENDING", "targetUrl": "" }
            ]
        });
        let url = "https://github.com/o/r/pull/9";
        let expected = parse(gh_view.to_string().as_bytes(), url, DateMillis(0.0)).unwrap();
        let batched =
            parse_view(&gh_view_from_graphql(&node).unwrap(), url, DateMillis(0.0)).unwrap();
        assert_eq!(batched, expected);
        assert_eq!(batched.author.as_deref(), Some("app/"));
        assert_eq!(batched.checks.as_ref().unwrap()[0].name, " / ext");

        // A head commit without a rollup exports no checks; a PR with no
        // commits exports null. Both read as no checks.
        node["commits"]["nodes"] = serde_json::json!([{ "commit": { "statusCheckRollup": null } }]);
        assert_eq!(
            gh_view_from_graphql(&node).unwrap()["statusCheckRollup"],
            serde_json::json!([])
        );
        node["commits"]["nodes"] = serde_json::json!([]);
        assert_eq!(
            gh_view_from_graphql(&node).unwrap()["statusCheckRollup"],
            Value::Null
        );
    }

    #[cfg(unix)]
    #[test]
    fn gh_output_larger_than_a_pipe_buffer_is_read_in_full() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let gh = temp.path().join("gh");
        // 1 MiB: far past the 64 KiB pipe buffer the old poll-then-read
        // loop needed gh to fit in before it could exit.
        std::fs::write(&gh, "#!/bin/sh\nhead -c 1048576 /dev/zero | tr '\\0' x\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = run_gh(gh.to_str().unwrap(), &[], Duration::from_secs(10)).unwrap();
        assert_eq!(output.len(), 1 << 20);
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_gh_is_killed_at_the_timeout() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let gh = temp.path().join("gh");
        std::fs::write(&gh, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let started = Instant::now();
        assert_eq!(
            run_gh(gh.to_str().unwrap(), &[], Duration::from_millis(300)),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

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
    fn a_merged_or_closed_pr_on_screen_is_not_polled_every_minute() {
        let status = |state: &str| {
            parse(
                serde_json::json!({ "number": 1, "state": state })
                    .to_string()
                    .as_bytes(),
                "https://github.com/o/r/pull/1",
                diri_proto::DateMillis(0.0),
            )
            .expect("status")
        };
        let url = "https://github.com/o/r/pull/1".to_string();
        let targets = HashMap::from([(url.clone(), PollInterest::Foreground)]);
        let now = Instant::now();
        let refresh = HashMap::from([(
            url.clone(),
            RefreshState {
                last_attempt: Some(now),
                ..RefreshState::default()
            },
        )]);
        let delay = |state: &str| {
            let cache = HashMap::from([(url.clone(), status(state))]);
            next_refresh_delay(&targets, &refresh, &cache, &HashSet::new(), now)
        };
        assert_eq!(delay("OPEN"), Duration::from_secs(60));
        assert_eq!(delay("MERGED"), Duration::from_secs(30 * 60));
        assert_eq!(delay("CLOSED"), Duration::from_secs(30 * 60));
        // Viewing the session still refetches a settled PR at once.
        let cache = HashMap::from([(url.clone(), status("MERGED"))]);
        assert_eq!(
            next_refresh_delay(
                &targets,
                &refresh,
                &cache,
                &HashSet::from([url.clone()]),
                now
            ),
            Duration::ZERO
        );
    }

    #[test]
    fn foreground_and_background_cadences_match_visible_pr_ui() {
        let mut state = RefreshState::default();
        assert_eq!(
            state.interval(PollInterest::Foreground, false),
            Duration::from_secs(60)
        );
        assert_eq!(
            state.interval(PollInterest::Background, false),
            Duration::from_secs(5 * 60)
        );

        state.record_result(PollInterest::Background, false);
        assert_eq!(
            state.interval(PollInterest::Background, false),
            Duration::from_secs(10 * 60)
        );
        state.record_result(PollInterest::Background, false);
        state.record_result(PollInterest::Background, false);
        state.record_result(PollInterest::Background, false);
        assert_eq!(
            state.interval(PollInterest::Background, false),
            Duration::from_secs(30 * 60),
            "background polling caps at thirty minutes"
        );

        state.record_result(PollInterest::Background, true);
        assert_eq!(
            state.interval(PollInterest::Background, false),
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
            next_refresh_delay(
                &targets,
                &HashMap::new(),
                &HashMap::new(),
                &HashSet::new(),
                Instant::now()
            ),
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
            next_refresh_delay(
                &targets,
                &refresh,
                &HashMap::new(),
                &HashSet::from([url]),
                Instant::now(),
            ),
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
