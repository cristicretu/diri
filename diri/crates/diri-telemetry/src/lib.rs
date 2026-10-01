//! Diri's flight recorder.
//!
//! Every Diri process records typed, privacy-bounded events to a local
//! spool; the Engine uploads the spool in batches to the telemetry Worker so
//! a bug report ("look at alex") can be investigated from the timeline.
//! See `diri/TELEMETRY.md` for the event catalog and wire contract.
//!
//! ```ignore
//! diri_telemetry::init_default(diri_telemetry::Process::Engine);
//! diri_telemetry::event!("session.spawn", session = id(&sid), agent = id(&agent), mode = "resume");
//! diri_telemetry::incident!("session.spawn_failed", session = id(&sid), exit = 127);
//! ```
//!
//! Recording never blocks the caller and is a no-op until [`init`] ran, so
//! libraries can record unconditionally and tests record nothing.

mod health;
mod identity;
mod metrics;
mod panic;
mod redact;
pub mod spool;
pub mod upload;
mod value;

use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use health::{ProcessStats, register_gauge, start_health_sampler};
pub use identity::{Config, Identity, login_name, support_id, telemetry_dir};
pub use metrics::{count, observe, observe_ms};
pub use panic::{install_panic_hook, signature_id};
pub use redact::scrub;
pub use value::{AgentClass, Id, Text, Value, agent_class, id, io_error, path_hash, text};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Process {
    App,
    Engine,
    Holder,
}

impl Process {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Process::App => "app",
            Process::Engine => "engine",
            Process::Holder => "holder",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Debug,
    Info,
    Warn,
    Error,
    /// Something a user would call a bug: a crash, a failed spawn, a blank
    /// pane, a long hang. Triggers an upload within a minute and is indexed
    /// server-side.
    Incident,
}

impl Severity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Severity::Debug => "debug",
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
            Severity::Incident => "incident",
        }
    }
}

pub(crate) struct Recorder {
    tx: SyncSender<spool::Message>,
    pub(crate) seq: AtomicU64,
    pub(crate) dropped: AtomicU64,
}

pub(crate) static RECORDER: OnceLock<Recorder> = OnceLock::new();

/// Starts recording for this process into `<state_dir>/telemetry/spool`.
/// Idempotent; returns whether recording is active. `DIRI_TELEMETRY=off`
/// disables recording entirely.
pub fn init(process: Process, state_dir: &Path) -> bool {
    if RECORDER.get().is_some() {
        return true;
    }
    if std::env::var("DIRI_TELEMETRY").is_ok_and(|v| v == "off" || v == "0") {
        return false;
    }
    let Ok(tx) = spool::spawn(spool::spool_dir(state_dir), process) else {
        return false;
    };
    let recorder = Recorder {
        tx,
        seq: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
    };
    if RECORDER.set(recorder).is_err() {
        return true;
    }
    record(
        "process.start",
        Severity::Info,
        vec![
            (
                "recorder_version",
                Value::from(id(env!("CARGO_PKG_VERSION"))),
            ),
            ("os", Value::from(std::env::consts::OS)),
            ("arch", Value::from(std::env::consts::ARCH)),
            ("debug_build", Value::from(cfg!(debug_assertions))),
        ],
    );
    true
}

/// [`init`] under the platform state directory derived from `$HOME`.
pub fn init_default(process: Process) -> bool {
    match default_state_dir() {
        Some(dir) => init(process, &dir),
        None => false,
    }
}

/// The platform state directory (`~/Library/Application Support/Dirijor` on
/// macOS) from `$HOME`.
#[must_use]
pub fn default_state_dir() -> Option<std::path::PathBuf> {
    let home = diri_platform::home_dir().map(|p| p.into_os_string())?;
    Some(diri_proto::paths::DirijorPaths::state_dir(home))
}

#[must_use]
pub fn is_enabled() -> bool {
    RECORDER.get().is_some()
}

/// Records one event. Prefer the [`event!`], [`warn_event!`],
/// [`error_event!`] and [`incident!`] macros, which skip building fields when
/// recording is off.
pub fn record(kind: &'static str, sev: Severity, fields: Vec<(&'static str, Value)>) {
    let Some(recorder) = RECORDER.get() else {
        return;
    };
    let record = spool::Record {
        t: now_ms(),
        seq: recorder.seq.fetch_add(1, Ordering::Relaxed),
        kind,
        sev,
        fields,
    };
    if let Err(TrySendError::Full(_)) = recorder.tx.try_send(spool::Message::Record(record)) {
        recorder.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Waits (bounded) until everything recorded so far is on disk. For exit
/// paths and panic hooks; never call it on a UI or PTY thread in steady state.
pub fn flush(timeout: Duration) {
    let Some(recorder) = RECORDER.get() else {
        return;
    };
    let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(1);
    if recorder
        .tx
        .send_timeout_compat(spool::Message::Flush(ack_tx), timeout)
    {
        let _ = ack_rx.recv_timeout(timeout);
    }
}

trait SendTimeoutCompat {
    fn send_timeout_compat(&self, message: spool::Message, timeout: Duration) -> bool;
}

impl SendTimeoutCompat for SyncSender<spool::Message> {
    fn send_timeout_compat(&self, mut message: spool::Message, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.try_send(message) {
                Ok(()) => return true,
                Err(TrySendError::Disconnected(_)) => return false,
                Err(TrySendError::Full(back)) => {
                    if std::time::Instant::now() >= deadline {
                        return false;
                    }
                    message = back;
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
}

#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[doc(hidden)]
#[macro_export]
macro_rules! __record {
    ($sev:expr, $kind:literal $(, $key:ident = $value:expr)* $(,)?) => {
        if $crate::is_enabled() {
            $crate::record(
                $kind,
                $sev,
                vec![$((stringify!($key), $crate::Value::from($value))),*],
            );
        }
    };
}

/// Records an info event: `event!("kind", key = value, ...)`.
#[macro_export]
macro_rules! event {
    ($($tt:tt)*) => { $crate::__record!($crate::Severity::Info, $($tt)*) };
}

/// Records a debug-severity event (verbose breadcrumbs).
#[macro_export]
macro_rules! debug_event {
    ($($tt:tt)*) => { $crate::__record!($crate::Severity::Debug, $($tt)*) };
}

/// Records a warning: degraded but recovered.
#[macro_export]
macro_rules! warn_event {
    ($($tt:tt)*) => { $crate::__record!($crate::Severity::Warn, $($tt)*) };
}

/// Records an error: an operation failed.
#[macro_export]
macro_rules! error_event {
    ($($tt:tt)*) => { $crate::__record!($crate::Severity::Error, $($tt)*) };
}

/// Records an incident: user-visible breakage. Uploaded within a minute.
#[macro_export]
macro_rules! incident {
    ($($tt:tt)*) => { $crate::__record!($crate::Severity::Incident, $($tt)*) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one test that initializes the global recorder in this binary.
    #[test]
    fn records_reach_the_spool_as_json_lines() {
        let state = tempfile::tempdir().unwrap();
        assert!(init(Process::Engine, state.path()));
        let sid = "s_26bf32debd4c";
        event!(
            "session.spawn",
            session = id(sid),
            agent = id("claude-code"),
            mode = "resume",
            ms = 12u64
        );
        incident!("session.spawn_failed", session = id(sid), exit = 127);
        count("rpc.calls", 3);
        flush(Duration::from_secs(2));

        let dir = spool::spool_dir(state.path());
        let file = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "open"))
            .unwrap();
        let text = std::fs::read_to_string(file).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["k"], "process.start");
        assert_eq!(lines[1]["k"], "session.spawn");
        assert_eq!(lines[1]["p"], "engine");
        assert_eq!(lines[1]["s"], "info");
        assert_eq!(lines[1]["f"]["session"], sid);
        assert_eq!(lines[1]["f"]["mode"], "resume");
        assert_eq!(lines[2]["s"], "incident");
        assert_eq!(lines[2]["f"]["exit"], 127);
        assert!(dir.join(spool::URGENT_MARKER).exists());
    }
}
