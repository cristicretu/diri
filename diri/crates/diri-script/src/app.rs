//! Native Diri Apps: an agent's `app.lua`, run in a fresh sandbox per call.
//!
//! An app defines up to three functions:
//!
//! - `render(state, data, ctx)` returns a tree of components built with the
//!   `ui` table (`ui.row{...}`, `ui.text("hi", {size = 20})`,
//!   `ui.canvas{...}`). Diri validates it against the surface bounds and
//!   draws it with GPUI: no web view, no other process.
//! - `on_event(state, event, data, ctx)` runs when the person clicks, ticks,
//!   chooses or types. It returns the new state (or changes `state` in
//!   place), optionally followed by a list of actions (`{kind =
//!   "open_url", url = ...}`), which pass the surface's click gate.
//! - `sync(ctx)` returns new data (`data.json`). It may read its own folder
//!   (`ctx.read`) and Diri state (`ctx.sessions`); `ctx.exec` and
//!   `ctx.fetch` work only when the person approved this exact `app.lua`
//!   ([`digest`]), so an edited app is asked again.
//!
//! Every call gets its own Lua state with only `string`, `table`, `math` and
//! `utf8`, text chunks only, a memory cap and an instruction budget checked
//! every 1000 instructions. The app never runs on the UI thread: a slow or
//! looping app stops with an error, and the window keeps drawing.

use std::path::Path;
use std::time::{Duration, Instant};

use mlua::{
    Function, HookTriggers, Lua, LuaOptions, LuaSerdeExt, MultiValue, StdLib, Table,
    Value as LuaValue, VmState,
};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use crate::runtime::exec;

/// How much one call may use.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub memory: usize,
    /// Wall time, checked every thousand instructions.
    pub time: Duration,
    /// Lua instructions, counted in thousands.
    pub instructions: u64,
}

/// For `render` and `on_event`: a frame's worth of work, generously.
pub const INTERACTIVE: Limits = Limits {
    memory: 32 * 1024 * 1024,
    time: Duration::from_millis(250),
    instructions: 20_000_000,
};

/// For `sync`: the time is the spec's timeout; processes it starts count
/// against it too.
pub fn sync_limits(timeout: Duration) -> Limits {
    Limits {
        memory: 64 * 1024 * 1024,
        time: timeout,
        instructions: 200_000_000,
    }
}

/// The most a tree or data set may serialize to.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// SHA-256 of an app's source, hex: the identity an approval is bound to.
pub fn digest(source: &str) -> String {
    Sha256::digest(source.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// One app, as a call sees it.
#[derive(Clone, Copy, Debug)]
pub struct App<'a> {
    pub source: &'a str,
    /// The surface folder: `ctx.read` and processes stay here.
    pub dir: &'a Path,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppError {
    /// `lua`, `budget`, `memory`, `missing`, `output`, `approval_required`.
    pub code: &'static str,
    pub message: String,
}

impl AppError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AppError {}

/// What `render` produced.
#[derive(Clone, Debug, PartialEq)]
pub struct Rendered {
    /// The component list (a single node is wrapped in a list).
    pub root: Value,
    /// The app's `app` table, if it set one: title, subtitle, icon, accent,
    /// style.
    pub meta: Option<Value>,
}

/// What `on_event` produced.
#[derive(Clone, Debug, PartialEq)]
pub struct Handled {
    pub state: Value,
    /// Actions to pass through the click gate (the event was a click).
    pub actions: Vec<Value>,
}

/// What `sync` may do beyond its own folder.
#[derive(Clone, Copy, Debug)]
pub struct SyncCaps {
    /// The person approved this exact source (see [`digest`]).
    pub approved: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Synced {
    /// `None`: `sync` returned nothing; keep the data as it is.
    pub data: Option<Value>,
    /// `ctx.exec` or `ctx.fetch` was refused for want of approval.
    pub needs_approval: bool,
    pub log: Vec<String>,
}

const PRELUDE: &str = include_str!("app_prelude.lua");

struct Budget {
    deadline: Instant,
    remaining: std::cell::Cell<u64>,
    limits: Limits,
}

fn sandbox(limits: Limits) -> Result<Lua, AppError> {
    let lua = Lua::new_with(
        StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8,
        LuaOptions::default(),
    )
    .map_err(lua_error)?;
    lua.set_memory_limit(limits.memory).map_err(lua_error)?;
    let budget = std::rc::Rc::new(Budget {
        deadline: Instant::now() + limits.time,
        remaining: std::cell::Cell::new(limits.instructions / 1000),
        limits,
    });
    let hook = std::rc::Rc::clone(&budget);
    lua.set_global_hook(
        HookTriggers::new().every_nth_instruction(1000),
        move |_, _| {
            let left = hook.remaining.get();
            if left == 0 {
                return Err(mlua::Error::runtime(format!(
                    "budget: stopped after {} instructions",
                    hook.limits.instructions
                )));
            }
            hook.remaining.set(left - 1);
            if Instant::now() > hook.deadline {
                return Err(mlua::Error::runtime(format!(
                    "budget: stopped after {} ms",
                    hook.limits.time.as_millis()
                )));
            }
            Ok(VmState::Continue)
        },
    )
    .map_err(lua_error)?;
    let globals = lua.globals();
    for name in [
        "dofile",
        "loadfile",
        "load",
        "require",
        "collectgarbage",
        "print",
    ] {
        globals.raw_set(name, LuaValue::Nil).map_err(lua_error)?;
    }
    let string: Table = globals.get("string").map_err(lua_error)?;
    string.raw_set("dump", LuaValue::Nil).map_err(lua_error)?;
    globals
        .raw_set("__array_mt", lua.array_metatable())
        .map_err(lua_error)?;
    lua.load(PRELUDE)
        .set_name("=ui")
        .exec()
        .map_err(lua_error)?;
    Ok(lua)
}

fn lua_error(error: mlua::Error) -> AppError {
    let message = error.to_string();
    let code = if message.contains("budget: stopped") {
        "budget"
    } else if matches!(error, mlua::Error::MemoryError(_)) || message.contains("not enough memory")
    {
        "memory"
    } else {
        "lua"
    };
    // Keep the first line with the location; tracebacks name the sandbox.
    let first = message.lines().next().unwrap_or_default().to_owned();
    AppError::new(code, first)
}

fn load(lua: &Lua, app: App<'_>) -> Result<(), AppError> {
    // Text only: a binary chunk can break out of the sandbox.
    lua.load(app.source)
        .set_name("=app.lua")
        .set_mode(mlua::prelude::LuaChunkMode::Text)
        .exec()
        .map_err(lua_error)
}

fn function(lua: &Lua, name: &str) -> Result<Option<Function>, AppError> {
    match lua.globals().get::<LuaValue>(name).map_err(lua_error)? {
        LuaValue::Function(function) => Ok(Some(function)),
        LuaValue::Nil => Ok(None),
        _ => Err(AppError::new("lua", format!("{name} is not a function"))),
    }
}

fn to_lua(lua: &Lua, value: &Value) -> Result<LuaValue, AppError> {
    lua.to_value(value).map_err(lua_error)
}

fn from_lua(lua: &Lua, value: LuaValue) -> Result<Value, AppError> {
    let mut value: Value = lua
        .from_value(value)
        .map_err(|error| AppError::new("output", error.to_string()))?;
    normalize(&mut value);
    let size = serde_json::to_vec(&value).map_or(usize::MAX, |bytes| bytes.len());
    if size > MAX_OUTPUT_BYTES {
        return Err(AppError::new(
            "output",
            format!("{size} bytes; the limit is {MAX_OUTPUT_BYTES}"),
        ));
    }
    Ok(value)
}

/// Lists Lua left empty serialize as `{}`; the fields that are lists become
/// `[]`, so an empty row is still a row.
fn normalize(value: &mut Value) {
    const LISTS: &[&str] = &[
        "children", "items", "shapes", "options", "tabs", "columns", "rows", "points",
    ];
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if LISTS.contains(&key.as_str()) && child.as_object().is_some_and(Map::is_empty) {
                    *child = Value::Array(Vec::new());
                } else {
                    normalize(child);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalize),
        _ => {}
    }
}

/// The app's initial state: `init(ctx)` when it defines one, else `{}`.
fn initial_state(lua: &Lua, state: &Value, ctx: &LuaValue) -> Result<LuaValue, AppError> {
    if !state.is_null() {
        return to_lua(lua, state);
    }
    match function(lua, "init")? {
        Some(init) => init.call(ctx.clone()).map_err(lua_error),
        None => Ok(LuaValue::Table(lua.create_table().map_err(lua_error)?)),
    }
}

fn meta(lua: &Lua) -> Result<Option<Value>, AppError> {
    match lua.globals().get::<LuaValue>("app").map_err(lua_error)? {
        LuaValue::Table(table) => from_lua(lua, LuaValue::Table(table)).map(Some),
        _ => Ok(None),
    }
}

/// Runs `render(state, data, ctx)`.
pub fn render(
    app: App<'_>,
    state: &Value,
    data: &Value,
    ctx: &Value,
    limits: Limits,
) -> Result<Rendered, AppError> {
    let lua = sandbox(limits)?;
    load(&lua, app)?;
    let render = function(&lua, "render")?
        .ok_or_else(|| AppError::new("missing", "app.lua defines no render(state, data, ctx)"))?;
    let ctx = to_lua(&lua, ctx)?;
    let state = initial_state(&lua, state, &ctx)?;
    let tree: LuaValue = render
        .call((state, to_lua(&lua, data)?, ctx))
        .map_err(lua_error)?;
    let tree = from_lua(&lua, tree)?;
    let root = match tree {
        Value::Array(_) => tree,
        Value::Object(_) => Value::Array(vec![tree]),
        Value::Null => Value::Array(Vec::new()),
        _ => {
            return Err(AppError::new(
                "output",
                "render returns a component or a list",
            ));
        }
    };
    Ok(Rendered {
        root,
        meta: meta(&lua)?,
    })
}

/// The state a fresh app starts from (`init(ctx)` or `{}`).
pub fn init(app: App<'_>, ctx: &Value, limits: Limits) -> Result<Value, AppError> {
    let lua = sandbox(limits)?;
    load(&lua, app)?;
    let ctx = to_lua(&lua, ctx)?;
    let state = initial_state(&lua, &Value::Null, &ctx)?;
    from_lua(&lua, state)
}

/// Runs `on_event(state, event, data, ctx)`. An app without `on_event`
/// keeps its state.
pub fn handle_event(
    app: App<'_>,
    state: &Value,
    event: &Value,
    data: &Value,
    ctx: &Value,
    limits: Limits,
) -> Result<Handled, AppError> {
    let lua = sandbox(limits)?;
    load(&lua, app)?;
    let ctx = to_lua(&lua, ctx)?;
    let state = initial_state(&lua, state, &ctx)?;
    let Some(handler) = function(&lua, "on_event")? else {
        return Ok(Handled {
            state: from_lua(&lua, state)?,
            actions: Vec::new(),
        });
    };
    let returned: MultiValue = handler
        .call((
            state.clone(),
            to_lua(&lua, event)?,
            to_lua(&lua, data)?,
            ctx,
        ))
        .map_err(lua_error)?;
    let mut values = returned.into_iter();
    let next = match values.next() {
        Some(LuaValue::Nil) | None => state,
        Some(value) => value,
    };
    let actions = match values.next() {
        Some(LuaValue::Nil) | None => Vec::new(),
        Some(value) => match from_lua(&lua, value)? {
            Value::Array(actions) => actions,
            Value::Object(map) if map.is_empty() => Vec::new(),
            action @ Value::Object(_) => vec![action],
            _ => {
                return Err(AppError::new(
                    "output",
                    "on_event's second result is a list of actions",
                ));
            }
        },
    };
    let state = from_lua(&lua, next)?;
    if !state.is_object() {
        return Err(AppError::new("output", "on_event returns the state table"));
    }
    Ok(Handled { state, actions })
}

/// Runs `sync(ctx)`. `ctx` carries `data`, `now` and `sessions`; this adds
/// `ctx.read`, `ctx.log`, and the approval-gated `ctx.exec` and `ctx.fetch`.
pub fn sync(app: App<'_>, ctx: &Value, caps: SyncCaps, limits: Limits) -> Result<Synced, AppError> {
    let lua = sandbox(limits)?;
    load(&lua, app)?;
    let Some(sync) = function(&lua, "sync")? else {
        return Ok(Synced {
            data: None,
            needs_approval: false,
            log: Vec::new(),
        });
    };
    let ctx = match to_lua(&lua, ctx)? {
        LuaValue::Table(table) => table,
        _ => lua.create_table().map_err(lua_error)?,
    };
    let refused = std::rc::Rc::new(std::cell::Cell::new(false));
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let deadline = Instant::now() + limits.time;
    let dir = app.dir.to_path_buf();

    let read_dir = dir.clone();
    ctx.raw_set(
        "read",
        lua.create_function(move |_, path: String| {
            if !diri_proto::surface::identity::is_valid_app_path(&path) {
                return Err(mlua::Error::runtime(format!(
                    "read: {path:?} is not a file in this app's folder"
                )));
            }
            let file = read_dir.join(&path);
            match std::fs::metadata(&file) {
                Ok(meta) if meta.len() as usize > MAX_OUTPUT_BYTES => {
                    Err(mlua::Error::runtime("read: file is over 1 MiB"))
                }
                Ok(_) => std::fs::read_to_string(&file).map_err(mlua::Error::external),
                Err(_) => Ok(String::new()),
            }
        })
        .map_err(lua_error)?,
    )
    .map_err(lua_error)?;

    let lines = std::rc::Rc::clone(&log);
    ctx.raw_set(
        "log",
        lua.create_function(move |_, message: String| {
            let mut lines = lines.borrow_mut();
            if lines.len() < 100 {
                lines.push(message.chars().take(500).collect());
            }
            Ok(())
        })
        .map_err(lua_error)?,
    )
    .map_err(lua_error)?;

    let exec_dir = dir.clone();
    let exec_refused = std::rc::Rc::clone(&refused);
    ctx.raw_set(
        "exec",
        lua.create_function(move |lua, (argv, options): (Vec<String>, Option<Table>)| {
            if !caps.approved {
                exec_refused.set(true);
                return Err(mlua::Error::runtime(
                    "approval_required: ctx.exec runs only after the person approves this app",
                ));
            }
            let stdin = match &options {
                Some(options) => options.get::<Option<String>>("stdin")?,
                None => None,
            };
            let result = run(&exec_dir, argv, stdin, deadline)?;
            finished_table(lua, result)
        })
        .map_err(lua_error)?,
    )
    .map_err(lua_error)?;

    let fetch_dir = dir.clone();
    let fetch_refused = std::rc::Rc::clone(&refused);
    ctx.raw_set(
        "fetch",
        lua.create_function(move |lua, (url, options): (String, Option<Table>)| {
            if !caps.approved {
                fetch_refused.set(true);
                return Err(mlua::Error::runtime(
                    "approval_required: ctx.fetch runs only after the person approves this app",
                ));
            }
            if !url.starts_with("https://")
                || url.chars().any(|c| c.is_control() || c.is_whitespace())
            {
                return Err(mlua::Error::runtime("fetch: https URLs only"));
            }
            let mut argv = vec![
                "curl".to_owned(),
                "--silent".into(),
                "--show-error".into(),
                "--location".into(),
                "--max-redirs".into(),
                "3".into(),
                "--proto".into(),
                "=https".into(),
                "--max-filesize".into(),
                MAX_OUTPUT_BYTES.to_string(),
                "--write-out".into(),
                "\n%{http_code}".into(),
            ];
            if let Some(options) = &options
                && let Some(headers) = options.get::<Option<Table>>("headers")?
            {
                for pair in headers.pairs::<String, String>() {
                    let (name, value) = pair?;
                    if name.contains(['\r', '\n', ':']) || value.contains(['\r', '\n']) {
                        return Err(mlua::Error::runtime("fetch: invalid header"));
                    }
                    argv.push("--header".into());
                    argv.push(format!("{name}: {value}"));
                }
            }
            argv.push("--".into());
            argv.push(url);
            let result = run(&fetch_dir, argv, None, deadline)?;
            let stdout = String::from_utf8_lossy(&result.stdout).into_owned();
            let (body, status) = stdout.rsplit_once('\n').unwrap_or((&stdout, "0"));
            let table = lua.create_table()?;
            table.raw_set("status", status.trim().parse::<i64>().unwrap_or(0))?;
            table.raw_set("body", body)?;
            if let Some(error) = result.error.or_else(|| {
                (!result.stderr.is_empty())
                    .then(|| String::from_utf8_lossy(&result.stderr).into_owned())
            }) {
                table.raw_set("error", error)?;
            }
            Ok(table)
        })
        .map_err(lua_error)?,
    )
    .map_err(lua_error)?;

    let result = sync.call::<LuaValue>(ctx);
    let log = log.borrow().clone();
    match result {
        Ok(LuaValue::Nil) => Ok(Synced {
            data: None,
            needs_approval: refused.get(),
            log,
        }),
        Ok(value) => Ok(Synced {
            data: Some(from_lua(&lua, value)?),
            needs_approval: refused.get(),
            log,
        }),
        Err(_) if refused.get() => Ok(Synced {
            data: None,
            needs_approval: true,
            log,
        }),
        Err(error) => Err(lua_error(error)),
    }
}

fn run(
    dir: &Path,
    argv: Vec<String>,
    stdin: Option<String>,
    deadline: Instant,
) -> mlua::Result<exec::Finished> {
    if argv.is_empty() || argv.len() > 64 {
        return Err(mlua::Error::runtime("exec: argv is 1-64 strings"));
    }
    let timeout = deadline.saturating_duration_since(Instant::now());
    if timeout.is_zero() {
        return Err(mlua::Error::runtime("budget: sync ran out of time"));
    }
    // A minimal environment: no Diri session, socket or token reaches the
    // process, only what finding programs needs.
    let mut env = vec![("DIRI_APP_DIR".to_owned(), dir.display().to_string())];
    for name in ["PATH", "HOME", "LANG", "TMPDIR"] {
        if let Ok(value) = std::env::var(name) {
            env.push((name.to_owned(), value));
        }
    }
    exec::run_blocking(exec::Spec {
        argv,
        cwd: Some(dir.to_path_buf()),
        env,
        stdin: stdin.map(String::into_bytes),
        timeout,
        clear_env: true,
    })
    .map_err(mlua::Error::runtime)
}

fn finished_table(lua: &Lua, result: exec::Finished) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.raw_set("code", result.code)?;
    table.raw_set("stdout", lua.create_string(&result.stdout)?)?;
    table.raw_set("stderr", lua.create_string(&result.stderr)?)?;
    table.raw_set("timed_out", result.timed_out)?;
    table.raw_set("truncated", result.truncated)?;
    if let Some(error) = result.error {
        table.raw_set("error", error)?;
    }
    Ok(table)
}

/// A context for calls made without Diri around it (tests, `dirijor`).
pub fn plain_context(now_ms: u64) -> Value {
    json!({"now": now_ms, "theme": "dark", "sessions": [], "width": 900})
}

#[cfg(test)]
mod tests;
