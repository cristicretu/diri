//! The Playwright sidecar behind `test.run` and `browser.act`.
//!
//! Ported from `BrowserPool`: a lazily-launched node process speaking one
//! JSON-RPC line per request over stdio (`{id, method, params}` in,
//! `{id, result|error}` out). Self-contained `test.run` flows and per-session
//! interactive browsers share the sidecar; an idle sweep recycles it (and all
//! browser RAM) once nothing has run for a while and no page is open.
//! Silently unavailable without node or the sidecar script.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// No runs for this long → kill the sidecar and reclaim all browser RAM.
const IDLE_TIMEOUT: Duration = Duration::from_secs(180);
/// A single request's patience: cross-browser flows launch real browsers.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub struct BrowserPool {
    inner: Arc<Mutex<PoolInner>>,
    artifact_dir: PathBuf,
}

struct PoolInner {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    pending: HashMap<u64, mpsc::Sender<Result<Value, String>>>,
    next_id: u64,
    last_activity: Instant,
    open_browser_sessions: HashSet<String>,
    idle_sweeper_running: bool,
}

impl BrowserPool {
    pub fn new(logs_dir: &Path) -> Self {
        Self {
            inner: Arc::new(Mutex::new(PoolInner {
                child: None,
                stdin: None,
                pending: HashMap::new(),
                next_id: 0,
                last_activity: Instant::now(),
                open_browser_sessions: HashSet::new(),
                idle_sweeper_running: false,
            })),
            artifact_dir: logs_dir.join("test-artifacts"),
        }
    }

    pub fn is_available() -> bool {
        resolve_node().is_some() && locate_sidecar().is_some()
    }

    /// Runs a cross-browser flow; the sidecar's structured result verbatim.
    pub fn run(&self, mut params: Value) -> Result<Value, String> {
        inject_artifact_dir(&mut params, &self.artifact_dir);
        self.request("run", params, None)
    }

    /// One step of an interactive, per-session browser; the sidecar keeps the
    /// page alive between calls, keyed by session id.
    pub fn browse(&self, mut params: Value) -> Result<Value, String> {
        // The wire spells it `sessionID`; the sidecar wants `sessionId`.
        let session_id = params
            .get("sessionID")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let (Some(object), Some(id)) = (params.as_object_mut(), &session_id) {
            object.remove("sessionID");
            object.insert("sessionId".into(), Value::String(id.clone()));
        }
        inject_artifact_dir(&mut params, &self.artifact_dir);
        let action = params
            .get("action")
            .and_then(Value::as_str)
            .map(str::to_string);
        let result = self.request("browser", params, None)?;
        // Track open pages so the idle sweep never drops one from under an
        // agent mid-use.
        if let (Some(id), Some(action)) = (session_id, action) {
            let mut inner = self.inner.lock().expect("pool");
            match action.as_str() {
                "open" => {
                    inner.open_browser_sessions.insert(id);
                }
                "close" => {
                    inner.open_browser_sessions.remove(&id);
                }
                _ => {}
            }
        }
        Ok(result)
    }

    fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, String> {
        let receiver = {
            let mut inner = self.inner.lock().expect("pool");
            self.ensure_running(&mut inner)?;
            inner.last_activity = Instant::now();
            inner.next_id += 1;
            let id = inner.next_id;
            let (sender, receiver) = mpsc::channel();
            inner.pending.insert(id, sender);
            let line = format!(
                "{}\n",
                json!({ "id": id, "method": method, "params": params })
            );
            let Some(stdin) = inner.stdin.as_mut() else {
                inner.pending.remove(&id);
                return Err("sidecar not running".into());
            };
            if let Err(error) = stdin
                .write_all(line.as_bytes())
                .and_then(|()| stdin.flush())
            {
                inner.pending.remove(&id);
                return Err(format!("sidecar write failed: {error}"));
            }
            receiver
        };
        match receiver.recv_timeout(timeout.unwrap_or(REQUEST_TIMEOUT)) {
            Ok(result) => result,
            Err(_) => Err("sidecar timed out".into()),
        }
    }

    fn ensure_running(&self, inner: &mut PoolInner) -> Result<(), String> {
        if let Some(child) = inner.child.as_mut()
            && child.try_wait().ok().flatten().is_none()
        {
            return Ok(());
        }
        let node =
            resolve_node().ok_or("node not found on PATH — install Node.js to use test_run")?;
        let sidecar = locate_sidecar().ok_or("test sidecar not found")?;
        let _ = std::fs::create_dir_all(&self.artifact_dir);

        let mut child = Command::new(&node)
            .arg(&sidecar)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("failed to launch sidecar: {error}"))?;
        inner.stdin = child.stdin.take();
        let stdout = child.stdout.take();

        // Route each response line to its waiting request.
        if let Some(stdout) = stdout {
            let pool = Arc::clone(&self.inner);
            let _ = std::thread::Builder::new()
                .name("diri-browser-sidecar".into())
                .spawn(move || {
                    let reader = BufReader::new(stdout);
                    for line in reader.lines() {
                        let Ok(line) = line else { break };
                        let Ok(message) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        let Some(id) = message.get("id").and_then(Value::as_u64) else {
                            continue;
                        };
                        let Ok(mut inner) = pool.lock() else { break };
                        inner.last_activity = Instant::now();
                        if let Some(sender) = inner.pending.remove(&id) {
                            let outcome = match message.get("error").and_then(Value::as_str) {
                                Some(error) => Err(error.to_string()),
                                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                            };
                            let _ = sender.send(outcome);
                        }
                    }
                    // Sidecar gone: fail whatever was still waiting.
                    if let Ok(mut inner) = pool.lock() {
                        for (_, sender) in inner.pending.drain() {
                            let _ = sender.send(Err("sidecar exited".into()));
                        }
                        inner.child = None;
                        inner.stdin = None;
                    }
                });
        }
        inner.child = Some(child);

        if !inner.idle_sweeper_running {
            inner.idle_sweeper_running = true;
            let pool = Arc::clone(&self.inner);
            let _ = std::thread::Builder::new()
                .name("diri-browser-idle".into())
                .spawn(move || {
                    loop {
                        std::thread::sleep(Duration::from_secs(30));
                        let Ok(mut inner) = pool.lock() else { return };
                        let idle = inner.pending.is_empty()
                            && inner.open_browser_sessions.is_empty()
                            && inner.last_activity.elapsed() > IDLE_TIMEOUT;
                        if idle && let Some(mut child) = inner.child.take() {
                            // Recycle: reclaim all browser RAM.
                            inner.stdin = None;
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                    }
                });
        }
        Ok(())
    }
}

fn inject_artifact_dir(params: &mut Value, artifact_dir: &Path) {
    if let Some(object) = params.as_object_mut() {
        object.insert(
            "artifactDir".into(),
            Value::String(artifact_dir.to_string_lossy().into_owned()),
        );
    }
}

fn resolve_node() -> Option<PathBuf> {
    let path = std::env::var("PATH").ok()?;
    path.split(':')
        .map(|dir| Path::new(dir).join("node"))
        .find(|candidate| candidate.is_file())
}

/// The sidecar script: env override, then app-bundle Resources, then upward
/// from the executable (dev checkouts run from target dirs under diri/).
fn locate_sidecar() -> Option<PathBuf> {
    if let Ok(configured) = std::env::var("DIRIJOR_SIDECAR") {
        let path = PathBuf::from(configured);
        if path.is_file() {
            return Some(path);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe().and_then(|exe| exe.canonicalize()) {
        // Resources/bin/<exe> → Resources/sidecar/server.js in the bundle.
        let mut dir = exe.parent().map(Path::to_path_buf);
        for _ in 0..7 {
            let Some(current) = dir else { break };
            candidates.push(current.join("sidecar/server.js"));
            dir = current.parent().map(Path::to_path_buf);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir = Some(cwd);
        for _ in 0..7 {
            let Some(current) = dir else { break };
            candidates.push(current.join("sidecar/server.js"));
            dir = current.parent().map(Path::to_path_buf);
        }
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}
