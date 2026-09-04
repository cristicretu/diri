//! The resource governor: memory footprints, listening ports, artifact
//! publication, and the auto-hibernation policies.
//!
//! Ported from `ResourceGovernor`. One sweep every 30 seconds walks each live
//! session's process tree, sums physical footprints, scans listening ports
//! for attached sessions (every 4th tick), folds the session's screen-scanned
//! artifacts in, and publishes what changed as a `session.resources` event.
//! Three policies then reclaim memory — always only from idle, unattended
//! sessions: a hard per-session limit, a sustained-idle freeze, and a global
//! budget that freezes idle sessions oldest-first until under.
//!
//! "Idle" is not taken on the status machine's word alone. Status is a
//! heuristic read off the screen, hooks, and transcripts, and it has been
//! wrong in every way that matters here: a shell whose dev server is
//! serving, an agent inside a long silent tool call, a Cursor turn the
//! transcript closed early. Freezing any of those is indistinguishable from
//! killing them — connections drop, servers stop answering, parents waiting
//! on a child hang. So the governor keeps its own evidence, independent of
//! status: how much CPU the tree burned since the last sweep, how much it
//! wrote to the PTY, whether its process set changed, and — right before a
//! freeze — whether anything in it is listening on a port. A session is
//! only frozen after a whole idle-threshold's worth of consecutive quiet
//! sweeps on top of reading idle, and the quiet clock restarts at daemon
//! boot, so stale record timestamps never freeze anything on their own.

use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diri_proto::{PortInfo, SessionStatus};

use crate::attach::AttachHub;
use crate::events::EventBus;
use crate::holder::process_tree;
use crate::registry::Registry;

/// Tunables. `governor.configure` overrides the two the app exposes; the
/// rest are the daemon's own, and err towards leaving sessions alone: a
/// freeze the user did not ask for costs far more than the memory it saves.
#[derive(Clone, Debug)]
pub struct GovernorConfig {
    pub idle_threshold_seconds: f64,
    pub hard_memory_bytes: u64,
    pub global_budget_fraction: f64,
    pub budget_min_idle_seconds: f64,
    pub hibernated_sample_every: u64,
    pub port_scan_enabled: bool,
    pub scan_interval: Duration,
    /// A tree that burned more than this fraction of the wall time between
    /// two sweeps is working. Measured over 20 s on a Mac with 14 live
    /// sessions: idle Claude Code trees of 5–23 processes (shell, agent,
    /// MCP servers) sat at 0.4–1.0%; the trees with a turn in flight read
    /// 3.7% and 8.7%. A build or test run is far above any of these.
    pub busy_cpu_fraction: f64,
    /// A tree that wrote at least this many bytes to its PTY between two
    /// sweeps is working. A spinner frame alone is tens of bytes at ~10 fps,
    /// so any in-flight turn clears this by orders of magnitude.
    pub busy_output_bytes: u64,
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self {
            idle_threshold_seconds: 3600.0,
            hard_memory_bytes: 16 << 30,
            global_budget_fraction: 0.9,
            budget_min_idle_seconds: 1800.0,
            hibernated_sample_every: 5,
            port_scan_enabled: true,
            scan_interval: Duration::from_secs(30),
            busy_cpu_fraction: 0.03,
            busy_output_bytes: 1024,
        }
    }
}

// MARK: Liveness

/// What one sweep saw of a session's tree, in the terms the quiet check
/// compares.
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub at_ms: f64,
    /// Summed user+system CPU time of every process in the tree.
    pub cpu_nanos: u64,
    /// The session's output-log tail offset.
    pub output_tail: u64,
    /// The tree's pids, sorted, so membership changes are visible.
    pub pids: Vec<i32>,
}

struct Tracked {
    last: Observation,
    quiet_since_ms: f64,
}

/// The governor's own, status-independent record of which sessions have
/// been doing nothing, and for how long. Lives on the governor thread; a
/// fresh daemon starts with no evidence at all.
#[derive(Default)]
pub struct Liveness {
    seen: HashMap<String, Tracked>,
}

impl Liveness {
    /// Folds one sweep's observation. Returns when the session's current
    /// quiet stretch began, or `None` when there is no quiet stretch — the
    /// tree was busy since the last sweep, or this is its first sighting and
    /// there is nothing yet to compare against.
    pub fn observe(&mut self, id: &str, now: Observation, config: &GovernorConfig) -> Option<f64> {
        let Some(tracked) = self.seen.get_mut(id) else {
            let quiet_since_ms = now.at_ms;
            self.seen.insert(
                id.to_owned(),
                Tracked {
                    last: now,
                    quiet_since_ms,
                },
            );
            return None;
        };
        let busy = is_busy(&tracked.last, &now, config);
        if busy {
            tracked.quiet_since_ms = now.at_ms;
        }
        tracked.last = now;
        (!busy).then_some(tracked.quiet_since_ms)
    }

    /// Drops what was known about `id`. Called when a session is frozen, so
    /// that after it wakes the quiet clock starts over rather than resuming
    /// a stretch measured before the freeze.
    pub fn forget(&mut self, id: &str) {
        self.seen.remove(id);
    }

    /// Keeps only the sessions still on the books.
    pub fn retain<'a>(&mut self, live: impl IntoIterator<Item = &'a str>) {
        let live: std::collections::HashSet<&str> = live.into_iter().collect();
        self.seen.retain(|id, _| live.contains(id.as_str()));
    }
}

/// Whether the tree did anything between `prev` and `now`. Every comparison
/// fails towards busy: a clock that did not advance, a CPU total that went
/// backwards (a member exited), a process set that changed.
pub fn is_busy(prev: &Observation, now: &Observation, config: &GovernorConfig) -> bool {
    let elapsed_ms = now.at_ms - prev.at_ms;
    if elapsed_ms.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        return true;
    }
    if now.pids != prev.pids {
        return true;
    }
    let Some(cpu_delta) = now.cpu_nanos.checked_sub(prev.cpu_nanos) else {
        return true;
    };
    if cpu_delta as f64 > elapsed_ms * 1_000_000.0 * config.busy_cpu_fraction {
        return true;
    }
    match now.output_tail.checked_sub(prev.output_tail) {
        Some(written) => written >= config.busy_output_bytes,
        None => true,
    }
}

/// An idle, unattended, quiet session the policies may freeze.
struct Candidate {
    id: String,
    /// When the session's idle stretch began: the later of what the record
    /// says and what the governor has itself observed.
    idle_since_ms: f64,
    footprint: u64,
    pids: Vec<i32>,
}

pub fn should_scan_ports(enabled: bool, attached: bool, tick: u64) -> bool {
    enabled && attached && tick.is_multiple_of(4)
}

/// Runs sweeps until `stop`. The shared config is read fresh each sweep, so
/// `governor.configure` applies on the next tick.
pub fn spawn_governor(
    registry: Arc<Mutex<Registry>>,
    events: EventBus,
    attach: AttachHub,
    pr_monitor_wake: crate::pr_monitor::PrMonitorWake,
    config: Arc<Mutex<GovernorConfig>>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("diri-governor".into())
        .spawn(move || {
            let mut tick: u64 = 0;
            let mut liveness = Liveness::default();
            while !stop.load(Ordering::SeqCst) {
                // Sleep first: a fresh daemon's startup work should settle
                // before the first sweep.
                let interval = config.lock().expect("config").scan_interval;
                let waited = std::time::Instant::now();
                while waited.elapsed() < interval {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                tick += 1;
                sweep(
                    &registry,
                    &events,
                    &attach,
                    &pr_monitor_wake,
                    &config,
                    &mut liveness,
                    tick,
                );
            }
        })
        .expect("spawn governor")
}

fn sweep(
    registry: &Arc<Mutex<Registry>>,
    events: &EventBus,
    attach: &AttachHub,
    pr_monitor_wake: &crate::pr_monitor::PrMonitorWake,
    config: &Arc<Mutex<GovernorConfig>>,
    liveness: &mut Liveness,
    tick: u64,
) {
    let config = config.lock().expect("config").clone();
    let records = {
        let Ok(guard) = registry.lock() else { return };
        guard.records()
    };
    liveness.retain(records.iter().map(|record| record.id.0.as_str()));

    let mut total_footprint: u64 = 0;
    let mut idle_candidates: Vec<Candidate> = Vec::new();

    for record in &records {
        let id = record.id.0.clone();
        if let Some(hibernation) = &record.hibernation {
            liveness.forget(&id);
            // Frozen trees barely change; sample occasionally so the badge
            // stays honest.
            if tick.is_multiple_of(config.hibernated_sample_every) {
                let footprint = footprint_of(&hibernation.tree_pids);
                total_footprint = total_footprint.wrapping_add(footprint);
                apply_sample(
                    registry,
                    events,
                    pr_monitor_wake,
                    &id,
                    Some(footprint),
                    None,
                    None,
                );
            } else {
                total_footprint = total_footprint.wrapping_add(record.memory_bytes.unwrap_or(0));
            }
            continue;
        }

        let (child_pid, artifacts, output_tail, live) = {
            let Ok(guard) = registry.lock() else { return };
            match guard.get(&id) {
                Some(session) => (
                    session.child_pid(),
                    session.artifacts(),
                    session.output_tail(),
                    true,
                ),
                None => (0, Vec::new(), 0, false),
            }
        };
        if !live || child_pid <= 1 {
            continue;
        }

        let tree = process_tree::enumerate(child_pid);
        let mut pids: Vec<i32> = tree.iter().map(|sample| sample.pid).collect();
        pids.sort_unstable();
        let footprint = footprint_of(&pids);
        total_footprint = total_footprint.wrapping_add(footprint);

        let attached = attach.has_sinks(&id);
        let ports = should_scan_ports(config.port_scan_enabled, attached, tick)
            .then(|| listening_ports(&pids, Duration::from_secs(3)))
            .flatten();

        apply_sample(
            registry,
            events,
            pr_monitor_wake,
            &id,
            Some(footprint),
            ports,
            (!artifacts.is_empty()).then_some(artifacts),
        );

        // What the tree actually did since the last sweep, independent of
        // what the status machine believes about it. Observed for every
        // live session so the quiet clock is running by the time the record
        // reads idle.
        let quiet_since = liveness.observe(
            &id,
            Observation {
                at_ms: now_millis(),
                cpu_nanos: cpu_time_of(&pids),
                output_tail,
                pids: pids.clone(),
            },
            &config,
        );

        // Eligibility for ANY auto-hibernation: idle by status, unattended,
        // not serving, and quiet by observation. A working / needs-input
        // session, one a client is viewing, or one still burning CPU or
        // writing output is never frozen out from under the user.
        let Some(record_idle_since) = idle_since(record, attached) else {
            continue;
        };
        let Some(quiet_since) = quiet_since else {
            continue;
        };
        let candidate = Candidate {
            id,
            idle_since_ms: record_idle_since.max(quiet_since),
            footprint,
            pids,
        };
        // The hard per-session limit still waits out the budget's minimum
        // quiet stretch: one sweep of quiet is a pause, not idleness.
        if footprint > config.hard_memory_bytes
            && (now_millis() - candidate.idle_since_ms) / 1000.0 > config.budget_min_idle_seconds
        {
            hibernate(
                registry,
                events,
                pr_monitor_wake,
                &config,
                &candidate,
                diri_proto::HibernationReason::MemoryPressure,
            );
            continue;
        }
        idle_candidates.push(candidate);
    }

    let now_ms = now_millis();

    // Sustained-idle freeze. Whatever it freezes leaves the candidate list
    // so the budget pass below does not freeze it a second time.
    if config.idle_threshold_seconds > 0.0 {
        let mut still_awake = Vec::with_capacity(idle_candidates.len());
        for candidate in idle_candidates {
            if (now_ms - candidate.idle_since_ms) / 1000.0 > config.idle_threshold_seconds
                && hibernate(
                    registry,
                    events,
                    pr_monitor_wake,
                    &config,
                    &candidate,
                    diri_proto::HibernationReason::Idle,
                )
            {
                continue;
            }
            still_awake.push(candidate);
        }
        idle_candidates = still_awake;
    }

    // Global budget: over → freeze idle sessions oldest-first until under.
    let budget = (physical_memory() as f64 * config.global_budget_fraction) as u64;
    if total_footprint > budget {
        let mut excess = total_footprint - budget;
        let mut by_oldest = idle_candidates;
        by_oldest.sort_by(|a, b| {
            a.idle_since_ms
                .partial_cmp(&b.idle_since_ms)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for candidate in by_oldest {
            if excess == 0 {
                break;
            }
            if (now_ms - candidate.idle_since_ms) / 1000.0 > config.budget_min_idle_seconds
                && hibernate(
                    registry,
                    events,
                    pr_monitor_wake,
                    &config,
                    &candidate,
                    diri_proto::HibernationReason::MemoryPressure,
                )
            {
                excess = excess.saturating_sub(candidate.footprint);
            }
        }
    }
}

fn apply_sample(
    registry: &Arc<Mutex<Registry>>,
    events: &EventBus,
    pr_monitor_wake: &crate::pr_monitor::PrMonitorWake,
    id: &str,
    memory: Option<u64>,
    ports: Option<Vec<PortInfo>>,
    artifacts: Option<Vec<diri_proto::SessionArtifact>>,
) {
    let event = {
        let Ok(mut guard) = registry.lock() else {
            return;
        };
        guard.apply_resource_sample(id, memory, ports, artifacts)
    };
    if let Some(event) = event {
        if event.artifacts.is_some() {
            pr_monitor_wake.wake_session(id.to_owned());
        }
        events.publish_encoded(diri_proto::EventName::SESSION_RESOURCES, &event, Some(id));
    }
}

/// Freezes `candidate`, unless a last look at its tree finds a listening
/// port. A server nobody is viewing is still serving, and a quiet one shows
/// the sweep neither CPU nor output — only lsof can tell. Ports found here
/// go on the record, which keeps the session off the candidate list until a
/// client's own scan refreshes them. Returns whether the tree was frozen.
fn hibernate(
    registry: &Arc<Mutex<Registry>>,
    events: &EventBus,
    pr_monitor_wake: &crate::pr_monitor::PrMonitorWake,
    config: &GovernorConfig,
    candidate: &Candidate,
    reason: diri_proto::HibernationReason,
) -> bool {
    if config.port_scan_enabled
        && let Some(ports) = listening_ports(&candidate.pids, Duration::from_secs(3))
        && !ports.is_empty()
    {
        apply_sample(
            registry,
            events,
            pr_monitor_wake,
            &candidate.id,
            None,
            Some(ports),
            None,
        );
        return false;
    }
    let id = candidate.id.as_str();
    let record = {
        let Ok(mut guard) = registry.lock() else {
            return false;
        };
        if guard.hibernate(id, reason).is_err() {
            return false;
        }
        let _ = guard.persist();
        guard.records().into_iter().find(|record| record.id.0 == id)
    };
    if let Some(record) = record {
        events.publish_encoded(diri_proto::EventName::SESSION_UPDATED, &record, Some(id));
    }
    true
}

/// Non-nil (ms since epoch of the idle stretch's start) when the record
/// alone says the session may be frozen: idle by status, not pinned, not
/// attached, and not known to be serving a port. The governor's own quiet
/// observation is layered on top of this by the sweep.
pub fn idle_since(record: &diri_proto::SessionRecord, attached: bool) -> Option<f64> {
    if record.hibernation.is_some() || record.pinned || attached {
        return None;
    }
    if record
        .listening_ports
        .as_deref()
        .is_some_and(|ports| !ports.is_empty())
    {
        return None;
    }
    if !matches!(record.status, SessionStatus::Idle) {
        return None;
    }
    let recency = [
        record.last_turn_completed_at.as_ref(),
        Some(&record.updated_at),
        record.last_seen_at.as_ref(),
    ]
    .into_iter()
    .flatten()
    .map(|date| date.0)
    .fold(f64::NAN, f64::max);
    Some(if recency.is_nan() {
        record.created_at.0
    } else {
        recency
    })
}

fn now_millis() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as f64
}

// MARK: Footprint

/// Sum of the trees' physical footprints (`phys_footprint` on macOS, VmRSS on
/// Linux) — the same number Activity Monitor's Memory column shows.
pub fn footprint_of(pids: &[i32]) -> u64 {
    pids.iter().map(|&pid| footprint_of_pid(pid)).sum()
}

/// Summed user+system CPU time of the trees, in nanoseconds. Dead pids
/// contribute 0, so a total can only shrink when a member exits — which the
/// quiet check reads as activity.
pub fn cpu_time_of(pids: &[i32]) -> u64 {
    pids.iter().map(|&pid| cpu_time_of_pid(pid)).sum()
}

#[cfg(target_os = "macos")]
fn footprint_of_pid(pid: i32) -> u64 {
    rusage_v2(pid).map_or(0, |info| info.ri_phys_footprint)
}

#[cfg(target_os = "macos")]
fn cpu_time_of_pid(pid: i32) -> u64 {
    // ri_user_time / ri_system_time are in Mach absolute time units, which
    // are nanoseconds on Intel and 125/3 ns per unit on Apple silicon.
    // Declared here rather than taken from `libc`, whose binding is
    // deprecated in favour of a crate this workspace does not otherwise need.
    #[repr(C)]
    #[derive(Default)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
    }
    let Some(info) = rusage_v2(pid) else { return 0 };
    let mut timebase = MachTimebaseInfo::default();
    // SAFETY: plain out-parameter call with a properly sized struct.
    unsafe { mach_timebase_info(&mut timebase) };
    let ticks = info.ri_user_time.saturating_add(info.ri_system_time);
    if timebase.denom == 0 || timebase.numer == 0 {
        return ticks;
    }
    (u128::from(ticks) * u128::from(timebase.numer) / u128::from(timebase.denom)) as u64
}

// rusage_info_v2, laid out exactly as <sys/resource.h>.
#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Default)]
struct RusageInfoV2 {
    ri_uuid: [u8; 16],
    ri_user_time: u64,
    ri_system_time: u64,
    ri_pkg_idle_wkups: u64,
    ri_interrupt_wkups: u64,
    ri_pageins: u64,
    ri_wired_size: u64,
    ri_resident_size: u64,
    ri_phys_footprint: u64,
    ri_proc_start_abstime: u64,
    ri_proc_exit_abstime: u64,
    ri_child_user_time: u64,
    ri_child_system_time: u64,
    ri_child_pkg_idle_wkups: u64,
    ri_child_interrupt_wkups: u64,
    ri_child_pageins: u64,
    ri_child_elapsed_abstime: u64,
    ri_diskio_bytesread: u64,
    ri_diskio_byteswritten: u64,
}

#[cfg(target_os = "macos")]
fn rusage_v2(pid: i32) -> Option<RusageInfoV2> {
    const RUSAGE_INFO_V2: libc::c_int = 2;
    // The header spells the parameter `rusage_info_t *buffer`, but
    // `rusage_info_t` is `void *` and every caller passes the STRUCT address
    // cast to it — the kernel writes the struct there, not through a
    // pointer-to-pointer.
    unsafe extern "C" {
        fn proc_pid_rusage(
            pid: libc::c_int,
            flavor: libc::c_int,
            buffer: *mut libc::c_void,
        ) -> libc::c_int;
    }
    let mut info = RusageInfoV2::default();
    // SAFETY: the buffer is a properly sized, writable rusage_info_v2.
    let rc = unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V2, std::ptr::from_mut(&mut info).cast()) };
    (rc == 0).then_some(info)
}

#[cfg(target_os = "linux")]
fn cpu_time_of_pid(pid: i32) -> u64 {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return 0;
    };
    // comm may contain spaces and parentheses; fields resume after the LAST
    // closing parenthesis. utime and stime are fields 14 and 15 overall,
    // i.e. the 12th and 13th after comm.
    let Some(close) = stat.rfind(')') else {
        return 0;
    };
    let mut fields = stat[close + 2..].split_whitespace().skip(11);
    let utime: u64 = fields.next().and_then(|f| f.parse().ok()).unwrap_or(0);
    let stime: u64 = fields.next().and_then(|f| f.parse().ok()).unwrap_or(0);
    // SAFETY: plain sysconf.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let hz = if hz > 0 { hz as u64 } else { 100 };
    utime
        .saturating_add(stime)
        .saturating_mul(1_000_000_000 / hz)
}

#[cfg(target_os = "linux")]
fn footprint_of_pid(pid: i32) -> u64 {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return 0;
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

fn physical_memory() -> u64 {
    #[cfg(target_os = "macos")]
    {
        let mut size: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: sysctlbyname with a properly sized out buffer.
        let rc = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                std::ptr::from_mut(&mut size).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 { size } else { 16 << 30 }
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|meminfo| {
                meminfo
                    .lines()
                    .find_map(|line| line.strip_prefix("MemTotal:"))
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|kb| kb.parse::<u64>().ok())
                    .map(|kb| kb * 1024)
            })
            .unwrap_or(16 << 30)
    }
}

// MARK: Ports

/// `lsof -a -iTCP -sTCP:LISTEN -p <pids> -Fpcn` over the tree — simple and
/// off the hot path, with a watchdog so a wedged lsof can't stall the sweep.
/// `-F` machine format: `p<pid>` `c<command>` `n<host:port>`.
pub fn listening_ports(pids: &[i32], timeout: Duration) -> Option<Vec<PortInfo>> {
    if pids.is_empty() {
        return Some(Vec::new());
    }
    let joined = pids
        .iter()
        .map(|pid| pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut child = Command::new("/usr/sbin/lsof")
        .args([
            "-a",
            "-iTCP",
            "-sTCP:LISTEN",
            "-p",
            &joined,
            "-Fpcn",
            "-n",
            "-P",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .or_else(|_| {
            Command::new("lsof")
                .args([
                    "-a",
                    "-iTCP",
                    "-sTCP:LISTEN",
                    "-p",
                    &joined,
                    "-Fpcn",
                    "-n",
                    "-P",
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
        })
        .ok()?;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
    let mut output = String::new();
    use std::io::Read;
    child.stdout.take()?.read_to_string(&mut output).ok()?;
    Some(parse_lsof(&output))
}

/// Parses `-Fpcn` output into unique (port, process) pairs, ordered by port.
pub fn parse_lsof(output: &str) -> Vec<PortInfo> {
    let mut command = String::new();
    let mut seen = std::collections::BTreeMap::new();
    for line in output.lines() {
        if let Some(name) = line.strip_prefix('c') {
            command = name.to_string();
        } else if let Some(endpoint) = line.strip_prefix('n') {
            // n*:3000, n127.0.0.1:5173, n[::1]:8080 — the port is after the
            // LAST colon.
            if let Some(port) = endpoint
                .rsplit(':')
                .next()
                .and_then(|raw| raw.parse::<i64>().ok())
            {
                seen.entry(port).or_insert_with(|| command.clone());
            }
        }
    }
    seen.into_iter()
        .map(|(port, process_name)| PortInfo { port, process_name })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsof_machine_format_parses_to_unique_ports() {
        let output = "p123\ncnode\nn*:3000\nn127.0.0.1:3000\np456\ncpython\nn[::1]:8080\n";
        let ports = parse_lsof(output);
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].port, 3000);
        assert_eq!(ports[0].process_name, "node");
        assert_eq!(ports[1].port, 8080);
        assert_eq!(ports[1].process_name, "python");
    }

    #[test]
    fn footprint_of_a_live_process_is_nonzero() {
        let own = footprint_of(&[std::process::id() as i32]);
        assert!(own > 1 << 20, "our own footprint is at least a MiB: {own}");
        assert_eq!(footprint_of(&[i32::MAX - 7]), 0, "a dead pid contributes 0");
    }

    #[test]
    fn port_scans_are_gated_to_attached_sessions_every_fourth_tick() {
        assert!(should_scan_ports(true, true, 8));
        assert!(!should_scan_ports(true, true, 7));
        assert!(!should_scan_ports(true, false, 8));
        assert!(!should_scan_ports(false, true, 8));
    }

    #[test]
    fn physical_memory_is_plausible() {
        let memory = physical_memory();
        assert!(memory >= 4 << 30, "at least 4 GiB: {memory}");
    }

    // MARK: Liveness

    fn observation(at_ms: f64, cpu_nanos: u64, output_tail: u64) -> Observation {
        Observation {
            at_ms,
            cpu_nanos,
            output_tail,
            pids: vec![100, 101],
        }
    }

    /// Thirty seconds apart, like real sweeps.
    const SWEEP_MS: f64 = 30_000.0;

    #[test]
    fn first_sighting_is_not_evidence_of_quiet() {
        let config = GovernorConfig::default();
        let mut liveness = Liveness::default();
        assert_eq!(liveness.observe("s", observation(0.0, 0, 0), &config), None);
        // The stretch starts at the first sighting once a quiet sweep confirms it.
        assert_eq!(
            liveness.observe("s", observation(SWEEP_MS, 0, 0), &config),
            Some(0.0)
        );
    }

    #[test]
    fn quiet_sweeps_keep_the_original_quiet_start() {
        let config = GovernorConfig::default();
        let mut liveness = Liveness::default();
        liveness.observe("s", observation(0.0, 1_000, 500), &config);
        for sweep in 1..=5 {
            // Idle-agent noise: ~1.2% CPU and a few bytes of redraw per sweep.
            let at = SWEEP_MS * sweep as f64;
            let cpu = 1_000 + (at * 1_000_000.0 * 0.012) as u64;
            let tail = 500 + sweep * 40;
            assert_eq!(
                liveness.observe("s", observation(at, cpu, tail), &config),
                Some(0.0),
                "sweep {sweep}"
            );
        }
    }

    #[test]
    fn cpu_burn_resets_the_quiet_stretch() {
        let config = GovernorConfig::default();
        let mut liveness = Liveness::default();
        liveness.observe("s", observation(0.0, 0, 0), &config);
        liveness.observe("s", observation(SWEEP_MS, 0, 0), &config);
        // A build: 100% of one core for the whole interval.
        let burned = (SWEEP_MS * 1_000_000.0) as u64;
        assert_eq!(
            liveness.observe("s", observation(2.0 * SWEEP_MS, burned, 0), &config),
            None
        );
        // Quiet again: the stretch restarts at the busy sweep, not at zero.
        assert_eq!(
            liveness.observe("s", observation(3.0 * SWEEP_MS, burned, 0), &config),
            Some(2.0 * SWEEP_MS)
        );
    }

    #[test]
    fn output_growth_resets_the_quiet_stretch() {
        let config = GovernorConfig::default();
        let mut liveness = Liveness::default();
        liveness.observe("s", observation(0.0, 0, 0), &config);
        liveness.observe("s", observation(SWEEP_MS, 0, 0), &config);
        // A streaming turn or a chatty dev server: kilobytes per sweep.
        assert_eq!(
            liveness.observe("s", observation(2.0 * SWEEP_MS, 0, 64 << 10), &config),
            None
        );
    }

    #[test]
    fn tree_membership_change_resets_the_quiet_stretch() {
        let config = GovernorConfig::default();
        let mut liveness = Liveness::default();
        liveness.observe("s", observation(0.0, 0, 0), &config);
        let mut grew = observation(SWEEP_MS, 0, 0);
        grew.pids.push(102);
        assert_eq!(liveness.observe("s", grew, &config), None);
    }

    #[test]
    fn cpu_total_going_backwards_reads_as_busy() {
        // A member exited between sweeps and took its CPU total with it.
        let config = GovernorConfig::default();
        assert!(is_busy(
            &observation(0.0, 5_000_000, 0),
            &observation(SWEEP_MS, 1_000_000, 0),
            &config
        ));
        assert!(
            is_busy(
                &observation(SWEEP_MS, 0, 0),
                &observation(SWEEP_MS, 0, 0),
                &config
            ),
            "a clock that did not advance proves nothing"
        );
    }

    #[test]
    fn forgetting_a_session_restarts_its_clock_on_wake() {
        let config = GovernorConfig::default();
        let mut liveness = Liveness::default();
        liveness.observe("s", observation(0.0, 0, 0), &config);
        liveness.observe("s", observation(SWEEP_MS, 0, 0), &config);
        liveness.forget("s");
        assert_eq!(
            liveness.observe("s", observation(2.0 * SWEEP_MS, 0, 0), &config),
            None
        );
        assert_eq!(
            liveness.observe("s", observation(3.0 * SWEEP_MS, 0, 0), &config),
            Some(2.0 * SWEEP_MS)
        );
    }

    #[test]
    fn retain_drops_sessions_that_left_the_registry() {
        let config = GovernorConfig::default();
        let mut liveness = Liveness::default();
        liveness.observe("gone", observation(0.0, 0, 0), &config);
        liveness.observe("kept", observation(0.0, 0, 0), &config);
        liveness.retain(["kept"]);
        assert!(liveness.seen.contains_key("kept"));
        assert!(!liveness.seen.contains_key("gone"));
    }

    #[test]
    fn cpu_time_of_own_process_is_in_nanoseconds_and_grows() {
        let own = std::process::id() as i32;
        let before = cpu_time_of(&[own]);
        // Burn roughly 150 ms of CPU.
        let started = std::time::Instant::now();
        let mut sink: u64 = 0;
        while started.elapsed() < Duration::from_millis(150) {
            for i in 0..10_000u64 {
                sink = sink.wrapping_mul(31).wrapping_add(i);
            }
        }
        std::hint::black_box(sink);
        let delta = cpu_time_of(&[own]).saturating_sub(before);
        // The unit conversion is the thing under test: mach absolute units
        // left unconverted would read ~24x too small on Apple silicon.
        assert!(
            (50_000_000..=5_000_000_000).contains(&delta),
            "150 ms of burn should read as 50 ms–5 s of CPU, got {delta} ns"
        );
        assert_eq!(cpu_time_of(&[i32::MAX - 7]), 0, "a dead pid contributes 0");
    }

    // MARK: Record eligibility

    fn record(status: SessionStatus) -> diri_proto::SessionRecord {
        use diri_proto::{AgentKind, DateMillis, ProjectId, Resumability, SessionId, TitleSource};
        diri_proto::SessionRecord {
            id: SessionId::new("s_one"),
            kind: AgentKind::CLAUDE_CODE,
            cwd: "/tmp/project".into(),
            project_id: ProjectId::new("p_project"),
            worktree_path: None,
            git_branch: None,
            title: "Session".into(),
            title_source: TitleSource::Placeholder,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::Live,
            capabilities: None,
            parent: None,
            created_at: DateMillis(1.0),
            updated_at: DateMillis(1.0),
            last_turn_completed_at: None,
            last_seen_at: None,
            pinned: false,
            archived_at: None,
            host: None,
            remote_persistence: None,
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
        }
    }

    #[test]
    fn only_unattended_idle_records_are_eligible() {
        assert_eq!(idle_since(&record(SessionStatus::Idle), false), Some(1.0));
        assert_eq!(idle_since(&record(SessionStatus::Idle), true), None);
        assert_eq!(idle_since(&record(SessionStatus::Working), false), None);
        let mut pinned = record(SessionStatus::Idle);
        pinned.pinned = true;
        assert_eq!(idle_since(&pinned, false), None);
    }

    #[test]
    fn a_record_with_listening_ports_is_never_eligible() {
        let mut serving = record(SessionStatus::Idle);
        serving.listening_ports = Some(vec![PortInfo {
            port: 3000,
            process_name: "node".into(),
        }]);
        assert_eq!(idle_since(&serving, false), None);
        serving.listening_ports = Some(Vec::new());
        assert_eq!(idle_since(&serving, false), Some(1.0));
    }
}
