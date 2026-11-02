//! One sandboxed Lua state and the `diri` table it exposes.
//!
//! Sandbox: only the `string`, `table`, `math`, `utf8` and `coroutine`
//! libraries, a reduced `os` (`time`, `clock`, `date`, `difftime`), `load`
//! for text only, and `require` limited to `.lua` files under the script's
//! own directory. There is no `io`, no `os.execute`/`getenv`, no `debug`,
//! no `package`, and no network: everything a script does outside Lua goes
//! through `diri.call`/`diri.action`, the same requests the CLI makes, or
//! through `diri.exec`, which runs only the programs the person granted.
//!
//! Every entry into Lua (loading the file, one event handler, one key, one
//! timer) runs as a coroutine under a wall-clock budget that excludes time
//! spent waiting on the Engine; a callback that runs past it is stopped
//! with an error. `diri.sleep`, `diri.wait` and `diri.exec` suspend that
//! coroutine instead of blocking the thread, and each resumption gets a
//! fresh budget. The state's memory is capped as well.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_proto::app_actions::{AppActionSpec, AppActionsListResult};
use mlua::chunk::ChunkMode;
use mlua::thread::ThreadStatus;
use mlua::{
    Function, HookTriggers, Lua, LuaOptions, LuaSerdeExt, MultiValue, StdLib, Table, Thread,
    Value as LuaValue, VmState,
};
use serde_json::{Value, json};

use crate::{api, events, keys};

mod exec;

/// How a notice reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Success,
    Warning,
    Error,
}

impl Level {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "info" => Some(Self::Info),
            "success" => Some(Self::Success),
            "warn" | "warning" => Some(Self::Warning),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// A message for the person: a toast in the app, stderr from the CLI.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub level: Level,
    pub message: String,
    pub detail: Option<String>,
    /// A script error, already written to the log in full.
    pub logged: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HostError {
    pub code: String,
    pub message: String,
}

impl HostError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

/// What a script asked to be allowed (`diri.require_grant`).
#[derive(Clone, Debug, PartialEq)]
pub struct GrantRequest {
    /// Programs `diri.exec` may start, by the name `argv[1]` uses.
    pub exec: Vec<String>,
    /// The file asking, as errors name it (`init.lua`, `deploy.lua`).
    pub script: String,
    /// SHA-256 of the script's text, hex: an answer stored for one version
    /// of a file is asked again when the file changes.
    pub digest: String,
}

/// What a runtime talks to.
pub trait Host {
    /// One Engine request.
    fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, HostError>;
    fn notify(&mut self, notice: Notice);
    fn log(&mut self, level: Level, message: &str);
    /// `"app"` inside Diri, `"cli"` from `dirijor script`.
    fn context(&self) -> &'static str;
    /// The window's actions, to check names a script binds. The app answers
    /// from its own catalog; elsewhere the Engine is asked.
    fn actions(&mut self) -> Result<Vec<AppActionSpec>, HostError> {
        let value = self.request(
            diri_proto::Method::APP_ACTIONS_LIST,
            json!({}),
            Duration::from_secs(5),
        )?;
        let list: AppActionsListResult = serde_json::from_value(value)
            .map_err(|error| HostError::new("protocol", error.to_string()))?;
        Ok(list.actions)
    }
    /// Whether the person grants `request`. Nothing is granted unless the
    /// host says so: the app grants what `init.lua` (the person's own file)
    /// declares, the CLI asks at the terminal or reads `--allow-exec`.
    fn grant(&mut self, request: &GrantRequest) -> bool {
        let _ = request;
        false
    }
    /// Enters key mode `name`, or leaves the current one (`None`). Only the
    /// window has key modes.
    fn set_mode(&mut self, name: Option<&str>) -> Result<(), HostError> {
        let _ = name;
        Err(HostError::new(
            "app_only",
            "key modes exist only inside the Diri app",
        ))
    }
}

/// Wakes the thread that drives a runtime when a process `diri.exec`
/// started printed or ended.
#[derive(Clone)]
pub struct Waker(pub Arc<dyn Fn() + Send + Sync>);

impl std::fmt::Debug for Waker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Waker")
    }
}

/// The answer to a palette command run by name (`app.perform`): what its
/// function returned, or why it failed.
pub type Reply = Box<dyn FnOnce(Result<Value, String>) + Send>;

#[derive(Clone, Debug)]
pub struct Options {
    /// The directory `require` may load from: the script's own.
    pub root: Option<PathBuf>,
    /// The budget for one handler, key, command or timer.
    pub callback_budget: Duration,
    /// The budget for running the file itself.
    pub load_budget: Duration,
    pub memory_limit: usize,
    /// `diri.args`: what `dirijor script FILE k=v` passed. An object.
    pub args: Value,
    /// `diri.session_id`: the session the script runs in
    /// (`$DIRIJOR_SESSION_ID`), if any.
    pub session_id: Option<String>,
    /// `dirijor script --check`: run the file's top level without calling
    /// the Engine, the window or a process. The first such call ends the
    /// check there (see [`Summary::stopped_at`]).
    pub check: bool,
    /// Lets `diri.exec` suspend a coroutine instead of blocking: without it
    /// a process runs to the end inside the call.
    pub waker: Option<Waker>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            root: None,
            callback_budget: Duration::from_millis(100),
            load_budget: Duration::from_secs(1),
            memory_limit: 64 * 1024 * 1024,
            args: json!({}),
            session_id: None,
            check: false,
            waker: None,
        }
    }
}

/// What a key or palette command runs.
#[derive(Clone, Debug, PartialEq)]
pub enum BindingTarget {
    /// A window action, by name.
    Action { name: String, args: Value },
    /// A Lua function; the host sends the id back to [`Runtime::run_binding`]
    /// or [`Runtime::run_command`].
    Callback,
    /// Enter a key mode (`{mode = "resize"}`), or leave the current one
    /// (`{mode = false}`). The window handles it without the script thread.
    Mode { name: Option<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    pub id: u64,
    pub keys: String,
    pub target: BindingTarget,
    pub description: Option<String>,
    /// The key mode this binding belongs to (`"resize/h"`), or `None` for
    /// the normal keymap.
    pub mode: Option<String>,
    /// Where the script bound it: `init.lua:12`.
    pub source: Option<String>,
}

/// A key mode: while it is on, the window reads keys from its bindings
/// first. `exclusive` swallows every key the mode does not bind; `once`
/// leaves the mode after the next key. Escape leaves any mode that does not
/// bind it.
#[derive(Clone, Debug, PartialEq)]
pub struct Mode {
    pub name: String,
    pub exclusive: bool,
    pub once: bool,
    pub description: Option<String>,
    pub source: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    pub id: u64,
    pub name: String,
    pub title: String,
    pub target: BindingTarget,
    pub description: Option<String>,
    /// Published with `risk = destructive`, so Agents need the person's
    /// rules to run it.
    pub destructive: bool,
    pub source: Option<String>,
}

/// The prefix script commands are published under as window actions:
/// `diri.command{name = "tidy"}` is `dirijor do script.tidy`.
pub const COMMAND_PREFIX: &str = "script.";

/// The keys and palette commands a script added: one layer over the app's
/// keymap, under the shortcuts the person records in Settings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layer {
    pub bindings: Vec<Binding>,
    /// Keys freed from their default command.
    pub unbinds: Vec<String>,
    pub commands: Vec<Command>,
    pub modes: Vec<Mode>,
}

impl Layer {
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
            && self.unbinds.is_empty()
            && self.commands.is_empty()
            && self.modes.is_empty()
    }

    /// The command published as `action` (`script.tidy`), if any.
    pub fn command_for_action(&self, action: &str) -> Option<&Command> {
        let name = action.strip_prefix(COMMAND_PREFIX)?;
        self.commands.iter().find(|command| command.name == name)
    }
}

/// What a loaded script set up, for `dirijor script --check`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub handlers: usize,
    pub timers: usize,
    pub keys: usize,
    pub modes: usize,
    pub commands: usize,
    /// Programs `diri.require_grant` asked for.
    pub grants_requested: Vec<String>,
    /// In a check, where the top level first called out (`deploy.lua:7`):
    /// the lines after it were compiled but not run.
    pub stopped_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadError {
    pub message: String,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

const MAX_TIMERS: usize = 256;
const MAX_WAITERS: usize = 256;
const MIN_REPEAT: Duration = Duration::from_millis(500);
const MAX_TIMEOUT_SECONDS: f64 = 600.0;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_MODULE_BYTES: u64 = 1024 * 1024;
const CALLBACKS: &str = "diri.callbacks";
/// Coroutines the runtime started: only they may `diri.sleep` or `diri.wait`.
const OWNED: &str = "diri.owned";
/// What `diri.sleep`/`diri.wait`/`diri.exec` yield first, so a plain
/// `coroutine.yield` is told apart.
const TOKEN: &str = "diri.token";
/// A call a `--check` run does not make; it ends the check's top level.
const CHECK_STOP: &str = "diri-check-stop";
/// The same error shown again within this window goes to the log only.
const REPEAT_QUIET: Duration = Duration::from_secs(10);

struct Timer {
    id: u64,
    due: Instant,
    every: Option<Duration>,
}

/// A coroutine suspended in `diri.sleep`, `diri.wait` or `diri.exec`.
struct Waiter {
    thread: Thread,
    what: String,
    reply: Option<Reply>,
    until: Until,
}

enum Until {
    At(Instant),
    Event {
        name: String,
        filter: LuaValue,
        deadline: Option<Instant>,
    },
    Exec(u64),
}

#[derive(Default)]
struct State {
    next_id: u64,
    handlers: Vec<(u64, String)>,
    bindings: Vec<Binding>,
    unbinds: Vec<String>,
    commands: Vec<Command>,
    modes: Vec<Mode>,
    timers: Vec<Timer>,
    waiters: Vec<Waiter>,
    layer_changed: bool,
    stopped: bool,
    actions: Option<Vec<AppActionSpec>>,
    last_notices: HashMap<String, Instant>,
    /// Programs the person granted `diri.exec`.
    granted: Vec<String>,
    grants_requested: Vec<String>,
    /// The loaded text's name and SHA-256, for grant requests.
    script: String,
    digest: String,
    stopped_at: Option<String>,
    exit_code: i32,
    /// `on_output` functions of running processes, by exec id.
    outputs: HashMap<u64, Function>,
}

impl State {
    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

/// The running budget: when the current entry into Lua must end.
#[derive(Default)]
struct Budget {
    deadline: Cell<Option<Instant>>,
    limit: Cell<Duration>,
}

struct Shared {
    host: RefCell<Box<dyn Host>>,
    state: RefCell<State>,
    budget: Budget,
    execs: RefCell<exec::Execs>,
    check: bool,
    callback_budget: Duration,
}

impl Shared {
    /// A host call, with the time it takes added back to the budget: waiting
    /// on the Engine is not the script's CPU.
    fn host<R>(&self, call: impl FnOnce(&mut dyn Host) -> R) -> R {
        let started = Instant::now();
        let result = call(self.host.borrow_mut().as_mut());
        self.not_counted(started);
        result
    }

    /// Adds the time since `started` back to the running budget.
    fn not_counted(&self, started: Instant) {
        if let Some(deadline) = self.budget.deadline.get() {
            self.budget.deadline.set(Some(deadline + started.elapsed()));
        }
    }

    /// In a `--check` run, the error that ends the top level at a call that
    /// would reach outside the script.
    fn check_stop(&self, what: &str) -> Result<(), String> {
        if self.check {
            Err(format!("{CHECK_STOP}: {what} is not run by --check"))
        } else {
            Ok(())
        }
    }

    fn log(&self, level: Level, message: &str) {
        self.host.borrow_mut().log(level, message);
    }
}

/// One loaded script.
pub struct Runtime {
    lua: Lua,
    shared: Rc<Shared>,
    options: Options,
}

/// How one entry into Lua ended.
enum Step {
    Done(MultiValue),
    Wait(Until),
}

impl Runtime {
    pub fn new(host: Box<dyn Host>, options: Options) -> Result<Self, LoadError> {
        let lua = Lua::new_with(
            StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8 | StdLib::COROUTINE,
            LuaOptions::default(),
        )
        .map_err(load_error)?;
        lua.set_memory_limit(options.memory_limit)
            .map_err(load_error)?;
        let shared = Rc::new(Shared {
            host: RefCell::new(host),
            state: RefCell::new(State::default()),
            budget: Budget::default(),
            execs: RefCell::new(exec::Execs::new(options.waker.clone())),
            check: options.check,
            callback_budget: options.callback_budget,
        });
        let runtime = Self {
            lua,
            shared,
            options,
        };
        runtime.install().map_err(load_error)?;
        Ok(runtime)
    }

    /// Runs `path` under the load budget.
    pub fn load_file(&self, path: &Path) -> Result<(), LoadError> {
        let source = std::fs::read_to_string(path).map_err(|error| LoadError {
            message: format!("{}: {error}", path.display()),
        })?;
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "script".into());
        self.load_source(&source, &name)
    }

    /// Runs `source` (named `name` in errors) under the load budget. A top
    /// level that waits (`diri.sleep`, `diri.wait`, `diri.exec`) has loaded;
    /// it goes on from where it waited, and a later error is reported like a
    /// callback's.
    pub fn load_source(&self, source: &str, name: &str) -> Result<(), LoadError> {
        {
            let mut state = self.shared.state.borrow_mut();
            state.script = name.to_owned();
            state.digest = digest(source);
        }
        let function = self
            .lua
            .load(source)
            .set_name(format!("@{name}"))
            .set_mode(ChunkMode::Text)
            .into_function()
            .map_err(load_error)?;
        let thread = self.thread(function).map_err(load_error)?;
        match self.resume(&thread, MultiValue::new(), self.options.load_budget) {
            Ok(Step::Done(_)) => Ok(()),
            Ok(Step::Wait(until)) => {
                if self.options.check {
                    let mut state = self.shared.state.borrow_mut();
                    if state.stopped_at.is_none() {
                        state.stopped_at = Some(format!("{name}, where it first waits"));
                    }
                    return Ok(());
                }
                self.suspend(thread, name.to_owned(), None, until);
                Ok(())
            }
            Err(error) => {
                let message = describe(&error);
                if self.options.check && message.contains(CHECK_STOP) {
                    return Ok(());
                }
                Err(load_error(error))
            }
        }
    }

    /// Evaluates a line from the REPL or `-e`: as an expression when it is
    /// one, else as statements. Answers what it returned, as text.
    pub fn eval(&self, code: &str) -> Result<String, String> {
        let as_expression = self
            .lua
            .load(format!("return {code}"))
            .set_name("=repl")
            .set_mode(ChunkMode::Text)
            .into_function();
        let function = match as_expression {
            Ok(function) => function,
            Err(_) => self
                .lua
                .load(code)
                .set_name("=repl")
                .set_mode(ChunkMode::Text)
                .into_function()
                .map_err(|error| describe(&error))?,
        };
        let thread = self.thread(function).map_err(|error| describe(&error))?;
        match self.resume(&thread, MultiValue::new(), self.options.load_budget) {
            Ok(Step::Done(values)) => Ok(values
                .iter()
                .map(|value| inspect(&self.lua, value))
                .collect::<Vec<_>>()
                .join("\t")),
            Ok(Step::Wait(until)) => {
                self.suspend(thread, "repl".into(), None, until);
                Ok(String::new())
            }
            Err(error) => Err(without_traceback(describe(&error))),
        }
    }

    /// Runs every handler for `event`, and resumes the coroutines waiting
    /// for it. Answers how many ran.
    pub fn emit(&self, event: &str, payload: &Value) -> usize {
        let woken = self.wake_waiters(event, payload);
        let ids: Vec<u64> = self
            .shared
            .state
            .borrow()
            .handlers
            .iter()
            .filter(|(_, name)| name == event)
            .map(|(id, _)| *id)
            .collect();
        if ids.is_empty() {
            return woken;
        }
        let Ok(argument) = to_lua(&self.lua, payload) else {
            return woken;
        };
        for id in &ids {
            self.call_callback(
                *id,
                argument.clone(),
                &format!("diri.on(\"{event}\")"),
                None,
            );
        }
        ids.len() + woken
    }

    /// A script key was pressed. `context` is what the function receives:
    /// `{session, pane, window, origin}`.
    pub fn run_binding(&self, id: u64, context: &Value) {
        let target = self
            .shared
            .state
            .borrow()
            .bindings
            .iter()
            .find(|binding| binding.id == id)
            .map(|binding| (binding.target.clone(), binding.keys.clone()));
        if let Some((target, keys)) = target {
            self.run_target(id, target, context, &format!("key {keys}"), None);
        }
    }

    /// A script's palette command was chosen, or run by name through
    /// `app.perform`. `reply` gets what its function returned, when it
    /// returns, which may be after it waited.
    pub fn run_command(&self, id: u64, context: &Value, reply: Option<Reply>) {
        let target = self
            .shared
            .state
            .borrow()
            .commands
            .iter()
            .find(|command| command.id == id)
            .map(|command| (command.target.clone(), command.name.clone()));
        match target {
            Some((target, name)) => {
                self.run_target(id, target, context, &format!("command {name}"), reply)
            }
            None => {
                if let Some(reply) = reply {
                    reply(Err("the script no longer has this command".into()));
                }
            }
        }
    }

    fn run_target(
        &self,
        id: u64,
        target: BindingTarget,
        context: &Value,
        what: &str,
        reply: Option<Reply>,
    ) {
        match target {
            BindingTarget::Callback => {
                let argument = to_lua(&self.lua, context).unwrap_or(LuaValue::Nil);
                self.call_callback(id, argument, what, reply);
            }
            BindingTarget::Action { name, args } => {
                let result = perform(&self.shared, &name, args);
                if let Err(error) = &result {
                    self.report(what, error);
                }
                if let Some(reply) = reply {
                    reply(result);
                }
            }
            // The window enters and leaves modes itself.
            BindingTarget::Mode { .. } => {
                if let Some(reply) = reply {
                    reply(Ok(Value::Null));
                }
            }
        }
    }

    /// When the next timer, timed wait or process deadline is due.
    pub fn next_timer(&self) -> Option<Instant> {
        let state = self.shared.state.borrow();
        let timers = state.timers.iter().map(|timer| timer.due);
        let waits = state
            .waiters
            .iter()
            .filter_map(|waiter| match &waiter.until {
                Until::At(due) => Some(*due),
                Until::Event { deadline, .. } => *deadline,
                Until::Exec(_) => None,
            });
        let execs = self.shared.execs.borrow().next_deadline();
        timers.chain(waits).chain(execs).min()
    }

    /// Runs the timers due by `now`, resumes the coroutines whose sleep or
    /// wait ran out, and kills processes past their timeout.
    pub fn run_timers(&self, now: Instant) {
        self.shared.execs.borrow_mut().expire(now);
        let due: Vec<u64> = {
            let mut state = self.shared.state.borrow_mut();
            let due: Vec<u64> = state
                .timers
                .iter()
                .filter(|timer| timer.due <= now)
                .map(|timer| timer.id)
                .collect();
            for timer in &mut state.timers {
                if let Some(every) = timer.every
                    && timer.due <= now
                {
                    timer.due = now + every;
                }
            }
            state
                .timers
                .retain(|timer| timer.every.is_some() || timer.due > now);
            due
        };
        for id in due {
            self.call_callback(id, LuaValue::Nil, "timer", None);
        }
        let waiters = self.take_waiters(|until| match until {
            Until::At(due) => *due <= now,
            Until::Event { deadline, .. } => deadline.is_some_and(|deadline| deadline <= now),
            Until::Exec(_) => false,
        });
        // A sleep returns nothing; a wait that ran out returns nil.
        for waiter in waiters {
            self.resume_waiter(waiter, MultiValue::new());
        }
    }

    /// Handles what the processes `diri.exec` started sent: lines for their
    /// `on_output`, and the coroutines whose process ended. The driver calls
    /// it when the [`Waker`] fires.
    pub fn pump(&self) {
        let events = self.shared.execs.borrow_mut().drain();
        for event in events {
            record_exec_event(&self.lua, &self.shared, event);
        }
        let finished = self.shared.execs.borrow().finished();
        for id in finished {
            let mut waiters =
                self.take_waiters(|until| matches!(until, Until::Exec(waiting) if *waiting == id));
            let result = self.shared.execs.borrow_mut().take(id);
            let (Some(waiter), Some(result)) = (waiters.pop(), result) else {
                continue;
            };
            match exec_result(&self.lua, result) {
                Ok(table) => self.resume_waiter(waiter, std::iter::once(table).collect()),
                Err(error) => self.report(&waiter.what, &describe(&error)),
            }
        }
    }

    /// The keys, modes and commands this script registered.
    pub fn layer(&self) -> Layer {
        let state = self.shared.state.borrow();
        Layer {
            bindings: state.bindings.clone(),
            unbinds: state.unbinds.clone(),
            commands: state.commands.clone(),
            modes: state.modes.clone(),
        }
    }

    /// The layer, if it changed since the last call.
    pub fn take_layer_change(&self) -> Option<Layer> {
        let changed = std::mem::take(&mut self.shared.state.borrow_mut().layer_changed);
        changed.then(|| self.layer())
    }

    /// What the script set up, for `dirijor script --check`.
    pub fn summary(&self) -> Summary {
        let state = self.shared.state.borrow();
        Summary {
            handlers: state.handlers.len(),
            timers: state.timers.len(),
            keys: state.bindings.len() + state.unbinds.len(),
            modes: state.modes.len(),
            commands: state.commands.len(),
            grants_requested: state.grants_requested.clone(),
            stopped_at: state.stopped_at.clone(),
        }
    }

    /// Whether any handler or waiting coroutine needs the Engine's events.
    pub fn wants_engine_events(&self) -> bool {
        let state = self.shared.state.borrow();
        state
            .handlers
            .iter()
            .any(|(_, event)| events::needs_engine(event))
            || state.waiters.iter().any(|waiter| {
                matches!(&waiter.until, Until::Event { name, .. } if events::needs_engine(name))
            })
    }

    /// Whether anything is left to wait for: handlers, timers, waiting
    /// coroutines, running processes.
    pub fn has_pending_work(&self) -> bool {
        let state = self.shared.state.borrow();
        !state.handlers.is_empty()
            || !state.timers.is_empty()
            || !state.waiters.is_empty()
            || !self.shared.execs.borrow().is_empty()
    }

    pub fn stopped(&self) -> bool {
        self.shared.state.borrow().stopped
    }

    /// The exit code `diri.stop(code)` asked for.
    pub fn exit_code(&self) -> i32 {
        self.shared.state.borrow().exit_code
    }

    /// Reports an error to the person (toast, at most once per 10 s for the
    /// same text) and always to the log.
    pub fn report(&self, what: &str, message: &str) {
        report(&self.shared, what, message);
    }

    /// A coroutine the runtime owns, so `diri.sleep`/`diri.wait` work in it.
    fn thread(&self, function: Function) -> mlua::Result<Thread> {
        let thread = self.lua.create_thread(function)?;
        let owned: Table = self.lua.named_registry_value(OWNED)?;
        owned.raw_set(thread.clone(), true)?;
        Ok(thread)
    }

    /// Resumes `thread` under `budget`: what it returned, or what it waits
    /// for.
    fn resume(&self, thread: &Thread, args: MultiValue, budget: Duration) -> mlua::Result<Step> {
        let values: MultiValue = self.guarded(budget, || thread.resume(args))?;
        if thread.status() != ThreadStatus::Resumable {
            return Ok(Step::Done(values));
        }
        parse_yield(&self.lua, values).map(Step::Wait)
    }

    fn suspend(&self, thread: Thread, what: String, reply: Option<Reply>, until: Until) {
        let mut state = self.shared.state.borrow_mut();
        if state.waiters.len() >= MAX_WAITERS {
            drop(state);
            let message = format!("a script can keep at most {MAX_WAITERS} callbacks waiting");
            self.report(&what, &message);
            if let Some(reply) = reply {
                reply(Err(message));
            }
            return;
        }
        state.waiters.push(Waiter {
            thread,
            what,
            reply,
            until,
        });
    }

    fn settle(&self, thread: Thread, step: mlua::Result<Step>, what: String, reply: Option<Reply>) {
        match step {
            Ok(Step::Done(values)) => {
                if let Some(reply) = reply {
                    let value = values
                        .into_iter()
                        .next()
                        .map(|value| from_lua(&self.lua, value).unwrap_or(Value::Null))
                        .unwrap_or(Value::Null);
                    reply(Ok(value));
                }
            }
            Ok(Step::Wait(until)) => self.suspend(thread, what, reply, until),
            Err(error) => {
                let message = describe(&error);
                self.report(&what, &message);
                if let Some(reply) = reply {
                    let message = without_traceback(message);
                    reply(Err(message.lines().next().unwrap_or_default().to_owned()));
                }
            }
        }
    }

    fn resume_waiter(&self, waiter: Waiter, args: MultiValue) {
        let step = self.resume(&waiter.thread, args, self.options.callback_budget);
        self.settle(waiter.thread, step, waiter.what, waiter.reply);
    }

    fn take_waiters(&self, wanted: impl Fn(&Until) -> bool) -> Vec<Waiter> {
        let mut state = self.shared.state.borrow_mut();
        let (taken, kept) = std::mem::take(&mut state.waiters)
            .into_iter()
            .partition(|waiter| wanted(&waiter.until));
        state.waiters = kept;
        taken
    }

    /// Resumes the coroutines in `diri.wait(event, filter)` whose filter
    /// matches.
    fn wake_waiters(&self, event: &str, payload: &Value) -> usize {
        let candidates =
            self.take_waiters(|until| matches!(until, Until::Event { name, .. } if name == event));
        if candidates.is_empty() {
            return 0;
        }
        let argument = to_lua(&self.lua, payload).unwrap_or(LuaValue::Nil);
        let mut woken = 0;
        let mut kept = Vec::new();
        for waiter in candidates {
            let Until::Event { filter, .. } = &waiter.until else {
                continue;
            };
            match self.matches(filter, payload, &argument) {
                Ok(true) => {
                    woken += 1;
                    self.resume_waiter(waiter, std::iter::once(argument.clone()).collect());
                }
                Ok(false) => kept.push(waiter),
                Err(error) => {
                    let message = describe(&error);
                    self.report(&waiter.what, &message);
                    if let Some(reply) = waiter.reply {
                        reply(Err(message));
                    }
                }
            }
        }
        self.shared.state.borrow_mut().waiters.extend(kept);
        woken
    }

    /// A `diri.wait` filter: nil matches everything; a function decides; a
    /// table matches when each of its fields equals the payload's, or the
    /// `id` of the payload's record (`{session = id}`).
    fn matches(
        &self,
        filter: &LuaValue,
        payload: &Value,
        argument: &LuaValue,
    ) -> mlua::Result<bool> {
        match filter {
            LuaValue::Nil => Ok(true),
            LuaValue::Function(function) => {
                let answer: LuaValue = self.guarded(self.options.callback_budget, || {
                    function.call(argument.clone())
                })?;
                Ok(!matches!(answer, LuaValue::Nil | LuaValue::Boolean(false)))
            }
            LuaValue::Table(_) => {
                let wanted = from_lua(&self.lua, filter.clone())?;
                Ok(wanted.as_object().is_none_or(|fields| {
                    fields.iter().all(|(key, value)| {
                        let field = &payload[key.as_str()];
                        field == value || &field["id"] == value
                    })
                }))
            }
            _ => Ok(false),
        }
    }

    fn call_callback(&self, id: u64, argument: LuaValue, what: &str, reply: Option<Reply>) {
        let function = (|| {
            let callbacks: Table = self.lua.named_registry_value(CALLBACKS)?;
            callbacks.get::<Option<Function>>(id)
        })();
        let function = match function {
            Ok(Some(function)) => function,
            Ok(None) => {
                if let Some(reply) = reply {
                    reply(Ok(Value::Null));
                }
                return;
            }
            Err(error) => {
                self.settle_error(what, &error, reply);
                return;
            }
        };
        match self.thread(function) {
            Ok(thread) => {
                let step = self.resume(
                    &thread,
                    std::iter::once(argument).collect(),
                    self.options.callback_budget,
                );
                self.settle(thread, step, what.to_owned(), reply);
            }
            Err(error) => self.settle_error(what, &error, reply),
        }
    }

    fn settle_error(&self, what: &str, error: &mlua::Error, reply: Option<Reply>) {
        let message = describe(error);
        self.report(what, &message);
        if let Some(reply) = reply {
            reply(Err(message));
        }
    }

    fn guarded<R>(
        &self,
        limit: Duration,
        run: impl FnOnce() -> mlua::Result<R>,
    ) -> mlua::Result<R> {
        guarded(&self.shared, limit, run)
    }

    fn install(&self) -> mlua::Result<()> {
        let lua = &self.lua;
        let shared = &self.shared;

        // The budget: checked every thousand instructions.
        let budget = Rc::clone(shared);
        lua.set_global_hook(
            HookTriggers::new().every_nth_instruction(1000),
            move |_, _| {
                if let Some(deadline) = budget.budget.deadline.get()
                    && Instant::now() > deadline
                {
                    return Err(mlua::Error::runtime(format!(
                        "stopped after {} ms: a script callback must finish quickly",
                        budget.budget.limit.get().as_millis()
                    )));
                }
                Ok(VmState::Continue)
            },
        )?;

        let globals = lua.globals();
        for name in ["dofile", "loadfile"] {
            globals.raw_set(name, LuaValue::Nil)?;
        }
        let string: Table = globals.get("string")?;
        string.raw_set("dump", LuaValue::Nil)?;
        // Text chunks only: a binary chunk can break out of the sandbox.
        lua.load(
            "local raw = load\n\
             load = function(chunk, name, _, env) return raw(chunk, name, 't', env) end",
        )
        .set_name("=sandbox")
        .exec()?;
        let os = lua.create_table()?;
        os.raw_set(
            "time",
            lua.create_function(|_, table: Option<Table>| {
                Ok(match table {
                    None => std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|elapsed| elapsed.as_secs() as i64)
                        .unwrap_or_default(),
                    Some(_) => {
                        return Err(mlua::Error::runtime(
                            "os.time takes no table here; use os.date",
                        ));
                    }
                })
            })?,
        )?;
        let started = Instant::now();
        os.raw_set(
            "clock",
            lua.create_function(move |_, ()| Ok(started.elapsed().as_secs_f64()))?,
        )?;
        os.raw_set(
            "difftime",
            lua.create_function(|_, (a, b): (f64, Option<f64>)| Ok(a - b.unwrap_or(0.0)))?,
        )?;
        os.raw_set(
            "date",
            lua.create_function(|_, (format, time): (Option<String>, Option<i64>)| {
                Ok(date(format.as_deref(), time))
            })?,
        )?;
        globals.raw_set("os", os)?;

        let diri = lua.create_table()?;
        lua.set_named_registry_value(CALLBACKS, lua.create_table()?)?;

        let print_shared = Rc::clone(shared);
        globals.raw_set(
            "print",
            lua.create_function(move |lua, values: MultiValue| {
                print_shared.log(Level::Info, &join(lua, &values));
                Ok(())
            })?,
        )?;
        let log_shared = Rc::clone(shared);
        diri.raw_set(
            "log",
            lua.create_function(move |lua, values: MultiValue| {
                log_shared.log(Level::Info, &join(lua, &values));
                Ok(())
            })?,
        )?;

        self.install_require(&globals)?;
        install_events(lua, shared, &diri)?;
        install_calls(lua, shared, &diri)?;
        install_keymap(lua, shared, &diri)?;
        install_timers(lua, shared, &diri)?;
        install_values(lua, shared, &diri, &self.options)?;
        install_waits(lua, shared, &diri)?;
        globals.raw_set("diri", diri)?;
        Ok(())
    }

    fn install_require(&self, globals: &Table) -> mlua::Result<()> {
        let lua = &self.lua;
        let root = self
            .options
            .root
            .as_ref()
            .and_then(|root| root.canonicalize().ok());
        let loaded = lua.create_table()?;
        globals.raw_set(
            "require",
            lua.create_function(move |lua, name: String| {
                if let Some(value) = loaded.get::<Option<LuaValue>>(name.as_str())? {
                    return Ok(value);
                }
                let root = root.as_ref().ok_or_else(|| {
                    mlua::Error::runtime("require only works in a script loaded from a file")
                })?;
                let path = module_path(root, &name).map_err(mlua::Error::runtime)?;
                let source = std::fs::read_to_string(&path).map_err(|error| {
                    mlua::Error::runtime(format!("require \"{name}\": {error}"))
                })?;
                let value: LuaValue = lua
                    .load(source)
                    .set_name(format!("@{name}.lua"))
                    .set_mode(ChunkMode::Text)
                    .call(name.as_str())?;
                let value = if value.is_nil() {
                    LuaValue::Boolean(true)
                } else {
                    value
                };
                loaded.raw_set(name.as_str(), value.clone())?;
                Ok(value)
            })?,
        )
    }
}

/// The file a module name stands for, refused when it is not a plain name or
/// it resolves outside `root` (through `..` or a symlink).
fn module_path(root: &Path, name: &str) -> Result<PathBuf, String> {
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && !name.starts_with('.')
        && !name.contains("..");
    if !plain {
        return Err(format!(
            "require \"{name}\": only plain module names next to init.lua can be loaded"
        ));
    }
    let relative = format!("{}.lua", name.replace('.', "/"));
    let path = root.join(relative);
    let resolved = path
        .canonicalize()
        .map_err(|error| format!("require \"{name}\": {error}"))?;
    if !resolved.starts_with(root) {
        return Err(format!(
            "require \"{name}\": the file is outside the script directory"
        ));
    }
    let size = std::fs::metadata(&resolved)
        .map(|meta| meta.len())
        .unwrap_or(0);
    if size > MAX_MODULE_BYTES {
        return Err(format!("require \"{name}\": the file is larger than 1 MiB"));
    }
    Ok(resolved)
}

fn store_callback(lua: &Lua, id: u64, function: Function) -> mlua::Result<()> {
    let callbacks: Table = lua.named_registry_value(CALLBACKS)?;
    callbacks.raw_set(id, function)
}

fn drop_callback(lua: &Lua, id: u64) -> mlua::Result<()> {
    let callbacks: Table = lua.named_registry_value(CALLBACKS)?;
    callbacks.raw_set(id, LuaValue::Nil)
}

fn install_events(lua: &Lua, shared: &Rc<Shared>, diri: &Table) -> mlua::Result<()> {
    let on_shared = Rc::clone(shared);
    diri.raw_set(
        "on",
        lua.create_function(move |lua, (event, function): (String, Function)| {
            if !events::is_known(&event) {
                let known: Vec<&str> = events::ALL.iter().map(|(name, _)| *name).collect();
                return Err(mlua::Error::runtime(format!(
                    "diri.on: unknown event \"{event}\"; one of: {}",
                    known.join(", ")
                )));
            }
            let id = on_shared.state.borrow_mut().id();
            store_callback(lua, id, function)?;
            on_shared.state.borrow_mut().handlers.push((id, event));
            Ok(id)
        })?,
    )?;
    let off_shared = Rc::clone(shared);
    diri.raw_set(
        "off",
        lua.create_function(move |lua, id: u64| {
            let mut state = off_shared.state.borrow_mut();
            state.handlers.retain(|(handler, _)| *handler != id);
            state.timers.retain(|timer| timer.id != id);
            drop(state);
            drop_callback(lua, id)
        })?,
    )?;
    let names = lua.create_table()?;
    for (name, description) in events::ALL {
        names.raw_set(*name, *description)?;
    }
    diri.raw_set("events", names)
}

fn install_calls(lua: &Lua, shared: &Rc<Shared>, diri: &Table) -> mlua::Result<()> {
    let call_shared = Rc::clone(shared);
    let call = lua.create_function(
        move |lua, (method, params, opts): (String, Option<LuaValue>, Option<Table>)| {
            let params = match params {
                None | Some(LuaValue::Nil) => json!({}),
                Some(value) => from_lua(lua, value)?,
            };
            let timeout = timeout(opts.as_ref())?;
            let result = call_method(&call_shared, &method, params, timeout)
                .map_err(|error| stop_here(lua, &call_shared, error))?;
            to_lua(lua, &result)
        },
    )?;
    diri.raw_set("call", call.clone())?;

    // One function per Engine method: `diri.session.list(params, opts)`.
    let summaries = lua.create_table()?;
    for method in api::methods() {
        summaries.raw_set(method.name.as_str(), method.summary.as_str())?;
        let parts: Vec<&str> = method.name.split('.').collect();
        let mut table = diri.clone();
        for part in &parts[..parts.len() - 1] {
            table = match table.raw_get::<Option<Table>>(*part)? {
                Some(next) => next,
                None => {
                    let next = lua.create_table()?;
                    table.raw_set(*part, next.clone())?;
                    next
                }
            };
        }
        let name = method.name.clone();
        let call = call.clone();
        table.raw_set(
            parts[parts.len() - 1],
            lua.create_function(move |_, (params, opts): (LuaValue, LuaValue)| {
                call.call::<LuaValue>((name.as_str(), params, opts))
            })?,
        )?;
    }
    diri.raw_set("methods", summaries)?;

    let action_shared = Rc::clone(shared);
    let action = lua.create_function(move |lua, (name, args): (String, Option<LuaValue>)| {
        let args = match args {
            None | Some(LuaValue::Nil) => json!({}),
            Some(value) => from_lua(lua, value)?,
        };
        // One of this script's own commands runs here: asking the window
        // would wait on this very thread.
        let own = name.strip_prefix(COMMAND_PREFIX).and_then(|local| {
            action_shared
                .state
                .borrow()
                .commands
                .iter()
                .find(|command| command.name == local)
                .map(|command| (command.id, command.target.clone()))
        });
        let (name, args) = match own {
            Some((id, BindingTarget::Callback)) => {
                let callbacks: Table = lua.named_registry_value(CALLBACKS)?;
                let function: Function = callbacks.get(id)?;
                let context = to_lua(lua, &json!({"origin": "script", "args": args}))?;
                return function.call::<LuaValue>(context);
            }
            Some((_, BindingTarget::Action { name, args })) => (name, args),
            Some((_, BindingTarget::Mode { .. })) => return Ok(LuaValue::Nil),
            None => (name, args),
        };
        if let Err(error) = action_shared.check_stop(&format!("diri.action(\"{name}\")")) {
            return Err(stop_here(lua, &action_shared, error));
        }
        let result = perform(&action_shared, &name, args).map_err(mlua::Error::runtime)?;
        to_lua(lua, &result)
    })?;
    diri.raw_set("action", action.clone())?;

    // `diri.actions.split_pane_right(args)`.
    let actions = lua.create_table()?;
    let metatable = lua.create_table()?;
    metatable.raw_set(
        "__index",
        lua.create_function(move |lua, (_, key): (Table, String)| {
            let name = api::action_name(&key);
            let action = action.clone();
            lua.create_function(move |_, args: LuaValue| {
                action.call::<LuaValue>((name.as_str(), args))
            })
        })?,
    )?;
    actions.set_metatable(Some(metatable))?;
    diri.raw_set("actions", actions)
}

fn timeout(opts: Option<&Table>) -> mlua::Result<Duration> {
    seconds_option(opts, "timeout", DEFAULT_TIMEOUT)
}

fn seconds_option(opts: Option<&Table>, key: &str, default: Duration) -> mlua::Result<Duration> {
    let Some(seconds) = opts
        .map(|opts| opts.get::<Option<f64>>(key))
        .transpose()?
        .flatten()
    else {
        return Ok(default);
    };
    if !(seconds > 0.0 && seconds <= MAX_TIMEOUT_SECONDS) {
        return Err(mlua::Error::runtime(format!(
            "{key} is seconds, more than 0 and at most 600"
        )));
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn call_method(
    shared: &Shared,
    name: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, String> {
    if let Some(why) = api::reserved(name) {
        return Err(format!("{name} is not available to scripts: {why}"));
    }
    let Some(method) = api::method(name) else {
        return Err(format!(
            "unknown Engine method \"{name}\"; diri.methods lists them"
        ));
    };
    api::validate(method, &params).map_err(|error| format!("{name}: {error}"))?;
    shared.check_stop(name)?;
    shared
        .host(|host| host.request(name, params, timeout))
        .map_err(|error| format!("{name}: {error}"))
}

/// The error for a failed call; in a `--check` run, also where the top level
/// stopped.
fn stop_here(lua: &Lua, shared: &Shared, message: String) -> mlua::Error {
    if message.contains(CHECK_STOP) {
        let mut state = shared.state.borrow_mut();
        if state.stopped_at.is_none() {
            state.stopped_at = caller(lua);
        }
    }
    mlua::Error::runtime(message)
}

/// `file.lua:12`: the innermost Lua line running now.
fn caller(lua: &Lua) -> Option<String> {
    (0..32).find_map(|level| {
        lua.inspect_stack(level, |debug| {
            let source = debug.source();
            if source.what == "C" {
                return None;
            }
            let file = source.short_src.as_deref().unwrap_or("?").to_owned();
            Some(match debug.current_line() {
                Some(line) => format!("{file}:{line}"),
                None => file,
            })
        })
        .flatten()
    })
}

/// Runs a window action through the Engine (`app.perform`).
fn perform(shared: &Shared, name: &str, args: Value) -> Result<Value, String> {
    let result = shared
        .host(|host| {
            host.request(
                diri_proto::Method::APP_PERFORM,
                json!({"action": name, "args": args}),
                Duration::from_millis(diri_proto::app_actions::DEFAULT_TIMEOUT_MS + 2_000),
            )
        })
        .map_err(|error| format!("{name}: {error}"))?;
    Ok(result.get("result").cloned().unwrap_or(Value::Null))
}

fn target(
    lua: &Lua,
    shared: &Shared,
    value: LuaValue,
    what: &str,
) -> mlua::Result<(BindingTarget, Option<Function>)> {
    match value {
        LuaValue::String(name) => Ok((
            BindingTarget::Action {
                name: name.to_str()?.to_owned(),
                args: json!({}),
            },
            None,
        )),
        LuaValue::Function(function) => Ok((BindingTarget::Callback, Some(function))),
        LuaValue::Table(table) => {
            match table.get::<LuaValue>("mode")? {
                LuaValue::Nil => {}
                LuaValue::Boolean(false) => return Ok((BindingTarget::Mode { name: None }, None)),
                LuaValue::String(name) => {
                    let name = name.to_str()?.to_owned();
                    known_mode(shared, &name, what)?;
                    return Ok((BindingTarget::Mode { name: Some(name) }, None));
                }
                _ => {
                    return Err(mlua::Error::runtime(format!(
                        "{what}: mode is a mode's name, or false to leave the mode"
                    )));
                }
            }
            let name: Option<String> = table.get("action")?;
            let Some(name) = name else {
                return Err(mlua::Error::runtime(format!(
                    "{what}: a table target needs action = \"name\" or mode = \"name\""
                )));
            };
            let args = match table.get::<LuaValue>("args")? {
                LuaValue::Nil => json!({}),
                value => from_lua(lua, value)?,
            };
            Ok((BindingTarget::Action { name, args }, None))
        }
        _ => Err(mlua::Error::runtime(format!(
            "{what}: the target is an action name, {{action=, args=}}, {{mode=}} or a function"
        ))),
    }
}

fn known_mode(shared: &Shared, name: &str, what: &str) -> mlua::Result<()> {
    if shared
        .state
        .borrow()
        .modes
        .iter()
        .any(|mode| mode.name == name)
    {
        Ok(())
    } else {
        Err(mlua::Error::runtime(format!(
            "{what}: no mode is named \"{name}\"; declare it first with diri.keymap.mode(\"{name}\")"
        )))
    }
}

/// A mode or command name: letters, digits, `-` and `_`.
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// `"resize/h"` is key `h` in mode `resize`; anything else is a normal key.
fn split_mode(shared: &Shared, keys: &str, what: &str) -> mlua::Result<(Option<String>, String)> {
    if let Some((mode, key)) = keys.split_once('/')
        && !key.is_empty()
        && plain_name(mode)
    {
        known_mode(shared, mode, what)?;
        let key = key.trim();
        if key.split_whitespace().count() != 1 {
            return Err(mlua::Error::runtime(format!(
                "{what}: a key in a mode is one keystroke, not a sequence"
            )));
        }
        return Ok((Some(mode.to_owned()), key.to_owned()));
    }
    Ok((None, keys.to_owned()))
}

/// Warns (never fails) when a script names an action this window does not
/// have: a typo would otherwise fail only when the key is pressed.
fn check_action(shared: &Shared, target: &BindingTarget, what: &str) {
    let BindingTarget::Action { name, .. } = target else {
        return;
    };
    let known = {
        let mut state = shared.state.borrow_mut();
        if state.actions.is_none() {
            drop(state);
            let actions = shared.host(|host| host.actions()).unwrap_or_default();
            state = shared.state.borrow_mut();
            state.actions = Some(actions);
        }
        name.starts_with(COMMAND_PREFIX)
            || state
                .actions
                .as_ref()
                .is_none_or(|actions| actions.is_empty() || actions.iter().any(|a| &a.name == name))
    };
    if !known {
        shared.log(
            Level::Warning,
            &format!("{what}: no window action is named \"{name}\" (dirijor actions lists them)"),
        );
    }
}

fn install_keymap(lua: &Lua, shared: &Rc<Shared>, diri: &Table) -> mlua::Result<()> {
    let keymap = lua.create_table()?;
    let set_shared = Rc::clone(shared);
    keymap.raw_set(
        "set",
        lua.create_function(
            move |lua, (keys, value, opts): (String, LuaValue, Option<Table>)| {
                let keys = keys.trim().to_owned();
                if keys.is_empty() {
                    return Err(mlua::Error::runtime("diri.keymap.set: keys are empty"));
                }
                let what = format!("diri.keymap.set(\"{keys}\")");
                let (mode, keys) = split_mode(&set_shared, &keys, &what)?;
                if let Err(why) = keys::check(&keys) {
                    set_shared.log(Level::Warning, &format!("{what}: {why}"));
                }
                let (target, function) = target(lua, &set_shared, value, &what)?;
                check_action(&set_shared, &target, &what);
                let description = opts
                    .map(|opts| opts.get::<Option<String>>("desc"))
                    .transpose()?
                    .flatten();
                let source = caller(lua);
                let mut state = set_shared.state.borrow_mut();
                let id = state.id();
                if let Some(function) = function {
                    store_callback(lua, id, function)?;
                }
                // The newest binding on a key wins.
                state
                    .bindings
                    .retain(|binding| binding.keys != keys || binding.mode != mode);
                state.bindings.push(Binding {
                    id,
                    keys,
                    target,
                    description,
                    mode,
                    source,
                });
                state.layer_changed = true;
                Ok(id)
            },
        )?,
    )?;
    let del_shared = Rc::clone(shared);
    keymap.raw_set(
        "del",
        lua.create_function(move |_, keys: String| {
            let keys = keys.trim().to_owned();
            let what = format!("diri.keymap.del(\"{keys}\")");
            let (mode, keys) = split_mode(&del_shared, &keys, &what)?;
            let mut state = del_shared.state.borrow_mut();
            state
                .bindings
                .retain(|binding| binding.keys != keys || binding.mode != mode);
            if mode.is_none() && !state.unbinds.contains(&keys) {
                state.unbinds.push(keys);
            }
            state.layer_changed = true;
            Ok(())
        })?,
    )?;
    let mode_shared = Rc::clone(shared);
    keymap.raw_set(
        "mode",
        lua.create_function(move |lua, (name, opts): (String, Option<Table>)| {
            if !plain_name(&name) {
                return Err(mlua::Error::runtime(format!(
                    "diri.keymap.mode: name \"{name}\" must be letters, digits, - or _"
                )));
            }
            let flag = |key: &str| -> mlua::Result<bool> {
                Ok(opts
                    .as_ref()
                    .map(|opts| opts.get::<Option<bool>>(key))
                    .transpose()?
                    .flatten()
                    .unwrap_or(false))
            };
            let mode = Mode {
                name: name.clone(),
                exclusive: flag("exclusive")?,
                once: flag("once")?,
                description: opts
                    .as_ref()
                    .map(|opts| opts.get::<Option<String>>("desc"))
                    .transpose()?
                    .flatten(),
                source: caller(lua),
            };
            let mut state = mode_shared.state.borrow_mut();
            state.modes.retain(|existing| existing.name != name);
            state.modes.push(mode);
            state.layer_changed = true;
            Ok(name)
        })?,
    )?;
    for (key, entering) in [("enter", true), ("exit", false)] {
        let switch_shared = Rc::clone(shared);
        keymap.raw_set(
            key,
            lua.create_function(move |lua, name: Option<String>| {
                let name = if entering {
                    let name =
                        name.ok_or_else(|| mlua::Error::runtime("diri.keymap.enter: which mode?"))?;
                    known_mode(&switch_shared, &name, "diri.keymap.enter")?;
                    Some(name)
                } else {
                    None
                };
                if let Err(error) = switch_shared.check_stop("diri.keymap.enter") {
                    return Err(stop_here(lua, &switch_shared, error));
                }
                switch_shared
                    .host(|host| host.set_mode(name.as_deref()))
                    .map_err(|error| mlua::Error::runtime(error.to_string()))
            })?,
        )?;
    }
    diri.raw_set("keymap", keymap)?;

    let command_shared = Rc::clone(shared);
    diri.raw_set(
        "command",
        lua.create_function(move |lua, spec: Table| {
            let name: String = spec
                .get::<Option<String>>("name")?
                .ok_or_else(|| mlua::Error::runtime("diri.command: name is required"))?;
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                return Err(mlua::Error::runtime(format!(
                    "diri.command: name \"{name}\" must be letters, digits, - _ or ."
                )));
            }
            let title = spec
                .get::<Option<String>>("title")?
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| name.clone());
            let what = format!("diri.command(\"{name}\")");
            let mut run: LuaValue = spec.get("run")?;
            if let LuaValue::String(_) = run {
                // `run = "action", args = {...}`
                let args: LuaValue = spec.get("args")?;
                let table = lua.create_table()?;
                table.raw_set("action", run)?;
                table.raw_set("args", args)?;
                run = LuaValue::Table(table);
            }
            let (target, function) = target(lua, &command_shared, run, &what)?;
            if matches!(target, BindingTarget::Mode { .. }) {
                return Err(mlua::Error::runtime(format!(
                    "{what}: a palette command cannot enter a mode; bind a key to {{mode = ...}}"
                )));
            }
            check_action(&command_shared, &target, &what);
            let description = spec
                .get::<Option<String>>("desc")?
                .or(spec.get::<Option<String>>("description")?);
            let destructive = spec.get::<Option<bool>>("destructive")?.unwrap_or(false);
            let source = caller(lua);
            let mut state = command_shared.state.borrow_mut();
            let id = state.id();
            if let Some(function) = function {
                store_callback(lua, id, function)?;
            }
            state.commands.retain(|command| command.name != name);
            state.commands.push(Command {
                id,
                name,
                title,
                target,
                description,
                destructive,
                source,
            });
            state.layer_changed = true;
            Ok(id)
        })?,
    )
}

fn install_timers(lua: &Lua, shared: &Rc<Shared>, diri: &Table) -> mlua::Result<()> {
    for (name, repeats) in [("after", false), ("every", true)] {
        let timer_shared = Rc::clone(shared);
        diri.raw_set(
            name,
            lua.create_function(move |lua, (seconds, function): (f64, Function)| {
                if !(seconds.is_finite() && (0.0..=86_400.0).contains(&seconds)) {
                    return Err(mlua::Error::runtime(
                        "a timer's delay is seconds, from 0 to 86400",
                    ));
                }
                let mut delay = Duration::from_secs_f64(seconds);
                if repeats {
                    delay = delay.max(MIN_REPEAT);
                }
                let mut state = timer_shared.state.borrow_mut();
                if state.timers.len() >= MAX_TIMERS {
                    return Err(mlua::Error::runtime("a script can keep at most 256 timers"));
                }
                let id = state.id();
                store_callback(lua, id, function)?;
                state.timers.push(Timer {
                    id,
                    due: Instant::now() + delay,
                    every: repeats.then_some(delay),
                });
                Ok(id)
            })?,
        )?;
    }
    let stop_shared = Rc::clone(shared);
    diri.raw_set(
        "stop",
        lua.create_function(move |_, code: Option<i32>| {
            let mut state = stop_shared.state.borrow_mut();
            state.stopped = true;
            state.exit_code = code.unwrap_or(0);
            Ok(())
        })?,
    )
}

fn install_values(
    lua: &Lua,
    shared: &Rc<Shared>,
    diri: &Table,
    options: &Options,
) -> mlua::Result<()> {
    let notify_shared = Rc::clone(shared);
    diri.raw_set(
        "notify",
        lua.create_function(move |_, (message, opts): (String, Option<Table>)| {
            let level = match opts
                .as_ref()
                .map(|opts| opts.get::<Option<String>>("level"))
                .transpose()?
                .flatten()
            {
                Some(level) => Level::parse(&level).ok_or_else(|| {
                    mlua::Error::runtime("diri.notify: level is info, success, warning or error")
                })?,
                None => Level::Info,
            };
            let detail = opts
                .map(|opts| opts.get::<Option<String>>("detail"))
                .transpose()?
                .flatten();
            notify_shared.host(|host| {
                host.notify(Notice {
                    level,
                    message,
                    detail,
                    logged: false,
                })
            });
            Ok(())
        })?,
    )?;
    let json_table = lua.create_table()?;
    json_table.raw_set(
        "encode",
        lua.create_function(|lua, value: LuaValue| {
            let value = from_lua(lua, value)?;
            serde_json::to_string(&value).map_err(mlua::Error::runtime)
        })?,
    )?;
    json_table.raw_set(
        "decode",
        lua.create_function(|lua, text: String| {
            let value: Value = serde_json::from_str(&text)
                .map_err(|error| mlua::Error::runtime(format!("diri.json.decode: {error}")))?;
            to_lua(lua, &value)
        })?,
    )?;
    diri.raw_set("json", json_table)?;
    diri.raw_set(
        "inspect",
        lua.create_function(|lua, value: LuaValue| Ok(inspect(lua, &value)))?,
    )?;
    let context = shared.host.borrow().context();
    diri.raw_set("context", context)?;
    let args = match &options.args {
        Value::Object(_) => to_lua(lua, &options.args)?,
        _ => LuaValue::Table(lua.create_table()?),
    };
    diri.raw_set("args", args)?;
    diri.raw_set("session_id", options.session_id.clone())?;
    diri.raw_set("version", 2)
}

/// `diri.sleep`, `diri.wait`, `diri.exec` and `diri.require_grant`.
///
/// Sleep and wait suspend the coroutine the runtime runs each callback in:
/// they yield a private token the runtime recognises, and the driver
/// resumes the coroutine when the time, event or process comes. They work
/// only in the runtime's own coroutines, so a script's own
/// `coroutine.wrap` cannot be resumed by surprise.
fn install_waits(lua: &Lua, shared: &Rc<Shared>, diri: &Table) -> mlua::Result<()> {
    let owned = lua.create_table()?;
    let weak = lua.create_table()?;
    weak.raw_set("__mode", "k")?;
    owned.set_metatable(Some(weak))?;
    lua.set_named_registry_value(OWNED, owned.clone())?;
    let token = lua.create_table()?;
    lua.set_named_registry_value(TOKEN, token.clone())?;

    let check_sleep = lua.create_function(|_, ms: LuaValue| {
        let ms = match ms {
            LuaValue::Integer(ms) => ms as f64,
            LuaValue::Number(ms) => ms,
            _ => f64::NAN,
        };
        if !(ms.is_finite() && (0.0..=86_400_000.0).contains(&ms)) {
            return Err(mlua::Error::runtime(
                "diri.sleep takes milliseconds, from 0 to 86400000",
            ));
        }
        Ok(ms)
    })?;
    let check_wait = lua.create_function(
        |_, (event, filter, timeout): (String, LuaValue, LuaValue)| {
            if !events::is_known(&event) {
                return Err(mlua::Error::runtime(format!(
                    "diri.wait: unknown event \"{event}\"; diri.events lists them"
                )));
            }
            if !matches!(
                filter,
                LuaValue::Nil | LuaValue::Table(_) | LuaValue::Function(_)
            ) {
                return Err(mlua::Error::runtime(
                    "diri.wait: the filter is a table of fields to match, or a function",
                ));
            }
            let timeout = match timeout {
                LuaValue::Nil => None,
                LuaValue::Integer(ms) => Some(ms as f64),
                LuaValue::Number(ms) => Some(ms),
                _ => Some(f64::NAN),
            };
            if timeout.is_some_and(|ms| !(ms.is_finite() && ms > 0.0 && ms <= 86_400_000.0)) {
                return Err(mlua::Error::runtime(
                    "diri.wait: the timeout is milliseconds, more than 0",
                ));
            }
            Ok(timeout)
        },
    )?;
    let start_shared = Rc::clone(shared);
    let start_exec = lua.create_function(move |lua, (argv, opts): (Table, Option<Table>)| {
        let (spec, output) = exec_spec(lua, &start_shared, &argv, opts.as_ref())?;
        let id = start_shared
            .execs
            .borrow_mut()
            .spawn(spec)
            .map_err(|error| mlua::Error::runtime(format!("diri.exec: {error}")))?;
        if let Some(output) = output {
            start_shared.state.borrow_mut().outputs.insert(id, output);
        }
        Ok(id)
    })?;
    let run_shared = Rc::clone(shared);
    let run_exec = lua.create_function(move |lua, (argv, opts): (Table, Option<Table>)| {
        let (spec, output) = exec_spec(lua, &run_shared, &argv, opts.as_ref())?;
        run_blocking(lua, &run_shared, spec, output)
    })?;
    let can_yield_shared = Rc::clone(shared);
    let can_yield =
        lua.create_function(move |_, ()| Ok(can_yield_shared.execs.borrow().can_wake()))?;

    let shim: Table = lua
        .load(
            r#"
local token, owned, check_sleep, check_wait, start_exec, run_exec, can_yield = ...
local yield, running = coroutine.yield, coroutine.running
local function ours()
  local co, main = running()
  return not main and owned[co] == true
end
local function require_ours(name)
  if not ours() then
    error(name .. " works directly in a callback or a script's top level, not in a coroutine of your own", 3)
  end
end
local api = {}
function api.sleep(ms)
  ms = check_sleep(ms)
  require_ours("diri.sleep")
  yield(token, "sleep", ms)
end
function api.wait(event, filter, timeout)
  timeout = check_wait(event, filter, timeout)
  require_ours("diri.wait")
  return yield(token, "wait", event, filter, timeout)
end
function api.exec(argv, opts)
  if ours() and can_yield() then
    return yield(token, "exec", start_exec(argv, opts))
  end
  return run_exec(argv, opts)
end
return api
"#,
        )
        .set_name("=diri")
        .set_mode(ChunkMode::Text)
        .call((
            token, owned, check_sleep, check_wait, start_exec, run_exec, can_yield,
        ))?;
    for name in ["sleep", "wait", "exec"] {
        diri.raw_set(name, shim.raw_get::<Function>(name)?)?;
    }

    let grant_shared = Rc::clone(shared);
    diri.raw_set(
        "require_grant",
        lua.create_function(move |_, spec: Table| {
            let mut programs = Vec::new();
            for pair in spec.pairs::<String, LuaValue>() {
                let (key, value) = pair?;
                if key != "exec" {
                    return Err(mlua::Error::runtime(format!(
                        "diri.require_grant: there are no \"{key}\" grants; exec is the one there is"
                    )));
                }
                let LuaValue::Table(list) = value else {
                    return Err(mlua::Error::runtime(
                        "diri.require_grant: exec is a list of program names, {\"git\"}",
                    ));
                };
                for program in list.sequence_values::<String>() {
                    let program = program?;
                    if program.trim().is_empty() || program.contains(char::is_whitespace) {
                        return Err(mlua::Error::runtime(format!(
                            "diri.require_grant: \"{program}\" is not a program name"
                        )));
                    }
                    programs.push(program);
                }
            }
            let request = {
                let mut state = grant_shared.state.borrow_mut();
                for program in &programs {
                    if !state.grants_requested.contains(program) {
                        state.grants_requested.push(program.clone());
                    }
                }
                let missing: Vec<String> = programs
                    .iter()
                    .filter(|program| !state.granted.contains(program))
                    .cloned()
                    .collect();
                if missing.is_empty() {
                    return Ok(true);
                }
                GrantRequest {
                    exec: missing,
                    script: state.script.clone(),
                    digest: state.digest.clone(),
                }
            };
            if grant_shared.check {
                return Ok(false);
            }
            // Asking can wait on the person; that is not the script's CPU.
            let granted = grant_shared.host(|host| host.grant(&request));
            if granted {
                grant_shared
                    .state
                    .borrow_mut()
                    .granted
                    .extend(request.exec);
            }
            Ok(granted)
        })?,
    )
}

/// Reads `diri.exec(argv, opts)`, checking the program was granted.
fn exec_spec(
    lua: &Lua,
    shared: &Shared,
    argv: &Table,
    opts: Option<&Table>,
) -> mlua::Result<(exec::Spec, Option<Function>)> {
    let argv: Vec<String> = argv
        .sequence_values::<String>()
        .collect::<mlua::Result<_>>()
        .map_err(|_| mlua::Error::runtime("diri.exec: argv is a list of strings"))?;
    let Some(program) = argv.first() else {
        return Err(mlua::Error::runtime("diri.exec: argv is empty"));
    };
    if let Err(error) = shared.check_stop(&format!("diri.exec(\"{program}\")")) {
        return Err(stop_here(lua, shared, error));
    }
    if !shared.state.borrow().granted.contains(program) {
        return Err(mlua::Error::runtime(format!(
            "diri.exec: \"{program}\" is not granted; ask with diri.require_grant{{exec = {{\"{program}\"}}}}"
        )));
    }
    let timeout = seconds_option(opts, "timeout", DEFAULT_EXEC_TIMEOUT)
        .map_err(|error| mlua::Error::runtime(format!("diri.exec: {}", describe(&error))))?;
    let get = |key: &str| -> mlua::Result<LuaValue> {
        opts.map(|opts| opts.get::<LuaValue>(key))
            .unwrap_or(Ok(LuaValue::Nil))
    };
    let cwd = match get("cwd")? {
        LuaValue::Nil => None,
        LuaValue::String(text) => {
            let text = text.to_str()?.to_owned();
            let path = match text.strip_prefix("~") {
                Some(rest) if rest.is_empty() || rest.starts_with('/') => diri_platform::home_dir()
                    .map(|home| home.join(rest.trim_start_matches('/')))
                    .unwrap_or_else(|| PathBuf::from(&text)),
                _ => PathBuf::from(&text),
            };
            if !path.is_absolute() || !path.is_dir() {
                return Err(mlua::Error::runtime(format!(
                    "diri.exec: cwd \"{text}\" is not an absolute directory"
                )));
            }
            Some(path)
        }
        _ => return Err(mlua::Error::runtime("diri.exec: cwd is a path")),
    };
    let env = match get("env")? {
        LuaValue::Nil => Vec::new(),
        LuaValue::Table(table) => table
            .pairs::<String, String>()
            .collect::<mlua::Result<Vec<_>>>()
            .map_err(|_| mlua::Error::runtime("diri.exec: env maps names to strings"))?,
        _ => return Err(mlua::Error::runtime("diri.exec: env is a table")),
    };
    let stdin = match get("stdin")? {
        LuaValue::Nil => None,
        LuaValue::String(text) => Some(text.as_bytes().to_vec()),
        _ => return Err(mlua::Error::runtime("diri.exec: stdin is a string")),
    };
    let output = match get("on_output")? {
        LuaValue::Nil => None,
        LuaValue::Function(function) => Some(function),
        _ => return Err(mlua::Error::runtime("diri.exec: on_output is a function")),
    };
    Ok((
        exec::Spec {
            argv,
            cwd,
            env,
            stdin,
            timeout,
        },
        output,
    ))
}

/// `diri.exec` outside one of the runtime's coroutines (the REPL, a nested
/// function): runs the process to its end inside the call. Waiting on it is
/// not counted against the budget; its `on_output` calls are.
fn run_blocking(
    lua: &Lua,
    shared: &Shared,
    spec: exec::Spec,
    output: Option<Function>,
) -> mlua::Result<LuaValue> {
    let id = shared
        .execs
        .borrow_mut()
        .spawn(spec)
        .map_err(|error| mlua::Error::runtime(format!("diri.exec: {error}")))?;
    if let Some(output) = output {
        shared.state.borrow_mut().outputs.insert(id, output);
    }
    loop {
        if let Some(result) = shared.execs.borrow_mut().take(id) {
            return exec_result(lua, result);
        }
        let now = Instant::now();
        let deadline = shared.execs.borrow().deadline(id).unwrap_or(now);
        let wait = deadline
            .saturating_duration_since(now)
            .max(Duration::from_millis(50));
        let event = shared.execs.borrow().recv(wait);
        shared.not_counted(now);
        match event {
            Some(event) => record_exec_event(lua, shared, event),
            None => {
                if Instant::now() >= deadline {
                    shared.execs.borrow_mut().kill(id);
                }
            }
        }
    }
}

fn record_exec_event(lua: &Lua, shared: &Shared, event: exec::Event) {
    let _ = lua;
    let (id, lines, ended) = shared.execs.borrow_mut().record(event);
    let output = shared.state.borrow().outputs.get(&id).cloned();
    if let Some(function) = output {
        for (stream, line) in lines {
            let result = guarded(shared, shared.callback_budget, || {
                function.call::<()>((line, stream.label()))
            });
            if let Err(error) = result {
                report(shared, "diri.exec on_output", &describe(&error));
                break;
            }
        }
    }
    if ended {
        shared.state.borrow_mut().outputs.remove(&id);
    }
}

fn exec_result(lua: &Lua, result: exec::Finished) -> mlua::Result<LuaValue> {
    let table = lua.create_table()?;
    table.raw_set(
        "ok",
        result.code == Some(0) && !result.timed_out && result.error.is_none(),
    )?;
    table.raw_set("code", result.code)?;
    table.raw_set("signal", result.signal)?;
    table.raw_set("stdout", lua.create_string(&result.stdout)?)?;
    table.raw_set("stderr", lua.create_string(&result.stderr)?)?;
    table.raw_set("timed_out", result.timed_out)?;
    table.raw_set("truncated", result.truncated)?;
    table.raw_set("error", result.error)?;
    Ok(LuaValue::Table(table))
}

/// What a runtime coroutine yielded: one of `diri.sleep`, `diri.wait` or
/// `diri.exec`, recognised by the private token.
fn parse_yield(lua: &Lua, values: MultiValue) -> mlua::Result<Until> {
    let token: Table = lua.named_registry_value(TOKEN)?;
    let mut values = values.into_iter();
    let ours = matches!(values.next(), Some(LuaValue::Table(first)) if first == token);
    if !ours {
        return Err(mlua::Error::runtime(
            "coroutine.yield outside a coroutine of your own: use diri.sleep or diri.wait to pause",
        ));
    }
    let kind = match values.next() {
        Some(LuaValue::String(kind)) => kind.to_str()?.to_owned(),
        _ => String::new(),
    };
    let number = |value: Option<LuaValue>| match value {
        Some(LuaValue::Integer(number)) => Some(number as f64),
        Some(LuaValue::Number(number)) => Some(number),
        _ => None,
    };
    match kind.as_str() {
        "sleep" => {
            let ms = number(values.next()).unwrap_or(0.0);
            Ok(Until::At(
                Instant::now() + Duration::from_secs_f64(ms / 1000.0),
            ))
        }
        "wait" => {
            let name = match values.next() {
                Some(LuaValue::String(name)) => name.to_str()?.to_owned(),
                _ => String::new(),
            };
            let filter = values.next().unwrap_or(LuaValue::Nil);
            let deadline = number(values.next())
                .map(|ms| Instant::now() + Duration::from_secs_f64(ms / 1000.0));
            Ok(Until::Event {
                name,
                filter,
                deadline,
            })
        }
        "exec" => Ok(Until::Exec(number(values.next()).unwrap_or(0.0) as u64)),
        _ => Err(mlua::Error::runtime("an unknown wait")),
    }
}

/// Runs `run` under `limit`, unless an outer entry already set a deadline.
fn guarded<R>(
    shared: &Shared,
    limit: Duration,
    run: impl FnOnce() -> mlua::Result<R>,
) -> mlua::Result<R> {
    let budget = &shared.budget;
    let outer = budget.deadline.get();
    if outer.is_none() {
        budget.limit.set(limit);
        budget.deadline.set(Some(Instant::now() + limit));
    }
    let result = run();
    if outer.is_none() {
        budget.deadline.set(None);
    }
    result
}

/// SHA-256 of a script's text, hex.
fn digest(source: &str) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(source.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn report(shared: &Shared, what: &str, message: &str) {
    shared.log(Level::Error, &format!("{what}: {message}"));
    let first = message.lines().next().unwrap_or(message).to_owned();
    let now = Instant::now();
    {
        let mut state = shared.state.borrow_mut();
        let key = format!("{what}\u{0}{first}");
        if state
            .last_notices
            .get(&key)
            .is_some_and(|shown| now.duration_since(*shown) < REPEAT_QUIET)
        {
            return;
        }
        if state.last_notices.len() > 64 {
            state.last_notices.clear();
        }
        state.last_notices.insert(key, now);
    }
    shared.host.borrow_mut().notify(Notice {
        level: Level::Error,
        message: format!("Script error in {what}"),
        detail: Some(first),
        logged: true,
    });
}

/// A Lua value as JSON. Whole floats become integers, so `2 * 1.0` passes an
/// `integer` schema.
fn from_lua(lua: &Lua, value: LuaValue) -> mlua::Result<Value> {
    let value: Value = lua.from_value(value)?;
    Ok(normalize(value))
}

fn normalize(value: Value) -> Value {
    match value {
        Value::Number(number) => match number.as_f64() {
            Some(float)
                if number.is_f64()
                    && float.fract() == 0.0
                    && float.abs() < 9_007_199_254_740_992.0 =>
            {
                Value::from(float as i64)
            }
            _ => Value::Number(number),
        },
        Value::Array(items) => Value::Array(items.into_iter().map(normalize).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, normalize(value)))
                .collect(),
        ),
        other => other,
    }
}

fn to_lua(lua: &Lua, value: &Value) -> mlua::Result<LuaValue> {
    lua.to_value_with(
        value,
        mlua::serde::SerializeOptions::new()
            .serialize_none_to_null(false)
            .serialize_unit_to_null(false),
    )
}

fn join(lua: &Lua, values: &MultiValue) -> String {
    values
        .iter()
        .map(|value| match value {
            LuaValue::String(text) => text.to_string_lossy(),
            other => inspect(lua, other),
        })
        .collect::<Vec<_>>()
        .join("\t")
}

/// A readable rendering: JSON for data, a tag for functions and such.
fn inspect(lua: &Lua, value: &LuaValue) -> String {
    match value {
        LuaValue::Nil => "nil".into(),
        LuaValue::Boolean(value) => value.to_string(),
        LuaValue::Integer(value) => value.to_string(),
        LuaValue::Number(value) => value.to_string(),
        LuaValue::String(text) => format!("{:?}", text.to_string_lossy()),
        LuaValue::Table(_) => match lua.from_value::<Value>(value.clone()) {
            Ok(json) => serde_json::to_string_pretty(&json).unwrap_or_default(),
            Err(_) => "<table>".into(),
        },
        LuaValue::Function(_) => "<function>".into(),
        other => format!("<{}>", other.type_name()),
    }
}

fn load_error(error: mlua::Error) -> LoadError {
    LoadError {
        message: without_traceback(describe(&error)),
    }
}

fn without_traceback(message: String) -> String {
    match message.find("\nstack traceback:") {
        Some(end) => message[..end].to_owned(),
        None => message,
    }
}

/// An error as a person reads it: the innermost message, without mlua's
/// "runtime error:" prefix or Rust-side wrapping.
pub(crate) fn describe(error: &mlua::Error) -> String {
    match error {
        mlua::Error::CallbackError { cause, traceback } => {
            let inner = describe(cause);
            if traceback.is_empty() {
                inner
            } else {
                format!("{inner}\n{traceback}")
            }
        }
        mlua::Error::RuntimeError(message) | mlua::Error::SyntaxError { message, .. } => {
            message.clone()
        }
        mlua::Error::MemoryError(message) => format!("out of memory: {message}"),
        other => other.to_string(),
    }
}

/// `os.date` for the formats scripts use (`%Y %m %d %H %M %S %c`), always in
/// UTC; a leading `!` is accepted and ignored. `*t` tables are not offered.
fn date(format: Option<&str>, time: Option<i64>) -> String {
    let seconds = time.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs() as i64)
            .unwrap_or_default()
    });
    let format = format.unwrap_or("%c");
    let format = format.strip_prefix('!').unwrap_or(format);
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (rest / 3600, rest % 3600 / 60, rest % 60);
    let mut out = String::new();
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&year.to_string()),
            Some('m') => out.push_str(&format!("{month:02}")),
            Some('d') => out.push_str(&format!("{day:02}")),
            Some('H') => out.push_str(&format!("{hour:02}")),
            Some('M') => out.push_str(&format!("{minute:02}")),
            Some('S') => out.push_str(&format!("{second:02}")),
            Some('c') => out.push_str(&format!(
                "{year}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC"
            )),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// Howard Hinnant's days-to-civil.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests;
