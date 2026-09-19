//! Enumerating and signalling the held child's whole process tree.
//!
//! A PTY child forks: shells background jobs, agents spawn subagents and
//! language servers. Killing only the direct child strands the rest, so every
//! signal walks the tree — children by parentage, plus anything sharing a
//! process group with a tree member. Each (pid, start time) pair is an
//! identity: a pid observed with one start time is never signalled once its
//! start time changes, which is what makes signalling a recycled pid safe.
//!
//! Ported from `HolderProcessTree.swift`. The walk semantics are identical;
//! only the process-listing syscalls differ per platform (libproc on macOS,
//! `/proc` on Linux).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::protocol::HolderProcessSample;

/// A process's identity and tree-relevant relations at one observation.
#[derive(Clone, Copy, Debug)]
struct Observed {
    pid: i32,
    ppid: i32,
    pgid: i32,
    start_sec: i64,
    stopped: bool,
}

/// The start time of `pid`, or `None` if it is gone. The identity check.
fn start_time(pid: i32) -> Option<i64> {
    platform::observe(pid).map(|process| process.start_sec)
}

/// Walks the tree under `root`: children transitively, plus every member of
/// any process group a tree member belongs to. The holder itself is excluded.
pub fn enumerate(root: i32) -> Vec<HolderProcessSample> {
    if root <= 1 {
        return Vec::new();
    }
    enumerate_in(&ProcessTable::capture(), root)
}

/// One observation of every process on the machine.
///
/// Listing the table is the expensive half of a walk — a syscall per process —
/// and it does not depend on which tree is being walked. A caller walking many
/// roots at once (the Engine's resource sweep) captures it once and walks each
/// root against the same table. Signalling never does this: it wants the
/// table as it is at the moment it acts.
pub struct ProcessTable(Vec<Observed>);

impl ProcessTable {
    pub fn capture() -> Self {
        #[cfg(test)]
        CAPTURES.with(|captures| captures.set(captures.get() + 1));
        Self(platform::snapshot())
    }
}

#[cfg(test)]
thread_local! {
    /// Tables captured on this thread, so a test can count them without
    /// seeing another test's walks.
    static CAPTURES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// [`enumerate`] against a table the caller already captured. Relations come
/// from the table; each member's identity is still looked up afresh, so a
/// table that has aged never vouches for a recycled pid.
pub fn enumerate_in(table: &ProcessTable, root: i32) -> Vec<HolderProcessSample> {
    if root <= 1 {
        return Vec::new();
    }
    let all = &table.0;
    let holder_pid = std::process::id() as i32;

    let mut seen: HashSet<i32> = HashSet::new();
    let mut scanned_groups: HashSet<i32> = HashSet::new();
    let mut frontier = vec![root];

    while let Some(pid) = frontier.pop() {
        if pid <= 1 || pid == holder_pid || !seen.insert(pid) {
            continue;
        }
        frontier.extend(
            all.iter()
                .filter(|process| process.ppid == pid)
                .map(|process| process.pid),
        );
        if let Some(process) = all.iter().find(|process| process.pid == pid)
            && process.pgid > 1
            && scanned_groups.insert(process.pgid)
        {
            frontier.extend(
                all.iter()
                    .filter(|member| member.pgid == process.pgid)
                    .map(|member| member.pid),
            );
        }
    }

    seen.into_iter()
        .filter_map(|pid| {
            // Fresh per-pid lookup, as Swift did: the sample must carry the
            // identity as observed now, not a stale snapshot.
            start_time(pid).map(|start_sec| HolderProcessSample { pid, start_sec })
        })
        .collect()
}

/// Sends `signal` to the tree under `root`; returns the processes signalled.
///
/// `SIGSTOP` converges: stopping the root can race children that fork before
/// the stop lands, so the walk repeats until every member is observed stopped.
/// `SIGCONT` resumes newest-first so children are running before their
/// parents resume and observe them.
pub fn signal(root: i32, signal: i32) -> Vec<HolderProcessSample> {
    if signal == libc::SIGSTOP {
        // SAFETY: plain kill(2).
        unsafe { libc::kill(root, libc::SIGSTOP) };
        let mut tree = Vec::new();
        for _ in 0..6 {
            tree = enumerate(root);
            let mut all_stopped = true;
            for sample in &tree {
                if platform::observe(sample.pid).is_some_and(|process| !process.stopped) {
                    // SAFETY: plain kill(2).
                    unsafe { libc::kill(sample.pid, libc::SIGSTOP) };
                    all_stopped = false;
                }
            }
            if all_stopped {
                break;
            }
        }
        return tree
            .into_iter()
            .filter(|sample| start_time(sample.pid) == Some(sample.start_sec))
            .collect();
    }

    let tree = enumerate(root);
    let mut ordered = tree.clone();
    if signal == libc::SIGCONT {
        ordered.sort_by_key(|sample| std::cmp::Reverse(sample.start_sec));
    }
    signal_group(root, signal);
    for sample in &ordered {
        if start_time(sample.pid) == Some(sample.start_sec) {
            // SAFETY: identity just re-verified; plain kill(2).
            unsafe { libc::kill(sample.pid, signal) };
        }
    }
    tree
}

/// SIGTERM the tree (waking stopped members with SIGCONT so the TERM is
/// deliverable), give it half a second, then SIGKILL whatever survived.
pub fn kill_tree(root: i32) {
    let mut tree = enumerate(root);
    let _ = signal(root, libc::SIGTERM);
    let _ = signal(root, libc::SIGCONT);

    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if tree
            .iter()
            .all(|sample| start_time(sample.pid) != Some(sample.start_sec))
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    tree.extend(enumerate(root));
    signal_group(root, libc::SIGKILL);
    let unique: HashSet<HolderProcessSample> = tree.into_iter().collect();
    for sample in unique {
        if start_time(sample.pid) == Some(sample.start_sec) {
            // SAFETY: identity just re-verified; plain kill(2).
            unsafe {
                libc::kill(sample.pid, libc::SIGKILL);
                libc::kill(sample.pid, libc::SIGCONT);
            }
        }
    }
}

/// Signals the root's process group; falls back to the root alone when the
/// group is already gone.
fn signal_group(root: i32, signal: i32) {
    // SAFETY: plain kill(2) on a group the holder created via setsid.
    unsafe {
        if libc::kill(-root, signal) != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            libc::kill(root, signal);
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    //! libproc-backed process observation.
    //!
    //! `proc_bsdinfo` is declared here rather than taken from the `libc`
    //! crate so the exact field set this module depends on is visible; the
    //! layout matches `<sys/proc_info.h>` and is ABI-stable.

    use super::Observed;

    const PROC_ALL_PIDS: u32 = 1;
    const PROC_PIDTBSDINFO: libc::c_int = 3;
    const SSTOP: u32 = 4;
    const MAXCOMLEN: usize = 16;

    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)] // layout-complete for the syscall; only some fields are read
    struct ProcBsdInfo {
        pbi_flags: u32,
        pbi_status: u32,
        pbi_xstatus: u32,
        pbi_pid: u32,
        pbi_ppid: u32,
        pbi_uid: libc::uid_t,
        pbi_gid: libc::gid_t,
        pbi_ruid: libc::uid_t,
        pbi_rgid: libc::gid_t,
        pbi_svuid: libc::uid_t,
        pbi_svgid: libc::gid_t,
        rfu_1: u32,
        pbi_comm: [u8; MAXCOMLEN],
        pbi_name: [u8; 2 * MAXCOMLEN],
        pbi_nfiles: u32,
        pbi_pgid: u32,
        pbi_pjobc: u32,
        e_tdev: u32,
        e_tpgid: u32,
        pbi_nice: i32,
        pbi_start_tvsec: u64,
        pbi_start_tvusec: u64,
    }

    unsafe extern "C" {
        fn proc_listpids(
            kind: u32,
            typeinfo: u32,
            buffer: *mut libc::c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut libc::c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }

    pub(super) fn observe(pid: i32) -> Option<Observed> {
        let mut info: ProcBsdInfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<ProcBsdInfo>() as libc::c_int;
        // SAFETY: the buffer is a properly sized, writable ProcBsdInfo.
        let filled = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDTBSDINFO,
                0,
                std::ptr::from_mut(&mut info).cast(),
                size,
            )
        };
        if filled != size {
            return None;
        }
        Some(Observed {
            pid,
            ppid: info.pbi_ppid as i32,
            pgid: info.pbi_pgid as i32,
            start_sec: info.pbi_start_tvsec as i64,
            stopped: info.pbi_status == SSTOP,
        })
    }

    pub(super) fn snapshot() -> Vec<Observed> {
        // SAFETY: a nil buffer asks for the size in bytes.
        let bytes = unsafe { proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
        if bytes <= 0 {
            return Vec::new();
        }
        let mut pids = vec![0i32; bytes as usize / std::mem::size_of::<i32>() + 32];
        // SAFETY: the buffer is sized to its byte length below.
        let filled = unsafe {
            proc_listpids(
                PROC_ALL_PIDS,
                0,
                pids.as_mut_ptr().cast(),
                (pids.len() * std::mem::size_of::<i32>()) as libc::c_int,
            )
        };
        if filled <= 0 {
            return Vec::new();
        }
        pids.truncate(filled as usize / std::mem::size_of::<i32>());
        pids.into_iter()
            .filter(|&pid| pid > 1)
            .filter_map(observe)
            .collect()
    }
}

#[cfg(target_os = "linux")]
mod platform {
    //! `/proc`-backed process observation.

    use super::Observed;

    pub(super) fn observe(pid: i32) -> Option<Observed> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // comm may contain spaces and parentheses; fields resume after the
        // LAST closing parenthesis.
        let after_comm = &stat[stat.rfind(')')? + 2..];
        let mut fields = after_comm.split_whitespace();
        let state = fields.next()?; // field 3
        let ppid: i32 = fields.next()?.parse().ok()?; // field 4
        let pgid: i32 = fields.next()?.parse().ok()?; // field 5
        // starttime is field 22 overall; 17 more past pgrp.
        let start: i64 = fields.nth(16)?.parse().ok()?;
        Some(Observed {
            pid,
            ppid,
            pgid,
            start_sec: start, // clock ticks, but only compared for identity
            stopped: state == "T" || state == "t",
        })
    }

    pub(super) fn snapshot() -> Vec<Observed> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
            .filter(|&pid| pid > 1)
            .filter_map(observe)
            .collect()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Child, Command};

    fn spawn_sleeper() -> Child {
        use std::os::unix::process::CommandExt;
        // Its own process group, as a real holder child (setsid) would have:
        // otherwise the group walk reaches the test runner itself and
        // kill_tree kills the whole cargo test session.
        Command::new("/bin/sh")
            .args(["-c", "sleep 30 & sleep 30"])
            .process_group(0)
            .spawn()
            .expect("spawn")
    }

    #[test]
    fn enumerate_finds_a_child_and_its_grandchild() {
        let mut child = spawn_sleeper();
        // Give the shell a beat to fork its background sleep.
        std::thread::sleep(Duration::from_millis(200));
        let tree = enumerate(child.id() as i32);
        assert!(
            tree.iter().any(|sample| sample.pid == child.id() as i32),
            "the root itself is in the tree: {tree:?}"
        );
        assert!(
            tree.len() >= 2,
            "the grandchild sleep should be found: {tree:?}"
        );
        kill_tree(child.id() as i32);
        let _ = child.wait();
    }

    #[test]
    fn kill_tree_leaves_nothing_running() {
        let mut child = spawn_sleeper();
        std::thread::sleep(Duration::from_millis(200));
        let before = enumerate(child.id() as i32);
        assert!(!before.is_empty());

        kill_tree(child.id() as i32);
        let _ = child.wait(); // reap so the identity check sees it gone

        std::thread::sleep(Duration::from_millis(100));
        for sample in &before {
            assert_ne!(
                start_time(sample.pid),
                Some(sample.start_sec),
                "pid {} survived kill_tree",
                sample.pid
            );
        }
    }

    #[test]
    fn a_shared_table_is_captured_once_however_many_trees_are_walked() {
        let mut children: Vec<_> = (0..4).map(|_| spawn_sleeper()).collect();
        std::thread::sleep(Duration::from_millis(200));
        let roots: Vec<i32> = children.iter().map(|child| child.id() as i32).collect();

        let started = Instant::now();
        let before = CAPTURES.with(std::cell::Cell::get);
        let separate: Vec<HashSet<i32>> = roots
            .iter()
            .map(|&root| enumerate(root).iter().map(|sample| sample.pid).collect())
            .collect();
        let per_root = CAPTURES.with(std::cell::Cell::get) - before;
        let per_root_took = started.elapsed();

        let started = Instant::now();
        let before = CAPTURES.with(std::cell::Cell::get);
        let table = ProcessTable::capture();
        let shared: Vec<HashSet<i32>> = roots
            .iter()
            .map(|&root| {
                enumerate_in(&table, root)
                    .iter()
                    .map(|sample| sample.pid)
                    .collect()
            })
            .collect();
        let once = CAPTURES.with(std::cell::Cell::get) - before;
        eprintln!(
            "{} trees: {per_root_took:?} with a table each, {:?} sharing one",
            roots.len(),
            started.elapsed()
        );

        assert_eq!(per_root, roots.len(), "one table per root before");
        assert_eq!(once, 1, "one table for every root after");
        assert_eq!(separate, shared, "the walk finds the same trees");
        for (child, root) in children.iter_mut().zip(roots) {
            kill_tree(root);
            let _ = child.wait();
        }
    }

    #[test]
    fn a_dead_pid_has_no_start_time() {
        let mut child = Command::new("true").spawn().expect("spawn");
        let pid = child.id() as i32;
        let _ = child.wait();
        assert_eq!(start_time(pid), None, "reaped children are gone");
    }
}
