//! Processes started with `diri.exec`: an argv (never a shell string), a
//! timeout, bounded output, and output read on threads of their own so a
//! chatty or stuck program never blocks the script thread.
//!
//! Each process gets one thread that waits for it and one reader per output
//! stream. Readers send chunks over a bounded channel, so a flood waits on
//! the pipe rather than growing memory, and wake the driver at most once
//! per batch (the `pending` flag), so a flood does not flood the driver's
//! channel either. On Unix the process leads its own group, so a timeout or
//! a reload kills everything it started.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

use super::Waker;

/// What a script keeps of one stream; past this it is cut and `truncated`.
pub(super) const MAX_OUTPUT: usize = 1024 * 1024;
/// A line longer than this reaches `on_output` in pieces.
const MAX_LINE: usize = 64 * 1024;
/// Processes one script may run at once.
pub(super) const MAX_RUNNING: usize = 16;
const READ_CHUNK: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

pub(super) enum Event {
    Output {
        id: u64,
        stream: Stream,
        data: Vec<u8>,
    },
    Exit {
        id: u64,
        code: Option<i32>,
        signal: Option<i32>,
        error: Option<String>,
    },
}

pub(super) struct Spec {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
}

/// How a process ended, with what it printed.
#[derive(Debug, Default)]
pub(super) struct Finished {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub error: Option<String>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
    pub timed_out: bool,
}

struct Running {
    #[cfg_attr(not(unix), allow(dead_code))]
    pid: u32,
    kill: Arc<AtomicBool>,
    deadline: Instant,
    result: Finished,
    partial: [Vec<u8>; 2],
    /// Set once the process has ended and its output is drained.
    done: bool,
}

impl Running {
    fn kill(&mut self) {
        self.kill.store(true, Ordering::SeqCst);
        #[cfg(unix)]
        // SAFETY: plain FFI call; the group is the one this module started
        // (its leader's pid), and killing a group that already exited is a
        // harmless ESRCH.
        unsafe {
            libc::killpg(self.pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

pub(super) struct Execs {
    next: u64,
    running: HashMap<u64, Running>,
    tx: SyncSender<Event>,
    rx: Receiver<Event>,
    waker: Option<Waker>,
    pending: Arc<AtomicBool>,
}

impl Execs {
    pub(super) fn new(waker: Option<Waker>) -> Self {
        let (tx, rx) = mpsc::sync_channel(256);
        Self {
            next: 0,
            running: HashMap::new(),
            tx,
            rx,
            waker,
            pending: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn can_wake(&self) -> bool {
        self.waker.is_some()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.running
            .values()
            .filter(|running| !running.done && !running.result.timed_out)
            .map(|running| running.deadline)
            .min()
    }

    /// Kills the processes past their deadline.
    pub(super) fn expire(&mut self, now: Instant) {
        for running in self.running.values_mut() {
            if !running.done && !running.result.timed_out && running.deadline <= now {
                running.result.timed_out = true;
                running.kill();
            }
        }
    }

    pub(super) fn spawn(&mut self, spec: Spec) -> Result<u64, String> {
        if self.running.len() >= MAX_RUNNING {
            return Err(format!(
                "a script can run at most {MAX_RUNNING} processes at once"
            ));
        }
        let program = spec.argv.first().ok_or("argv is empty")?;
        let mut command = std::process::Command::new(program);
        command
            .args(&spec.argv[1..])
            .stdin(if spec.stdin.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = diri_platform::child::spawn(&mut command)
            .map_err(|error| format!("{program}: {error}"))?;
        self.next += 1;
        let id = self.next;
        let pid = child.id();
        let kill = Arc::new(AtomicBool::new(false));
        let notify = Notify {
            tx: self.tx.clone(),
            waker: self.waker.clone(),
            pending: Arc::clone(&self.pending),
        };
        let stdin = child.stdin.take();
        let readers = [
            child
                .stdout
                .take()
                .map(|pipe| reader(id, Stream::Stdout, pipe, notify.clone())),
            child
                .stderr
                .take()
                .map(|pipe| reader(id, Stream::Stderr, pipe, notify.clone())),
        ];
        if let (Some(mut pipe), Some(input)) = (stdin, spec.stdin) {
            std::thread::spawn(move || {
                let _ = pipe.write_all(&input);
            });
        }
        let waited_kill = Arc::clone(&kill);
        let spawned = std::thread::Builder::new()
            .name("diri-script-exec".into())
            .spawn(move || {
                let status = wait(&mut child, &waited_kill);
                for reader in readers.into_iter().flatten() {
                    let _ = reader.join();
                }
                let (code, signal, error) = match status {
                    Ok(status) => (status.code(), signal_of(&status), None),
                    Err(error) => (None, None, Some(error.to_string())),
                };
                notify.send(Event::Exit {
                    id,
                    code,
                    signal,
                    error,
                });
            });
        if let Err(error) = spawned {
            return Err(format!("could not wait for {program}: {error}"));
        }
        self.running.insert(
            id,
            Running {
                pid,
                kill,
                deadline: Instant::now() + spec.timeout,
                result: Finished::default(),
                partial: [Vec::new(), Vec::new()],
                done: false,
            },
        );
        Ok(id)
    }

    /// Takes every event that has arrived. Clears the wake flag first, so an
    /// event sent meanwhile wakes the driver again.
    pub(super) fn drain(&mut self) -> Vec<Event> {
        self.pending.store(false, Ordering::SeqCst);
        self.rx.try_iter().collect()
    }

    /// Waits up to `timeout` for the next event, for a script that called
    /// `diri.exec` outside a coroutine.
    pub(super) fn recv(&self, timeout: Duration) -> Option<Event> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// Records one event. Answers the complete lines it carried, for
    /// `on_output`, and whether it ended the process.
    pub(super) fn record(&mut self, event: Event) -> (u64, Vec<(Stream, String)>, bool) {
        match event {
            Event::Output { id, stream, data } => {
                let Some(running) = self.running.get_mut(&id) else {
                    return (id, Vec::new(), false);
                };
                let kept = match stream {
                    Stream::Stdout => &mut running.result.stdout,
                    Stream::Stderr => &mut running.result.stderr,
                };
                let room = MAX_OUTPUT.saturating_sub(kept.len());
                if data.len() > room {
                    running.result.truncated = true;
                }
                kept.extend_from_slice(&data[..data.len().min(room)]);
                let partial = &mut running.partial[stream as usize];
                partial.extend_from_slice(&data);
                (id, split_lines(stream, partial, false), false)
            }
            Event::Exit {
                id,
                code,
                signal,
                error,
            } => {
                let Some(running) = self.running.get_mut(&id) else {
                    return (id, Vec::new(), false);
                };
                running.result.code = code;
                running.result.signal = signal;
                running.result.error = error;
                running.done = true;
                let mut lines = Vec::new();
                for stream in [Stream::Stdout, Stream::Stderr] {
                    lines.extend(split_lines(
                        stream,
                        &mut running.partial[stream as usize],
                        true,
                    ));
                }
                (id, lines, true)
            }
        }
    }

    /// The result of a process that ended, removing it.
    pub(super) fn take(&mut self, id: u64) -> Option<Finished> {
        if !self.running.get(&id)?.done {
            return None;
        }
        self.running.remove(&id).map(|running| running.result)
    }

    pub(super) fn finished(&self) -> Vec<u64> {
        self.running
            .iter()
            .filter(|(_, running)| running.done)
            .map(|(id, _)| *id)
            .collect()
    }

    pub(super) fn deadline(&self, id: u64) -> Option<Instant> {
        self.running.get(&id).map(|running| running.deadline)
    }

    pub(super) fn kill(&mut self, id: u64) {
        if let Some(running) = self.running.get_mut(&id)
            && !running.done
        {
            running.result.timed_out = true;
            running.kill();
        }
    }
}

impl Drop for Execs {
    fn drop(&mut self) {
        // A reload or `diri.stop()` ends what the old script started.
        for running in self.running.values_mut() {
            if !running.done {
                running.kill();
            }
        }
    }
}

#[derive(Clone)]
struct Notify {
    tx: SyncSender<Event>,
    waker: Option<Waker>,
    pending: Arc<AtomicBool>,
}

impl Notify {
    fn send(&self, event: Event) -> bool {
        if self.tx.send(event).is_err() {
            return false;
        }
        if let Some(waker) = &self.waker
            && !self.pending.swap(true, Ordering::SeqCst)
        {
            (waker.0)();
        }
        true
    }
}

fn reader(
    id: u64,
    stream: Stream,
    mut pipe: impl Read + Send + 'static,
    notify: Notify,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buffer = vec![0; READ_CHUNK];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(count) => {
                    let event = Event::Output {
                        id,
                        stream,
                        data: buffer[..count].to_vec(),
                    };
                    if !notify.send(event) {
                        return;
                    }
                }
            }
        }
    })
}

#[cfg(unix)]
fn wait(
    child: &mut diri_platform::child::Child,
    _kill: &AtomicBool,
) -> std::io::Result<std::process::ExitStatus> {
    // The runtime kills the whole group by pid; this only reaps.
    child.wait()
}

#[cfg(not(unix))]
fn wait(
    child: &mut diri_platform::child::Child,
    kill: &AtomicBool,
) -> std::io::Result<std::process::ExitStatus> {
    // No process groups here: the job object is killed through the child,
    // which this thread owns, so it checks the kill flag between waits.
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if kill.load(Ordering::SeqCst) {
            let _ = child.kill();
            return child.wait();
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    std::os::unix::process::ExitStatusExt::signal(status)
}

#[cfg(not(unix))]
fn signal_of(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

/// The complete lines in `partial`, without their newline. At the end of the
/// stream (`flush`), the rest is a line too; a line past [`MAX_LINE`] is
/// delivered in pieces.
fn split_lines(stream: Stream, partial: &mut Vec<u8>, flush: bool) -> Vec<(Stream, String)> {
    let mut lines = Vec::new();
    let mut start = 0;
    while let Some(end) = partial[start..].iter().position(|byte| *byte == b'\n') {
        let line = &partial[start..start + end];
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        lines.push((stream, String::from_utf8_lossy(line).into_owned()));
        start += end + 1;
    }
    partial.drain(..start);
    if (flush && !partial.is_empty()) || partial.len() > MAX_LINE {
        lines.push((stream, String::from_utf8_lossy(partial).into_owned()));
        partial.clear();
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_split_into_lines_and_flushed_at_the_end() {
        let mut partial = b"one\r\ntwo\nthr".to_vec();
        let lines = split_lines(Stream::Stdout, &mut partial, false);
        assert_eq!(
            lines
                .iter()
                .map(|(_, line)| line.as_str())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert_eq!(partial, b"thr");
        partial.extend_from_slice(b"ee");
        let rest = split_lines(Stream::Stdout, &mut partial, true);
        assert_eq!(rest[0].1, "three");
        assert!(partial.is_empty());
    }
}
