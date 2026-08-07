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

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diri_proto::{PortInfo, SessionStatus};

use crate::attach::AttachHub;
use crate::events::EventBus;
use crate::holder::process_tree;
use crate::registry::Registry;

/// Tunables, defaulting to the Swift daemon's values. `governor.configure`
/// overrides the two the app exposes.
#[derive(Clone, Debug)]
pub struct GovernorConfig {
    pub idle_threshold_seconds: f64,
    pub hard_memory_bytes: u64,
    pub global_budget_fraction: f64,
    pub budget_min_idle_seconds: f64,
    pub hibernated_sample_every: u64,
    pub port_scan_enabled: bool,
    pub scan_interval: Duration,
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self {
            idle_threshold_seconds: 900.0,
            hard_memory_bytes: 6 << 30,
            global_budget_fraction: 0.75,
            budget_min_idle_seconds: 300.0,
            hibernated_sample_every: 5,
            port_scan_enabled: true,
            scan_interval: Duration::from_secs(30),
        }
    }
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
                sweep(&registry, &events, &attach, &pr_monitor_wake, &config, tick);
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
    tick: u64,
) {
    let config = config.lock().expect("config").clone();
    let records = {
        let Ok(guard) = registry.lock() else { return };
        guard.records()
    };

    let mut total_footprint: u64 = 0;
    let mut idle_candidates: Vec<(String, f64, u64)> = Vec::new(); // (id, idle_since_ms, footprint)

    for record in &records {
        let id = record.id.0.clone();
        if let Some(hibernation) = &record.hibernation {
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

        let (child_pid, artifacts, live) = {
            let Ok(guard) = registry.lock() else { return };
            match guard.get(&id) {
                Some(session) => (session.child_pid(), session.artifacts(), true),
                None => (0, Vec::new(), false),
            }
        };
        if !live || child_pid <= 1 {
            continue;
        }

        let tree = process_tree::enumerate(child_pid);
        let pids: Vec<i32> = tree.iter().map(|sample| sample.pid).collect();
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

        // Eligibility for ANY auto-hibernation: idle and unattended. A
        // working / needs-input session, or one a client is viewing, is never
        // frozen out from under the user.
        if let Some(idle_since) = idle_since(record, attached) {
            if footprint > config.hard_memory_bytes {
                let _ = hibernate(
                    registry,
                    events,
                    &id,
                    diri_proto::HibernationReason::MemoryPressure,
                );
                continue;
            }
            idle_candidates.push((id, idle_since, footprint));
        }
    }

    let now_ms = now_millis();

    // Sustained-idle freeze.
    if config.idle_threshold_seconds > 0.0 {
        for (id, idle_since, _) in &idle_candidates {
            if (now_ms - idle_since) / 1000.0 > config.idle_threshold_seconds {
                let _ = hibernate(registry, events, id, diri_proto::HibernationReason::Idle);
            }
        }
    }

    // Global budget: over → freeze idle sessions oldest-first until under.
    let budget = (physical_memory() as f64 * config.global_budget_fraction) as u64;
    if total_footprint > budget {
        let mut excess = total_footprint - budget;
        let mut by_oldest = idle_candidates;
        by_oldest.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        for (id, idle_since, footprint) in by_oldest {
            if excess == 0 {
                break;
            }
            if (now_ms - idle_since) / 1000.0 > config.budget_min_idle_seconds {
                let _ = hibernate(
                    registry,
                    events,
                    &id,
                    diri_proto::HibernationReason::MemoryPressure,
                );
                excess = excess.saturating_sub(footprint);
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

fn hibernate(
    registry: &Arc<Mutex<Registry>>,
    events: &EventBus,
    id: &str,
    reason: diri_proto::HibernationReason,
) -> std::io::Result<()> {
    let record = {
        let Ok(mut guard) = registry.lock() else {
            return Ok(());
        };
        guard.hibernate(id, reason)?;
        let _ = guard.persist();
        guard.records().into_iter().find(|record| record.id.0 == id)
    };
    if let Some(record) = record {
        events.publish_encoded(diri_proto::EventName::SESSION_UPDATED, &record, Some(id));
    }
    Ok(())
}

/// Non-nil (ms since epoch of the idle stretch's start) when the session is
/// eligible for idle hibernation.
fn idle_since(record: &diri_proto::SessionRecord, attached: bool) -> Option<f64> {
    if record.hibernation.is_some() || record.pinned || attached {
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

#[cfg(target_os = "macos")]
fn footprint_of_pid(pid: i32) -> u64 {
    // rusage_info_v2, laid out exactly as <sys/resource.h>.
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
    if rc == 0 { info.ri_phys_footprint } else { 0 }
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
}
