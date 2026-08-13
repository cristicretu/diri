//! Executing MCP tools against a live registry.
//!
//! The calling agent's own session id arrives in its environment
//! (`DIRIJOR_SESSION_ID`), which is what lets `whoami` and `list_children`
//! answer questions about *this* session and the ones it spawned.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_proto::{SessionId, SessionStatus};
use serde_json::{Value, json};

use super::ToolHost;
use crate::git;
use crate::registry::Registry;

/// Environment variable carrying the calling session's id.
pub const SESSION_ID_ENV: &str = "DIRIJOR_SESSION_ID";

const DEFAULT_READ_BYTES: usize = 8_000;
const DEFAULT_WAIT_SECONDS: f64 = 300.0;
const DEFAULT_CHILDREN_WAIT_SECONDS: f64 = 600.0;

pub struct RegistryHost {
    registry: Arc<Mutex<Registry>>,
    logs_dir: PathBuf,
    holder: Option<crate::session::HolderConfig>,
    /// The session calling these tools, when it identified itself.
    caller: Option<String>,
}

impl RegistryHost {
    pub fn new(registry: Arc<Mutex<Registry>>, logs_dir: impl Into<PathBuf>) -> Self {
        Self {
            registry,
            logs_dir: logs_dir.into(),
            holder: None,
            caller: std::env::var(SESSION_ID_ENV).ok(),
        }
    }

    /// Spawn sessions through holders, so they survive this process.
    pub fn with_holder(mut self, holder: crate::session::HolderConfig) -> Self {
        self.holder = Some(holder);
        self
    }

    /// Overrides the calling session, for tests and for hosts that know the
    /// caller by other means.
    pub fn with_caller(mut self, caller: Option<String>) -> Self {
        self.caller = caller;
        self
    }

    fn registry(&self) -> Result<std::sync::MutexGuard<'_, Registry>, String> {
        self.registry
            .lock()
            .map_err(|_| "engine state is poisoned".to_string())
    }
}

fn required_str(arguments: &Value, key: &str) -> Result<String, String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{key} is required"))
}

fn status_word(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Starting => "starting",
        SessionStatus::Idle => "idle",
        SessionStatus::Working => "working",
        SessionStatus::NeedsInput(_) => "needsInput",
        SessionStatus::Exited(_) => "exited",
        SessionStatus::Unknown => "unknown",
    }
}

impl ToolHost for RegistryHost {
    fn call(&self, tool: &str, arguments: &Value) -> Result<Value, String> {
        match tool {
            "list_agents" => {
                let mut registry = self.registry()?;
                registry.sync_orchestration();
                let agents: Vec<Value> = registry
                    .records()
                    .into_iter()
                    .map(|record| {
                        json!({
                            "id": record.id.0,
                            "kind": record.kind.id(),
                            "title": record.title,
                            "status": status_word(&record.status),
                            "cwd": record.cwd,
                            "parent": record.parent.map(|parent| parent.0),
                            "run": record.run,
                        })
                    })
                    .collect();
                Ok(json!({ "agents": agents }))
            }

            "get_status" => {
                let id = required_str(arguments, "session_id")?;
                let mut registry = self.registry()?;
                registry.sync_orchestration();
                let record = registry
                    .records()
                    .into_iter()
                    .find(|record| record.id.0 == id)
                    .ok_or_else(|| format!("no session {id}"))?;
                Ok(json!({
                    "id": record.id.0,
                    "status": status_word(&record.status),
                    "title": record.title,
                    "cwd": record.cwd,
                    "needsInput": record.needs_input.map(|detail| json!({
                        "kind": format!("{:?}", detail.kind),
                        "summary": detail.summary,
                        "options": detail.options,
                    })),
                    "run": record.run,
                }))
            }

            "send_prompt" => {
                let id = required_str(arguments, "session_id")?;
                let text = required_str(arguments, "text")?;
                let submit = arguments
                    .get("submit")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);

                let expected = arguments.get("run_id").and_then(Value::as_u64);
                let request_id = arguments
                    .get("request_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let mut registry = self.registry()?;
                let (queued, run) = registry
                    .send_run_text(
                        &id,
                        text.clone(),
                        submit,
                        expected,
                        true,
                        request_id.clone(),
                    )
                    .map_err(|error| error.to_string())?;
                Ok(json!({
                    "sent": text.len(),
                    "submitted": submit,
                    "queued": queued,
                    "run": run,
                    "requestId": request_id,
                }))
            }

            "interrupt_agent" => {
                let id = required_str(arguments, "session_id")?;
                let expected = arguments.get("run_id").and_then(Value::as_u64);
                let request_id = arguments
                    .get("request_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let mut registry = self.registry()?;
                let run = registry
                    .interrupt_run(&id, expected, request_id.clone())
                    .map_err(|error| error.to_string())?;
                Ok(json!({"run": run, "requestId": request_id}))
            }

            "read_output" => {
                let id = required_str(arguments, "session_id")?;
                let max_bytes = arguments
                    .get("max_bytes")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize)
                    .unwrap_or(DEFAULT_READ_BYTES);

                let registry = self.registry()?;
                let session = registry
                    .get(&id)
                    .ok_or_else(|| format!("no session {id}"))?;
                // Read from the end: the recent screen is what a caller wants,
                // and the whole log can be megabytes.
                let tail = session.view().tail_offset;
                let from = tail.saturating_sub(max_bytes as u64);
                let (offset, bytes) = session.read_output(from, max_bytes);
                Ok(json!({
                    "offset": offset,
                    "output": String::from_utf8_lossy(&bytes),
                }))
            }

            "release_agent" => {
                let id = required_str(arguments, "session_id")?;
                let mut registry = self.registry()?;
                let exit = registry
                    .terminate(&id, Duration::from_secs(3))
                    .map_err(|error| error.to_string())?;
                if exit.is_none() {
                    return Err(format!("no session {id}"));
                }
                let _ = registry.persist();
                Ok(json!({ "released": id }))
            }

            "wait_for_agent" => {
                let id = required_str(arguments, "session_id")?;
                let until = arguments
                    .get("until")
                    .and_then(Value::as_str)
                    .unwrap_or("done")
                    .to_string();
                let timeout = arguments
                    .get("timeout_seconds")
                    .and_then(Value::as_f64)
                    .unwrap_or(DEFAULT_WAIT_SECONDS);
                let run_id = arguments.get("run_id").and_then(Value::as_u64);
                self.wait_for(&id, &until, timeout, run_id)
            }

            "create_worktree" => {
                let repo = required_str(arguments, "repo")?;
                let branch = arguments.get("branch").and_then(Value::as_str);
                let base = arguments.get("base").and_then(Value::as_str);
                let info = git::create_worktree(Path::new(&repo), branch, base)
                    .map_err(|error| error.to_string())?;
                Ok(json!({ "path": info.path, "branch": info.branch }))
            }

            "list_worktrees" => {
                let repo = required_str(arguments, "repo")?;
                let worktrees: Vec<Value> = git::list_worktrees(Path::new(&repo))
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .map(|info| json!({ "path": info.path, "branch": info.branch }))
                    .collect();
                Ok(json!({ "worktrees": worktrees }))
            }

            "remove_worktree" => {
                let repo = required_str(arguments, "repo")?;
                let worktree = required_str(arguments, "worktree")?;
                let force = arguments
                    .get("force")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                git::remove_worktree(Path::new(&repo), &worktree, force)
                    .map_err(|error| error.to_string())?;
                Ok(json!({ "removed": worktree }))
            }

            "whoami" => {
                let caller = self.caller.clone().ok_or_else(|| {
                    format!("this session did not identify itself; {SESSION_ID_ENV} is unset")
                })?;
                let mut registry = self.registry()?;
                registry.sync_orchestration();
                let record = registry
                    .records()
                    .into_iter()
                    .find(|record| record.id.0 == caller)
                    .ok_or_else(|| format!("no session {caller}"))?;
                Ok(json!({
                    "id": record.id.0,
                    "kind": record.kind.id(),
                    "title": record.title,
                    "cwd": record.cwd,
                    "parent": record.parent.map(|parent| parent.0),
                    "run": record.run,
                }))
            }

            "list_children" => {
                let caller = self.caller.clone().ok_or_else(|| {
                    format!("this session did not identify itself; {SESSION_ID_ENV} is unset")
                })?;
                let mut registry = self.registry()?;
                registry.sync_orchestration();
                let children: Vec<Value> = registry
                    .records()
                    .into_iter()
                    .filter(|record| record.parent.as_ref() == Some(&SessionId(caller.clone())))
                    .map(|record| {
                        json!({
                            "id": record.id.0,
                            "kind": record.kind.id(),
                            "title": record.title,
                            "status": status_word(&record.status),
                            "run": record.run,
                        })
                    })
                    .collect();
                Ok(json!({ "children": children }))
            }

            "wait_for_children" => {
                let caller = self.caller.clone().ok_or_else(|| {
                    format!("this session did not identify itself; {SESSION_ID_ENV} is unset")
                })?;
                let timeout = arguments
                    .get("timeout_seconds")
                    .and_then(Value::as_f64)
                    .unwrap_or(DEFAULT_CHILDREN_WAIT_SECONDS);
                self.wait_for_children(&caller, timeout)
            }

            // spawn_agent is served by the control layer, which owns log paths
            "spawn_agent" => self.spawn_agent(arguments),

            other => Err(format!("unknown tool {other:?}")),
        }
    }
}

impl RegistryHost {
    /// Starts a session on behalf of the calling agent.
    ///
    /// The new session records its caller as `parent`, which is what makes the
    /// lineage tools — `list_children`, `wait_for_children` — mean anything.
    ///
    /// A supplied initial prompt is part of the spawn acknowledgement: this
    /// call waits for the same readiness-gated, verified delivery as the
    /// control-backed MCP server and returns an error if the child never
    /// accepts it.
    fn spawn_agent(&self, arguments: &Value) -> Result<Value, String> {
        let kind = required_str(arguments, "kind")?;
        let cwd = required_str(arguments, "cwd")?;
        if let Some(host) = arguments.get("host").and_then(Value::as_str) {
            return self.spawn_agent_remote(arguments, &kind, &cwd, host);
        }
        let cwd_path = PathBuf::from(&cwd);
        if !cwd_path.is_dir() {
            return Err(format!("cwd {cwd:?} is not a directory"));
        }
        let wants_worktree = arguments
            .get("worktree")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let prompt = arguments
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::to_string);
        let title = arguments
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string);

        // A worktree is created before the session so the agent starts inside
        // it, rather than in the repo and moving later.
        let (working_dir, worktree_path, branch) = if wants_worktree {
            let info = git::create_worktree(&cwd_path, None, None)
                .map_err(|error| format!("could not create a worktree: {error}"))?;
            let path = PathBuf::from(&info.path);
            (path, Some(info.path), info.branch)
        } else {
            (cwd_path, None, git::branch(Path::new(&cwd)))
        };

        let mut registry = self.registry()?;
        let engine = registry.engine();
        let manifest = engine
            .manifest(&kind)
            .ok_or_else(|| format!("no manifest for agent {kind:?}"))?;
        let descriptor = manifest.agent.clone().unwrap_or_default();
        let authority = descriptor.authority();

        let inherited: Vec<(String, String)> = std::env::vars().collect();
        let pty = if let Some(pty) = descriptor.spawn_spec(&working_dir, inherited.clone(), &[]) {
            pty
        } else if kind == diri_proto::AgentKind::SHELL_ID {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
            let mut pty = crate::pty::PtySpec::new(vec![shell, "-l".to_owned()], &working_dir);
            pty.env = inherited;
            pty
        } else {
            return Err(format!(
                "agent {kind:?} declares no binary, so it cannot be spawned by name"
            ));
        };

        let id = crate::control::next_session_id();
        let mut record = crate::control::new_record(&id, &kind, &working_dir.to_string_lossy());
        record.parent = self.caller.clone().map(SessionId);
        if record.parent.is_some() {
            record.run = Some(diri_proto::AgentRun::starting(1, record.created_at));
        }
        record.worktree_path = worktree_path.clone();
        record.git_branch = branch.clone();
        if let Some(title) = title {
            record.title = title;
            record.title_source = diri_proto::TitleSource::DirijorAssigned;
        }

        let spec = crate::session::SessionSpec {
            id: id.clone(),
            pty,
            manifest_id: kind.clone(),
            authority,
            logs_dir: self.logs_dir.clone(),
            holder: self.holder.clone(),
            remote: None,
            defer_launch: true,
        };
        registry
            .spawn(spec, record)
            .map_err(|error| format!("could not start {kind}: {error}"))?;
        let _ = registry.persist();
        drop(registry);

        let accept_claude_workspace = kind == diri_proto::AgentKind::CLAUDE_CODE_ID;
        if let Some(prompt) = prompt.as_deref() {
            crate::control::prepare_agent_input_for_spawn(
                &self.registry,
                &id,
                accept_claude_workspace,
                Some(prompt),
            )
            .map_err(|error| {
                format!(
                    "initial_prompt_delivery_failed: session {id} was created, but its initial prompt was not delivered: {error}"
                )
            })?;
        } else if accept_claude_workspace {
            let registry = Arc::clone(&self.registry);
            let session_id = id.clone();
            std::thread::spawn(move || {
                let _ = crate::control::prepare_agent_input_for_spawn(
                    &registry,
                    &session_id,
                    true,
                    None,
                );
            });
        }

        Ok(json!({
            "id": id,
            "kind": kind,
            "cwd": working_dir.to_string_lossy(),
            "worktree": worktree_path,
            "branch": branch,
            "parent": self.caller,
        }))
    }

    /// Remote spawning is not offered by this host. The previous
    /// implementation built a local `ssh … tmux` argv, which the Holder
    /// transport replaced; the equivalent now needs the Helper manager and
    /// binding store that `ControlServer::session_spawn_remote` owns and this
    /// host is not constructed with. Failing here — before a host is resolved
    /// or any code is synced — keeps the path free of external side effects.
    ///
    /// This is a gap, not a removal: `session.spawn` over the control socket
    /// still spawns remotely. Wiring it up means giving `RegistryHost` the
    /// same manager/binding-store dependencies.
    fn spawn_agent_remote(
        &self,
        _arguments: &Value,
        _kind: &str,
        _cwd: &str,
        _host_id: &str,
    ) -> Result<Value, String> {
        Err(format!(
            "{}: {}",
            crate::remote::TRANSPORT_UNAVAILABLE_CODE,
            crate::remote::transport_unavailable().message
        ))
    }

    fn wait_for(
        &self,
        id: &str,
        until: &str,
        timeout_seconds: f64,
        run_id: Option<u64>,
    ) -> Result<Value, String> {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout_seconds.max(0.0));
        let changes = self.registry()?.state_changes();
        let mut observed = changes.observe();

        loop {
            let targets = vec![until.to_owned()];
            let (record, decision) = {
                let mut registry = self.registry()?;
                registry.sync_orchestration();
                let record = registry
                    .records()
                    .into_iter()
                    .find(|record| record.id.0 == id)
                    .ok_or_else(|| format!("no session {id}"))?;
                let decision = registry
                    .wait_decision(id, run_id, &targets)
                    .ok_or_else(|| format!("no session {id}"))?;
                (record, decision)
            };
            if decision.resolved {
                return Ok(json!({
                    "id": id,
                    "status": status_word(&record.status),
                    "reached": decision.reached,
                    "superseded": decision.superseded,
                    "run": decision.run,
                }));
            }
            if Instant::now() >= deadline {
                return Ok(json!({
                    "id": id,
                    "status": status_word(&record.status),
                    "reached": false,
                    "timedOut": true,
                    "run": decision.run,
                }));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            observed = changes.wait_after(observed, remaining);
        }
    }

    fn wait_for_children(&self, caller: &str, timeout_seconds: f64) -> Result<Value, String> {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout_seconds.max(0.0));
        let parent = SessionId(caller.to_string());
        let changes = self.registry()?.state_changes();
        let mut observed = changes.observe();

        loop {
            let records: Vec<diri_proto::SessionRecord> = {
                let mut registry = self.registry()?;
                registry.sync_orchestration();
                registry
                    .records()
                    .into_iter()
                    .filter(|record| record.parent.as_ref() == Some(&parent))
                    .collect()
            };

            let pending: Vec<&String> = records
                .iter()
                .filter(|record| {
                    record.run.as_ref().map_or_else(
                        || {
                            !matches!(
                                record.status,
                                SessionStatus::Idle | SessionStatus::Exited(_)
                            )
                        },
                        |run| !run.state.is_terminal(),
                    )
                })
                .map(|record| &record.id.0)
                .collect();
            let unsettled = records.iter().any(|record| {
                record.run.as_ref().map_or_else(
                    || {
                        !matches!(
                            record.status,
                            SessionStatus::Idle
                                | SessionStatus::NeedsInput(_)
                                | SessionStatus::Exited(_)
                        )
                    },
                    |run| {
                        !run.state.is_terminal()
                            && !matches!(run.state, diri_proto::AgentRunState::NeedsInput)
                    },
                )
            });

            if !unsettled || Instant::now() >= deadline {
                return Ok(json!({
                    "children": records.iter().map(|record| json!({
                        "id": record.id.0,
                        "status": status_word(&record.status),
                        "run": record.run,
                    })).collect::<Vec<_>>(),
                    "allDone": pending.is_empty(),
                    "needsInput": records.iter().any(|record| record.run.as_ref().is_some_and(|run| matches!(run.state, diri_proto::AgentRunState::NeedsInput))),
                }));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            observed = changes.wait_after(observed, remaining);
        }
    }

    /// Where session logs live, for hosts that spawn.
    pub fn logs_dir(&self) -> &Path {
        &self.logs_dir
    }
}
