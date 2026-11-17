//! Learn mode: while (and only while) a session is learning a skill, its
//! `computer` actions are written down, with element tokens turned into
//! selectors from the window reads the same session made. Held in memory,
//! bounded, never persisted; dropped on save, abandon, or session end.

use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;

use serde_json::{Map, Value, json};

use super::AppRef;
use super::compile::BATCHABLE_TOOLS;

/// Most actions one learning pass records.
const MAX_ENTRIES: usize = 200;
/// Window reads remembered for token lookups.
const MAX_SNAPSHOTS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Row {
    pub role: String,
    pub label: String,
}

#[derive(Clone, Debug)]
struct Snapshot {
    id: String,
    pid: i64,
    window_id: i64,
    title: Option<String>,
    rows: BTreeMap<u64, Row>,
}

/// One action the Agent took, as a draft skill step.
#[derive(Clone, Debug)]
struct Entry {
    step: Value,
    problems: Vec<String>,
}

pub(super) struct Learning {
    pub app: AppRef,
    pub goal: String,
    pub started: Instant,
    entries: Vec<Entry>,
    snapshots: VecDeque<Snapshot>,
    /// The first window the session read: steps in it need no `window`.
    home_title: Option<String>,
    reads: usize,
    failed: usize,
    skipped: Vec<String>,
    overflow: bool,
}

impl Learning {
    pub fn new(app: AppRef, goal: String) -> Self {
        Self {
            app,
            goal,
            started: Instant::now(),
            entries: Vec::new(),
            snapshots: VecDeque::new(),
            home_title: None,
            reads: 0,
            failed: 0,
            skipped: Vec::new(),
            overflow: false,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// One `computer` call and how it ended. `result` is the relayed MCP
    /// result (`{"mcpToolResult": ...}`) when the call got that far.
    pub fn record(&mut self, tool: &str, args: &Map<String, Value>, result: Option<&Value>) {
        let mcp = result.map(|result| &result["mcpToolResult"]);
        let ok = mcp.is_some_and(|mcp| mcp["isError"] != true);
        let structured = mcp.map(|mcp| &mcp["structuredContent"]);
        match tool {
            "get_window_state" => {
                self.reads += 1;
                if ok && let Some(structured) = structured {
                    self.ingest_window_state(args, structured);
                }
            }
            "run_steps" => {
                let steps = args.get("steps").and_then(Value::as_array);
                let reports = structured.and_then(|s| s["steps"].as_array());
                for (i, step) in steps.into_iter().flatten().enumerate() {
                    let step_ok = reports
                        .and_then(|reports| reports.get(i))
                        .is_some_and(|report| report["ok"] == true);
                    if !step_ok {
                        // Steps after the failure never ran.
                        self.failed += 1;
                        break;
                    }
                    self.record_step(step);
                }
            }
            tool if BATCHABLE_TOOLS.contains(&tool) => {
                if ok {
                    self.push(tool, args.clone(), None);
                } else {
                    self.failed += 1;
                }
            }
            tool if super::super::READ_ONLY_TOOLS.contains(&tool) || tool == "tools" => {
                self.reads += 1;
            }
            other => {
                if ok && !self.skipped.iter().any(|s| s == other) {
                    self.skipped.push(other.to_owned());
                }
            }
        }
    }

    /// One `run_steps` step that ran, in either step shape.
    fn record_step(&mut self, step: &Value) {
        let Some(map) = step.as_object() else { return };
        let (tool, args) = if let Some(tool) = map.get("tool").and_then(Value::as_str) {
            (
                tool.to_owned(),
                map.get("args")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default(),
            )
        } else if let Some((tool, args)) = map
            .iter()
            .find(|(key, _)| BATCHABLE_TOOLS.contains(&key.as_str()))
        {
            (tool.clone(), args.as_object().cloned().unwrap_or_default())
        } else {
            // A check or wait step: kept as written.
            let mut step = map.clone();
            step.remove("pid");
            step.remove("window_id");
            self.push_entry(Entry {
                step: Value::Object(step),
                problems: Vec::new(),
            });
            return;
        };
        let checks: Map<String, Value> = map
            .iter()
            .filter(|(key, _)| matches!(key.as_str(), "wait_for" | "expect" | "timeout_ms"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if BATCHABLE_TOOLS.contains(&tool.as_str()) {
            self.push(&tool, args, Some(checks));
        }
    }

    fn push(
        &mut self,
        tool: &str,
        mut args: Map<String, Value>,
        checks: Option<Map<String, Value>>,
    ) {
        let problems = self.selectorize(&mut args);
        let mut step = Map::new();
        step.insert("tool".into(), json!(tool));
        step.insert("intent".into(), json!(intent(tool, &args)));
        if args.contains_key("x") && args.contains_key("y") {
            step.insert("pixel".into(), json!(true));
        }
        step.insert("args".into(), Value::Object(args));
        for (key, value) in checks.unwrap_or_default() {
            step.insert(key, value);
        }
        self.push_entry(Entry {
            step: Value::Object(step),
            problems,
        });
    }

    fn push_entry(&mut self, entry: Entry) {
        if self.entries.len() >= MAX_ENTRIES {
            self.overflow = true;
            return;
        }
        self.entries.push(entry);
    }

    /// Replaces an element token (or index) with role/name/nth from the
    /// read it came from, and drops the launch-bound pid/window_id.
    fn selectorize(&mut self, args: &mut Map<String, Value>) -> Vec<String> {
        let mut problems = Vec::new();
        args.remove("session");
        let pid = args.get("pid").and_then(Value::as_i64);
        let window_id = args.get("window_id").and_then(Value::as_i64);
        let target = if let Some(token) = args.get("element_token").and_then(Value::as_str) {
            let token = token.to_owned();
            match token
                .rsplit_once(':')
                .and_then(|(id, n)| Some((id, n.parse::<u64>().ok()?)))
            {
                Some((id, index)) => self
                    .snapshots
                    .iter()
                    .rev()
                    .find(|snapshot| snapshot.id == id)
                    .map(|snapshot| (snapshot, index)),
                None => None,
            }
            .ok_or(token)
        } else if let Some(index) = args.get("element_index").and_then(Value::as_u64) {
            self.snapshots
                .iter()
                .rev()
                .find(|snapshot| Some(snapshot.pid) == pid && Some(snapshot.window_id) == window_id)
                .map(|snapshot| (snapshot, index))
                .ok_or(format!("element_index {index}"))
        } else {
            args.remove("pid");
            args.remove("window_id");
            return problems;
        };
        let resolved = target.as_ref().ok().and_then(|(snapshot, index)| {
            selector(&snapshot.rows, *index).map(|selector| (selector, snapshot.title.clone()))
        });
        match resolved {
            Some((selector, title)) => {
                args.remove("element_token");
                args.remove("element_index");
                args.remove("pid");
                args.remove("window_id");
                for (key, value) in selector {
                    args.insert(key, value);
                }
                if let Some(title) = title
                    && self.home_title.as_ref().is_some_and(|home| *home != title)
                {
                    args.insert("window".into(), json!(title));
                }
            }
            None => {
                let what = match &target {
                    Ok((snapshot, index)) => format!("{}:{index}", snapshot.id),
                    Err(what) => what.clone(),
                };
                problems.push(format!(
                    "{what} could not be turned into a selector (its row has no label or its read is gone); rewrite this step with role/name/nth"
                ));
            }
        }
        problems
    }

    fn ingest_window_state(&mut self, args: &Map<String, Value>, structured: &Value) {
        let (Some(pid), Some(window_id), Some(id)) = (
            args.get("pid").and_then(Value::as_i64),
            args.get("window_id").and_then(Value::as_i64),
            structured["snapshot_id"].as_str(),
        ) else {
            return;
        };
        let title = structured["window_title"].as_str().map(str::to_owned);
        if self.home_title.is_none() {
            self.home_title = title.clone();
        }
        let previous = self
            .snapshots
            .iter()
            .rev()
            .find(|snapshot| snapshot.pid == pid && snapshot.window_id == window_id)
            .map(|snapshot| snapshot.rows.clone());
        let rows = if let Some(markdown) = structured["tree_markdown"].as_str() {
            parse_rows(markdown)
        } else if let Some(elements) = structured["elements"].as_array() {
            elements
                .iter()
                .filter_map(|element| {
                    Some((
                        element["element_index"].as_u64()?,
                        Row {
                            role: element["role"].as_str()?.to_owned(),
                            label: element["label"].as_str().unwrap_or_default().to_owned(),
                        },
                    ))
                })
                .collect()
        } else if let Some(diff) = structured["tree_diff"].as_str() {
            apply_diff(previous.unwrap_or_default(), diff)
        } else {
            return;
        };
        self.snapshots.push_back(Snapshot {
            id: id.to_owned(),
            pid,
            window_id,
            title,
            rows,
        });
        while self.snapshots.len() > MAX_SNAPSHOTS {
            self.snapshots.pop_front();
        }
    }

    /// `trace`: the recorded steps as a draft skill.
    pub fn trace(&self) -> Value {
        let steps: Vec<&Value> = self.entries.iter().map(|entry| &entry.step).collect();
        let mut candidates = Vec::new();
        let mut warnings = Vec::new();
        for (i, entry) in self.entries.iter().enumerate() {
            for problem in &entry.problems {
                warnings.push(format!("step {}: {problem}", i + 1));
            }
            if entry.step["pixel"] == true {
                warnings.push(format!(
                    "step {}: clicks raw pixels; replace with a selector if one exists",
                    i + 1
                ));
            }
            let args = &entry.step["args"];
            for field in ["text", "value"] {
                if let Some(literal) = args[field].as_str() {
                    candidates.push(json!({"step": i + 1, "field": field, "literal": literal}));
                }
            }
        }
        if self.overflow {
            warnings.push(format!(
                "only the first {MAX_ENTRIES} actions were recorded"
            ));
        }
        json!({
            "app": {"bundle_id": self.app.bundle_id, "name": self.app.name},
            "goal": self.goal,
            "steps": steps,
            "candidate_params": candidates,
            "warnings": warnings,
            "dropped": {
                "reads": self.reads,
                "failed_actions": self.failed,
                "not_batchable": self.skipped,
            },
            "next": "Edit this into a skill: replace typed literals with {param} placeholders and declare them in params, keep only the steps the task needs, fix each intent sentence, add expect checks where a step's effect matters, then call save with example_args. Never save passwords or secrets as literals.",
        })
    }
}

/// role/name/nth for row `index`; `nth` only when several rows share both.
pub(super) fn selector(rows: &BTreeMap<u64, Row>, index: u64) -> Option<Map<String, Value>> {
    let row = rows.get(&index)?;
    if row.label.trim().is_empty() {
        return None;
    }
    let role = bare_role(&row.role);
    let same = |other: &Row| {
        bare_role(&other.role) == role && other.label.eq_ignore_ascii_case(&row.label)
    };
    let nth = rows.range(..index).filter(|(_, other)| same(other)).count();
    let total = rows.values().filter(|other| same(other)).count();
    let mut selector = Map::new();
    selector.insert("role".into(), json!(role));
    selector.insert("name".into(), json!(row.label));
    if total > 1 {
        selector.insert("nth".into(), json!(nth));
    }
    Some(selector)
}

/// "AXTextField" → "textfield", the driver's own folding.
fn bare_role(role: &str) -> String {
    let folded: String = role
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(|c| c.to_lowercase())
        .collect();
    folded
        .strip_prefix("ax")
        .map(str::to_owned)
        .unwrap_or(folded)
}

/// `[14] AXButton "Save" ...` (or `(Save)`) → (14, Row).
fn parse_row(text: &str) -> Option<(u64, Row)> {
    let rest = text.trim().strip_prefix('[')?;
    let (index, rest) = rest.split_once(']')?;
    let index = index.parse().ok()?;
    let rest = rest.trim_start();
    let (role, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    // `"title"`, then `= "value"`, then `(description)`: the driver names
    // an element by its title, else its description. The value (what was
    // typed) is never kept.
    let mut rest = rest.trim_start();
    let mut title = "";
    if let Some(quoted) = rest.strip_prefix('"') {
        let (inside, after) = quoted.split_once('"').unwrap_or((quoted, ""));
        title = inside;
        rest = after.trim_start();
    }
    if let Some(value) = rest.strip_prefix("= \"") {
        rest = value
            .split_once('"')
            .map_or("", |(_, after)| after)
            .trim_start();
    }
    let description = rest
        .strip_prefix('(')
        .and_then(|inner| inner.split_once(')'))
        .map_or("", |(inside, _)| inside);
    let label = if title.trim().is_empty() {
        description
    } else {
        title
    };
    Some((
        index,
        Row {
            role: role.to_owned(),
            label: label.to_owned(),
        },
    ))
}

pub(super) fn parse_rows(markdown: &str) -> BTreeMap<u64, Row> {
    markdown
        .lines()
        .filter_map(|line| parse_row(line.trim_start().strip_prefix("- ")?))
        .collect()
}

/// A `since` read's diff applied to the rows it was taken against:
/// removals (old indices), then index shifts, then additions and changes.
pub(super) fn apply_diff(mut rows: BTreeMap<u64, Row>, diff: &str) -> BTreeMap<u64, Row> {
    let mut added = Vec::new();
    let mut shifts = Vec::new();
    for line in diff.lines() {
        if let Some(text) = line.strip_prefix("- ") {
            if let Some((index, _)) = parse_row(text) {
                rows.remove(&index);
            }
        } else if let Some(text) = line.strip_prefix("+ ").or_else(|| line.strip_prefix("~ ")) {
            if let Some(row) = parse_row(text) {
                added.push(row);
            }
        } else if let Some(run) = line.strip_prefix("reindexed: [") {
            // `reindexed: [a-b] -> [c-d]`
            let parsed = run.split_once(']').and_then(|(old, new)| {
                let (a, b) = old.split_once('-')?;
                let new = new.trim().strip_prefix("-> [")?.strip_suffix(']')?;
                let (c, _) = new.split_once('-')?;
                Some((
                    a.parse::<u64>().ok()?,
                    b.parse::<u64>().ok()?,
                    c.parse::<i64>().ok()?,
                ))
            });
            if let Some((a, b, c)) = parsed {
                shifts.push((a, b, c - a as i64));
            }
        }
    }
    if !shifts.is_empty() {
        rows = rows
            .into_iter()
            .map(|(index, row)| {
                let delta = shifts
                    .iter()
                    .find(|(a, b, _)| (*a..=*b).contains(&index))
                    .map_or(0, |(_, _, delta)| *delta);
                ((index as i64 + delta).max(0) as u64, row)
            })
            .collect();
    }
    for (index, row) in added {
        rows.insert(index, row);
    }
    rows
}

/// A first-draft intent sentence: what the step does, in words.
fn intent(tool: &str, args: &Map<String, Value>) -> String {
    let target = match (
        args.get("role").and_then(Value::as_str),
        args.get("name").and_then(Value::as_str),
    ) {
        (Some(role), Some(name)) => format!(" {role} \"{name}\""),
        (None, Some(name)) => format!(" \"{name}\""),
        _ if args.contains_key("x") => " a point".to_owned(),
        _ => String::new(),
    };
    match tool {
        "type_text" => format!(
            "Type the text into{}",
            if target.is_empty() {
                " the focused field".to_owned()
            } else {
                target
            }
        ),
        "set_value" => format!("Set{target} to the value"),
        "press_key" => format!(
            "Press {}",
            args.get("key").and_then(Value::as_str).unwrap_or("a key")
        ),
        "hotkey" => format!(
            "Press {}",
            args.get("keys")
                .and_then(Value::as_array)
                .map(|keys| keys
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("+"))
                .unwrap_or_else(|| "a shortcut".into())
        ),
        tool => format!("{}{target}", tool.replace('_', " ")),
    }
}
