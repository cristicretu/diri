//! One sandboxed Lua state and the `diri` table it exposes.
//!
//! Sandbox: only the `string`, `table`, `math`, `utf8` and `coroutine`
//! libraries, a reduced `os` (`time`, `clock`, `date`, `difftime`), `load`
//! for text only, and `require` limited to `.lua` files under the script's
//! own directory. There is no `io`, no `os.execute`/`getenv`, no `debug`,
//! no `package`, and no network: everything a script does outside Lua goes
//! through `diri.call`/`diri.action`, the same requests the CLI makes.
//!
//! Every entry into Lua (loading the file, one event handler, one key, one
//! timer) runs under a wall-clock budget that excludes time spent waiting on
//! the Engine; a callback that runs past it is stopped with an error. The
//! state's memory is capped as well.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use diri_proto::app_actions::{AppActionSpec, AppActionsListResult};
use mlua::chunk::ChunkMode;
use mlua::{
    Function, HookTriggers, Lua, LuaOptions, LuaSerdeExt, MultiValue, StdLib, Table,
    Value as LuaValue, VmState,
};
use serde_json::{Value, json};

use crate::{api, events};

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
}

#[derive(Clone, Debug)]
pub struct Options {
    /// The directory `require` may load from: the script's own.
    pub root: Option<PathBuf>,
    /// The budget for one handler, key, command or timer.
    pub callback_budget: Duration,
    /// The budget for running the file itself.
    pub load_budget: Duration,
    pub memory_limit: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            root: None,
            callback_budget: Duration::from_millis(100),
            load_budget: Duration::from_secs(1),
            memory_limit: 64 * 1024 * 1024,
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
}

#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    pub id: u64,
    pub keys: String,
    pub target: BindingTarget,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    pub id: u64,
    pub name: String,
    pub title: String,
    pub target: BindingTarget,
}

/// The keys and palette commands a script added: one layer over the app's
/// keymap, under the shortcuts the person records in Settings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layer {
    pub bindings: Vec<Binding>,
    /// Keys freed from their default command.
    pub unbinds: Vec<String>,
    pub commands: Vec<Command>,
}

impl Layer {
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty() && self.unbinds.is_empty() && self.commands.is_empty()
    }
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
const MIN_REPEAT: Duration = Duration::from_millis(500);
const MAX_TIMEOUT_SECONDS: f64 = 600.0;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_MODULE_BYTES: u64 = 1024 * 1024;
const CALLBACKS: &str = "diri.callbacks";
/// The same error shown again within this window goes to the log only.
const REPEAT_QUIET: Duration = Duration::from_secs(10);

struct Timer {
    id: u64,
    due: Instant,
    every: Option<Duration>,
}

#[derive(Default)]
struct State {
    next_id: u64,
    handlers: Vec<(u64, String)>,
    bindings: Vec<Binding>,
    unbinds: Vec<String>,
    commands: Vec<Command>,
    timers: Vec<Timer>,
    layer_changed: bool,
    stopped: bool,
    actions: Option<Vec<AppActionSpec>>,
    last_notices: HashMap<String, Instant>,
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
}

impl Shared {
    /// A host call, with the time it takes added back to the budget: waiting
    /// on the Engine is not the script's CPU.
    fn host<R>(&self, call: impl FnOnce(&mut dyn Host) -> R) -> R {
        let started = Instant::now();
        let result = call(self.host.borrow_mut().as_mut());
        if let Some(deadline) = self.budget.deadline.get() {
            self.budget.deadline.set(Some(deadline + started.elapsed()));
        }
        result
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

    /// Runs `source` (named `name` in errors) under the load budget.
    pub fn load_source(&self, source: &str, name: &str) -> Result<(), LoadError> {
        let chunk = self
            .lua
            .load(source)
            .set_name(format!("@{name}"))
            .set_mode(ChunkMode::Text);
        self.guarded(self.options.load_budget, || chunk.exec())
            .map_err(load_error)
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
        let values: MultiValue = self
            .guarded(self.options.load_budget, || function.call(()))
            .map_err(|error| without_traceback(describe(&error)))?;
        Ok(values
            .iter()
            .map(|value| inspect(&self.lua, value))
            .collect::<Vec<_>>()
            .join("\t"))
    }

    /// Runs every handler for `event`. Answers how many ran.
    pub fn emit(&self, event: &str, payload: &Value) -> usize {
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
            return 0;
        }
        let Ok(argument) = to_lua(&self.lua, payload) else {
            return 0;
        };
        for id in &ids {
            self.call_callback(*id, argument.clone(), &format!("diri.on(\"{event}\")"));
        }
        ids.len()
    }

    /// A script key was pressed.
    pub fn run_binding(&self, id: u64) {
        let target = self
            .shared
            .state
            .borrow()
            .bindings
            .iter()
            .find(|binding| binding.id == id)
            .map(|binding| (binding.target.clone(), binding.keys.clone()));
        if let Some((target, keys)) = target {
            self.run_target(id, target, &format!("key {keys}"));
        }
    }

    /// A script's palette command was chosen.
    pub fn run_command(&self, id: u64) {
        let target = self
            .shared
            .state
            .borrow()
            .commands
            .iter()
            .find(|command| command.id == id)
            .map(|command| (command.target.clone(), command.name.clone()));
        if let Some((target, name)) = target {
            self.run_target(id, target, &format!("command {name}"));
        }
    }

    fn run_target(&self, id: u64, target: BindingTarget, what: &str) {
        match target {
            BindingTarget::Callback => self.call_callback(id, LuaValue::Nil, what),
            BindingTarget::Action { name, args } => {
                if let Err(error) = perform(&self.shared, &name, args) {
                    self.report(what, &error.to_string());
                }
            }
        }
    }

    /// When the next timer is due.
    pub fn next_timer(&self) -> Option<Instant> {
        self.shared
            .state
            .borrow()
            .timers
            .iter()
            .map(|timer| timer.due)
            .min()
    }

    /// Runs the timers due by `now`.
    pub fn run_timers(&self, now: Instant) {
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
            self.call_callback(id, LuaValue::Nil, "timer");
        }
    }

    /// The keys and commands this script registered.
    pub fn layer(&self) -> Layer {
        let state = self.shared.state.borrow();
        Layer {
            bindings: state.bindings.clone(),
            unbinds: state.unbinds.clone(),
            commands: state.commands.clone(),
        }
    }

    /// The layer, if it changed since the last call.
    pub fn take_layer_change(&self) -> Option<Layer> {
        let changed = std::mem::take(&mut self.shared.state.borrow_mut().layer_changed);
        changed.then(|| self.layer())
    }

    /// Whether any handler needs the Engine's event stream.
    pub fn wants_engine_events(&self) -> bool {
        self.shared
            .state
            .borrow()
            .handlers
            .iter()
            .any(|(_, event)| events::needs_engine(event))
    }

    /// Whether anything is left to wait for: handlers, timers, keys.
    pub fn has_pending_work(&self) -> bool {
        let state = self.shared.state.borrow();
        !state.handlers.is_empty() || !state.timers.is_empty()
    }

    pub fn stopped(&self) -> bool {
        self.shared.state.borrow().stopped
    }

    /// Reports an error to the person (toast, at most once per 10 s for the
    /// same text) and always to the log.
    pub fn report(&self, what: &str, message: &str) {
        report(&self.shared, what, message);
    }

    fn call_callback(&self, id: u64, argument: LuaValue, what: &str) {
        let result = (|| {
            let callbacks: Table = self.lua.named_registry_value(CALLBACKS)?;
            let function: Option<Function> = callbacks.get(id)?;
            match function {
                Some(function) => self.guarded(self.options.callback_budget, || {
                    function.call::<()>(argument)
                }),
                None => Ok(()),
            }
        })();
        if let Err(error) = result {
            self.report(what, &describe(&error));
        }
    }

    fn guarded<R>(
        &self,
        limit: Duration,
        run: impl FnOnce() -> mlua::Result<R>,
    ) -> mlua::Result<R> {
        let budget = &self.shared.budget;
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
        install_values(lua, shared, &diri)?;
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
                .map_err(mlua::Error::runtime)?;
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
    let Some(seconds) = opts
        .map(|opts| opts.get::<Option<f64>>("timeout"))
        .transpose()?
        .flatten()
    else {
        return Ok(DEFAULT_TIMEOUT);
    };
    if !(seconds > 0.0 && seconds <= MAX_TIMEOUT_SECONDS) {
        return Err(mlua::Error::runtime(
            "timeout is seconds, more than 0 and at most 600",
        ));
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
    shared
        .host(|host| host.request(name, params, timeout))
        .map_err(|error| format!("{name}: {error}"))
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
            let name: Option<String> = table.get("action")?;
            let Some(name) = name else {
                return Err(mlua::Error::runtime(format!(
                    "{what}: a table target needs action = \"name\""
                )));
            };
            let args = match table.get::<LuaValue>("args")? {
                LuaValue::Nil => json!({}),
                value => from_lua(lua, value)?,
            };
            Ok((BindingTarget::Action { name, args }, None))
        }
        _ => Err(mlua::Error::runtime(format!(
            "{what}: the target is an action name, {{action=, args=}} or a function"
        ))),
    }
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
        state
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
                let (target, function) = target(lua, value, &what)?;
                check_action(&set_shared, &target, &what);
                let description = opts
                    .map(|opts| opts.get::<Option<String>>("desc"))
                    .transpose()?
                    .flatten();
                let mut state = set_shared.state.borrow_mut();
                let id = state.id();
                if let Some(function) = function {
                    store_callback(lua, id, function)?;
                }
                // The newest binding on a key wins.
                state.bindings.retain(|binding| binding.keys != keys);
                state.bindings.push(Binding {
                    id,
                    keys,
                    target,
                    description,
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
            let mut state = del_shared.state.borrow_mut();
            state.bindings.retain(|binding| binding.keys != keys);
            if !state.unbinds.contains(&keys) {
                state.unbinds.push(keys);
            }
            state.layer_changed = true;
            Ok(())
        })?,
    )?;
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
            let (target, function) = target(lua, run, &what)?;
            check_action(&command_shared, &target, &what);
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
        lua.create_function(move |_, ()| {
            stop_shared.state.borrow_mut().stopped = true;
            Ok(())
        })?,
    )
}

fn install_values(lua: &Lua, shared: &Rc<Shared>, diri: &Table) -> mlua::Result<()> {
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
    diri.raw_set("version", 1)
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
