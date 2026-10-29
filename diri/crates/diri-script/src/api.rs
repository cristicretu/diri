//! The Engine methods a script can call, taken from the same catalog that
//! serves `api.describe` (`diri_proto::api::methods`, generated from the
//! Rust types the Engine decodes). `diri.call` checks parameters against a
//! method's JSON Schema before sending, and the runtime installs one
//! function per method (`diri.session.list()`), so the Lua API cannot drift
//! from the Engine.
//!
//! [`reference_markdown`] and [`lua_definitions`] render the same catalog
//! (plus the app's actions) as the checked-in reference and editor type
//! stubs; a test in the app keeps both files current.

use std::fmt::Write as _;
use std::sync::OnceLock;

use diri_proto::api::ApiMethod;
use diri_proto::app_actions::{ActionRisk, AppActionSpec};
use serde_json::Value;

/// Methods a script may not call, and why. Each would let a script take over
/// something that belongs to the app or the Engine itself.
pub const RESERVED: &[(&str, &str)] = &[
    ("hello", "the connection says hello itself"),
    ("events.subscribe", "listen with diri.on instead"),
    ("events.wait", "listen with diri.on instead"),
    (
        "app.actions.publish",
        "only the window publishes its actions",
    ),
    ("app.reply", "only the window answers its actions"),
    ("client.set_active", "only the window reports its focus"),
    ("hook.report", "only Agents' hooks report"),
    (
        "daemon.prepare_shutdown",
        "stopping the Engine ends every session",
    ),
    ("daemon.shutdown", "stopping the Engine ends every session"),
    (
        "daemon.shutdown_if_idle",
        "stopping the Engine ends every session",
    ),
];

/// Why a script may not call `name`, if it may not.
pub fn reserved(name: &str) -> Option<&'static str> {
    RESERVED
        .iter()
        .find(|(reserved, _)| *reserved == name)
        .map(|(_, why)| *why)
}

/// Every method a script can call, in the catalog's order.
pub fn methods() -> &'static [ApiMethod] {
    static METHODS: OnceLock<Vec<ApiMethod>> = OnceLock::new();
    METHODS.get_or_init(|| {
        diri_proto::api::methods()
            .into_iter()
            .filter(|method| reserved(&method.name).is_none())
            .collect()
    })
}

pub fn method(name: &str) -> Option<&'static ApiMethod> {
    methods().iter().find(|method| method.name == name)
}

/// Checks `params` against `method`'s schema.
pub fn validate(method: &ApiMethod, params: &Value) -> Result<(), String> {
    diri_proto::json_schema::validate(params, &method.params, "params")
}

/// `split-pane-right` → `split_pane_right`, the key `diri.actions` takes.
pub fn lua_action_key(action: &str) -> String {
    action.replace('-', "_")
}

/// `diri.actions.split_pane_right` → `split-pane-right`.
pub fn action_name(lua_key: &str) -> String {
    lua_key.replace('_', "-")
}

/// Core `diri` functions. A method namespace may never shadow one.
pub const CORE: &[&str] = &[
    "on", "off", "call", "action", "actions", "keymap", "command", "notify", "log", "after",
    "every", "stop", "json", "inspect", "methods", "events", "context", "version",
];

/// The API reference: core functions, events, Engine methods, app actions.
pub fn reference_markdown(methods: &[ApiMethod], actions: &[AppActionSpec]) -> String {
    let mut out = String::from(
        "<!-- Generated from the Engine's method catalog and the app's action catalog. \
         Do not edit: run `DIRI_UPDATE_DOCS=1 cargo test -p diri-app scripting_reference` -->\n\n\
         # Diri Lua API reference\n\n\
         The design and the core functions are in [scripting.md](scripting.md). \
         This page lists what is generated: the events `diri.on` accepts, every Engine method \
         a script can call, and every window action.\n\n## Events\n\n| Event | Payload |\n| --- | --- |\n",
    );
    for (name, description) in crate::events::ALL {
        let _ = writeln!(out, "| `{name}` | {} |", escape(description));
    }
    out.push_str(
        "\n## Engine methods\n\n\
         Call one as `diri.call(\"session.list\")` or through its generated function, \
         `diri.session.list()`. Parameters are checked against the method's JSON Schema \
         before they are sent; `dirijor api describe NAME` prints the schema and an example. \
         Required parameters come first; `?` marks an optional one.\n\n\
         | Function | Parameters | Returns | |\n| --- | --- | --- | --- |\n",
    );
    for method in methods {
        let _ = writeln!(
            out,
            "| `diri.{}` | {} | `{}` | {} |",
            method.name,
            signature(&method.params)
                .map(|signature| format!("`{signature}`"))
                .unwrap_or_default(),
            method.result,
            escape(&method.summary)
        );
    }
    out.push_str("\nNot callable from scripts:\n\n");
    for (name, why) in RESERVED {
        let _ = writeln!(out, "- `{name}`: {why}.");
    }
    out.push_str(
        "\n## Window actions\n\n\
         Run one with `diri.action(\"split-pane-right\")` or `diri.actions.split_pane_right()`, \
         bind it with `diri.keymap.set(\"cmd-shift-d\", \"split-pane-right\")`. Actions need \
         the Diri app to be running. Destructive ones close, quit, archive or clear something.\n\n\
         | Action | Arguments | Default key | |\n| --- | --- | --- | --- |\n",
    );
    for action in actions {
        let destructive = if action.risk == ActionRisk::Destructive {
            " **Destructive.**"
        } else {
            ""
        };
        let text = if action.description.is_empty() {
            action.title.as_str()
        } else {
            action.description.as_str()
        };
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {}{} |",
            action.name,
            signature(&action.args)
                .map(|signature| format!("`{signature}`"))
                .unwrap_or_default(),
            action.shortcut.as_deref().unwrap_or(""),
            escape(text),
            destructive
        );
    }
    out
}

/// Type stubs for the Lua language server, so an editor completes and
/// checks `diri.*` calls.
pub fn lua_definitions(methods: &[ApiMethod], actions: &[AppActionSpec]) -> String {
    let mut out = String::from(
        "---@meta diri\n\
         -- Generated from the Engine's method catalog and the app's action catalog.\n\
         -- Do not edit: run `DIRI_UPDATE_DOCS=1 cargo test -p diri-app scripting_reference`.\n\
         -- Point your editor's Lua language server at this directory (workspace.library).\n\n\
         ---@class diri\n\
         diri = {}\n\n\
         ---@alias diri.Event\n",
    );
    for (name, description) in crate::events::ALL {
        let _ = writeln!(out, "---| \"{name}\" # {description}");
    }
    out.push_str(CORE_DEFINITIONS);
    // Namespaces first, each once, so `diri.session.list` type-checks.
    let mut namespaces: Vec<String> = Vec::new();
    for method in methods {
        let parts: Vec<&str> = method.name.split('.').collect();
        for depth in 1..parts.len() {
            let namespace = parts[..depth].join(".");
            if !namespaces.contains(&namespace) {
                let _ = writeln!(out, "diri.{namespace} = {{}}");
                namespaces.push(namespace);
            }
        }
    }
    out.push('\n');
    for method in methods {
        let _ = writeln!(out, "---{}", method.summary);
        let optional = method
            .params
            .get("required")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty);
        let _ = writeln!(
            out,
            "---@param params{} {}",
            if optional { "?" } else { "" },
            lua_table_type(&method.params)
        );
        out.push_str("---@param opts? {timeout?: number}\n");
        let _ = writeln!(out, "---@return table # {}", method.result);
        let _ = writeln!(out, "function diri.{}(params, opts) end\n", method.name);
    }
    out.push_str("---The window's actions (`diri.actions.split_pane_right()`).\n---@class diri.actions\ndiri.actions = {}\n\n");
    for action in actions {
        let text = if action.description.is_empty() {
            action.title.as_str()
        } else {
            action.description.as_str()
        };
        let _ = writeln!(out, "---{text}");
        let optional = action
            .args
            .get("required")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty);
        let _ = writeln!(
            out,
            "---@param args{} {}",
            if optional { "?" } else { "" },
            lua_table_type(&action.args)
        );
        let _ = writeln!(
            out,
            "function diri.actions.{}(args) end\n",
            lua_action_key(&action.name)
        );
    }
    out
}

const CORE_DEFINITIONS: &str = r#"
diri.keymap = {}
diri.json = {}

---Run `fn(payload)` whenever `event` happens. Returns an id for `diri.off`.
---@param event diri.Event
---@param fn fun(payload: table)
---@return integer
function diri.on(event, fn) end

---Stop a handler or timer.
---@param id integer
function diri.off(id) end

---Call an Engine method by name. Parameters are checked against its schema.
---@param method string
---@param params? table
---@param opts? {timeout?: number}
---@return any
function diri.call(method, params, opts) end

---Run a window action by name (needs the Diri app).
---@param name string
---@param args? table
---@return any
function diri.action(name, args) end

---Bind keys to an action name, `{action=, args=}`, or a function. The
---shortcuts a person records in Settings still win over these.
---@param keys string  e.g. "cmd-shift-d" or "ctrl-a c"
---@param target string|table|fun()
---@param opts? {desc?: string}
---@return integer
function diri.keymap.set(keys, target, opts) end

---Free a key from its default command.
---@param keys string
function diri.keymap.del(keys) end

---Add a command to the palette.
---@param spec {name: string, title?: string, run: string|fun(), args?: table}
---@return integer
function diri.command(spec) end

---Show a toast in the window (stderr from `dirijor script`).
---@param message string
---@param opts? {level?: "info"|"success"|"warning"|"error", detail?: string}
function diri.notify(message, opts) end

---Write to the script log.
function diri.log(...) end

---Run `fn` once after `seconds`.
---@param seconds number
---@param fn fun()
---@return integer
function diri.after(seconds, fn) end

---Run `fn` every `seconds` (at least 0.5).
---@param seconds number
---@param fn fun()
---@return integer
function diri.every(seconds, fn) end

---End a `dirijor script` run.
function diri.stop() end

---Render a value as readable text.
---@return string
function diri.inspect(value) end

---@return string
function diri.json.encode(value) end
---@return any
function diri.json.decode(text) end

---Engine method names to one-line summaries.
---@type table<string, string>
diri.methods = {}

---Event names to one-line descriptions.
---@type table<string, string>
diri.events = {}

---"app" inside Diri, "cli" from `dirijor script`.
---@type string
diri.context = ""

"#;

fn lua_table_type(schema: &Value) -> String {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return "table".into();
    };
    if properties.is_empty() {
        return "table".into();
    }
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|keys| keys.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let fields: Vec<String> = properties
        .iter()
        .map(|(key, property)| {
            let optional = if required.contains(&key.as_str()) {
                ""
            } else {
                "?"
            };
            let key = if key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                key.clone()
            } else {
                format!("[\"{key}\"]")
            };
            format!("{key}{optional}: {}", lua_type(property))
        })
        .collect();
    format!("{{{}}}", fields.join(", "))
}

fn lua_type(schema: &Value) -> &'static str {
    let names: Vec<&str> = match schema.get("type") {
        Some(Value::String(name)) => vec![name.as_str()],
        Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).collect(),
        _ => return "any",
    };
    let names: Vec<&str> = names.into_iter().filter(|name| *name != "null").collect();
    match names.as_slice() {
        ["string"] => "string",
        ["integer"] => "integer",
        ["number"] => "number",
        ["boolean"] => "boolean",
        ["array"] => "any[]",
        ["object"] => "table",
        _ => "any",
    }
}

/// `a, b, c?` for an object schema's properties, required first.
fn signature(schema: &Value) -> Option<String> {
    let properties = schema.get("properties").and_then(Value::as_object)?;
    if properties.is_empty() {
        return None;
    }
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|keys| keys.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut keys: Vec<&String> = properties.keys().collect();
    keys.sort_by_key(|key| !required.contains(&key.as_str()));
    Some(
        keys.into_iter()
            .map(|key| {
                if required.contains(&key.as_str()) {
                    key.clone()
                } else {
                    format!("{key}?")
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn escape(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_methods_exist_and_are_never_offered() {
        let all: Vec<String> = diri_proto::api::methods()
            .into_iter()
            .map(|method| method.name)
            .collect();
        for (name, _) in RESERVED {
            assert!(all.iter().any(|method| method == name), "{name} is stale");
            assert!(method(name).is_none());
        }
        assert!(method("session.list").is_some());
        assert_eq!(methods().len(), all.len() - RESERVED.len());
    }

    #[test]
    fn no_method_namespace_shadows_a_core_function() {
        for method in methods() {
            let head = method.name.split('.').next().unwrap();
            assert!(
                !CORE.contains(&head) && method.name.contains('.'),
                "{} would shadow diri.{head}",
                method.name
            );
        }
    }

    #[test]
    fn parameters_are_checked_against_the_method_schema() {
        let spawn = method("session.spawn").unwrap();
        assert!(validate(spawn, &serde_json::json!({"kind": "shell", "cwd": "/tmp"})).is_ok());
        let error = validate(spawn, &serde_json::json!({"cwd": 3})).unwrap_err();
        assert!(error.contains("kind"), "{error}");
    }

    #[test]
    fn action_keys_round_trip() {
        assert_eq!(lua_action_key("split-pane-right"), "split_pane_right");
        assert_eq!(action_name("split_pane_right"), "split-pane-right");
    }
}
