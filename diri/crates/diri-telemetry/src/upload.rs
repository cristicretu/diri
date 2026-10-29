//! Batch upload of the spool to the telemetry Worker.
//!
//! Only the Engine runs the uploader. It reads every process's spool files
//! (app, Engine, Holders) from the byte offset it last acknowledged, sends
//! complete lines as one gzip NDJSON request per ≤1 MiB, and deletes files
//! once they are fully uploaded and no longer written. `gzip` and `curl` are
//! the system binaries, as in `diri-updater`, so no HTTP or compression stack
//! is linked in.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc::{RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;

use crate::identity::{Config, Identity};
use crate::spool::{self, SpoolName};

/// The deployed Worker (telemetry/worker). `DIRI_TELEMETRY_ENDPOINT` at build
/// time overrides it, at run time overrides both (`off` disables uploading).
/// 0.8.9 shipped with this unset and uploaded nothing; the test below keeps a
/// release from doing that again.
pub const DEFAULT_ENDPOINT: Option<&str> = Some("https://telemetry.diri.sh");
/// Kept small so one batch fits the Worker's CPU budget on the free plan.
pub const BATCH_RAW_BYTES: usize = 1024 * 1024;
/// Routine uploads are hourly: each batch costs the Worker a request, an R2
/// PUT and a handful of D1 row writes, so the interval sets how many installs
/// the free tiers carry. Incidents still upload within a minute.
pub const ROUTINE_INTERVAL: Duration = Duration::from_secs(60 * 60);
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);
const OFFSETS_FILE: &str = "offsets.json";
/// An `.open` file untouched this long is treated as abandoned.
const ABANDONED_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Where uploads go, or `None` when uploading is not configured. Debug builds
/// (tests, `cargo run`) never upload unless the run-time variable is set.
#[must_use]
pub fn endpoint() -> Option<String> {
    match std::env::var("DIRI_TELEMETRY_ENDPOINT") {
        Ok(value) if value == "off" || value.is_empty() => return None,
        Ok(value) => return Some(value),
        Err(_) => {}
    }
    if cfg!(debug_assertions) {
        return None;
    }
    option_env!("DIRI_TELEMETRY_ENDPOINT")
        .or(DEFAULT_ENDPOINT)
        .map(str::to_owned)
}

/// Build facts sent in every batch header.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Meta {
    pub app_version: String,
    pub build: String,
    pub channel: String,
    pub os: String,
    pub os_version: String,
    pub arch: String,
}

#[derive(Serialize)]
struct Header<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    install: &'a str,
    support_id: String,
    name: Option<String>,
    #[serde(flatten)]
    meta: &'a Meta,
    sent_at: u64,
    lines: usize,
}

/// Sends one gzip-compressed body and returns the HTTP status.
pub type Transport = Box<dyn FnMut(&str, &[u8], &str) -> std::io::Result<u16> + Send>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CycleReport {
    pub batches: usize,
    pub lines: usize,
    pub raw_bytes: usize,
    pub failed: bool,
}

pub struct Uploader {
    state_dir: PathBuf,
    endpoint: String,
    meta: Meta,
    transport: Transport,
}

impl Uploader {
    #[must_use]
    pub fn new(state_dir: PathBuf, endpoint: String, meta: Meta) -> Self {
        Self {
            state_dir,
            endpoint,
            meta,
            transport: Box::new(curl_transport),
        }
    }

    #[must_use]
    pub fn with_transport(mut self, transport: Transport) -> Self {
        self.transport = transport;
        self
    }

    /// Runs forever on a background thread: every minute it checks for an
    /// urgent marker (an incident was recorded), and otherwise uploads every
    /// hour.
    pub fn spawn(mut self) -> std::io::Result<UploadHandle> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<SyncSender<CycleReport>>(4);
        std::thread::Builder::new()
            .name("diri-telemetry-upload".into())
            .spawn(move || {
                let urgent = spool::spool_dir(&self.state_dir).join(spool::URGENT_MARKER);
                // Upload what previous runs left behind shortly after start.
                let mut last = Instant::now() - ROUTINE_INTERVAL + Duration::from_secs(30);
                let mut requests = Some(rx);
                loop {
                    let request = match requests.as_ref().map(|rx| rx.recv_timeout(POLL_INTERVAL)) {
                        Some(Ok(reply)) => Some(reply),
                        Some(Err(RecvTimeoutError::Timeout)) => None,
                        Some(Err(RecvTimeoutError::Disconnected)) | None => {
                            requests = None;
                            std::thread::sleep(POLL_INTERVAL);
                            None
                        }
                    };
                    if let Some(reply) = request {
                        // Asked for by the user: send now, even with sharing
                        // off, including what this process recorded just now.
                        crate::flush(Duration::from_secs(1));
                        let _ = std::fs::remove_file(&urgent);
                        let report = self.run_cycle_with(true);
                        if !report.failed {
                            last = Instant::now();
                        }
                        let _ = reply.send(report);
                    } else if urgent.exists() || last.elapsed() >= ROUTINE_INTERVAL {
                        let _ = std::fs::remove_file(&urgent);
                        let report = self.run_cycle();
                        if !report.failed {
                            last = Instant::now();
                        }
                    }
                }
            })?;
        Ok(UploadHandle { tx })
    }

    /// One routine pass over the spool. Returns what was sent.
    pub fn run_cycle(&mut self) -> CycleReport {
        self.run_cycle_with(false)
    }

    /// One pass over the spool; `forced` (an explicit user request: Send now
    /// or Report a Problem) sends even when sharing is turned off. Routine
    /// passes send nothing while sharing is off, and after an opt-in only
    /// what was recorded since.
    pub fn run_cycle_with(&mut self, forced: bool) -> CycleReport {
        let dir = spool::spool_dir(&self.state_dir);
        let mut report = CycleReport::default();
        let config = Config::load(&self.state_dir);
        let since = if forced { None } else { config.chosen_ms };
        if !config.sharing_allowed() && !forced {
            spool::prune(&dir, spool::SPOOL_CAP_BYTES);
            return report;
        }
        let Ok(identity) = Identity::load_or_create(&self.state_dir) else {
            report.failed = true;
            return report;
        };
        let mut offsets = load_offsets(&dir);
        let files = list_spool(&dir);

        let mut body: Vec<u8> = Vec::new();
        let mut lines = 0usize;
        let mut pending: Vec<(String, u64)> = Vec::new();
        for (name, path, _) in &files {
            let offset = offsets.get(name).copied().unwrap_or(0);
            let Ok(chunk) = read_complete_lines(path, offset, BATCH_RAW_BYTES) else {
                continue;
            };
            if chunk.is_empty() {
                continue;
            }
            if !body.is_empty()
                && body.len() + chunk.len() > BATCH_RAW_BYTES
                && !self.send(
                    &identity,
                    &config,
                    since,
                    &mut body,
                    &mut lines,
                    &mut pending,
                    &mut offsets,
                    &mut report,
                )
            {
                break;
            }
            lines += chunk.iter().filter(|b| **b == b'\n').count();
            body.extend_from_slice(&chunk);
            pending.push((name.clone(), offset + chunk.len() as u64));
        }
        if !body.is_empty() && !report.failed {
            self.send(
                &identity,
                &config,
                since,
                &mut body,
                &mut lines,
                &mut pending,
                &mut offsets,
                &mut report,
            );
        }

        // Drop files that are fully sent and no longer being written.
        for (name, path, parsed) in &files {
            let sent = offsets.get(name).copied().unwrap_or(0);
            let len = std::fs::metadata(path).map_or(0, |m| m.len());
            if sent >= len
                && (parsed.sealed || writer_gone(parsed, path))
                && std::fs::remove_file(path).is_ok()
            {
                offsets.remove(name);
            }
        }
        offsets.retain(|name, _| dir.join(name).exists());
        let _ = save_offsets(&dir, &offsets);
        spool::prune(&dir, spool::SPOOL_CAP_BYTES);
        report
    }

    #[allow(clippy::too_many_arguments)]
    fn send(
        &mut self,
        identity: &Identity,
        config: &Config,
        since: Option<u64>,
        body: &mut Vec<u8>,
        lines: &mut usize,
        pending: &mut Vec<(String, u64)>,
        offsets: &mut BTreeMap<String, u64>,
        report: &mut CycleReport,
    ) -> bool {
        // Old app/Holder processes may still write the former error fields.
        // Project every queued record before it can leave this machine.
        let safe_body = upload_records(body, since);
        *lines = safe_body.iter().filter(|byte| **byte == b'\n').count();
        if *lines == 0 {
            // Everything here predates consent: mark it handled, send nothing.
            for (name, offset) in pending.drain(..) {
                offsets.insert(name, offset);
            }
            let _ = save_offsets(&spool::spool_dir(&self.state_dir), offsets);
            body.clear();
            return true;
        }
        let header = Header {
            v: 1,
            kind: "batch",
            install: &identity.install_id,
            support_id: identity.support_id(),
            name: config.effective_name(),
            meta: &self.meta,
            sent_at: crate::now_ms(),
            lines: *lines,
        };
        let mut payload = serde_json::to_vec(&header).unwrap_or_default();
        payload.push(b'\n');
        payload.extend_from_slice(&safe_body);
        let status = gzip(&payload)
            .and_then(|gz| (self.transport)(&self.endpoint, &gz, &identity.install_id));
        let accepted = match status {
            Ok(code) if (200..300).contains(&code) => true,
            // The Worker rejected the batch itself; resending it would fail
            // forever, so skip it and say so.
            Ok(code @ (400 | 413 | 422)) => {
                crate::record(
                    "telemetry.upload_rejected",
                    crate::Severity::Warn,
                    vec![("status", code.into()), ("lines", (*lines).into())],
                );
                true
            }
            _ => false,
        };
        if accepted {
            report.batches += 1;
            report.lines += *lines;
            report.raw_bytes += body.len();
            for (name, offset) in pending.drain(..) {
                offsets.insert(name, offset);
            }
            let _ = save_offsets(&spool::spool_dir(&self.state_dir), offsets);
        } else {
            report.failed = true;
            pending.clear();
        }
        body.clear();
        *lines = 0;
        accepted
    }
}

/// Removes retired content-bearing fields, including records written by old
/// processes before an upgrade, and with `since`, records made before the
/// person opted in. Preserve the scanner's canonical key order.
fn upload_records(body: &[u8], since: Option<u64>) -> Vec<u8> {
    let mut output = Vec::with_capacity(body.len());
    for line in body
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let Ok(serde_json::Value::Object(mut record)) = serde_json::from_slice(line) else {
            continue;
        };
        if let Some(since) = since
            && record
                .get("t")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
                < since
        {
            continue;
        }
        let hello = record.get("k").and_then(serde_json::Value::as_str) == Some("client.hello");
        let panic = record.get("k").and_then(serde_json::Value::as_str) == Some("panic");
        if let Some(serde_json::Value::Object(fields)) = record.get_mut("f") {
            fields.remove("message");
            fields.remove("error");
            if hello {
                fields.remove("build");
            }
            if panic {
                fields.remove("thread");
            }
        } else if record.contains_key("f") {
            continue;
        }
        output.push(b'{');
        let mut first = true;
        for key in ["t", "seq", "p", "pid", "k", "s", "f"] {
            if let Some(value) = record.get(key) {
                if !first {
                    output.push(b',');
                }
                first = false;
                serde_json::to_writer(&mut output, key).expect("Vec writes cannot fail");
                output.push(b':');
                serde_json::to_writer(&mut output, value).expect("JSON value serializes");
            }
        }
        output.extend_from_slice(b"}\n");
    }
    output
}

/// Asks a running [`Uploader`] to upload immediately.
#[derive(Clone)]
pub struct UploadHandle {
    tx: SyncSender<SyncSender<CycleReport>>,
}

/// The outcome of [`upload_now`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadNow {
    /// No uploader runs in this process (not configured in this build).
    Unavailable,
    /// The uploader was busy or the network slow; it will keep trying.
    TimedOut,
    Done(CycleReport),
}

impl UploadHandle {
    /// Requests an upload and waits up to `timeout` for its report.
    #[must_use]
    pub fn upload_now(&self, timeout: Duration) -> UploadNow {
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        if self.tx.try_send(reply_tx).is_err() {
            return UploadNow::TimedOut;
        }
        reply_rx
            .recv_timeout(timeout)
            .map_or(UploadNow::TimedOut, UploadNow::Done)
    }

    /// Makes this handle the one [`upload_now`] uses.
    pub fn install(self) {
        let _ = GLOBAL_HANDLE.set(self);
    }
}

static GLOBAL_HANDLE: OnceLock<UploadHandle> = OnceLock::new();

/// Uploads through the handle installed in this process, if any.
#[must_use]
pub fn upload_now(timeout: Duration) -> UploadNow {
    GLOBAL_HANDLE
        .get()
        .map_or(UploadNow::Unavailable, |handle| handle.upload_now(timeout))
}

fn list_spool(dir: &Path) -> Vec<(String, PathBuf, SpoolName)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let parsed = SpoolName::parse(&name)?;
            Some((name, entry.path(), parsed))
        })
        .collect();
    files.sort_by_key(|(_, _, p)| (p.start_ms, p.pid, p.index));
    files
}

/// Bytes from `offset` up to and including the last newline, at most
/// `limit` bytes (a single oversized line is skipped whole).
fn read_complete_lines(path: &Path, offset: u64, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len <= offset {
        return Ok(Vec::new());
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut chunk = Vec::new();
    file.take(limit as u64).read_to_end(&mut chunk)?;
    match chunk.iter().rposition(|b| *b == b'\n') {
        Some(end) => chunk.truncate(end + 1),
        None => chunk.clear(),
    }
    Ok(chunk)
}

fn writer_gone(name: &SpoolName, path: &Path) -> bool {
    let stale = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= ABANDONED_AFTER);
    stale || !pid_alive(name.pid)
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 only checks for existence and permission.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

fn load_offsets(dir: &Path) -> BTreeMap<String, u64> {
    std::fs::read(dir.join(OFFSETS_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_offsets(dir: &Path, offsets: &BTreeMap<String, u64>) -> std::io::Result<()> {
    crate::identity::write_private(&dir.join(OFFSETS_FILE), &serde_json::to_vec(offsets)?)
}

/// Compresses through the system `gzip`.
pub fn gzip(input: &[u8]) -> std::io::Result<Vec<u8>> {
    pipe(
        Command::new(diri_platform::gzip_executable()).args(["-c", "-6"]),
        input,
    )
}

fn pipe(command: &mut Command, input: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("no stdin"))?;
    let input = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output()?;
    writer
        .join()
        .map_err(|_| std::io::Error::other("stdin writer panicked"))??;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "{:?} exited {}",
            command.get_program(),
            output.status
        )));
    }
    Ok(output.stdout)
}

fn curl_transport(endpoint: &str, body: &[u8], install: &str) -> std::io::Result<u16> {
    let url = format!("{}/v1/ingest", endpoint.trim_end_matches('/'));
    let output = pipe(
        Command::new(diri_platform::curl_executable()).args([
            "-sS",
            "-m",
            "30",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/x-ndjson",
            "-H",
            "Content-Encoding: gzip",
            "-H",
            &format!("X-Diri-Install: {install}"),
            "--data-binary",
            "@-",
            &url,
        ]),
        body,
    )?;
    String::from_utf8_lossy(&output)
        .trim()
        .parse()
        .map_err(|_| std::io::Error::other("curl printed no status"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn gunzip(input: &[u8]) -> Vec<u8> {
        pipe(
            Command::new(diri_platform::gzip_executable()).args(["-d", "-c"]),
            input,
        )
        .unwrap()
    }

    fn capture(status: u16) -> (Transport, Arc<Mutex<Vec<String>>>) {
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&bodies);
        let transport: Transport = Box::new(move |_, body, _| {
            sink.lock()
                .unwrap()
                .push(String::from_utf8(gunzip(body)).unwrap());
            Ok(status)
        });
        (transport, bodies)
    }

    fn uploader(state: &Path, transport: Transport) -> Uploader {
        if !crate::identity::telemetry_dir(state)
            .join("config.json")
            .exists()
        {
            Config::default().choose(true, 0).save(state).unwrap();
        }
        Uploader::new(
            state.to_path_buf(),
            "https://t.example".into(),
            Meta {
                app_version: "0.9.0".into(),
                ..Meta::default()
            },
        )
        .with_transport(transport)
    }

    #[test]
    fn queued_legacy_error_payloads_are_removed_before_transport() {
        let state = tempfile::tempdir().unwrap();
        let dir = spool::spool_dir(state.path());
        std::fs::create_dir_all(&dir).unwrap();
        let record = r#"{"t":1,"seq":1,"p":"engine","pid":1,"k":"rpc.error","s":"error","f":{"code":"internal","message":"PRIVATE_PROJECT password=hunter2","error":"SECRET_JSON_KEY"}}"#;
        std::fs::write(dir.join("engine-1-0-0000.jsonl"), format!("{record}\n")).unwrap();
        let (transport, bodies) = capture(202);
        assert_eq!(uploader(state.path(), transport).run_cycle().lines, 1);
        let bodies = bodies.lock().unwrap();
        for forbidden in ["PRIVATE_PROJECT", "hunter2", "SECRET_JSON_KEY"] {
            assert!(!bodies[0].contains(forbidden));
        }
        assert!(bodies[0].contains(r#""code":"internal""#));
        assert!(
            bodies[0]
                .lines()
                .nth(1)
                .unwrap()
                .starts_with(r#"{"t":1,"seq":1,"p":"engine""#)
        );
        let legacy_panic = br#"{"k":"panic","f":{"thread":"PRIVATE_THREAD","message":"PRIVATE_PANIC","signature":"known_symbol"}}"#;
        let projected = String::from_utf8(upload_records(legacy_panic, None)).unwrap();
        assert!(!projected.contains("PRIVATE_"));
        assert!(projected.contains("known_symbol"));
    }

    #[test]
    fn uploads_complete_lines_once_and_resumes_from_offset() {
        let state = tempfile::tempdir().unwrap();
        let dir = spool::spool_dir(state.path());
        std::fs::create_dir_all(&dir).unwrap();
        // Our own pid: the writer is alive, so the .open file must stay.
        let open = dir.join(format!("engine-{}-1-0000.open", std::process::id()));
        std::fs::write(&open, "{\"k\":\"a\"}\n{\"k\":\"b\"}\n{\"k\":\"partial").unwrap();
        let sealed = dir.join("app-1-0-0000.jsonl");
        std::fs::write(&sealed, "{\"k\":\"z\"}\n").unwrap();

        let (transport, bodies) = capture(202);
        let mut up = uploader(state.path(), transport);
        let report = up.run_cycle();
        assert_eq!(report.batches, 1);
        assert_eq!(report.lines, 3);
        {
            let bodies = bodies.lock().unwrap();
            let lines: Vec<&str> = bodies[0].lines().collect();
            let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
            assert_eq!(header["type"], "batch");
            assert_eq!(header["app_version"], "0.9.0");
            assert_eq!(header["lines"], 3);
            assert!(header["support_id"].as_str().unwrap().starts_with("D-"));
            assert_eq!(
                &lines[1..],
                ["{\"k\":\"z\"}", "{\"k\":\"a\"}", "{\"k\":\"b\"}"]
            );
        }
        assert!(!sealed.exists(), "fully sent sealed file is deleted");
        assert!(open.exists(), "live writer's file stays");

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&open)
            .unwrap();
        file.write_all(b"\"}\n").unwrap();
        let report = up.run_cycle();
        assert_eq!(report.lines, 1);
        assert_eq!(
            bodies.lock().unwrap()[1].lines().nth(1),
            Some("{\"k\":\"partial\"}")
        );
        assert_eq!(up.run_cycle().batches, 0, "nothing new, nothing sent");
    }

    #[test]
    fn failed_upload_keeps_everything_for_next_cycle() {
        let state = tempfile::tempdir().unwrap();
        let dir = spool::spool_dir(state.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app-1-0-0000.jsonl"), "{\"k\":\"z\"}\n").unwrap();
        let (transport, _) = capture(503);
        let report = uploader(state.path(), transport).run_cycle();
        assert!(report.failed);
        assert!(dir.join("app-1-0-0000.jsonl").exists());
        let (transport, bodies) = capture(200);
        assert_eq!(uploader(state.path(), transport).run_cycle().lines, 1);
        assert_eq!(bodies.lock().unwrap().len(), 1);
    }

    #[test]
    fn new_installs_send_nothing_until_opt_in_and_only_from_then() {
        let state = tempfile::tempdir().unwrap();
        let dir = spool::spool_dir(state.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("app-1-0-0000.jsonl"),
            "{\"t\":10,\"k\":\"before\"}\n{\"t\":30,\"k\":\"after\"}\n",
        )
        .unwrap();
        // A new install that has not answered yet.
        Config::default().save(state.path()).unwrap();
        let (transport, bodies) = capture(202);
        assert_eq!(
            uploader(state.path(), transport).run_cycle(),
            CycleReport::default()
        );
        assert!(bodies.lock().unwrap().is_empty());

        Config::default()
            .choose(true, 20)
            .save(state.path())
            .unwrap();
        let (transport, bodies) = capture(202);
        assert_eq!(uploader(state.path(), transport).run_cycle().lines, 1);
        let bodies = bodies.lock().unwrap();
        assert!(bodies[0].contains("after"));
        assert!(!bodies[0].contains("before"), "recorded before opting in");
        assert!(bodies[0].lines().next().unwrap().contains(r#""name":null"#));
    }

    #[test]
    fn settings_from_before_opt_in_keep_uploading() {
        let state = tempfile::tempdir().unwrap();
        let dir = spool::spool_dir(state.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app-1-0-0000.jsonl"), "{\"t\":10,\"k\":\"a\"}\n").unwrap();
        let path = crate::identity::telemetry_dir(state.path()).join("config.json");
        std::fs::write(&path, r#"{"upload": true, "name": null}"#).unwrap();
        let (transport, bodies) = capture(202);
        assert_eq!(uploader(state.path(), transport).run_cycle().lines, 1);
        assert_eq!(bodies.lock().unwrap().len(), 1);
    }

    #[test]
    fn disabled_upload_sends_nothing() {
        let state = tempfile::tempdir().unwrap();
        let dir = spool::spool_dir(state.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app-1-0-0000.jsonl"), "{}\n").unwrap();
        Config::default()
            .choose(false, 0)
            .save(state.path())
            .unwrap();
        let (transport, bodies) = capture(200);
        assert_eq!(
            uploader(state.path(), transport).run_cycle(),
            CycleReport::default()
        );
        assert!(bodies.lock().unwrap().is_empty());
    }

    #[test]
    fn upload_now_sends_immediately_even_with_sharing_off() {
        let state = tempfile::tempdir().unwrap();
        let dir = spool::spool_dir(state.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app-1-0-0000.jsonl"), "{}\n").unwrap();
        Config::default()
            .choose(false, 0)
            .save(state.path())
            .unwrap();
        let (transport, bodies) = capture(202);
        let handle = uploader(state.path(), transport).spawn().unwrap();
        match handle.upload_now(Duration::from_secs(10)) {
            UploadNow::Done(report) => assert_eq!((report.batches, report.lines), (1, 1)),
            other => panic!("{other:?}"),
        }
        assert_eq!(bodies.lock().unwrap().len(), 1);
        match handle.upload_now(Duration::from_secs(10)) {
            UploadNow::Done(report) => assert_eq!(report.batches, 0, "nothing new"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn release_builds_have_an_upload_endpoint() {
        let endpoint = option_env!("DIRI_TELEMETRY_ENDPOINT").or(DEFAULT_ENDPOINT);
        let endpoint = endpoint.expect("a release without an endpoint records but never uploads");
        assert!(endpoint.starts_with("https://"), "{endpoint}");
    }

    #[test]
    fn upload_now_without_an_uploader_is_unavailable() {
        assert_eq!(
            upload_now(Duration::from_millis(10)),
            UploadNow::Unavailable
        );
    }

    #[test]
    fn dead_writers_open_files_are_removed_after_upload() {
        let state = tempfile::tempdir().unwrap();
        let dir = spool::spool_dir(state.path());
        std::fs::create_dir_all(&dir).unwrap();
        // pid_t::MAX is never a live process.
        let dead = dir.join(format!("holder-{}-0-0000.open", i32::MAX));
        std::fs::write(&dead, "{}\n").unwrap();
        let (transport, _) = capture(200);
        uploader(state.path(), transport).run_cycle();
        assert!(!dead.exists());
    }
}
