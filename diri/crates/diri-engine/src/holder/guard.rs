//! The manager's liveness guard: kills hosted sessions' process groups if the
//! manager itself dies.
//!
//! A session's tree is tied to its holder only by the PTY. When the manager
//! process dies (a crash, `kill -9`, a reinstall), the kernel hangs the
//! terminals up and each session leader gets SIGHUP — but a hibernated tree is
//! stopped, and a stopped process that handles SIGHUP (Codex's node wrapper,
//! Claude Code) cannot act on it. Nothing will ever continue it, so it sits
//! stopped, parented to launchd, holding a revoked terminal, until reboot.
//!
//! The guard is the remote Helper's per-session process guard, shared by the
//! manager's sessions: one tiny process (about 1.3 MB), in its own session,
//! that reads a pipe only the manager holds. The manager writes `+<pgid>` when
//! a holder spawns a session leader and `-<pgid>` once that leader has exited
//! and its stragglers are gone — always before the leader is reaped, so a
//! registered id is pinned by a live or zombie leader for as long as it is
//! registered. A hibernation also stops processes that left the group
//! (Chrome DevTools MCP's watchdog, Codex's code-mode host), so the manager
//! writes `s<pid>:<start>` for each process it stops and `c<pid>:<start>` once
//! it has continued or killed it; those are signalled only after their start
//! time is re-verified, which is what makes a recycled pid safe. Kernel
//! closure of the pipe is reliable even under SIGKILL; on EOF the guard
//! SIGKILLs every group and frozen process still registered, and exits. It
//! owns no PTY, socket, or session state beyond those ids, and wakes only when
//! a session starts, ends, hibernates or wakes.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;

use super::process_tree;
use super::protocol::HolderProcessSample;

/// The argument that runs `diri-holder` as a guard.
pub const GROUP_GUARD_FLAG: &str = "--group-guard";

/// The guard loop. Returns once `input` reaches EOF (or fails), after killing
/// every group and frozen process still registered; returns the groups it
/// signalled.
pub fn run_group_guard(input: impl BufRead) -> Vec<i32> {
    let mut groups: HashSet<i32> = HashSet::new();
    let mut frozen: HashSet<HolderProcessSample> = HashSet::new();
    for line in input.lines() {
        let Ok(line) = line else { break };
        let group = |id: &str| id.parse::<i32>().ok().filter(|&id| id > 1);
        let sample = |text: &str| {
            let (pid, start) = text.split_once(':')?;
            Some(HolderProcessSample {
                pid: group(pid)?,
                start_sec: start.parse().ok()?,
            })
        };
        if let Some(id) = line.strip_prefix('+').and_then(group) {
            groups.insert(id);
        } else if let Some(id) = line.strip_prefix('-').and_then(group) {
            groups.remove(&id);
        } else if let Some(sample) = line.strip_prefix('s').and_then(sample) {
            frozen.insert(sample);
        } else if let Some(sample) = line.strip_prefix('c').and_then(sample) {
            frozen.remove(&sample);
        }
    }
    let mut killed: Vec<i32> = groups.into_iter().collect();
    killed.sort_unstable();
    for &group in &killed {
        // SAFETY: plain kill(2) on a group id the manager registered and never
        // released; see the module docs for why the id is still that group's.
        unsafe { libc::kill(-group, libc::SIGKILL) };
    }
    for sample in frozen {
        if process_tree::is_alive(&sample) {
            // SAFETY: identity just re-verified; plain kill(2).
            unsafe {
                libc::kill(sample.pid, libc::SIGKILL);
                libc::kill(sample.pid, libc::SIGCONT);
            }
        }
    }
    killed
}

/// The manager's handle on its guard.
pub struct GroupGuard {
    child: Mutex<Option<Child>>,
    input: Mutex<Option<ChildStdin>>,
}

impl GroupGuard {
    /// Spawns `executable --group-guard` in its own session. Call it before
    /// any PTY exists, so the guard inherits nothing a session needs closed.
    pub fn spawn(executable: &Path) -> std::io::Result<Self> {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(executable);
        command
            .arg(GROUP_GUARD_FLAG)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: setsid is async-signal-safe. The guard must not share the
        // manager's process group or session, or whatever kills the manager's
        // group would take the guard with it.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let input = child.stdin.take();
        Ok(Self {
            child: Mutex::new(Some(child)),
            input: Mutex::new(input),
        })
    }

    /// Records a new session leader's group. Best effort: a guard that has
    /// gone away only loses the crash cleanup, never the session.
    pub fn register(&self, group: i32) {
        self.send('+', group);
    }

    /// Forgets a group. The caller must still hold the leader unreaped.
    pub fn release(&self, group: i32) {
        self.send('-', group);
    }

    /// Records processes a hibernation stopped.
    pub fn freeze(&self, stopped: &[HolderProcessSample]) {
        self.send_all('s', stopped);
    }

    /// Forgets processes that were continued or killed.
    pub fn thaw(&self, continued: &[HolderProcessSample]) {
        self.send_all('c', continued);
    }

    fn send_all(&self, sign: char, samples: &[HolderProcessSample]) {
        if samples.is_empty() {
            return;
        }
        let mut lines = String::new();
        for sample in samples {
            lines.push_str(&format!("{sign}{}:{}\n", sample.pid, sample.start_sec));
        }
        self.write(lines.as_bytes());
    }

    fn send(&self, sign: char, group: i32) {
        self.write(format!("{sign}{group}\n").as_bytes());
    }

    fn write(&self, bytes: &[u8]) {
        let mut input = self.input.lock().expect("guard input");
        if let Some(pipe) = input.as_mut()
            && pipe.write_all(bytes).is_err()
        {
            *input = None;
        }
    }

    /// Closes the pipe and reaps the guard. With every session released this
    /// kills nothing.
    pub fn finish(&self) {
        drop(self.input.lock().expect("guard input").take());
        if let Some(mut child) = self.child.lock().expect("guard child").take() {
            let _ = child.wait();
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};

    fn group() -> Child {
        Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn")
    }

    fn exited(child: &mut Child) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if child.try_wait().expect("try_wait").is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn sample(child: &Child) -> HolderProcessSample {
        let pid = child.id() as i32;
        let tree = process_tree::enumerate(pid);
        *tree
            .iter()
            .find(|sample| sample.pid == pid)
            .expect("observed")
    }

    #[test]
    fn eof_kills_frozen_identities_that_left_the_group_but_not_recycled_ones() {
        let mut frozen = group();
        let mut thawed = group();
        let (f, t) = (sample(&frozen), sample(&thawed));
        let stale = HolderProcessSample {
            pid: t.pid,
            start_sec: t.start_sec - 1,
        };
        let input = format!(
            "s{}:{}\ns{}:{}\nc{}:{}\ns{}:{}\n",
            f.pid, f.start_sec, t.pid, t.start_sec, t.pid, t.start_sec, stale.pid, stale.start_sec
        );

        assert!(run_group_guard(input.as_bytes()).is_empty(), "no groups");
        assert!(exited(&mut frozen), "the frozen process was killed");
        assert!(
            thawed.try_wait().expect("try_wait").is_none(),
            "neither a thawed process nor a stale identity for its pid is signalled"
        );
        thawed.kill().expect("cleanup");
        thawed.wait().expect("reap");
    }

    #[test]
    fn eof_kills_registered_groups_and_spares_released_ones() {
        let mut registered = group();
        let mut released = group();
        let (r, s) = (registered.id() as i32, released.id() as i32);
        let input = format!("+{r}\n+{s}\n-{s}\n+1\n+0\ngarbage\n");

        let killed = run_group_guard(input.as_bytes());

        assert_eq!(killed, vec![r], "only the still-registered group");
        assert!(exited(&mut registered), "the registered group was killed");
        assert!(
            released.try_wait().expect("try_wait").is_none(),
            "a released group is left alone"
        );
        released.kill().expect("cleanup");
        released.wait().expect("reap");
    }
}
