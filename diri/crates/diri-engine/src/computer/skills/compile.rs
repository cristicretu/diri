//! Turning an Agent's draft into a valid skill, and a skill plus arguments
//! into the `run_steps` steps the driver runs.
//!
//! Step string values may hold `{param}` placeholders; `{{` and `}}` are
//! literal braces. A string that is exactly one placeholder of a number or
//! boolean param becomes that JSON value; anywhere else it is spliced in as
//! text.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use super::{Param, ParamKind};

/// What a `run_steps` step may run in Cua Driver 0.38.4 (and 0.53).
pub(super) const BATCHABLE_TOOLS: &[&str] = &[
    "click",
    "double_click",
    "right_click",
    "set_value",
    "set_state",
    "clear_value",
    "scroll_to",
    "type_text",
    "press_key",
    "hotkey",
    "scroll",
    "drag",
    "move_cursor",
];
/// The driver's own cap on one batch.
pub(super) const MAX_STEPS: usize = 64;
/// Step keys the driver understands, plus Diri's own (`intent`, `pixel`),
/// which are stripped before sending.
const STEP_KEYS: &[&str] = &["tool", "args", "wait_for", "expect", "timeout_ms"];
const DIRI_STEP_KEYS: &[&str] = &["intent", "pixel"];
/// Arguments that name one launch of an app; a skill names apps and
/// elements by selector instead, so it outlives the launch it was learned in.
const LAUNCH_BOUND_KEYS: &[&str] = &[
    "element_token",
    "element_index",
    "pid",
    "window_id",
    "session",
];

#[derive(Clone, Debug, PartialEq)]
enum Piece {
    Text(String),
    Param(String),
}

/// Splits a template into text and placeholders.
fn parse_template(text: &str) -> Result<Vec<Piece>, String> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                literal.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                literal.push('}');
            }
            '{' => {
                let mut name = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    name.push(c);
                }
                if !closed || !valid_param_name(&name) {
                    return Err(format!(
                        "{text:?} has a brace that is not a {{param}} placeholder; write {{{{ and }}}} for literal braces"
                    ));
                }
                if !literal.is_empty() {
                    pieces.push(Piece::Text(std::mem::take(&mut literal)));
                }
                pieces.push(Piece::Param(name));
            }
            '}' => {
                return Err(format!(
                    "{text:?} has a lone }}; write }}}} for a literal brace"
                ));
            }
            c => literal.push(c),
        }
    }
    if !literal.is_empty() {
        pieces.push(Piece::Text(literal));
    }
    Ok(pieces)
}

pub(super) fn valid_param_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 40
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Every placeholder in every string of `value`.
fn placeholders(value: &Value, out: &mut BTreeSet<String>, problems: &mut Vec<String>) {
    match value {
        Value::String(text) => match parse_template(text) {
            Ok(pieces) => out.extend(pieces.into_iter().filter_map(|piece| match piece {
                Piece::Param(name) => Some(name),
                Piece::Text(_) => None,
            })),
            Err(problem) => problems.push(problem),
        },
        Value::Array(items) => items
            .iter()
            .for_each(|item| placeholders(item, out, problems)),
        Value::Object(map) => map
            .values()
            .for_each(|item| placeholders(item, out, problems)),
        _ => {}
    }
}

/// Fills placeholders from `args` (already checked against the params).
pub(super) fn substitute(value: &Value, args: &Map<String, Value>) -> Value {
    match value {
        Value::String(text) => {
            let Ok(pieces) = parse_template(text) else {
                return value.clone();
            };
            if let [Piece::Param(name)] = pieces.as_slice()
                && let Some(arg) = args.get(name)
                && !arg.is_string()
            {
                return arg.clone();
            }
            let mut out = String::new();
            for piece in pieces {
                match piece {
                    Piece::Text(text) => out.push_str(&text),
                    Piece::Param(name) => match args.get(&name) {
                        Some(Value::String(arg)) => out.push_str(arg),
                        Some(other) => out.push_str(&other.to_string()),
                        None => {}
                    },
                }
            }
            Value::String(out)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| substitute(item, args)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), substitute(item, args)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Checks one run's arguments against the declared params.
pub(super) fn check_args(params: &[Param], args: &Value) -> Result<Map<String, Value>, String> {
    let args = match args {
        Value::Null => Map::new(),
        Value::Object(map) => map.clone(),
        _ => return Err("args must be an object of param values".into()),
    };
    let mut problems = Vec::new();
    for param in params {
        match args.get(&param.name) {
            None => problems.push(format!("missing {}", param.name)),
            Some(value) => {
                let fits = match param.kind {
                    ParamKind::String => value.is_string(),
                    ParamKind::Number => value.is_number(),
                    ParamKind::Boolean => value.is_boolean(),
                };
                if !fits {
                    problems.push(format!("{} must be a {}", param.name, param.kind.as_str()));
                }
            }
        }
    }
    for key in args.keys() {
        if !params.iter().any(|param| &param.name == key) {
            problems.push(format!("{key} is not a param of this skill"));
        }
    }
    if problems.is_empty() {
        Ok(args)
    } else {
        Err(problems.join("; "))
    }
}

/// One step in the canonical `{tool, args, ...}` shape (or a check-only or
/// `{wait: ...}` step), with every problem found. `index` is 1-based.
fn normalize_step(raw: &Value, index: usize, problems: &mut Vec<String>) -> Value {
    let at = format!("step {index}");
    let Some(map) = raw.as_object() else {
        problems.push(format!("{at} is not an object"));
        return raw.clone();
    };
    let mut step = Map::new();
    let mut action: Option<(String, Value)> = None;
    for (key, value) in map {
        match key.as_str() {
            "tool" => match value.as_str() {
                Some(tool) => {
                    let args = map
                        .get("args")
                        .cloned()
                        .unwrap_or_else(|| Value::Object(Map::new()));
                    action = Some((tool.to_owned(), args));
                }
                None => problems.push(format!("{at}: tool must be a string")),
            },
            "args" => {}
            "wait" if value.is_object() => {
                step.insert("wait".into(), value.clone());
            }
            k if STEP_KEYS.contains(&k) || DIRI_STEP_KEYS.contains(&k) => {
                step.insert(key.clone(), value.clone());
            }
            k if BATCHABLE_TOOLS.contains(&k) => {
                if action.is_some() {
                    problems.push(format!("{at} names more than one action"));
                }
                action = Some((key.clone(), value.clone()));
            }
            other => problems.push(format!("{at}: unknown key {other:?}")),
        }
    }
    if let Some((tool, args)) = action {
        if !BATCHABLE_TOOLS.contains(&tool.as_str()) {
            problems.push(format!(
                "{at}: {tool} cannot run in a skill; steps use {}",
                BATCHABLE_TOOLS.join(", ")
            ));
        }
        if !args.is_object() {
            problems.push(format!("{at}: args must be an object"));
        }
        let pixel = map.get("pixel").and_then(Value::as_bool) == Some(true);
        if args.get("x").is_some() && args.get("y").is_some() && !pixel {
            problems.push(format!(
                "{at} clicks raw pixels; target it by role/name, or mark it \"pixel\": true if no selector reaches it"
            ));
        }
        step.insert("tool".into(), Value::String(tool));
        step.insert("args".into(), args);
    } else if !step.contains_key("wait")
        && !step.contains_key("wait_for")
        && !step.contains_key("expect")
    {
        problems.push(format!("{at} has no action and no check"));
    }
    if let Some(intent) = step.get("intent")
        && !intent.is_string()
    {
        problems.push(format!("{at}: intent must be a sentence"));
    }
    launch_bound(&Value::Object(step.clone()), &at, problems);
    if takes_focus(&Value::Object(step.clone())) {
        problems.push(format!(
            "{at} asks for the foreground; skills run in the background only"
        ));
    }
    Value::Object(step)
}

fn launch_bound(value: &Value, at: &str, problems: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                if LAUNCH_BOUND_KEYS.contains(&key.as_str()) {
                    problems.push(format!(
                        "{at} uses {key}, which only lasts one launch; name the target by role/name/label/nth and the window by app/window"
                    ));
                } else {
                    launch_bound(item, at, problems);
                }
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|item| launch_bound(item, at, problems)),
        _ => {}
    }
}

fn takes_focus(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.get("delivery_mode").and_then(Value::as_str) == Some("foreground")
                || map.get("foreground_fallback").and_then(Value::as_bool) == Some(true)
                || map.values().any(takes_focus)
        }
        Value::Array(items) => items.iter().any(takes_focus),
        _ => false,
    }
}

fn normalize_check(raw: &Value, label: &str, problems: &mut Vec<String>) -> Value {
    if !raw.is_object() {
        problems.push(format!("{label} is not a check object"));
    }
    launch_bound(raw, label, problems);
    raw.clone()
}

/// The parts of a draft the compiler checks.
pub(super) struct Compiled {
    pub steps: Vec<Value>,
    pub preconditions: Vec<Value>,
    pub postconditions: Vec<Value>,
}

/// Validates a draft: batchable tools only, selectors not tokens, at most
/// [`MAX_STEPS`] steps, every param used and every placeholder declared.
pub(super) fn compile(
    params: &[Param],
    steps: &[Value],
    preconditions: &[Value],
    postconditions: &[Value],
) -> Result<Compiled, Vec<String>> {
    let mut problems = Vec::new();
    if steps.is_empty() {
        problems.push("a skill needs at least one step".into());
    }
    let total = steps.len() + preconditions.len() + postconditions.len();
    if total > MAX_STEPS {
        problems.push(format!(
            "{total} steps and checks; a skill holds at most {MAX_STEPS}"
        ));
    }
    let mut seen = BTreeSet::new();
    for param in params {
        if !valid_param_name(&param.name) {
            problems.push(format!("param {:?} must be snake_case", param.name));
        }
        if !seen.insert(param.name.clone()) {
            problems.push(format!("param {} is declared twice", param.name));
        }
    }
    let steps: Vec<Value> = steps
        .iter()
        .enumerate()
        .map(|(i, step)| normalize_step(step, i + 1, &mut problems))
        .collect();
    let preconditions: Vec<Value> = preconditions
        .iter()
        .enumerate()
        .map(|(i, check)| normalize_check(check, &format!("precondition {}", i + 1), &mut problems))
        .collect();
    let postconditions: Vec<Value> = postconditions
        .iter()
        .enumerate()
        .map(|(i, check)| {
            normalize_check(check, &format!("postcondition {}", i + 1), &mut problems)
        })
        .collect();
    let mut used = BTreeSet::new();
    for value in steps.iter().chain(&preconditions).chain(&postconditions) {
        // Intents are prose for the repairing Agent, not sent.
        let mut value = value.clone();
        if let Some(map) = value.as_object_mut() {
            map.remove("intent");
        }
        placeholders(&value, &mut used, &mut problems);
    }
    for name in &used {
        if !seen.contains(name) {
            problems.push(format!("{{{name}}} is used but not declared in params"));
        }
    }
    for name in &seen {
        if !used.contains(name) {
            problems.push(format!("param {name} is declared but no step uses it"));
        }
    }
    if problems.is_empty() {
        Ok(Compiled {
            steps,
            preconditions,
            postconditions,
        })
    } else {
        Err(problems)
    }
}

/// A precondition or postcondition as a check-only step.
pub(super) fn check_step(check: &Value, phase: &str) -> Value {
    let mut step = Map::new();
    step.insert(phase.into(), check.clone());
    Value::Object(step)
}

/// A skill step as the driver takes it: Diri's own keys removed.
pub(super) fn driver_step(step: &Value, args: &Map<String, Value>) -> Value {
    let mut step = substitute(step, args);
    if let Some(map) = step.as_object_mut() {
        for key in DIRI_STEP_KEYS {
            map.remove(*key);
        }
    }
    step
}

/// Whether any step had to fall back to pixels.
pub(super) fn has_pixel_steps(steps: &[Value]) -> bool {
    steps
        .iter()
        .any(|step| step.get("pixel").and_then(Value::as_bool) == Some(true))
}
