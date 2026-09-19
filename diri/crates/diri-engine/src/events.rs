//! Seq-stamped pub/sub with a bounded replay ring, backing `events.subscribe`
//! (bounded replay via `sinceSeq`, server-side filtering) and `events.wait`
//! (long-poll).
//!
//! Ported from the Swift `EventBus` actor. Backpressure is the load-bearing
//! property: the daemon is long-lived, and a subscriber may be a script that
//! stopped reading, a laptop that slept mid-`ssh`, or a crashed app whose
//! socket hasn't been reaped. `publish` therefore never blocks on a consumer —
//! each subscriber owns a fixed-size queue, and on overflow the *oldest*
//! queued events are evicted so the newest state still gets through. The
//! subscriber learns about the hole exactly once per burst via a synthetic
//! `events.dropped` marker, which makes the loss recoverable rather than
//! silent.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use diri_proto::JsonValue;
use serde_json::json;

/// The synthetic hole marker. Its seq is 0 — outside the published seq space,
/// which starts at 1 — so a consumer tracking `lastSeq` for gapless resume
/// can ignore it without special-casing.
pub const EVENTS_DROPPED: &str = "events.dropped";

/// One published event, as a subscriber receives it.
#[derive(Clone, Debug)]
pub struct Event {
    pub name: String,
    pub seq: u64,
    /// The session this event is about, when it is about one. Kept out of
    /// `params` so filtering never costs a JSON decode per publish.
    pub session_id: Option<String>,
    pub params: JsonValue,
}

/// Server-side subscription filter. Filtering here rather than at the
/// connection means a narrow subscriber's queue only fills with events it
/// asked for, so its bound actually protects it.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub sessions: Option<HashSet<String>>,
    pub kinds: Option<HashSet<String>>,
}

impl Filter {
    pub fn all() -> Self {
        Self::default()
    }

    pub fn new(sessions: Option<Vec<String>>, kinds: Option<Vec<String>>) -> Self {
        let normalize = |list: Option<Vec<String>>| {
            list.map(HashSet::from_iter)
                .filter(|set: &HashSet<String>| !set.is_empty())
        };
        Self {
            sessions: normalize(sessions),
            kinds: normalize(kinds),
        }
    }

    fn admits(&self, event: &Event) -> bool {
        // The drop marker is the one thing a filter can never hide: a narrow
        // subscriber still has to learn its slice has a hole.
        if event.name == EVENTS_DROPPED {
            return true;
        }
        if let Some(kinds) = &self.kinds
            && !kinds.contains(&event.name)
        {
            return false;
        }
        if let Some(sessions) = &self.sessions {
            match &event.session_id {
                Some(id) if sessions.contains(id) => {}
                _ => return false,
            }
        }
        true
    }
}

/// A replay entry keeps the encoded params rather than the JSON object graph,
/// so the ring's byte bound describes resident memory.
struct Archived {
    name: String,
    seq: u64,
    session_id: Option<String>,
    params: Vec<u8>,
}

impl Archived {
    fn storage_bytes(&self) -> usize {
        storage_bytes(&self.name, self.session_id.as_deref(), self.params.len())
    }

    fn event(&self) -> Event {
        Event {
            name: self.name.clone(),
            seq: self.seq,
            session_id: self.session_id.clone(),
            params: serde_json::from_slice(&self.params).unwrap_or(JsonValue::Null),
        }
    }
}

/// What one event is charged against a byte bound: its encoded params plus
/// its envelope. The ring holds exactly these bytes. A subscriber queue holds
/// the parsed object graph, which is larger, but by a factor and not without
/// limit — so the same charge bounds it too.
fn storage_bytes(name: &str, session_id: Option<&str>, encoded_params: usize) -> usize {
    name.len() + encoded_params + session_id.map_or(0, str::len) + 16
}

/// One live subscription's queue, shared between the bus and its stream.
struct SubscriberQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

struct QueueState {
    /// Each event with what it was charged, so eviction refunds exactly that.
    queue: VecDeque<(Event, usize)>,
    queued_bytes: usize,
    filter: Filter,
    capacity: usize,
    byte_capacity: usize,
    dropped: u64,
    first_dropped_seq: u64,
    last_dropped_seq: u64,
    closed: bool,
}

impl QueueState {
    fn note_gap(&mut self, first: u64, last: u64, count: u64) {
        if self.dropped == 0 {
            self.first_dropped_seq = first;
        }
        self.last_dropped_seq = last;
        self.dropped = self.dropped.saturating_add(count);
    }

    /// Loss is reported by the reader, even when no more events arrive. The
    /// marker lives outside the bounded data queue and cannot evict an event.
    fn pop(&mut self) -> Option<Event> {
        if self.dropped > 0 {
            let marker = Event {
                name: EVENTS_DROPPED.into(),
                seq: 0,
                session_id: None,
                params: json!({
                    "dropped": self.dropped,
                    "fromSeq": self.first_dropped_seq,
                    "toSeq": self.last_dropped_seq,
                }),
            };
            self.dropped = 0;
            return Some(marker);
        }
        let (event, bytes) = self.queue.pop_front()?;
        self.queued_bytes -= bytes;
        Some(event)
    }
}

impl SubscriberQueue {
    /// Enqueues without consumer I/O. Overflow evicts the oldest data events;
    /// the next read reports the hole before delivering surviving events.
    ///
    /// The count alone is no bound on memory: a `session.updated` carries a
    /// whole record, and a client that is connected but not reading — a
    /// suspended App, a wedged socket — would hold thousands of them. `bytes`
    /// is the event's [`storage_bytes`]. An event larger than the whole
    /// allowance is still delivered, alone: the reader must be able to make
    /// progress, and one event is its own bound.
    fn push(&self, event: &Event, bytes: usize) {
        let mut state = self.state.lock().expect("queue");
        if state.closed || !state.filter.admits(event) {
            return;
        }
        while state.queue.len() >= state.capacity
            || (!state.queue.is_empty() && state.queued_bytes + bytes > state.byte_capacity)
        {
            let Some((evicted, refund)) = state.queue.pop_front() else {
                break;
            };
            state.queued_bytes -= refund;
            state.note_gap(evicted.seq, evicted.seq, 1);
        }
        state.queued_bytes += bytes;
        state.queue.push_back((event.clone(), bytes));
        drop(state);
        self.ready.notify_all();
    }
}

struct BusInner {
    next_seq: u64,
    ring: VecDeque<Archived>,
    ring_bytes: usize,
    subscribers: HashMap<u64, Arc<SubscriberQueue>>,
    next_subscriber: u64,
}

/// The bus itself; cheap to clone, shared by the control server and the
/// registry watcher.
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<Mutex<BusInner>>,
    activity: Arc<Mutex<Option<crate::activity::ActivityLog>>>,
    ring_capacity: usize,
    ring_byte_capacity: usize,
    subscriber_capacity: usize,
    subscriber_byte_capacity: usize,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self::with_capacities(4096, 8 << 20, None)
    }

    /// `subscriber_capacity` defaults to twice the ring, so a full `sinceSeq`
    /// replay — which lands before the consumer reads a single event — can
    /// never itself trigger a drop. A subscriber's byte allowance is twice the
    /// ring's for the same reason; a bus with no ring has no byte bound to
    /// take it from and keeps the count alone.
    pub fn with_capacities(
        ring_capacity: usize,
        ring_byte_capacity: usize,
        subscriber_capacity: Option<usize>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(BusInner {
                next_seq: 1,
                ring: VecDeque::new(),
                ring_bytes: 0,
                subscribers: HashMap::new(),
                next_subscriber: 0,
            })),
            activity: Arc::new(Mutex::new(None)),
            ring_capacity,
            ring_byte_capacity,
            subscriber_capacity: subscriber_capacity
                .unwrap_or(ring_capacity.max(1) * 2)
                .max(1),
            subscriber_byte_capacity: match ring_byte_capacity {
                0 => usize::MAX,
                bytes => bytes.saturating_mul(2),
            },
        }
    }

    pub fn publish(&self, name: &str, params: JsonValue, session_id: Option<&str>) {
        let mut inner = self.inner.lock().expect("bus");
        let event = Event {
            name: name.to_string(),
            seq: inner.next_seq,
            session_id: session_id.map(str::to_string),
            params,
        };
        inner.next_seq += 1;

        let encoded = serde_json::to_vec(&event.params).ok();
        let bytes = storage_bytes(
            &event.name,
            event.session_id.as_deref(),
            encoded.as_ref().map_or(0, Vec::len),
        );
        if self.ring_capacity > 0
            && self.ring_byte_capacity > 0
            && let Some(encoded) = encoded
        {
            let archived = Archived {
                name: event.name.clone(),
                seq: event.seq,
                session_id: event.session_id.clone(),
                params: encoded,
            };
            inner.ring_bytes += archived.storage_bytes();
            inner.ring.push_back(archived);
            while inner.ring.len() > self.ring_capacity
                || inner.ring_bytes > self.ring_byte_capacity
            {
                if let Some(oldest) = inner.ring.pop_front() {
                    inner.ring_bytes -= oldest.storage_bytes();
                } else {
                    break;
                }
            }
        }

        // Keep sequence assignment, archiving and live enqueue in the same
        // existing critical section. Unlocking before enqueue lets another
        // publisher overtake us. Subscribe uses this same lock, so replay
        // and the following live tail share that order. Queues never do I/O.
        for queue in inner.subscribers.values() {
            queue.push(&event, bytes);
        }
    }

    /// Encodes and publishes a typed payload. An event that cannot serialize
    /// is a daemon bug, never a reason to fail the caller's mutation.
    pub fn publish_encoded<T: serde::Serialize>(
        &self,
        name: &str,
        value: &T,
        session_id: Option<&str>,
    ) {
        if let Ok(params) = serde_json::to_value(value) {
            if name == diri_proto::EventName::SESSION_UPDATED
                && let Ok(record) =
                    serde_json::from_value::<diri_proto::SessionRecord>(params.clone())
                && let Ok(mut activity) = self.activity.lock()
                && let Some(activity) = activity.as_mut()
                && let Err(error) = activity.observe(&record)
            {
                eprintln!("diri-engine: activity log append failed: {error}");
            }
            self.publish(name, params, session_id);
        }
    }

    /// Enables durable history at the same publication seam used by every
    /// live-status producer. Reconfiguration is used only during daemon/test
    /// construction, before publishers start.
    pub fn enable_activity_log(&self, path: impl Into<std::path::PathBuf>) -> std::io::Result<()> {
        let log = crate::activity::ActivityLog::load(path)?;
        *self.activity.lock().expect("activity log") = Some(log);
        Ok(())
    }

    pub fn recent_activity(&self, limit: usize) -> Vec<diri_proto::ActivityEntry> {
        self.activity
            .lock()
            .ok()
            .and_then(|activity| activity.as_ref().map(|activity| activity.recent(limit)))
            .unwrap_or_default()
    }

    /// Records a final snapshot before `session.remove` makes the Registry
    /// record unavailable, while keeping the existing wire event unchanged.
    pub fn record_removed(&self, record: &diri_proto::SessionRecord) {
        if let Ok(mut activity) = self.activity.lock()
            && let Some(activity) = activity.as_mut()
            && let Err(error) = activity.observe_removed(record)
        {
            eprintln!("diri-engine: activity log append failed: {error}");
        }
    }

    /// Subscribes; ring events with `seq > since_seq` are replayed first.
    /// The filter applies to both the replay and the live tail. If the cursor
    /// predates retained data, an unfiltered gap marker precedes replay. Its
    /// range describes unavailable global events; some may not match the filter.
    /// Sequence cursors belong to this Engine lifetime, not a durable journal.
    pub fn subscribe(&self, since_seq: Option<u64>, filter: Filter) -> EventStream {
        let queue = Arc::new(SubscriberQueue {
            state: Mutex::new(QueueState {
                queue: VecDeque::new(),
                queued_bytes: 0,
                filter,
                capacity: self.subscriber_capacity,
                byte_capacity: self.subscriber_byte_capacity,
                dropped: 0,
                first_dropped_seq: 0,
                last_dropped_seq: 0,
                closed: false,
            }),
            ready: Condvar::new(),
        });

        let mut inner = self.inner.lock().expect("bus");
        if let Some(since) = since_seq {
            let first_available = inner.ring.front().map_or(inner.next_seq, |event| event.seq);
            if let Some(first_missing) = since.checked_add(1)
                && first_missing < first_available
            {
                queue.state.lock().expect("queue").note_gap(
                    first_missing,
                    first_available - 1,
                    first_available - first_missing,
                );
            }
            for archived in inner.ring.iter().filter(|archived| archived.seq > since) {
                queue.push(&archived.event(), archived.storage_bytes());
            }
        }
        let id = inner.next_subscriber;
        inner.next_subscriber += 1;
        inner.subscribers.insert(id, Arc::clone(&queue));
        EventStream {
            bus: Arc::clone(&self.inner),
            id,
            queue,
        }
    }

    pub fn current_seq(&self) -> u64 {
        self.inner.lock().expect("bus").next_seq - 1
    }

    #[cfg(test)]
    fn subscriber_count(&self) -> usize {
        self.inner.lock().expect("bus").subscribers.len()
    }
}

/// The receiving half of a subscription; dropping it unsubscribes.
pub struct EventStream {
    bus: Arc<Mutex<BusInner>>,
    id: u64,
    queue: Arc<SubscriberQueue>,
}

impl EventStream {
    /// Blocks until an event arrives or `timeout` elapses.
    pub fn recv(&self, timeout: Duration) -> Option<Event> {
        let deadline = Instant::now() + timeout;
        let mut state = self.queue.state.lock().expect("queue");
        loop {
            if let Some(event) = state.pop() {
                return Some(event);
            }
            let remaining = deadline.checked_duration_since(Instant::now())?;
            let (next, wait) = self
                .queue
                .ready
                .wait_timeout(state, remaining)
                .expect("queue");
            state = next;
            if wait.timed_out() && state.queue.is_empty() && state.dropped == 0 {
                return None;
            }
        }
    }

    /// An event already queued, without waiting.
    pub fn try_recv(&self) -> Option<Event> {
        self.queue.state.lock().expect("queue").pop()
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.queue.state.lock().expect("queue").closed = true;
        if let Ok(mut inner) = self.bus.lock() {
            inner.subscribers.remove(&self.id);
        }
    }
}

/// Whether `status` satisfies an `events.wait` target. The alias table
/// ("done" ⇒ idle, "needs_me"/"needs-input"/"blocked" ⇒ needsInput) is the
/// Swift daemon's, so every caller resolves the same vocabulary.
pub fn satisfies_wait_target(status: &diri_proto::SessionStatus, target: &str) -> bool {
    use diri_proto::SessionStatus as S;
    match target {
        "idle" | "done" => matches!(status, S::Idle),
        "working" => matches!(status, S::Working),
        "starting" => matches!(status, S::Starting),
        "unknown" => matches!(status, S::Unknown),
        "needsInput" | "needs_input" | "needs-input" | "needs_me" | "blocked" => {
            matches!(status, S::NeedsInput(_))
        }
        "exited" | "dead" => matches!(status, S::Exited(_)),
        _ => false,
    }
}

/// Publishes `session.updated` whenever a live session's observable state
/// changes, by diffing registry views on a short cadence. The Swift daemon
/// publishes at each mutation site inside its status engine; this engine's
/// state changes on pump threads, so a watcher is the equivalent seam.
fn next_notification_id() -> u64 {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

pub fn spawn_registry_watcher(
    registry: Arc<Mutex<crate::registry::Registry>>,
    events: EventBus,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("diri-events-watcher".into())
        .spawn(move || {
            // Each session bumps a version counter exactly when its status,
            // needs-input, or title change, so the steady-state poll is one
            // integer compare per live session — the previous implementation
            // cloned and JSON-serialized every record (live and archived) on
            // every pass, all under the registry lock.
            let mut published: HashMap<String, u64> = HashMap::new();
            while !stop.load(Ordering::SeqCst) {
                let (mut changed, cursor_requests, native_title_requests, completed) = {
                    let Ok(mut registry) = registry.lock() else {
                        break;
                    };
                    (
                        registry.changed_since(&mut published),
                        registry.cursor_refresh_requests(),
                        registry.native_title_refresh_requests(),
                        registry.take_completed_publications(),
                    )
                };
                // Retained terminals are written here, off the Registry lock,
                // so a slow disk never stalls input or grid publication.
                let published_any = !completed.is_empty();
                for publication in completed {
                    let id = publication.session_id().to_owned();
                    if let Err(error) = publication.publish() {
                        eprintln!(
                            "diri-engine: completed terminal for {id} was not retained: {error}"
                        );
                    }
                }
                // Growth happens only on publication, so that is the only
                // moment the bounds need enforcing.
                if published_any {
                    let retention = registry
                        .lock()
                        .ok()
                        .map(|registry| registry.completed_retention());
                    if let Some(retention) = retention
                        && let Err(error) = retention.apply()
                    {
                        eprintln!("diri-engine: completed terminal retention failed: {error}");
                    }
                }
                let cursor_refreshes = crate::registry::scan_cursor_refreshes(cursor_requests);
                let native_title_refreshes =
                    crate::registry::scan_native_title_refreshes(native_title_requests);
                if !cursor_refreshes.is_empty() || !native_title_refreshes.is_empty() {
                    let Ok(mut registry) = registry.lock() else {
                        break;
                    };
                    changed.extend(registry.apply_cursor_refreshes(cursor_refreshes));
                    changed.extend(registry.apply_native_title_refreshes(native_title_refreshes));
                }
                for (id, record) in changed {
                    events.publish_encoded(
                        diri_proto::EventName::SESSION_UPDATED,
                        &record,
                        Some(&id),
                    );
                    let notifications = registry.lock().expect("registry").take_notifications(&id);
                    for notification in notifications {
                        let event = diri_proto::SessionNotificationEvent {
                            id: format!(
                                "osc-{}-{}",
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_nanos(),
                                next_notification_id()
                            ),
                            session_id: record.id.clone(),
                            session_created_at: record.created_at,
                            occurred_at: diri_proto::DateMillis::from(std::time::SystemTime::now()),
                            title: notification.title,
                            body: notification.body,
                        };
                        events.publish_encoded(
                            diri_proto::EventName::SESSION_NOTIFICATION,
                            &event,
                            Some(&id),
                        );
                    }
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        })
        .expect("spawn watcher")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_names(stream: &EventStream) -> Vec<String> {
        let mut names = Vec::new();
        while let Some(event) = stream.try_recv() {
            names.push(event.name);
        }
        names
    }

    #[test]
    fn events_arrive_in_publish_order_with_increasing_seqs() {
        let bus = EventBus::new();
        let stream = bus.subscribe(None, Filter::all());
        bus.publish("a", json!({"n": 1}), None);
        bus.publish("b", json!({"n": 2}), None);

        let first = stream.recv(Duration::from_secs(1)).expect("first");
        let second = stream.recv(Duration::from_secs(1)).expect("second");
        assert_eq!((first.name.as_str(), first.seq), ("a", 1));
        assert_eq!((second.name.as_str(), second.seq), ("b", 2));
    }

    #[test]
    fn since_seq_replays_the_ring_gaplessly() {
        let bus = EventBus::new();
        bus.publish("one", json!({}), None);
        bus.publish("two", json!({}), None);
        bus.publish("three", json!({}), None);

        let stream = bus.subscribe(Some(1), Filter::all());
        assert_eq!(event_names(&stream), ["two", "three"]);
    }

    #[test]
    fn filters_narrow_by_kind_and_session() {
        let bus = EventBus::new();
        let stream = bus.subscribe(
            None,
            Filter::new(
                Some(vec!["s_1".into()]),
                Some(vec!["session.updated".into()]),
            ),
        );
        bus.publish("session.updated", json!({}), Some("s_1"));
        bus.publish("session.updated", json!({}), Some("s_2")); // other session
        bus.publish("worktree.created", json!({}), Some("s_1")); // other kind
        assert_eq!(event_names(&stream), ["session.updated"]);
    }

    #[test]
    fn overflow_evicts_oldest_and_marks_the_hole_once() {
        let bus = EventBus::with_capacities(64, 1 << 20, Some(2));
        let stream = bus.subscribe(None, Filter::all());
        for n in 0..5 {
            bus.publish("burst", json!({ "n": n }), None);
        }
        // The final burst needs no later publish to report its loss. The
        // marker is returned before the two surviving events.
        let marker = stream.recv(Duration::from_secs(1)).expect("marker");
        assert_eq!(marker.name, EVENTS_DROPPED);
        assert_eq!(marker.seq, 0, "outside the published seq space");
        assert_eq!(marker.params["dropped"], 3);
        assert_eq!(marker.params["fromSeq"], 1);
        assert_eq!(marker.params["toSeq"], 3);
        let survivors: Vec<Event> = std::iter::from_fn(|| stream.try_recv()).collect();
        assert_eq!(survivors.len(), 2);
        assert_eq!(survivors[0].seq, 4);
        assert_eq!(survivors[1].seq, 5);

        bus.publish("after", json!({}), None);
        assert_eq!(event_names(&stream), ["after"]);
    }

    #[test]
    fn a_subscriber_that_stops_reading_is_bounded_by_bytes_not_only_count() {
        // Room for thousands of events by count, and 4 KiB by bytes (twice
        // the 2 KiB ring). Each event below is charged a little over 1 KiB.
        let bus = EventBus::with_capacities(4096, 2 << 10, None);
        let stream = bus.subscribe(None, Filter::all());
        let body = "x".repeat(1 << 10);
        for _ in 0..100 {
            bus.publish("session.updated", json!({ "record": body }), None);
        }
        {
            let state = stream.queue.state.lock().unwrap();
            assert_eq!(state.queue.len(), 3, "100 by count alone");
            assert!(state.queued_bytes <= 4 << 10);
        }
        // The reader learns of the hole first, then gets the newest events.
        let marker = stream.try_recv().expect("marker");
        assert_eq!(marker.name, EVENTS_DROPPED);
        assert_eq!(marker.params["dropped"], 97);
        assert_eq!(marker.params["fromSeq"], 1);
        assert_eq!(marker.params["toSeq"], 97);
        let survivors: Vec<u64> = std::iter::from_fn(|| stream.try_recv())
            .map(|event| event.seq)
            .collect();
        assert_eq!(survivors, [98, 99, 100]);
        assert_eq!(stream.queue.state.lock().unwrap().queued_bytes, 0);

        // One event over the whole allowance still gets through, alone.
        bus.publish("small", json!({}), None);
        bus.publish("huge", json!({ "record": "x".repeat(8 << 10) }), None);
        assert_eq!(event_names(&stream), [EVENTS_DROPPED, "huge"]);
    }

    #[test]
    fn a_full_replay_fits_a_new_subscriber_without_a_drop() {
        let bus = EventBus::with_capacities(4096, 2 << 10, None);
        for _ in 0..100 {
            bus.publish(
                "session.updated",
                json!({ "record": "x".repeat(256) }),
                None,
            );
        }
        let replayed = event_names(&bus.subscribe(Some(0), Filter::all()));
        // The ring already dropped what it could not hold: one marker for
        // that, and then everything it retained, none of it evicted again.
        assert_eq!(
            replayed
                .iter()
                .filter(|name| *name == EVENTS_DROPPED)
                .count(),
            1
        );
        assert!(replayed.len() > 2);
        assert_eq!(replayed[0], EVENTS_DROPPED);
    }

    #[test]
    fn a_dropped_stream_unsubscribes() {
        let bus = EventBus::new();
        let stream = bus.subscribe(None, Filter::all());
        assert_eq!(bus.subscriber_count(), 1);
        drop(stream);
        assert_eq!(bus.subscriber_count(), 0);
        bus.publish("into the void", json!({}), None); // must not panic
    }

    #[test]
    fn recv_times_out_when_nothing_is_published() {
        let bus = EventBus::new();
        let stream = bus.subscribe(None, Filter::all());
        let started = Instant::now();
        assert!(stream.recv(Duration::from_millis(50)).is_none());
        assert!(started.elapsed() >= Duration::from_millis(45));
    }

    #[test]
    fn wait_targets_resolve_the_swift_alias_table() {
        use diri_proto::{ExitInfo, ExitReason, SessionStatus};
        let idle = SessionStatus::Idle;
        assert!(satisfies_wait_target(&idle, "idle"));
        assert!(satisfies_wait_target(&idle, "done"));
        assert!(!satisfies_wait_target(&idle, "working"));

        let exited = SessionStatus::Exited(ExitInfo {
            reason: ExitReason::Exited,
            code: Some(0),
            signal: None,
        });
        assert!(satisfies_wait_target(&exited, "exited"));
        assert!(satisfies_wait_target(&exited, "dead"));
        assert!(!satisfies_wait_target(&exited, "nonsense"));
    }
}
