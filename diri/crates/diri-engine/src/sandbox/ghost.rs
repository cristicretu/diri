//! The ghost backend: one windowless Chromium per sandbox, driven over the
//! DevTools protocol on a private pipe.
//!
//! Why this and not a virtual display: a headless browser never registers a
//! window, a display or a menu bar with the window server, so it cannot take
//! focus, move the person's windows, or receive the person's cursor. Input
//! is synthesised inside the renderer (`Input.dispatch*`), never posted as a
//! system event. A `CGVirtualDisplay` would add a real display to the
//! person's arrangement (their windows reflow, their cursor can wander onto
//! it) and apps launched onto it still activate. See `diri/PERF.md`
//! "Ghost Desktops" for the measurements that chose this design.
//!
//! Isolation and cleanup:
//! - every sandbox gets a fresh, owner-only profile directory under the
//!   pool's state root: no cookies, storage or cache shared with the
//!   person's browser or with another sandbox;
//! - CDP runs over fds 3/4 (`--remote-debugging-pipe`), so there is no TCP
//!   port another local process could attach to;
//! - the browser is its own process group leader; `shutdown` closes the
//!   pipe (the browser exits on EOF, measured ~40 ms), then kills the group;
//! - if the Engine dies, the kernel closes the pipe and the browser exits on
//!   its own: no orphan survives an Engine crash.
//!
//! Zero setup: it uses Playwright's `chrome-headless-shell` when present
//! (smallest, ~195 MB shared on disk, already installed by `test.run`), else
//! an installed Chrome/Chromium in headless mode.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::Engine as _;
use diri_proto::sandbox::{self as proto, SandboxAction, SandboxBackendKind};
use serde_json::{Value, json};

use super::{FrameSource, LaunchSpec, PreviewFrame, SandboxBackend, SandboxError, SandboxInstance};

const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const NAVIGATE_TIMEOUT: Duration = Duration::from_secs(30);
/// A preview keeps streaming this long after the last `frame` call.
const PREVIEW_LINGER: Duration = Duration::from_secs(2);
/// Preview frames are small: tiles are a few hundred points wide.
const PREVIEW_MAX_WIDTH: u32 = 960;
const PREVIEW_MAX_HEIGHT: u32 = 600;
/// The longest CDP message accepted (a full-page PNG of a 4K viewport fits).
const MAX_MESSAGE_BYTES: usize = 64 << 20;

/// Where the browser came from, for `sandbox.list`.
#[derive(Debug, Clone)]
pub struct BrowserBinary {
    pub path: PathBuf,
    pub headless_shell: bool,
}

#[derive(Default)]
pub struct GhostBackend {
    binary: OnceLock<Option<BrowserBinary>>,
    explicit: Option<Option<BrowserBinary>>,
}

impl GhostBackend {
    /// Finds a browser lazily, on first use.
    pub fn discover() -> Self {
        Self::default()
    }

    /// A fixed binary (tests, benchmarks) or none (unavailable).
    pub fn with_binary(binary: Option<BrowserBinary>) -> Self {
        Self {
            binary: OnceLock::new(),
            explicit: Some(binary),
        }
    }

    pub fn binary(&self) -> Option<&BrowserBinary> {
        if let Some(explicit) = &self.explicit {
            return explicit.as_ref();
        }
        self.binary.get_or_init(find_browser).as_ref()
    }
}

impl SandboxBackend for GhostBackend {
    fn kind(&self) -> SandboxBackendKind {
        SandboxBackendKind::Ghost
    }

    fn availability(&self) -> Result<String, String> {
        match self.binary() {
            Some(binary) => Ok(format!(
                "{} ({})",
                if binary.headless_shell {
                    "chrome-headless-shell"
                } else {
                    "Chrome, headless"
                },
                binary.path.display()
            )),
            None => Err(
                "no Chromium-based browser found: install Google Chrome, or run \
                 `npx playwright install chromium-headless-shell`"
                    .into(),
            ),
        }
    }

    fn launch(&self, spec: &LaunchSpec) -> Result<Box<dyn SandboxInstance>, SandboxError> {
        let binary = self.binary().ok_or_else(|| {
            SandboxError::new(proto::BACKEND_UNAVAILABLE, self.availability().unwrap_err())
        })?;
        GhostInstance::launch(binary, spec).map(|instance| Box::new(instance) as _)
    }
}

/// `DIRI_SANDBOX_BROWSER`, then Playwright's headless shell (newest), then
/// an installed Chrome or Chromium.
pub fn find_browser() -> Option<BrowserBinary> {
    if let Some(path) = std::env::var_os("DIRI_SANDBOX_BROWSER").map(PathBuf::from) {
        return path.is_file().then(|| BrowserBinary {
            headless_shell: path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("headless")),
            path,
        });
    }
    if let Some(path) = playwright_headless_shell() {
        return Some(BrowserBinary {
            path,
            headless_shell: true,
        });
    }
    installed_chrome().map(|path| BrowserBinary {
        path,
        headless_shell: false,
    })
}

fn playwright_cache() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH").map(PathBuf::from)
        && path.is_absolute()
    {
        return Some(path);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    if cfg!(target_os = "macos") {
        Some(home.join("Library/Caches/ms-playwright"))
    } else {
        Some(home.join(".cache/ms-playwright"))
    }
}

fn playwright_headless_shell() -> Option<PathBuf> {
    let cache = playwright_cache()?;
    let mut builds: Vec<(u32, PathBuf)> = std::fs::read_dir(&cache)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let build = name
                .strip_prefix("chromium_headless_shell-")?
                .parse()
                .ok()?;
            Some((build, entry.path()))
        })
        .collect();
    builds.sort_by_key(|(build, _)| std::cmp::Reverse(*build));
    let platform = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "chrome-headless-shell-mac-arm64"
    } else if cfg!(target_os = "macos") {
        "chrome-headless-shell-mac-x64"
    } else if cfg!(target_arch = "aarch64") {
        "chrome-headless-shell-linux-arm64"
    } else {
        "chrome-headless-shell-linux64"
    };
    builds
        .into_iter()
        .map(|(_, dir)| dir.join(platform).join("chrome-headless-shell"))
        .find(|path| path.is_file())
}

fn installed_chrome() -> Option<PathBuf> {
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        ]
    } else {
        &[
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/snap/bin/chromium",
        ]
    };
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

/// State the reader thread shares with the instance and the preview.
struct Wire {
    writer: Mutex<Option<File>>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>,
    dead: AtomicBool,
    loads: Mutex<u64>,
    loaded: Condvar,
    frame: Mutex<Option<PreviewFrame>>,
    frame_seq: AtomicU64,
    /// The page's CDP session, once attached.
    session: OnceLock<String>,
    /// Until when someone wants preview frames, and whether they flow.
    preview_wanted_until: Mutex<Option<Instant>>,
    casting: AtomicBool,
}

impl Wire {
    fn send(&self, method: &str, params: Value, on_page: bool) -> Result<u64, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({"id": id, "method": method, "params": params});
        if on_page {
            let session = self
                .session
                .get()
                .ok_or_else(|| "page not attached".to_owned())?;
            message["sessionId"] = json!(session);
        }
        let mut bytes = serde_json::to_vec(&message).map_err(|e| e.to_string())?;
        bytes.push(0);
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let file = writer.as_mut().ok_or_else(|| "browser closed".to_owned())?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        Ok(id)
    }

    fn call(
        &self,
        method: &str,
        params: Value,
        on_page: bool,
        timeout: Duration,
    ) -> Result<Value, String> {
        if self.dead.load(Ordering::Acquire) {
            return Err("browser exited".into());
        }
        let (tx, rx) = mpsc::channel();
        // Register before sending: the answer can beat the insert.
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        let mut message = json!({"id": id, "method": method, "params": params});
        if on_page {
            match self.session.get() {
                Some(session) => message["sessionId"] = json!(session),
                None => {
                    self.forget(id);
                    return Err("page not attached".into());
                }
            }
        }
        let written = serde_json::to_vec(&message)
            .map_err(|e| e.to_string())
            .and_then(|mut bytes| {
                bytes.push(0);
                let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
                let file = writer.as_mut().ok_or_else(|| "browser closed".to_owned())?;
                file.write_all(&bytes).map_err(|e| e.to_string())
            });
        if let Err(error) = written {
            self.forget(id);
            return Err(error);
        }
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(_) => {
                self.forget(id);
                Err(format!(
                    "{method} timed out after {} ms",
                    timeout.as_millis()
                ))
            }
        }
    }

    fn forget(&self, id: u64) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }

    fn load_count(&self) -> u64 {
        *self.loads.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait_for_load(&self, after: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut loads = self.loads.lock().unwrap_or_else(|e| e.into_inner());
        while *loads <= after {
            if self.dead.load(Ordering::Acquire) {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            loads = self
                .loaded
                .wait_timeout(loads, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }

    fn close(&self) {
        self.writer.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

impl FrameSource for Wire {
    fn latest(&self) -> Option<PreviewFrame> {
        self.frame.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Starts the screencast now if it isn't running, so the first tile
    /// paints without waiting for the reaper's beat.
    fn want(&self) {
        *self
            .preview_wanted_until
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now() + PREVIEW_LINGER);
        if !self.casting.swap(true, Ordering::AcqRel)
            && self
                .send(
                    "Page.startScreencast",
                    json!({
                        "format": "jpeg",
                        "quality": 70,
                        "maxWidth": PREVIEW_MAX_WIDTH,
                        "maxHeight": PREVIEW_MAX_HEIGHT,
                        "everyNthFrame": 1
                    }),
                    true,
                )
                .is_err()
        {
            self.casting.store(false, Ordering::Release);
        }
    }
}

pub struct GhostInstance {
    child: Child,
    wire: Arc<Wire>,
    reader: Option<JoinHandle<()>>,
    width: u32,
    height: u32,
    url: String,
    title: String,
}

impl GhostInstance {
    fn launch(binary: &BrowserBinary, spec: &LaunchSpec) -> Result<Self, SandboxError> {
        let launch_error = |what: &str, error: &dyn std::fmt::Display| {
            SandboxError::new(proto::BACKEND_UNAVAILABLE, format!("{what}: {error}"))
        };
        // Two pipes: we write commands into `to_browser`, it writes answers
        // into `from_browser`. All four ends are close-on-exec here; the
        // child re-numbers its two onto fds 3 and 4.
        let (to_browser_read, to_browser_write) =
            cloexec_pipe().map_err(|e| launch_error("pipe", &e))?;
        let (from_browser_read, from_browser_write) =
            cloexec_pipe().map_err(|e| launch_error("pipe", &e))?;
        let mut command = Command::new(&binary.path);
        command
            .args(browser_args(binary, spec))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_remove("DIRIJOR_SESSION_ID");
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            use std::os::unix::process::CommandExt as _;
            let child_read = to_browser_read.as_raw_fd();
            let child_write = from_browser_write.as_raw_fd();
            // Async-signal-safe only: fcntl, dup2, close, setpgid.
            unsafe {
                command.pre_exec(move || {
                    let read = libc::fcntl(child_read, libc::F_DUPFD, 10);
                    let write = libc::fcntl(child_write, libc::F_DUPFD, 10);
                    if read < 0 || write < 0 || libc::dup2(read, 3) < 0 || libc::dup2(write, 4) < 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::close(read);
                    libc::close(write);
                    // Its own group, so shutdown reaches every helper.
                    libc::setpgid(0, 0);
                    Ok(())
                });
            }
        }
        let child = command
            .spawn()
            .map_err(|e| launch_error(&binary.path.display().to_string(), &e))?;
        // Our copies of the child's ends must close, or EOF never comes.
        drop(to_browser_read);
        drop(from_browser_write);
        let wire = Arc::new(Wire {
            writer: Mutex::new(Some(File::from(to_browser_write))),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
            loads: Mutex::new(0),
            loaded: Condvar::new(),
            frame: Mutex::new(None),
            frame_seq: AtomicU64::new(0),
            session: OnceLock::new(),
            preview_wanted_until: Mutex::new(None),
            casting: AtomicBool::new(false),
        });
        let reader = {
            let wire = Arc::clone(&wire);
            let file = File::from(from_browser_read);
            std::thread::Builder::new()
                .name(format!("diri-sandbox-{}", spec.id))
                .spawn(move || read_loop(file, &wire))
                .map_err(|e| launch_error("reader thread", &e))?
        };
        let mut instance = Self {
            child,
            wire,
            reader: Some(reader),
            width: spec.width,
            height: spec.height,
            url: "about:blank".into(),
            title: String::new(),
        };
        if let Err(error) = instance.attach() {
            instance.shutdown();
            return Err(SandboxError::new(
                proto::BACKEND_UNAVAILABLE,
                format!("browser did not start: {error}"),
            ));
        }
        Ok(instance)
    }

    /// Attaches to the browser's first page, or opens one.
    fn attach(&mut self) -> Result<(), String> {
        let targets = self
            .wire
            .call("Target.getTargets", json!({}), false, LAUNCH_TIMEOUT)?;
        let existing = targets["targetInfos"].as_array().and_then(|targets| {
            targets
                .iter()
                .find(|target| target["type"] == "page")
                .and_then(|target| target["targetId"].as_str().map(str::to_owned))
        });
        let target = match existing {
            Some(target) => target,
            None => self.wire.call(
                "Target.createTarget",
                json!({"url": "about:blank"}),
                false,
                LAUNCH_TIMEOUT,
            )?["targetId"]
                .as_str()
                .ok_or("no target id")?
                .to_owned(),
        };
        let attached = self.wire.call(
            "Target.attachToTarget",
            json!({"targetId": target, "flatten": true}),
            false,
            LAUNCH_TIMEOUT,
        )?;
        let session = attached["sessionId"]
            .as_str()
            .ok_or("no session id")?
            .to_owned();
        let _ = self.wire.session.set(session);
        self.wire
            .call("Page.enable", json!({}), true, LAUNCH_TIMEOUT)?;
        self.set_viewport(self.width, self.height)?;
        Ok(())
    }

    fn set_viewport(&mut self, width: u32, height: u32) -> Result<(), String> {
        self.wire.call(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": width, "height": height, "deviceScaleFactor": 1, "mobile": false}),
            true,
            COMMAND_TIMEOUT,
        )?;
        self.width = width;
        self.height = height;
        Ok(())
    }

    fn page(&self, method: &str, params: Value) -> Result<Value, SandboxError> {
        self.wire
            .call(method, params, true, COMMAND_TIMEOUT)
            .map_err(|error| self.failure(error))
    }

    fn failure(&self, error: String) -> SandboxError {
        if self.wire.dead.load(Ordering::Acquire) {
            SandboxError::new(proto::EXITED, "the sandbox's browser exited")
        } else {
            SandboxError::failed(error)
        }
    }

    fn evaluate(&self, expression: &str, timeout: Duration) -> Result<Value, SandboxError> {
        let result = self
            .wire
            .call(
                "Runtime.evaluate",
                json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
                true,
                timeout,
            )
            .map_err(|error| self.failure(error))?;
        if let Some(details) = result.get("exceptionDetails") {
            let text = details["exception"]["description"]
                .as_str()
                .or_else(|| details["text"].as_str())
                .unwrap_or("exception");
            return Err(SandboxError::failed(format!("script threw: {text}")));
        }
        Ok(result["result"]["value"].clone())
    }

    fn mouse(&self, kind: &str, x: f64, y: f64, extra: Value) -> Result<(), SandboxError> {
        let mut params = json!({"type": kind, "x": x, "y": y});
        if let (Some(params), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            params.extend(extra.clone());
        }
        self.page("Input.dispatchMouseEvent", params).map(|_| ())
    }

    fn click_point(&self, x: f64, y: f64) -> Result<(), SandboxError> {
        self.mouse("mouseMoved", x, y, json!({}))?;
        self.mouse(
            "mousePressed",
            x,
            y,
            json!({"button": "left", "buttons": 1, "clickCount": 1}),
        )?;
        self.mouse(
            "mouseReleased",
            x,
            y,
            json!({"button": "left", "buttons": 0, "clickCount": 1}),
        )
    }

    fn element_center(&self, selector: &str) -> Result<(f64, f64), SandboxError> {
        let literal = serde_json::to_string(selector).unwrap_or_default();
        let point = self.evaluate(
            &format!(
                "(() => {{ const el = document.querySelector({literal}); if (!el) return null; \
                 el.scrollIntoView({{block: 'center', inline: 'center'}}); \
                 const r = el.getBoundingClientRect(); \
                 return [r.left + r.width / 2, r.top + r.height / 2]; }})()"
            ),
            COMMAND_TIMEOUT,
        )?;
        match (point[0].as_f64(), point[1].as_f64()) {
            (Some(x), Some(y)) => Ok((x, y)),
            _ => Err(SandboxError::failed(format!(
                "no element matches {selector}"
            ))),
        }
    }

    fn press(&self, key: &str) -> Result<(), SandboxError> {
        let (code, vk, text) =
            key_facts(key).ok_or_else(|| SandboxError::failed(format!("unknown key {key}")))?;
        let mut down = json!({
            "type": if text.is_some() { "keyDown" } else { "rawKeyDown" },
            "key": key_name(key),
            "code": code,
            "windowsVirtualKeyCode": vk,
            "nativeVirtualKeyCode": vk,
        });
        if let Some(text) = &text {
            down["text"] = json!(text);
            down["unmodifiedText"] = json!(text);
        }
        self.page("Input.dispatchKeyEvent", down)?;
        self.page(
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyUp",
                "key": key_name(key),
                "code": code,
                "windowsVirtualKeyCode": vk,
                "nativeVirtualKeyCode": vk,
            }),
        )
        .map(|_| ())
    }

    fn navigate(&mut self, url: &str) -> Result<Value, SandboxError> {
        let url = normalize_url(url)?;
        let before = self.wire.load_count();
        let started = Instant::now();
        let result = self.page("Page.navigate", json!({"url": url}))?;
        if let Some(error) = result["errorText"].as_str().filter(|e| !e.is_empty()) {
            return Err(SandboxError::failed(format!(
                "could not load {url}: {error}"
            )));
        }
        // Same-document navigations (a #hash) have no loader and no load.
        let loaded = result.get("loaderId").is_none_or(Value::is_null)
            || self.wire.wait_for_load(before, NAVIGATE_TIMEOUT);
        let (url, title) = self.refresh_location();
        Ok(json!({
            "url": url,
            "title": title,
            "loaded": loaded,
            "ms": started.elapsed().as_millis() as u64,
        }))
    }

    fn refresh_location(&mut self) -> (String, String) {
        if let Ok(value) = self.evaluate("[location.href, document.title]", Duration::from_secs(2))
        {
            if let Some(url) = value[0].as_str() {
                self.url = url.to_owned();
            }
            if let Some(title) = value[1].as_str() {
                self.title = title.to_owned();
            }
        }
        (self.url.clone(), self.title.clone())
    }

    fn capture(&self, format: &str, max_edge: Option<u32>) -> Result<Vec<u8>, SandboxError> {
        let mut params = json!({"format": format});
        if format == "jpeg" {
            params["quality"] = json!(75);
        }
        if let Some(max_edge) = max_edge {
            let longest = self.width.max(self.height).max(1);
            let scale = (f64::from(max_edge) / f64::from(longest)).min(1.0);
            params["clip"] = json!({
                "x": 0, "y": 0,
                "width": self.width, "height": self.height,
                "scale": scale,
            });
        }
        let result = self.page("Page.captureScreenshot", params)?;
        let data = result["data"]
            .as_str()
            .ok_or_else(|| SandboxError::failed("screenshot carried no data"))?;
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|error| SandboxError::failed(format!("screenshot: {error}")))
    }
}

impl GhostInstance {
    fn step(&mut self, action: &SandboxAction) -> Result<Value, SandboxError> {
        match action {
            SandboxAction::Navigate { url } => self.navigate(url),
            SandboxAction::Click { x, y, selector } => {
                let (x, y) = match (x, y, selector) {
                    (_, _, Some(selector)) => self.element_center(selector)?,
                    (Some(x), Some(y), None) => (*x, *y),
                    _ => {
                        return Err(SandboxError::failed("click needs x and y, or a selector"));
                    }
                };
                self.click_point(x, y)?;
                Ok(json!({"x": x, "y": y}))
            }
            SandboxAction::Type { text } => {
                self.page("Input.insertText", json!({"text": text}))?;
                Ok(proto::ok())
            }
            SandboxAction::Press { key } => {
                self.press(key)?;
                Ok(proto::ok())
            }
            SandboxAction::Scroll { x, y, dx, dy } => {
                self.mouse("mouseWheel", *x, *y, json!({"deltaX": dx, "deltaY": dy}))?;
                Ok(proto::ok())
            }
            SandboxAction::Eval { expression } => {
                Ok(json!({"value": self.evaluate(expression, COMMAND_TIMEOUT)?}))
            }
            SandboxAction::WaitFor {
                selector,
                timeout_ms,
            } => {
                let timeout =
                    Duration::from_millis(timeout_ms.unwrap_or(5_000).min(proto::MAX_WAIT_MS));
                let literal = serde_json::to_string(selector).unwrap_or_default();
                let deadline = Instant::now() + timeout;
                loop {
                    let found = self.evaluate(
                        &format!("!!document.querySelector({literal})"),
                        COMMAND_TIMEOUT,
                    )?;
                    if found == Value::Bool(true) {
                        return Ok(json!({"found": true}));
                    }
                    if Instant::now() >= deadline {
                        return Err(SandboxError::failed(format!(
                            "{selector} did not appear within {} ms",
                            timeout.as_millis()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            SandboxAction::Wait { ms } => {
                std::thread::sleep(Duration::from_millis((*ms).min(proto::MAX_WAIT_MS)));
                Ok(proto::ok())
            }
            SandboxAction::Resize { width, height } => {
                let width = proto::clamp_dimension(Some(*width), proto::DEFAULT_WIDTH);
                let height = proto::clamp_dimension(Some(*height), proto::DEFAULT_HEIGHT);
                self.set_viewport(width, height)
                    .map_err(|error| self.failure(error))?;
                Ok(json!({"width": width, "height": height}))
            }
        }
    }
}

impl SandboxInstance for GhostInstance {
    fn act(&mut self, action: &SandboxAction) -> Result<Value, SandboxError> {
        let result = self.step(action);
        // A click or key can navigate; keep the listing's location current
        // so `location` never has to ask the page.
        if result.is_ok()
            && matches!(
                action,
                SandboxAction::Click { .. }
                    | SandboxAction::Press { .. }
                    | SandboxAction::Eval { .. }
            )
        {
            self.refresh_location();
        }
        result
    }

    fn screenshot_png(&mut self) -> Result<Vec<u8>, SandboxError> {
        self.capture("png", None)
    }

    fn screenshot_jpeg(&mut self, max_edge: u32) -> Result<Vec<u8>, SandboxError> {
        self.capture("jpeg", Some(max_edge))
    }

    fn frames(&self) -> Option<Arc<dyn FrameSource>> {
        Some(Arc::clone(&self.wire) as Arc<dyn FrameSource>)
    }

    fn location(&mut self) -> (String, String) {
        (self.url.clone(), self.title.clone())
    }

    fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }

    fn trim(&mut self) {
        // Fire and forget: the renderer drops caches and collects garbage.
        let _ = self.wire.send(
            "Memory.simulatePressureNotification",
            json!({"level": "critical"}),
            false,
        );
        let _ = self
            .wire
            .send("HeapProfiler.collectGarbage", json!({}), true);
    }

    fn tick(&mut self) {
        let wanted = self
            .wire
            .preview_wanted_until
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|until| until > Instant::now());
        if !wanted && self.wire.casting.swap(false, Ordering::AcqRel) {
            let _ = self.wire.send("Page.stopScreencast", json!({}), true);
        }
    }

    fn alive(&mut self) -> bool {
        !self.wire.dead.load(Ordering::Acquire) && matches!(self.child.try_wait(), Ok(None))
    }

    fn shutdown(&mut self) {
        let _ = self.wire.send("Browser.close", json!({}), false);
        // EOF on its pipe makes the browser exit; give it a moment.
        self.wire.close();
        let deadline = Instant::now() + Duration::from_millis(1_500);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        #[cfg(unix)]
        {
            // Helpers share its process group; take them all.
            let pgid = self.child.id() as i32;
            if pgid > 0 {
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for GhostInstance {
    fn drop(&mut self) {
        if self.reader.is_some() {
            self.shutdown();
        }
    }
}

fn browser_args(binary: &BrowserBinary, spec: &LaunchSpec) -> Vec<String> {
    let mut args =
        vec![
        if binary.headless_shell {
            "--headless".to_owned()
        } else {
            "--headless=new".to_owned()
        },
        "--remote-debugging-pipe".into(),
        format!("--user-data-dir={}", spec.state_dir.display()),
        format!("--window-size={},{}", spec.width, spec.height),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-extensions".into(),
        "--disable-background-networking".into(),
        "--disable-component-update".into(),
        "--disable-default-apps".into(),
        "--disable-sync".into(),
        "--disable-breakpad".into(),
        "--disable-crash-reporter".into(),
        "--metrics-recording-only".into(),
        "--mute-audio".into(),
        "--hide-scrollbars".into(),
        // Never prompt for, or touch, the person's keychain.
        "--use-mock-keychain".into(),
        "--password-store=basic".into(),
        // Small private state: the HTTP cache stays tiny and in the
        // sandbox's own directory.
        "--disk-cache-size=1048576".into(),
        format!("--disk-cache-dir={}", spec.state_dir.join("cache").display()),
        "--disable-features=Translate,MediaRouter,OptimizationHints,AutofillServerCommunication,\
         CalculateNativeWinOcclusion,DialMediaRouteProvider"
            .into(),
    ];
    if std::env::var_os("DIRI_SANDBOX_SINGLE_PROCESS").is_some() {
        args.push("--single-process".into());
    }
    args.push("about:blank".into());
    args
}

/// Reads NUL-delimited CDP messages until EOF, routing answers to their
/// callers and handling the few events the sandbox needs.
fn read_loop(mut file: File, wire: &Wire) {
    let mut buffer: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut chunk = vec![0_u8; 64 * 1024];
    'read: loop {
        let read = match file.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let mut scan_from = buffer.len();
        buffer.extend_from_slice(&chunk[..read]);
        while let Some(end) = buffer[scan_from..].iter().position(|b| *b == 0) {
            let end = scan_from + end;
            if let Ok(message) = serde_json::from_slice::<Value>(&buffer[..end]) {
                dispatch(wire, message);
            }
            buffer.drain(..=end);
            scan_from = 0;
        }
        if buffer.len() > MAX_MESSAGE_BYTES {
            break 'read;
        }
    }
    wire.dead.store(true, Ordering::Release);
    wire.close();
    let pending: Vec<_> = wire
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain()
        .collect();
    for (_, sender) in pending {
        let _ = sender.send(Err("browser exited".into()));
    }
    wire.loaded.notify_all();
}

fn dispatch(wire: &Wire, message: Value) {
    if let Some(id) = message.get("id").and_then(Value::as_u64) {
        let sender = wire
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        if let Some(sender) = sender {
            let result = match message.get("error") {
                Some(error) => Err(error["message"]
                    .as_str()
                    .unwrap_or("protocol error")
                    .to_owned()),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = sender.send(result);
        }
        return;
    }
    match message["method"].as_str() {
        Some("Page.loadEventFired") => {
            *wire.loads.lock().unwrap_or_else(|e| e.into_inner()) += 1;
            wire.loaded.notify_all();
        }
        Some("Page.screencastFrame") => {
            let params = &message["params"];
            if let Some(ack) = params["sessionId"].as_u64() {
                let _ = wire.send("Page.screencastFrameAck", json!({"sessionId": ack}), true);
            }
            if let Some(Ok(jpeg)) = params["data"]
                .as_str()
                .map(|data| base64::engine::general_purpose::STANDARD.decode(data))
            {
                let (width, height) = jpeg_dimensions(&jpeg).unwrap_or((0, 0));
                let seq = wire.frame_seq.fetch_add(1, Ordering::Relaxed) + 1;
                *wire.frame.lock().unwrap_or_else(|e| e.into_inner()) = Some(PreviewFrame {
                    seq,
                    width,
                    height,
                    jpeg: Arc::new(jpeg),
                });
            }
        }
        // A dialog would block the page (and every later call) forever.
        Some("Page.javascriptDialogOpening") => {
            let _ = wire.send("Page.handleJavaScriptDialog", json!({"accept": true}), true);
        }
        _ => {}
    }
}

/// Width and height from a baseline or progressive JPEG's SOF marker.
pub fn jpeg_dimensions(jpeg: &[u8]) -> Option<(u32, u32)> {
    let mut index = 2;
    while index + 9 < jpeg.len() {
        if jpeg[index] != 0xFF {
            index += 1;
            continue;
        }
        let marker = jpeg[index + 1];
        let length = usize::from(u16::from_be_bytes([jpeg[index + 2], jpeg[index + 3]]));
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            let height = u16::from_be_bytes([jpeg[index + 5], jpeg[index + 6]]);
            let width = u16::from_be_bytes([jpeg[index + 7], jpeg[index + 8]]);
            return Some((u32::from(width), u32::from(height)));
        }
        index += 2 + length;
    }
    None
}

/// `localhost:3000` → `http://localhost:3000`; only web and file URLs.
fn normalize_url(url: &str) -> Result<String, SandboxError> {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    if ["http://", "https://", "file://", "about:", "data:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
    {
        return Ok(url.to_owned());
    }
    if lower.contains("://") || lower.starts_with("javascript:") {
        return Err(SandboxError::failed(format!(
            "unsupported URL scheme in {url}"
        )));
    }
    let local =
        lower.starts_with("localhost") || lower.starts_with("127.") || lower.starts_with("[::1]");
    Ok(format!(
        "{}{url}",
        if local { "http://" } else { "https://" }
    ))
}

fn key_name(key: &str) -> &str {
    if key.eq_ignore_ascii_case("space") {
        " "
    } else {
        key
    }
}

/// DOM `code`, Windows virtual key code, and the text a key types.
fn key_facts(key: &str) -> Option<(String, u32, Option<String>)> {
    let named = |code: &str, vk: u32, text: Option<&str>| {
        Some((code.to_owned(), vk, text.map(str::to_owned)))
    };
    match key {
        "Enter" => named("Enter", 13, Some("\r")),
        "Tab" => named("Tab", 9, None),
        "Escape" => named("Escape", 27, None),
        "Backspace" => named("Backspace", 8, None),
        "Delete" => named("Delete", 46, None),
        "ArrowLeft" => named("ArrowLeft", 37, None),
        "ArrowUp" => named("ArrowUp", 38, None),
        "ArrowRight" => named("ArrowRight", 39, None),
        "ArrowDown" => named("ArrowDown", 40, None),
        "Home" => named("Home", 36, None),
        "End" => named("End", 35, None),
        "PageUp" => named("PageUp", 33, None),
        "PageDown" => named("PageDown", 34, None),
        " " | "Space" | "space" => named("Space", 32, Some(" ")),
        _ => {
            let mut chars = key.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            let upper = c.to_ascii_uppercase();
            let (code, vk) = if c.is_ascii_alphabetic() {
                (format!("Key{upper}"), upper as u32)
            } else if c.is_ascii_digit() {
                (format!("Digit{c}"), c as u32)
            } else {
                (String::new(), 0)
            };
            Some((code, vk, Some(c.to_string())))
        }
    }
}

#[cfg(unix)]
fn cloexec_pipe() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::FromRawFd as _;
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let owned = unsafe {
        (
            std::os::fd::OwnedFd::from_raw_fd(fds[0]),
            std::os::fd::OwnedFd::from_raw_fd(fds[1]),
        )
    };
    for fd in fds {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(owned)
}

/// Used by tests and the benchmark to name the browser they ran.
pub fn describe(binary: &BrowserBinary) -> String {
    format!(
        "{} {}",
        if binary.headless_shell {
            "chrome-headless-shell"
        } else {
            "Chrome --headless=new"
        },
        Path::new(&binary.path)
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_normalize_and_refuse_odd_schemes() {
        assert_eq!(
            normalize_url("localhost:3000/x").unwrap(),
            "http://localhost:3000/x"
        );
        assert_eq!(normalize_url("example.com").unwrap(), "https://example.com");
        assert_eq!(
            normalize_url("data:text/html,hi").unwrap(),
            "data:text/html,hi"
        );
        assert!(normalize_url("javascript:alert(1)").is_err());
        assert!(normalize_url("chrome://settings").is_err());
    }

    #[test]
    fn keys_map_to_codes() {
        assert_eq!(key_facts("Enter").unwrap().1, 13);
        assert_eq!(key_facts("a").unwrap().0, "KeyA");
        assert_eq!(key_facts("7").unwrap().0, "Digit7");
        assert!(key_facts("NotAKey").is_none());
    }

    #[test]
    fn jpeg_size_is_read_from_the_frame_header() {
        // SOI, then a SOF0 segment: length 17, precision 8, 300 x 400.
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0x2C, 0x01, 0x90, 0x03, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0,
        ];
        assert_eq!(jpeg_dimensions(&jpeg), Some((400, 300)));
    }
}
