//! Runs one script on its own thread.
//!
//! Everything reaches the script as an [`Input`] on one channel: Engine
//! events (from a reader thread that exists only while a script listens to
//! them), the window's own events, key presses, palette commands, reloads
//! and REPL lines. Timers are the channel's receive timeout. One thread,
//! one Lua state, so callbacks never race each other and never run on the UI
//! thread or near a PTY.
//!
//! A reload builds a fresh runtime and swaps it in only when the file loads
//! cleanly; a broken edit leaves the last good script running.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::engine::{self, EngineClient};
use crate::events::{self, Deriver};
use crate::{Host, Layer, Level, LoadError, Options, Runtime};

/// Edits often land as several writes; a reload waits this long for quiet.
const RELOAD_SETTLE: Duration = Duration::from_millis(120);
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

pub enum Input {
    /// An event from the Engine's stream.
    Engine {
        name: String,
        params: Value,
    },
    /// The sessions that existed when the event stream started.
    Seed(Vec<Value>),
    /// The event stream could not be opened before it ever worked.
    EventsUnavailable(String),
    /// An event only the window sees (`focus`).
    App {
        name: String,
        payload: Value,
    },
    /// A key bound to a script function, by binding id.
    Binding(u64),
    /// A script palette command, by id.
    Command(u64),
    /// The script file changed.
    Reload,
    /// A REPL line; the answer goes back on `reply`.
    Eval {
        code: String,
        reply: Sender<Result<String, String>>,
    },
    Stop,
}

/// What the script runs.
#[derive(Clone, Debug)]
pub enum Source {
    File(PathBuf),
    /// Inline code (`dirijor script -e`, or empty for a REPL).
    Code(String),
}

/// Where a driver reports what the window needs to know.
pub trait Sink {
    /// The script's keys and commands, after every load and change.
    fn layer(&mut self, layer: Layer);
    /// A load or reload finished. `first` is the first load.
    fn loaded(&mut self, result: &Result<(), LoadError>, first: bool);
}

/// Why [`Driver::run`] returned.
#[derive(Debug, PartialEq, Eq)]
pub enum Exit {
    /// `diri.stop()`, or [`Input::Stop`].
    Stopped,
    /// Nothing left to wait for (no handlers or timers), in run-once mode.
    Idle,
    /// The `until` deadline passed.
    Deadline,
    /// Every sender is gone.
    Disconnected,
}

pub struct Config {
    pub source: Source,
    pub options: Options,
    /// The Engine socket the event reader connects to.
    pub socket: PathBuf,
    /// The build named in the reader's `hello`.
    pub build: String,
    /// Return once nothing is left to wait for (`dirijor script`). The app
    /// keeps its script for the whole session.
    pub exit_when_idle: bool,
}

pub struct Driver {
    config: Config,
    make_host: Box<dyn FnMut() -> Box<dyn Host>>,
    sink: Box<dyn Sink>,
    runtime: Option<Runtime>,
    deriver: Deriver,
    reader_started: bool,
    tx: Sender<Input>,
    loads: usize,
}

impl Driver {
    /// `tx` is the sending side of the channel [`Driver::run`] reads; the
    /// Engine event reader uses it.
    pub fn new(
        config: Config,
        make_host: Box<dyn FnMut() -> Box<dyn Host>>,
        sink: Box<dyn Sink>,
        tx: Sender<Input>,
    ) -> Self {
        Self {
            config,
            make_host,
            sink,
            runtime: None,
            deriver: Deriver::default(),
            reader_started: false,
            tx,
            loads: 0,
        }
    }

    /// Loads (or reloads) the script. The previous runtime keeps running
    /// when the new one fails to load.
    pub fn load(&mut self) -> Result<(), LoadError> {
        let first = self.loads == 0;
        self.loads += 1;
        let result = self.build();
        match result {
            Ok(runtime) => {
                self.sink.layer(runtime.layer());
                // Drop the flag the load itself set.
                let _ = runtime.take_layer_change();
                self.runtime = Some(runtime);
                self.ensure_reader();
                self.sink.loaded(&Ok(()), first);
                Ok(())
            }
            Err(error) => {
                self.sink.loaded(&Err(error.clone()), first);
                Err(error)
            }
        }
    }

    fn build(&mut self) -> Result<Runtime, LoadError> {
        let mut options = self.config.options.clone();
        if let Source::File(path) = &self.config.source
            && options.root.is_none()
        {
            options.root = std::path::absolute(path)
                .ok()
                .and_then(|path| path.parent().map(PathBuf::from));
        }
        let runtime = Runtime::new((self.make_host)(), options)?;
        match &self.config.source {
            Source::File(path) => runtime.load_file(path)?,
            Source::Code(code) => runtime.load_source(code, "(command line)")?,
        }
        Ok(runtime)
    }

    pub fn runtime(&self) -> Option<&Runtime> {
        self.runtime.as_ref()
    }

    /// Starts the Engine event reader the first time a script listens to an
    /// Engine event. It then lives as long as the driver.
    fn ensure_reader(&mut self) {
        if self.reader_started
            || !self
                .runtime
                .as_ref()
                .is_some_and(Runtime::wants_engine_events)
        {
            return;
        }
        self.reader_started = true;
        let tx = self.tx.clone();
        let socket = self.config.socket.clone();
        let build = self.config.build.clone();
        let spawned = std::thread::Builder::new()
            .name("diri-script-events".into())
            .spawn(move || read_events(&socket, &build, &tx));
        if let Err(error) = spawned
            && let Some(runtime) = &self.runtime
        {
            runtime.report(
                "diri.on",
                &format!("could not start the event reader: {error}"),
            );
        }
    }

    /// Handles one input. Answers false when the driver should stop.
    pub fn handle(&mut self, input: Input) -> bool {
        match input {
            Input::Stop => return false,
            Input::Reload => {
                let _ = self.load();
            }
            Input::Seed(records) => self.deriver.seed(&records),
            Input::EventsUnavailable(message) => {
                if let Some(runtime) = &self.runtime {
                    runtime.report("diri.on", &message);
                }
            }
            Input::Engine { name, params } => {
                let derived = self.deriver.derive(&name, params);
                if let Some(runtime) = &self.runtime {
                    for (event, payload) in derived {
                        runtime.emit(event, &payload);
                    }
                }
            }
            Input::App { name, payload } => {
                if let Some(runtime) = &self.runtime {
                    runtime.emit(&name, &payload);
                }
            }
            Input::Binding(id) => {
                if let Some(runtime) = &self.runtime {
                    runtime.run_binding(id);
                }
            }
            Input::Command(id) => {
                if let Some(runtime) = &self.runtime {
                    runtime.run_command(id);
                }
            }
            Input::Eval { code, reply } => {
                let answer = match &self.runtime {
                    Some(runtime) => runtime.eval(&code),
                    None => Err("the script did not load".into()),
                };
                let _ = reply.send(answer);
            }
        }
        self.after_input();
        !self.runtime.as_ref().is_some_and(Runtime::stopped)
    }

    fn after_input(&mut self) {
        if let Some(runtime) = &self.runtime
            && let Some(layer) = runtime.take_layer_change()
        {
            self.sink.layer(layer);
        }
        self.ensure_reader();
    }

    /// Reads inputs and fires timers until stopped, idle (in run-once mode),
    /// past `until`, or disconnected.
    pub fn run(&mut self, rx: &Receiver<Input>, until: Option<Instant>) -> Exit {
        loop {
            if let Some(runtime) = &self.runtime {
                runtime.run_timers(Instant::now());
                if runtime.stopped() {
                    return Exit::Stopped;
                }
            }
            self.after_input();
            if self.config.exit_when_idle
                && self
                    .runtime
                    .as_ref()
                    .is_none_or(|runtime| !runtime.has_pending_work())
            {
                return Exit::Idle;
            }
            let now = Instant::now();
            let next = self
                .runtime
                .as_ref()
                .and_then(Runtime::next_timer)
                .into_iter()
                .chain(until)
                .min();
            if until.is_some_and(|until| now >= until) {
                return Exit::Deadline;
            }
            let input = match next {
                Some(next) => match rx.recv_timeout(next.saturating_duration_since(now)) {
                    Ok(input) => input,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return Exit::Disconnected,
                },
                None => match rx.recv() {
                    Ok(input) => input,
                    Err(_) => return Exit::Disconnected,
                },
            };
            let after = if matches!(input, Input::Reload) {
                settle_reloads(rx)
            } else {
                None
            };
            if !self.handle(input) {
                return Exit::Stopped;
            }
            if let Some(after) = after
                && !self.handle(after)
            {
                return Exit::Stopped;
            }
        }
    }
}

/// Folds a burst of reloads into one. Answers the first other input that
/// arrives meanwhile, to handle right after the reload.
fn settle_reloads(rx: &Receiver<Input>) -> Option<Input> {
    let deadline = Instant::now() + RELOAD_SETTLE;
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Input::Reload) => continue,
            Ok(other) => return Some(other),
            Err(_) => return None,
        }
    }
}

/// The Engine event reader: subscribes, seeds the session snapshot, and
/// forwards events until the driver is gone. Reconnects after an Engine
/// restart with backoff.
fn read_events(socket: &std::path::Path, build: &str, tx: &Sender<Input>) {
    let mut backoff = RECONNECT_MIN;
    let closed = std::cell::Cell::new(false);
    let connected = std::cell::Cell::new(false);
    let mut reported = false;
    loop {
        let started = Instant::now();
        let result = engine::subscribe(
            socket,
            build,
            events::ENGINE_KINDS,
            || {
                let mut client = EngineClient::new(socket.to_path_buf(), build);
                let records = client
                    .request(
                        diri_proto::Method::SESSION_LIST,
                        serde_json::json!({}),
                        Duration::from_secs(10),
                    )
                    .ok()
                    .and_then(|list| list.get("sessions").and_then(Value::as_array).cloned())
                    .unwrap_or_default();
                connected.set(true);
                closed.set(tx.send(Input::Seed(records)).is_err());
            },
            |name, params| {
                let sent = !closed.get()
                    && tx
                        .send(Input::Engine {
                            name: name.to_owned(),
                            params,
                        })
                        .is_ok();
                closed.set(!sent);
                sent
            },
        );
        if closed.get() {
            return;
        }
        if let Err(error) = result
            && !connected.get()
            && !reported
        {
            reported = true;
            if tx
                .send(Input::EventsUnavailable(format!(
                    "no events yet, retrying: {}",
                    error.message
                )))
                .is_err()
            {
                return;
            }
        }
        if started.elapsed() > RECONNECT_MAX {
            backoff = RECONNECT_MIN;
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

/// A host for anything outside the app: the Engine socket for requests,
/// stdout for the log and stderr for notices.
pub struct ConsoleHost {
    client: EngineClient,
    context: &'static str,
}

impl ConsoleHost {
    pub fn new(socket: PathBuf, build: &str) -> Self {
        Self {
            client: EngineClient::new(socket, build),
            context: "cli",
        }
    }
}

impl Host for ConsoleHost {
    fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, crate::HostError> {
        self.client.request(method, params, timeout)
    }

    fn notify(&mut self, notice: crate::Notice) {
        if notice.logged {
            // `log` already printed it in full.
            return;
        }
        match notice.detail {
            Some(detail) => eprintln!("{}: {}: {detail}", notice.level.label(), notice.message),
            None => eprintln!("{}: {}", notice.level.label(), notice.message),
        }
    }

    fn log(&mut self, level: Level, message: &str) {
        match level {
            Level::Info | Level::Success => println!("{message}"),
            Level::Warning | Level::Error => eprintln!("{}: {message}", level.label()),
        }
    }

    fn context(&self) -> &'static str {
        self.context
    }
}
