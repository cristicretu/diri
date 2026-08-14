use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use diri_proto::{
    AgentKind, AgentReadinessResult, Method, ReadScreenResult, SessionId, SessionListResult,
    SessionRecord, SessionSpawnParams, SessionStatus,
};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::control::{ControlClient, ControlFailure, default_socket_path};
use crate::tools::{ToolDefinition, tool_definitions_for};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
// The Engine may wait through a first-run trust/login wall before it can
// confirm an initial prompt. Keep the MCP request alive longer than the
// Engine's bounded delivery window so callers receive its delivery result,
// rather than a client-side timeout while the session continues in limbo.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(300);
const WRITE_POLICY: &str = "Reads are open across all sessions. Writes to your parent or direct children are delivered verbatim; writes to anyone else are attributed to you. You cannot send_prompt to yourself, and release_agent refuses to kill you or any ancestor.";

#[derive(Clone, Debug)]
pub struct Bridge {
    socket_path: PathBuf,
    caller: Option<String>,
}

impl Default for Bridge {
    fn default() -> Self {
        Self::new(
            default_socket_path(),
            std::env::var("DIRIJOR_SESSION_ID").ok(),
        )
    }
}

impl Bridge {
    pub fn new(socket_path: PathBuf, caller: Option<String>) -> Self {
        Self {
            socket_path,
            caller,
        }
    }

    pub fn tool_definitions(&self) -> Result<Vec<ToolDefinition>, String> {
        let readiness: AgentReadinessResult =
            self.request_typed(Method::AGENT_READINESS, json!({}), DEFAULT_TIMEOUT)?;
        let mut kinds: Vec<String> = readiness
            .agents
            .into_iter()
            .filter(|agent| !agent.kind.is_terminal())
            .map(|agent| {
                agent
                    .descriptor
                    .map(|descriptor| descriptor.short_label)
                    .filter(|label| !label.is_empty())
                    .unwrap_or_else(|| short_label(agent.kind.id()).to_owned())
            })
            .collect();
        kinds.sort();
        kinds.dedup();
        kinds.push("shell".into());
        Ok(tool_definitions_for(&kinds))
    }

    pub fn call(&self, tool: &str, arguments: &Value) -> Result<Value, String> {
        match tool {
            "spawn_agent" => self.spawn_agent(arguments),
            "list_agents" => self.list_agents(),
            "get_status" => self.get_status(arguments),
            "send_prompt" => self.send_prompt(arguments),
            "wait_for_agent" => self.wait_for_agent(arguments),
            "read_output" => self.read_output(arguments),
            "get_artifacts" => self.get_artifacts(arguments),
            "create_worktree" => self.create_worktree(arguments),
            "list_worktrees" => self.list_worktrees(arguments),
            "remove_worktree" => self.remove_worktree(arguments),
            "release_agent" => self.release_agent(arguments),
            "test_run" => self.request("test.run", arguments.clone(), Duration::from_secs(180)),
            "browser" => self.browser(arguments),
            "whoami" => self.whoami(),
            "list_children" => self.list_children(arguments),
            "wait_for_children" => self.wait_for_children(arguments),
            "summarize_children" => self.summarize_children(arguments),
            "report_to_parent" => self.report_to_parent(arguments),
            other => Err(format!("unknown tool: {other}")),
        }
    }

    pub fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let mut client =
            ControlClient::connect(&self.socket_path, timeout).map_err(render_failure)?;
        client.request(method, params).map_err(render_failure)
    }

    fn request_typed<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<T, String> {
        let value = self.request(method, params, timeout)?;
        serde_json::from_value(value).map_err(|error| format!("invalid {method} response: {error}"))
    }

    fn sessions(&self) -> Result<Vec<SessionRecord>, String> {
        let list: SessionListResult =
            self.request_typed(Method::SESSION_LIST, json!({}), DEFAULT_TIMEOUT)?;
        Ok(list.sessions)
    }

    fn spawn_agent(&self, arguments: &Value) -> Result<Value, String> {
        let requested = required_string(arguments, "kind")?;
        let readiness: AgentReadinessResult =
            self.request_typed(Method::AGENT_READINESS, json!({}), DEFAULT_TIMEOUT)?;
        let kind = resolve_agent_kind(&readiness, &requested);

        let params = SessionSpawnParams {
            kind,
            cwd: required_string(arguments, "cwd")?,
            new_worktree: optional_bool(arguments, "worktree"),
            worktree_branch: optional_string(arguments, "branch"),
            title: optional_string(arguments, "name"),
            initial_prompt: optional_string(arguments, "prompt"),
            parent: self.caller.clone().map(SessionId),
            initial_cols: None,
            initial_rows: None,
            host: optional_string(arguments, "host"),
            same_repo_as: None,
        };
        let params = serde_json::to_value(params).map_err(|error| error.to_string())?;
        self.request(Method::SESSION_SPAWN, params, SPAWN_TIMEOUT)
    }

    fn list_agents(&self) -> Result<Value, String> {
        Ok(json!({
            "agents": self.sessions()?.iter().map(compact).collect::<Vec<_>>()
        }))
    }

    fn get_status(&self, arguments: &Value) -> Result<Value, String> {
        let id = required_string(arguments, "session_id")?;
        let sessions = self.sessions()?;
        let record = find_session(&sessions, &id)?;
        Ok(compact(record))
    }

    fn send_prompt(&self, arguments: &Value) -> Result<Value, String> {
        let id = required_string(arguments, "session_id")?;
        let text = required_string(arguments, "text")?;
        let submit = optional_bool(arguments, "submit").unwrap_or(true);
        let sessions = self.sessions()?;
        let lineage = Lineage::new(&sessions, self.caller.as_deref());
        let relation = lineage.relation(&id);
        if relation == Relation::Caller {
            return Err(format!(
                "send_prompt cannot target the calling session ({id}); answer normally instead"
            ));
        }
        let delivered = lineage.frame(&text, relation);
        self.request(
            Method::SESSION_SEND_TEXT,
            json!({"sessionID": id, "text": delivered, "submit": submit}),
            DEFAULT_TIMEOUT,
        )?;
        Ok(json!({
            "ok": true,
            "relation": relation.as_str(),
            "attributed": delivered != text,
        }))
    }

    fn wait_for_agent(&self, arguments: &Value) -> Result<Value, String> {
        let id = required_string(arguments, "session_id")?;
        let until = optional_string(arguments, "until").unwrap_or_else(|| "done".into());
        let timeout_seconds = optional_number(arguments, "timeout_s")
            .unwrap_or(600.0)
            .max(0.0);
        self.request(
            Method::EVENTS_WAIT,
            json!({
                "sessionID": id,
                "until": [until],
                "timeoutMs": (timeout_seconds * 1000.0) as i64,
            }),
            Duration::from_secs_f64(timeout_seconds + 5.0),
        )
    }

    fn read_output(&self, arguments: &Value) -> Result<Value, String> {
        let id = required_string(arguments, "session_id")?;
        let mut result: ReadScreenResult = self.request_typed(
            Method::SESSION_READ_SCREEN,
            json!({"sessionID": id}),
            DEFAULT_TIMEOUT,
        )?;
        if optional_string(arguments, "mode").as_deref() == Some("tail") {
            let lines = optional_number(arguments, "lines")
                .unwrap_or(50.0)
                .clamp(1.0, 500.0) as usize;
            let kept = result
                .text
                .lines()
                .rev()
                .take(lines)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            result.text = kept;
        }
        serde_json::to_value(result).map_err(|error| error.to_string())
    }

    fn get_artifacts(&self, arguments: &Value) -> Result<Value, String> {
        let id = required_string(arguments, "session_id")?;
        let sessions = self.sessions()?;
        let record = find_session(&sessions, &id)?;
        let prs: HashMap<&str, Value> = record
            .pull_requests
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|pr| {
                serde_json::to_value(pr)
                    .ok()
                    .map(|value| (pr.url.as_str(), value))
            })
            .collect();
        let artifacts: Vec<Value> = record
            .artifacts
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|artifact| {
                let mut value = serde_json::to_value(artifact).unwrap_or_else(|_| json!({}));
                if let Some(pr) = prs.get(artifact.url.as_str())
                    && let Some(object) = value.as_object_mut()
                {
                    object.insert("pr".into(), pr.clone());
                }
                value
            })
            .collect();
        Ok(json!({
            "artifacts": artifacts,
            "listeningPorts": record.listening_ports,
        }))
    }

    fn create_worktree(&self, arguments: &Value) -> Result<Value, String> {
        self.request(
            Method::WORKTREE_CREATE,
            json!({
                "repoPath": required_string(arguments, "repo")?,
                "branch": optional_string(arguments, "branch"),
                "base": optional_string(arguments, "base"),
            }),
            Duration::from_secs(30),
        )
    }

    fn list_worktrees(&self, arguments: &Value) -> Result<Value, String> {
        self.request(
            Method::WORKTREE_LIST,
            json!({"repoPath": required_string(arguments, "repo")?}),
            Duration::from_secs(30),
        )
    }

    fn remove_worktree(&self, arguments: &Value) -> Result<Value, String> {
        self.request(
            Method::WORKTREE_REMOVE,
            json!({
                "repoPath": required_string(arguments, "repo")?,
                "worktreePath": required_string(arguments, "path")?,
                "force": optional_bool(arguments, "force").unwrap_or(false),
            }),
            Duration::from_secs(30),
        )?;
        Ok(json!({"ok": true}))
    }

    fn release_agent(&self, arguments: &Value) -> Result<Value, String> {
        let id = required_string(arguments, "session_id")?;
        let sessions = self.sessions()?;
        let relation = Lineage::new(&sessions, self.caller.as_deref()).relation(&id);
        match relation {
            Relation::Caller => return Err("release_agent cannot terminate its caller".into()),
            Relation::Parent | Relation::Ancestor => {
                return Err(
                    "release_agent cannot terminate the session waiting on this result".into(),
                );
            }
            _ => {}
        }
        self.request(
            Method::SESSION_KILL,
            json!({"sessionID": id}),
            Duration::from_secs(10),
        )?;
        Ok(json!({"ok": true}))
    }

    fn browser(&self, arguments: &Value) -> Result<Value, String> {
        let caller = self.caller.as_ref().ok_or_else(|| {
            "browser is scoped to a Diri session and DIRIJOR_SESSION_ID is unset".to_owned()
        })?;
        required_string(arguments, "action")?;
        let mut params = arguments.as_object().cloned().unwrap_or_default();
        params.insert("sessionID".into(), json!(caller));
        self.request(
            "browser.act",
            Value::Object(params),
            Duration::from_secs(60),
        )
    }

    fn whoami(&self) -> Result<Value, String> {
        let sessions = self.sessions()?;
        let lineage = Lineage::new(&sessions, self.caller.as_deref());
        let Some(caller) = self.caller.as_deref() else {
            return Ok(json!({
                "hosted": false,
                "note": "DIRIJOR_SESSION_ID is unset; lineage tools are unavailable.",
            }));
        };
        let record = lineage
            .record(caller)
            .ok_or_else(|| format!("the daemon has no session {caller}"))?;
        let mut result = Map::from_iter([
            ("hosted".into(), json!(true)),
            ("session".into(), detailed(record, Relation::Caller)),
            (
                "children".into(),
                Value::Array(
                    lineage
                        .children(caller)
                        .into_iter()
                        .map(|record| detailed(record, Relation::Child))
                        .collect(),
                ),
            ),
            (
                "descendant_count".into(),
                json!(lineage.descendants(caller).len()),
            ),
            ("write_policy".into(), json!(WRITE_POLICY)),
        ]);
        if let Some(parent) = record.parent.as_ref().and_then(|id| lineage.record(&id.0)) {
            result.insert("parent".into(), detailed(parent, Relation::Parent));
        }
        let ancestors = lineage.ancestors(caller);
        if !ancestors.is_empty() {
            result.insert(
                "ancestors".into(),
                Value::Array(
                    ancestors
                        .into_iter()
                        .map(|record| detailed(record, Relation::Ancestor))
                        .collect(),
                ),
            );
        }
        Ok(Value::Object(result))
    }

    fn list_children(&self, arguments: &Value) -> Result<Value, String> {
        let caller = self.require_caller()?;
        let sessions = self.sessions()?;
        let lineage = Lineage::new(&sessions, Some(caller));
        let recursive = optional_bool(arguments, "recursive").unwrap_or(false);
        let include_exited = optional_bool(arguments, "include_exited").unwrap_or(true);
        let mut records = if recursive {
            lineage.descendants(caller)
        } else {
            lineage.children(caller)
        };
        if !include_exited {
            records.retain(|record| !matches!(record.status, SessionStatus::Exited(_)));
        }
        let children: Vec<Value> = records
            .into_iter()
            .map(|record| {
                let relation = if record.parent.as_ref().is_some_and(|id| id.0 == caller) {
                    Relation::Child
                } else {
                    Relation::Descendant
                };
                detailed(record, relation)
            })
            .collect();
        Ok(json!({"count": children.len(), "children": children}))
    }

    fn wait_for_children(&self, arguments: &Value) -> Result<Value, String> {
        let caller = self.require_caller()?.to_owned();
        let initial = self.sessions()?;
        let lineage = Lineage::new(&initial, Some(&caller));
        let targets = child_subset(arguments, &lineage, &caller)?;
        if targets.is_empty() {
            return Ok(json!({
                "settled": true,
                "children": [],
                "note": "You have no child sessions to wait for."
            }));
        }
        let wanted: HashSet<String> = targets.iter().map(|record| record.id.0.clone()).collect();
        let mode = optional_string(arguments, "until").unwrap_or_else(|| "settled".into());
        let timeout = optional_number(arguments, "timeout_s")
            .unwrap_or(600.0)
            .max(0.0);
        let deadline = Instant::now() + Duration::from_secs_f64(timeout);

        let reassess = || -> Result<(Vec<SessionRecord>, bool), String> {
            let latest: Vec<SessionRecord> = self
                .sessions()?
                .into_iter()
                .filter(|record| wanted.contains(&record.id.0))
                .collect();
            let settled = latest.len() != wanted.len()
                || latest.iter().all(|record| reached(&mode, &record.status));
            Ok((latest, settled))
        };
        let (mut latest, mut settled) = reassess()?;
        if !settled && Instant::now() < deadline {
            let mut client = ControlClient::connect(&self.socket_path, DEFAULT_TIMEOUT)
                .map_err(render_failure)?;
            let subscription = json!({
                "sessions": wanted.iter().cloned().collect::<Vec<_>>(),
                "kinds": ["session.updated", "session.removed"],
            });
            let result = client.subscribe(subscription, deadline, |_, _, _| {
                (latest, settled) = reassess().map_err(|message| {
                    ControlFailure::Protocol(format!("could not refresh children: {message}"))
                })?;
                Ok(!settled)
            });
            if !matches!(result, Ok(()) | Err(ControlFailure::Timeout)) {
                result.map_err(render_failure)?;
            }
        }
        Ok(json!({
            "settled": settled,
            "timed_out": !settled,
            "children": latest.iter().map(|record| detailed(record, Relation::Child)).collect::<Vec<_>>(),
            "waited_for": mode,
        }))
    }

    fn summarize_children(&self, arguments: &Value) -> Result<Value, String> {
        let caller = self.require_caller()?;
        let sessions = self.sessions()?;
        let lineage = Lineage::new(&sessions, Some(caller));
        let rows = optional_number(arguments, "rows")
            .unwrap_or(14.0)
            .clamp(1.0, 60.0) as usize;
        let children = child_subset(arguments, &lineage, caller)?;
        let summaries: Vec<Value> = children
            .iter()
            .map(|record| {
                let mut value = detailed(record, Relation::Child);
                let screen = self
                    .request_typed::<ReadScreenResult>(
                        Method::SESSION_READ_SCREEN,
                        json!({"sessionID": record.id.0}),
                        DEFAULT_TIMEOUT,
                    )
                    .ok();
                if let Some(object) = value.as_object_mut() {
                    if let Some(screen) = screen {
                        let tail = screen
                            .text
                            .lines()
                            .map(str::trim)
                            .filter(|line| !line.is_empty())
                            .rev()
                            .take(rows)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect::<Vec<_>>()
                            .join("\n");
                        object.insert("screen_tail".into(), json!(tail));
                    } else {
                        object.insert("screen_tail".into(), Value::Null);
                    }
                    if let Some(artifacts) = &record.artifacts {
                        object.insert(
                            "artifacts".into(),
                            json!(
                                artifacts
                                    .iter()
                                    .map(|artifact| &artifact.url)
                                    .collect::<Vec<_>>()
                            ),
                        );
                    }
                }
                value
            })
            .collect();
        Ok(json!({"count": summaries.len(), "children": summaries}))
    }

    fn report_to_parent(&self, arguments: &Value) -> Result<Value, String> {
        let caller = self.require_caller()?.to_owned();
        let sessions = self.sessions()?;
        let lineage = Lineage::new(&sessions, Some(&caller));
        let record = lineage
            .record(&caller)
            .ok_or_else(|| format!("no session record for {caller}"))?;
        let parent = record
            .parent
            .as_ref()
            .map(|id| id.0.clone())
            .ok_or_else(|| "this session has no parent to report to".to_owned())?;
        if lineage.record(&parent).is_none() {
            return Err(format!("parent session {parent} is gone"));
        }
        let status = optional_string(arguments, "status").unwrap_or_else(|| "update".into());
        if !matches!(status.as_str(), "update" | "done" | "blocked" | "failed") {
            return Err(format!("invalid report status: {status}"));
        }
        let mut lines = vec![
            format!(
                "[report from id:{} ({}) · status: {status}]",
                caller, record.title
            ),
            String::new(),
            format!("Summary: {}", required_string(arguments, "summary")?),
        ];
        if let Some(details) = optional_string(arguments, "details") {
            lines.extend([String::new(), details]);
        }
        for (label, key) in [
            ("Blockers", "blockers"),
            ("Questions", "questions"),
            ("Next steps", "next_steps"),
            ("Changed", "changed_paths"),
            ("Artifacts", "artifacts"),
            ("Proof", "proof"),
        ] {
            let entries = optional_strings(arguments, key);
            if !entries.is_empty() {
                lines.push(String::new());
                lines.push(format!("{label}:"));
                lines.extend(entries.into_iter().map(|entry| format!("- {entry}")));
            }
        }
        let delivered = lines.join("\n");
        self.request(
            Method::SESSION_SEND_TEXT,
            json!({
                "sessionID": parent,
                "text": delivered,
                "submit": optional_bool(arguments, "submit").unwrap_or(true),
            }),
            DEFAULT_TIMEOUT,
        )?;
        Ok(json!({
            "ok": true,
            "parent": parent,
            "status": status,
            "delivered": delivered,
        }))
    }

    fn require_caller(&self) -> Result<&str, String> {
        self.caller.as_deref().ok_or_else(|| {
            "this tool requires DIRIJOR_SESSION_ID and must run inside a Diri session".into()
        })
    }
}

fn render_failure(error: ControlFailure) -> String {
    match error {
        ControlFailure::Io(error) => format!("daemon socket: {error}"),
        other => other.to_string(),
    }
}

fn required_string(arguments: &Value, key: &str) -> Result<String, String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("missing required argument: {key}"))
}

fn optional_string(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn optional_bool(arguments: &Value, key: &str) -> Option<bool> {
    arguments.get(key).and_then(Value::as_bool)
}

fn optional_number(arguments: &Value, key: &str) -> Option<f64> {
    arguments.get(key).and_then(Value::as_f64)
}

fn optional_strings(arguments: &Value, key: &str) -> Vec<String> {
    arguments
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn short_label(kind: &str) -> &str {
    match kind {
        "claude-code" => "claude",
        other => other,
    }
}

fn resolve_agent_kind(readiness: &AgentReadinessResult, requested: &str) -> AgentKind {
    readiness
        .agents
        .iter()
        .find(|agent| {
            agent.kind.id().eq_ignore_ascii_case(requested)
                || short_label(agent.kind.id()).eq_ignore_ascii_case(requested)
                || agent.descriptor.as_ref().is_some_and(|descriptor| {
                    descriptor.short_label.eq_ignore_ascii_case(requested)
                        || descriptor.display_name.eq_ignore_ascii_case(requested)
                        || descriptor
                            .aliases
                            .iter()
                            .any(|alias| alias.eq_ignore_ascii_case(requested))
                })
        })
        .map(|agent| agent.kind.clone())
        .or_else(|| {
            matches!(
                requested.to_ascii_lowercase().as_str(),
                "shell" | "sh" | "bash" | "zsh" | "fish"
            )
            .then_some(AgentKind::SHELL)
        })
        .unwrap_or_else(|| AgentKind::generic(requested))
}

fn status_label(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Starting => "starting",
        SessionStatus::Idle => "idle",
        SessionStatus::Working => "working",
        SessionStatus::NeedsInput(_) => "needsInput",
        SessionStatus::Exited(_) => "exited",
        SessionStatus::Unknown => "unknown",
    }
}

fn compact(record: &SessionRecord) -> Value {
    let mut object = Map::from_iter([
        ("id".into(), json!(record.id.0)),
        (
            "kind".into(),
            json!(short_label(record.effective_kind().id())),
        ),
        ("title".into(), json!(record.title)),
        ("status".into(), json!(status_label(&record.status))),
        ("cwd".into(), json!(record.cwd)),
    ]);
    if let Some(parent) = &record.parent {
        object.insert("parent".into(), json!(parent.0));
    }
    if let Some(host) = &record.host {
        object.insert("host".into(), json!(host));
    }
    Value::Object(object)
}

fn detailed(record: &SessionRecord, relation: Relation) -> Value {
    let mut value = compact(record);
    let object = value.as_object_mut().expect("compact is an object");
    object.insert("relation".into(), json!(relation.as_str()));
    object.insert("created_at".into(), json!(record.created_at.0));
    if let Some(branch) = &record.git_branch {
        object.insert("branch".into(), json!(branch));
    }
    if let Some(worktree) = &record.worktree_path {
        object.insert("worktree".into(), json!(worktree));
    }
    if record.is_archived() {
        object.insert("archived".into(), json!(true));
    }
    value
}

fn find_session<'a>(sessions: &'a [SessionRecord], id: &str) -> Result<&'a SessionRecord, String> {
    sessions
        .iter()
        .find(|record| record.id.0 == id)
        .ok_or_else(|| format!("no such session: {id}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Relation {
    Caller,
    Parent,
    Child,
    Ancestor,
    Descendant,
    Sibling,
    Unrelated,
}

impl Relation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Caller => "self",
            Self::Parent => "parent",
            Self::Child => "child",
            Self::Ancestor => "ancestor",
            Self::Descendant => "descendant",
            Self::Sibling => "sibling",
            Self::Unrelated => "unrelated",
        }
    }

    fn delivers_verbatim(self) -> bool {
        matches!(self, Self::Parent | Self::Child)
    }
}

struct Lineage<'a> {
    records: &'a [SessionRecord],
    caller: Option<&'a str>,
}

impl<'a> Lineage<'a> {
    fn new(records: &'a [SessionRecord], caller: Option<&'a str>) -> Self {
        Self { records, caller }
    }

    fn record(&self, id: &str) -> Option<&'a SessionRecord> {
        self.records.iter().find(|record| record.id.0 == id)
    }

    fn children(&self, id: &str) -> Vec<&'a SessionRecord> {
        let mut children: Vec<_> = self
            .records
            .iter()
            .filter(|record| record.parent.as_ref().is_some_and(|parent| parent.0 == id))
            .collect();
        children.sort_by(|left, right| {
            left.created_at
                .0
                .partial_cmp(&right.created_at.0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        children
    }

    fn descendants(&self, id: &str) -> Vec<&'a SessionRecord> {
        let mut seen = HashSet::from([id.to_owned()]);
        let mut queue: VecDeque<_> = self.children(id).into();
        let mut result = Vec::new();
        while let Some(record) = queue.pop_front() {
            if !seen.insert(record.id.0.clone()) {
                continue;
            }
            result.push(record);
            queue.extend(self.children(&record.id.0));
        }
        result
    }

    fn ancestors(&self, id: &str) -> Vec<&'a SessionRecord> {
        let mut seen = HashSet::from([id.to_owned()]);
        let mut current = self.record(id).and_then(|record| record.parent.as_ref());
        let mut result = Vec::new();
        while let Some(parent) = current {
            if !seen.insert(parent.0.clone()) {
                break;
            }
            let Some(record) = self.record(&parent.0) else {
                break;
            };
            result.push(record);
            current = record.parent.as_ref();
        }
        result
    }

    fn relation(&self, target: &str) -> Relation {
        let Some(caller) = self.caller else {
            return Relation::Unrelated;
        };
        if caller == target {
            return Relation::Caller;
        }
        if self
            .record(caller)
            .and_then(|record| record.parent.as_ref())
            .is_some_and(|parent| parent.0 == target)
        {
            return Relation::Parent;
        }
        if self
            .record(target)
            .and_then(|record| record.parent.as_ref())
            .is_some_and(|parent| parent.0 == caller)
        {
            return Relation::Child;
        }
        if self
            .ancestors(caller)
            .iter()
            .any(|record| record.id.0 == target)
        {
            return Relation::Ancestor;
        }
        if self
            .descendants(caller)
            .iter()
            .any(|record| record.id.0 == target)
        {
            return Relation::Descendant;
        }
        let caller_parent = self
            .record(caller)
            .and_then(|record| record.parent.as_ref())
            .map(|parent| parent.0.as_str());
        if caller_parent.is_some()
            && caller_parent
                == self
                    .record(target)
                    .and_then(|record| record.parent.as_ref())
                    .map(|parent| parent.0.as_str())
        {
            return Relation::Sibling;
        }
        Relation::Unrelated
    }

    fn frame(&self, text: &str, relation: Relation) -> String {
        let Some(caller) = self.caller else {
            return text.to_owned();
        };
        if relation.delivers_verbatim() {
            return text.to_owned();
        }
        let who = self.record(caller).map_or_else(
            || format!("id:{caller}"),
            |record| format!("id:{caller} ({})", record.title),
        );
        format!(
            "[message from {who}, channel: dirijor — reply with send_prompt to that id]\n\n{text}"
        )
    }
}

fn child_subset<'a>(
    arguments: &Value,
    lineage: &Lineage<'a>,
    caller: &str,
) -> Result<Vec<&'a SessionRecord>, String> {
    let all = lineage.children(caller);
    let requested = optional_strings(arguments, "session_ids");
    if requested.is_empty() {
        return Ok(all);
    }
    requested
        .iter()
        .map(|id| {
            all.iter()
                .find(|record| record.id.0 == *id)
                .copied()
                .ok_or_else(|| format!("{id} is not one of your direct child sessions"))
        })
        .collect()
}

fn reached(mode: &str, status: &SessionStatus) -> bool {
    match mode {
        "exited" => matches!(status, SessionStatus::Exited(_)),
        "done" => matches!(status, SessionStatus::Idle | SessionStatus::Exited(_)),
        _ => matches!(
            status,
            SessionStatus::Idle | SessionStatus::NeedsInput(_) | SessionStatus::Exited(_)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::{DateMillis, ProjectId, Resumability, TitleSource};

    fn record(id: &str, parent: Option<&str>) -> SessionRecord {
        SessionRecord {
            id: SessionId::new(id),
            kind: AgentKind::CODEX,
            cwd: "/tmp".into(),
            project_id: ProjectId::new("p"),
            worktree_path: None,
            git_branch: None,
            title: id.into(),
            title_source: TitleSource::Placeholder,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status: SessionStatus::Idle,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::Live,
            parent: parent.map(SessionId::new),
            created_at: DateMillis(0.0),
            updated_at: DateMillis(0.0),
            last_turn_completed_at: None,
            last_seen_at: None,
            pinned: false,
            archived_at: None,
            host: None,
            remote_persistence: None,
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
        }
    }

    #[test]
    fn lineage_protects_and_attributes_cross_session_writes() {
        let records = vec![
            record("root", None),
            record("caller", Some("root")),
            record("child", Some("caller")),
            record("sibling", Some("root")),
        ];
        let lineage = Lineage::new(&records, Some("caller"));
        assert_eq!(lineage.relation("root"), Relation::Parent);
        assert_eq!(lineage.relation("child"), Relation::Child);
        assert_eq!(lineage.relation("sibling"), Relation::Sibling);
        assert_eq!(lineage.frame("hello", Relation::Child), "hello");
        assert!(
            lineage
                .frame("hello", Relation::Sibling)
                .starts_with("[message from id:caller")
        );
    }

    #[test]
    fn settled_counts_input_and_exit_but_done_does_not_count_input() {
        assert!(reached(
            "settled",
            &SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Question)
        ));
        assert!(!reached(
            "done",
            &SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Question)
        ));
        assert!(reached("done", &SessionStatus::Idle));
    }

    #[test]
    fn agent_resolution_uses_the_live_catalog_and_keeps_generic_commands() {
        let readiness: AgentReadinessResult = serde_json::from_value(json!({
            "agents": [{
                "kind": "claude-code",
                "binary": "claude",
                "descriptor": {
                    "id": "claude-code",
                    "displayName": "Claude Code",
                    "shortLabel": "claude",
                    "aliases": ["cc"]
                }
            }]
        }))
        .expect("catalog");
        assert_eq!(resolve_agent_kind(&readiness, "CC"), AgentKind::CLAUDE_CODE);
        assert_eq!(resolve_agent_kind(&readiness, "fish"), AgentKind::SHELL);
        let generic = resolve_agent_kind(&readiness, "htop");
        assert_eq!(generic.id(), AgentKind::GENERIC_ID);
        assert_eq!(generic.command(), Some("htop"));
    }
}
