use std::collections::BTreeMap;
#[cfg(not(unix))]
use std::io::Read;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use diri_proto::{AgentReadinessResult, Method, SessionListResult, SessionRecord, SessionStatus};
use dirijor_mcp::{Bridge, ControlClient, ControlFailure, default_socket_path};
use serde_json::{Value, json};

const EXIT_FAILURE: i32 = 1;
const EXIT_TIMEOUT: i32 = 2;
const EXIT_NOT_FOUND: i32 = 3;
const EXIT_UNREACHABLE: i32 = 4;

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let code = match run(&arguments) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("dirijor: {}", error.message);
            error.code
        }
    };
    if code != 0 {
        std::process::exit(code);
    }
}

#[derive(Debug)]
struct CliError {
    code: i32,
    message: String,
}

impl CliError {
    fn failure(message: impl Into<String>) -> Self {
        Self {
            code: EXIT_FAILURE,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            code: EXIT_NOT_FOUND,
            message: message.into(),
        }
    }
}

fn run(arguments: &[String]) -> Result<(), CliError> {
    let Some(command) = arguments.first().map(String::as_str) else {
        print_help();
        return Ok(());
    };
    match command {
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        "hook" => hook(arguments.get(1).map(String::as_str)),
        "notify" => notify(arguments.get(1..).unwrap_or_default()),
        "mcp-stdio" => mcp_stdio(),
        "mcp-tools" => mcp_tools(),
        "mcp-call" => mcp_call(arguments.get(1..).unwrap_or_default()),
        "status" => session_list(arguments.get(1..).unwrap_or_default(), true),
        "session" => session(arguments.get(1..).unwrap_or_default()),
        "worktree" => worktree(arguments.get(1..).unwrap_or_default()),
        "artifacts" => artifacts(arguments.get(1..).unwrap_or_default()),
        "events" => events(arguments.get(1..).unwrap_or_default()),
        "ports" => ports(arguments.get(1..).unwrap_or_default()),
        "doctor" => doctor(),
        "forward" => Err(CliError::failure(
            "companion TCP forwarding is not part of the Rust Engine",
        )),
        other => Err(CliError::failure(format!("unknown command: {other}"))),
    }
}

fn print_help() {
    println!(
        "dirijor — Diri automation CLI\n\n\
         Usage:\n  dirijor status [--json]\n  dirijor session <list|get|read|send|wait|spawn|release|archive> ...\n  \
         dirijor worktree <list|create|remove> ...\n  dirijor artifacts <session> [--json]\n  \
         dirijor events <subscribe|wait> ...\n  dirijor ports [--json]\n  dirijor doctor\n  \
         dirijor hook <event>\n  dirijor notify <json>\n  dirijor mcp-tools\n  \
         dirijor mcp-call --tool <name> < input.json\n\n\
         Deferred on Linux: companion forwarding (dirijor forward)."
    );
}

fn bridge() -> Bridge {
    Bridge::default()
}

fn request(method: &str, params: Value, timeout: Duration) -> Result<Value, CliError> {
    bridge()
        .request(method, params, timeout)
        .map_err(map_bridge_error)
}

fn map_bridge_error(message: String) -> CliError {
    let lower = message.to_ascii_lowercase();
    let code = if lower.contains("timed out") {
        EXIT_TIMEOUT
    } else if lower.contains("not_found") || lower.contains("no such session") {
        EXIT_NOT_FOUND
    } else if lower.contains("connect") || lower.contains("socket") {
        EXIT_UNREACHABLE
    } else {
        EXIT_FAILURE
    };
    CliError { code, message }
}

fn hook(event: Option<&str>) -> Result<(), CliError> {
    let event = event.unwrap_or_default();
    let payload = stdin_json(1 << 20, Duration::from_millis(500));
    let result = bridge().request(
        Method::HOOK_REPORT,
        json!({
            "kind": "claude-hook",
            "dirijorSessionID": std::env::var("DIRIJOR_SESSION_ID").ok(),
            "event": event,
            "payload": payload,
        }),
        Duration::from_secs(3),
    );
    // Hooks are deliberately fail-open: an unavailable UI must never prevent
    // an agent from starting or completing its own action.
    if event == "SessionStart"
        && let Ok(result) = result
        && let Some(title) = result.get("sessionTitle").and_then(Value::as_str)
    {
        println!(
            "{}",
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "sessionTitle": title,
                }
            })
        );
    } else {
        println!("{{}}");
    }
    Ok(())
}

fn notify(arguments: &[String]) -> Result<(), CliError> {
    let Some(raw) = arguments.last() else {
        return Ok(());
    };
    let payload = serde_json::from_str(raw).unwrap_or_else(|_| json!({"raw": raw}));
    let _ = bridge().request(
        Method::HOOK_REPORT,
        json!({
            "kind": "codex-notify",
            "dirijorSessionID": std::env::var("DIRIJOR_SESSION_ID").ok(),
            "event": null,
            "payload": payload,
        }),
        Duration::from_secs(3),
    );
    Ok(())
}

fn mcp_stdio() -> Result<(), CliError> {
    let executable =
        std::env::current_exe().map_err(|error| CliError::failure(error.to_string()))?;
    let proxy = executable
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("dirijor-mcp");
    if !proxy.is_file() {
        return Err(CliError::failure(format!(
            "MCP frontend is missing at {}",
            proxy.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = std::process::Command::new(&proxy).exec();
        Err(CliError::failure(format!(
            "could not exec {}: {error}",
            proxy.display()
        )))
    }
    #[cfg(not(unix))]
    {
        let status = std::process::Command::new(&proxy)
            .status()
            .map_err(|error| CliError::failure(error.to_string()))?;
        if status.success() {
            Ok(())
        } else {
            Err(CliError::failure(format!("MCP frontend exited {status}")))
        }
    }
}

fn mcp_tools() -> Result<(), CliError> {
    let tools = bridge()
        .tool_definitions()
        .map_err(map_bridge_error)?
        .iter()
        .map(|tool| tool.wire_value())
        .collect::<Vec<_>>();
    print_json(&json!({"tools": tools}));
    Ok(())
}

fn mcp_call(arguments: &[String]) -> Result<(), CliError> {
    let tool = option_value(arguments, "--tool")
        .ok_or_else(|| CliError::failure("mcp-call requires --tool <name>"))?;
    let input = stdin_json(4 << 20, Duration::from_secs(5));
    let envelope = match bridge().call(&tool, &input) {
        Ok(result) => json!({"ok": result}),
        Err(error) => json!({"error": error}),
    };
    print_json(&envelope);
    Ok(())
}

fn session(arguments: &[String]) -> Result<(), CliError> {
    let (action, rest) = arguments
        .split_first()
        .map_or(("list", &[][..]), |(action, rest)| {
            if action.starts_with('-') {
                ("list", arguments)
            } else {
                (action.as_str(), rest)
            }
        });
    match action {
        "list" => session_list(rest, false),
        "get" => session_get(rest),
        "read" => session_read(rest),
        "send" => session_send(rest),
        "wait" => session_wait(rest),
        "spawn" => session_spawn(rest),
        "release" => session_release(rest),
        "archive" => session_archive(rest),
        other => Err(CliError::failure(format!(
            "unknown session action: {other}"
        ))),
    }
}

fn session_list(arguments: &[String], include_archived_by_default: bool) -> Result<(), CliError> {
    let mut sessions = sessions()?;
    if !include_archived_by_default && !has_flag(arguments, "--all") {
        sessions.retain(|record| !record.is_archived());
    }
    if let Some(prefix) = option_value(arguments, "--status") {
        sessions.retain(|record| status_label(&record.status).starts_with(&prefix));
    }
    if has_flag(arguments, "--json") {
        print_json(&json!({"sessions": sessions}));
    } else {
        print_session_table(&sessions);
    }
    Ok(())
}

fn session_get(arguments: &[String]) -> Result<(), CliError> {
    let target = positional(arguments, 0)
        .ok_or_else(|| CliError::failure("session get requires a target"))?;
    let sessions = sessions()?;
    let record = resolve_session(&target, &sessions)?;
    if has_flag(arguments, "--json") {
        print_json(
            &serde_json::to_value(record).map_err(|error| CliError::failure(error.to_string()))?,
        );
    } else {
        println!("{}  {}", record.id.0, record.title);
        println!("kind:   {}", record.effective_kind().id());
        println!("status: {}", status_label(&record.status));
        println!("cwd:    {}", record.cwd);
        if let Some(host) = &record.host {
            println!("host:   {host}");
        }
    }
    Ok(())
}

fn session_read(arguments: &[String]) -> Result<(), CliError> {
    let target = positional(arguments, 0)
        .ok_or_else(|| CliError::failure("session read requires a target"))?;
    let sessions = sessions()?;
    let record = resolve_session(&target, &sessions)?;
    let source = option_value(arguments, "--source").unwrap_or_else(|| "screen".into());
    if !matches!(source.as_str(), "screen" | "scrollback") {
        return Err(CliError::failure(
            "--source must be \"screen\" or \"scrollback\"",
        ));
    }
    let result = if source == "scrollback" {
        request(
            Method::SESSION_READ_SCROLLBACK,
            json!({"sessionID": record.id.0}),
            Duration::from_secs(10),
        )?
    } else {
        request(
            Method::SESSION_READ_SCREEN,
            json!({"sessionID": record.id.0}),
            Duration::from_secs(10),
        )?
    };
    let cols = result.get("cols").and_then(Value::as_u64).unwrap_or(0);
    let rows = result.get("rows").and_then(Value::as_u64).unwrap_or(0);
    let mut lines: Vec<String> = if source == "scrollback" {
        result
            .get("lines")
            .and_then(Value::as_array)
            .map(|lines| {
                lines
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        result
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .split('\n')
            .map(str::to_owned)
            .collect()
    };
    if let Some(count) = option_value(arguments, "--lines").and_then(|raw| raw.parse().ok())
        && count > 0
        && lines.len() > count
    {
        lines.drain(..lines.len() - count);
    }
    if has_flag(arguments, "--json") {
        print_json(&json!({
            "id": record.id.0,
            "source": source,
            "cols": cols,
            "rows": rows,
            "lines": lines,
        }));
    } else {
        for line in lines {
            println!("{line}");
        }
    }
    Ok(())
}

fn session_send(arguments: &[String]) -> Result<(), CliError> {
    let target = positional(arguments, 0)
        .ok_or_else(|| CliError::failure("session send requires a target"))?;
    let sessions = sessions()?;
    let record = resolve_session(&target, &sessions)?;
    let mut text = positionals(arguments)
        .into_iter()
        .skip(1)
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        text = String::from_utf8_lossy(&stdin_bytes(1 << 20, Duration::from_millis(500)))
            .trim_end_matches(['\r', '\n'])
            .to_owned();
    }
    if text.is_empty() {
        return Err(CliError::failure("session send requires text"));
    }
    let text_len = text.chars().count();
    request(
        Method::SESSION_SEND_TEXT,
        json!({
            "sessionID": record.id.0,
            "text": text,
            "submit": !has_flag(arguments, "--no-submit"),
        }),
        Duration::from_secs(10),
    )?;
    if has_flag(arguments, "--json") {
        print_json(&json!({"ok": true, "id": record.id.0}));
    } else {
        println!("sent {text_len} chars to {}", record.id.0);
    }
    Ok(())
}

fn session_wait(arguments: &[String]) -> Result<(), CliError> {
    let target = positional(arguments, 0)
        .ok_or_else(|| CliError::failure("session wait requires a target"))?;
    let sessions = sessions()?;
    let record = resolve_session(&target, &sessions)?;
    let until = repeated_option(arguments, "--until");
    let until = if until.is_empty() {
        vec!["done".into()]
    } else {
        until
    };
    let timeout = option_value(arguments, "--timeout")
        .and_then(|raw| raw.parse::<f64>().ok())
        .unwrap_or(600.0)
        .max(0.0);
    let result = request(
        Method::EVENTS_WAIT,
        json!({"sessionID": record.id.0, "until": until, "timeoutMs": (timeout * 1000.0) as i64}),
        Duration::from_secs_f64(timeout + 5.0),
    )?;
    if result.get("timedOut").and_then(Value::as_bool) == Some(true) {
        return Err(CliError {
            code: EXIT_TIMEOUT,
            message: format!("timed out waiting for {}", record.id.0),
        });
    }
    if has_flag(arguments, "--json") {
        print_json(&result);
    } else if let Some(session) = result.get("session") {
        println!(
            "{}  {}  {}",
            session["id"].as_str().unwrap_or(&record.id.0),
            session
                .get("status")
                .and_then(|status| serde_json::from_value::<SessionStatus>(status.clone()).ok())
                .as_ref()
                .map(status_label)
                .unwrap_or("unknown"),
            session["title"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

fn session_spawn(arguments: &[String]) -> Result<(), CliError> {
    let kind = positional(arguments, 0)
        .ok_or_else(|| CliError::failure("session spawn requires an agent kind"))?;
    let cwd = option_value(arguments, "--cwd")
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|path| path.to_string_lossy().into_owned())
        })
        .ok_or_else(|| CliError::failure("could not determine the working directory"))?;
    let result = bridge()
        .call(
            "spawn_agent",
            &json!({
                "kind": kind,
                "cwd": cwd,
                "worktree": has_flag(arguments, "--worktree"),
                "branch": option_value(arguments, "--branch"),
                "prompt": option_value(arguments, "--prompt"),
                "name": option_value(arguments, "--title").or_else(|| option_value(arguments, "--name")),
                "host": option_value(arguments, "--host"),
            }),
        )
        .map_err(map_bridge_error)?;
    if has_flag(arguments, "--json") {
        print_json(&result);
    } else if let Some(id) = result.get("id").and_then(Value::as_str) {
        println!("spawned {id}");
    } else {
        print_json(&result);
    }
    Ok(())
}

fn session_release(arguments: &[String]) -> Result<(), CliError> {
    let target = positional(arguments, 0)
        .ok_or_else(|| CliError::failure("session release requires a target"))?;
    let sessions = sessions()?;
    let record = resolve_session(&target, &sessions)?;
    request(
        Method::SESSION_KILL,
        json!({"sessionID": record.id.0}),
        Duration::from_secs(10),
    )?;
    if has_flag(arguments, "--remove") {
        request(
            Method::SESSION_REMOVE,
            json!({"sessionID": record.id.0}),
            Duration::from_secs(10),
        )?;
    }
    if has_flag(arguments, "--json") {
        print_json(&json!({"ok": true, "id": record.id.0}));
    } else {
        println!("released {}", record.id.0);
    }
    Ok(())
}

fn session_archive(arguments: &[String]) -> Result<(), CliError> {
    let target = positional(arguments, 0)
        .ok_or_else(|| CliError::failure("session archive requires a target"))?;
    let sessions = sessions()?;
    let record = resolve_session(&target, &sessions)?;
    let method = if has_flag(arguments, "--undo") {
        Method::SESSION_UNARCHIVE
    } else {
        Method::SESSION_ARCHIVE
    };
    request(
        method,
        json!({"sessionID": record.id.0}),
        Duration::from_secs(10),
    )?;
    if has_flag(arguments, "--json") {
        print_json(&json!({"ok": true, "id": record.id.0}));
    } else {
        println!(
            "{} {}",
            if method == Method::SESSION_ARCHIVE {
                "archived"
            } else {
                "unarchived"
            },
            record.id.0
        );
    }
    Ok(())
}

fn worktree(arguments: &[String]) -> Result<(), CliError> {
    let (action, rest) = arguments
        .split_first()
        .map_or(("list", &[][..]), |(action, rest)| {
            if action.starts_with('-') {
                ("list", arguments)
            } else {
                (action.as_str(), rest)
            }
        });
    let repo = positional(rest, 0)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|path| path.to_string_lossy().into_owned())
        })
        .ok_or_else(|| CliError::failure("could not determine repository path"))?;
    match action {
        "list" => {
            let result = request(
                Method::WORKTREE_LIST,
                json!({"repoPath": repo}),
                Duration::from_secs(30),
            )?;
            if has_flag(rest, "--json") {
                print_json(&json!({"repo": repo, "worktrees": result}));
            } else if let Some(worktrees) = result.as_array() {
                if worktrees.is_empty() {
                    println!("No worktrees for {repo}.");
                }
                for worktree in worktrees {
                    let branch = worktree["branch"].as_str().unwrap_or("-");
                    let path = worktree["path"].as_str().unwrap_or("");
                    let mut flags = Vec::new();
                    for (key, label) in [
                        ("isBare", "bare"),
                        ("isDetached", "detached"),
                        ("isPrunable", "prunable"),
                    ] {
                        if worktree[key].as_bool() == Some(true) {
                            flags.push(label);
                        }
                    }
                    let suffix = if flags.is_empty() {
                        String::new()
                    } else {
                        format!("  [{}]", flags.join(","))
                    };
                    println!("{branch}  {path}{suffix}");
                }
            }
        }
        "create" => {
            let result = request(
                Method::WORKTREE_CREATE,
                json!({"repoPath": repo, "branch": option_value(rest, "--branch"), "base": option_value(rest, "--base")}),
                Duration::from_secs(120),
            )?;
            if has_flag(rest, "--json") {
                print_json(&result);
            } else {
                println!(
                    "{}  {}",
                    result["branch"].as_str().unwrap_or("-"),
                    result["path"].as_str().unwrap_or("")
                );
            }
        }
        "remove" => {
            let path = positional(rest, 1)
                .ok_or_else(|| CliError::failure("worktree remove requires <repo> <path>"))?;
            request(
                Method::WORKTREE_REMOVE,
                json!({"repoPath": repo, "worktreePath": path, "force": has_flag(rest, "--force")}),
                Duration::from_secs(60),
            )?;
            if has_flag(rest, "--json") {
                print_json(&json!({"ok": true, "path": path}));
            } else {
                println!("removed {path}");
            }
        }
        other => {
            return Err(CliError::failure(format!(
                "unknown worktree action: {other}"
            )));
        }
    }
    Ok(())
}

fn artifacts(arguments: &[String]) -> Result<(), CliError> {
    let target = positional(arguments, 0)
        .ok_or_else(|| CliError::failure("artifacts requires a session"))?;
    let sessions = sessions()?;
    let record = resolve_session(&target, &sessions)?;
    let artifacts = record.artifacts.as_deref().unwrap_or_default();
    let ports = record.listening_ports.as_deref().unwrap_or_default();
    let pull_requests = record.pull_requests.as_deref().unwrap_or_default();
    if has_flag(arguments, "--json") {
        print_json(&json!({
            "id": record.id.0,
            "artifacts": artifacts,
            "listeningPorts": ports,
            "pullRequests": pull_requests,
        }));
    } else if artifacts.is_empty() && ports.is_empty() {
        println!("No artifacts for {}.", record.id.0);
    } else {
        for artifact in artifacts {
            let value = serde_json::to_value(artifact).unwrap_or_default();
            println!(
                "{:<12}  {}",
                value["kind"].as_str().unwrap_or("link"),
                artifact.url
            );
        }
        for port in ports {
            println!(
                "{:<12}  localhost:{}  ({})",
                "port", port.port, port.process_name
            );
        }
    }
    Ok(())
}

fn events(arguments: &[String]) -> Result<(), CliError> {
    let (action, rest) =
        arguments
            .split_first()
            .map_or(("subscribe", &[][..]), |(action, rest)| {
                if action.starts_with('-') {
                    ("subscribe", arguments)
                } else {
                    (action.as_str(), rest)
                }
            });
    match action {
        "wait" => {
            let until = repeated_option(rest, "--until");
            let kinds = repeated_option(rest, "--kind");
            if until.is_empty() && kinds.is_empty() {
                return Err(CliError::failure("events wait requires --until or --kind"));
            }
            if !until.is_empty() && !kinds.is_empty() {
                return Err(CliError::failure(
                    "the Rust Engine cannot combine --until and --kind in one wait",
                ));
            }
            let timeout = option_value(rest, "--timeout")
                .and_then(|raw| raw.parse::<f64>().ok())
                .unwrap_or(600.0);
            let session = option_value(rest, "--session")
                .map(|target| {
                    let known = sessions()?;
                    Ok::<_, CliError>(resolve_session(&target, &known)?.id.0.clone())
                })
                .transpose()?;
            if !until.is_empty() {
                let target = session
                    .ok_or_else(|| CliError::failure("events wait --until requires --session"))?;
                let result = request(
                    Method::EVENTS_WAIT,
                    json!({"sessionID": target, "until": until, "timeoutMs": (timeout * 1000.0) as i64}),
                    Duration::from_secs_f64(timeout + 5.0),
                )?;
                if has_flag(rest, "--json") {
                    print_json(&result);
                } else if let Some(record) = result.get("session") {
                    println!(
                        "{}  {}  {}",
                        record["id"].as_str().unwrap_or("?"),
                        record
                            .get("status")
                            .and_then(|status| serde_json::from_value::<SessionStatus>(
                                status.clone()
                            )
                            .ok())
                            .as_ref()
                            .map(status_label)
                            .unwrap_or("unknown"),
                        record["title"].as_str().unwrap_or("")
                    );
                }
                if result.get("timedOut").and_then(Value::as_bool) == Some(true) {
                    return Err(CliError {
                        code: EXIT_TIMEOUT,
                        message: "event wait timed out".into(),
                    });
                }
            } else {
                wait_for_event_kind(rest, session, kinds, timeout)?;
            }
        }
        "subscribe" => {
            let timeout = option_value(rest, "--timeout")
                .and_then(|raw| raw.parse::<f64>().ok())
                .unwrap_or(86_400.0);
            let known = sessions()?;
            let session_filters = repeated_option(rest, "--session")
                .iter()
                .map(|target| resolve_session(target, &known).map(|record| record.id.0.clone()))
                .collect::<Result<Vec<_>, _>>()?;
            let kind_filters = repeated_option(rest, "--kind");
            let params = json!({
                "sinceSeq": option_value(rest, "--since-seq").and_then(|raw| raw.parse::<u64>().ok()),
                "sessions": (!session_filters.is_empty()).then_some(session_filters),
                "kinds": (!kind_filters.is_empty()).then_some(kind_filters),
            });
            let mut client = ControlClient::connect(&default_socket_path(), Duration::from_secs(3))
                .map_err(map_control_error)?;
            let json_output = has_flag(rest, "--json");
            let count = option_value(rest, "--count").and_then(|raw| raw.parse::<usize>().ok());
            let mut seen = 0usize;
            let result = client.subscribe(
                params,
                Instant::now() + Duration::from_secs_f64(timeout),
                |name, seq, params| {
                    if json_output {
                        print_json(&json!({"event": name, "seq": seq, "params": params}));
                    } else {
                        println!("{seq} {name} {params}");
                    }
                    let _ = io::stdout().flush();
                    seen += 1;
                    Ok(count.is_none_or(|limit| seen < limit))
                },
            );
            if !matches!(result, Ok(()) | Err(ControlFailure::Timeout)) {
                result.map_err(map_control_error)?;
            }
        }
        other => return Err(CliError::failure(format!("unknown events action: {other}"))),
    }
    Ok(())
}

fn wait_for_event_kind(
    arguments: &[String],
    session: Option<String>,
    kinds: Vec<String>,
    timeout: f64,
) -> Result<(), CliError> {
    let mut client = ControlClient::connect(&default_socket_path(), Duration::from_secs(3))
        .map_err(map_control_error)?;
    let mut matched: Option<(String, u64, Value)> = None;
    let result = client.subscribe(
        json!({
            "sessions": session.map(|id| vec![id]),
            "kinds": kinds,
        }),
        Instant::now() + Duration::from_secs_f64(timeout.max(0.0)),
        |name, seq, params| {
            matched = Some((name.to_owned(), seq, params.clone()));
            Ok(false)
        },
    );
    match result {
        Ok(()) => {}
        Err(ControlFailure::Timeout) if matched.is_none() => {
            if !has_flag(arguments, "--json") {
                println!("timed out");
            }
            return Err(CliError {
                code: EXIT_TIMEOUT,
                message: "event wait timed out".into(),
            });
        }
        Err(error) => return Err(map_control_error(error)),
    }
    let Some((name, seq, params)) = matched else {
        return Err(CliError::failure(
            "event subscription ended without a match",
        ));
    };
    if has_flag(arguments, "--json") {
        print_json(&json!({
            "event": {"name": name, "seq": seq, "params": params},
            "timedOut": false,
        }));
    } else {
        println!("{seq} {name} {params}");
    }
    Ok(())
}

fn ports(arguments: &[String]) -> Result<(), CliError> {
    let sessions = sessions()?;
    let rows: Vec<Value> = sessions
        .iter()
        .flat_map(|record| {
            record
                .listening_ports
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(move |port| {
                    json!({"port": port.port, "process": port.process_name, "session": record.title})
                })
        })
        .collect();
    if has_flag(arguments, "--json") {
        print_json(&json!({"ports": rows}));
    } else if rows.is_empty() {
        println!("No listening ports tracked.");
    } else {
        println!("PORT  PROCESS  SESSION");
        for row in rows {
            println!(
                "{}  {}  {}",
                row["port"],
                row["process"].as_str().unwrap_or(""),
                row["session"].as_str().unwrap_or("")
            );
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentBinaryDiagnostic {
    binary: String,
    display_name: String,
}

impl AgentBinaryDiagnostic {
    fn label(&self) -> String {
        if self.display_name.eq_ignore_ascii_case(&self.binary) {
            self.display_name.clone()
        } else {
            format!("{} ({})", self.display_name, self.binary)
        }
    }
}

/// Pure catalog-to-check-list mapping. PATH probing stays in `doctor`, so
/// tests never depend on which optional agent CLIs a machine has installed.
fn agent_binary_diagnostics(catalog: &AgentReadinessResult) -> Vec<AgentBinaryDiagnostic> {
    let mut agents: Vec<_> = catalog.agents.iter().collect();
    agents.sort_by(|left, right| left.kind.id().cmp(right.kind.id()));

    let mut by_binary = BTreeMap::new();
    for agent in agents {
        if agent.kind.is_terminal()
            || agent.kind.id() == "generic"
            || agent.binary.trim().is_empty()
        {
            continue;
        }
        let display_name = agent
            .descriptor
            .as_ref()
            .map(|descriptor| descriptor.display_name.trim())
            .filter(|name| !name.is_empty())
            .unwrap_or(agent.binary.as_str())
            .to_owned();
        by_binary
            .entry(agent.binary.clone())
            .or_insert(AgentBinaryDiagnostic {
                binary: agent.binary.clone(),
                display_name,
            });
    }
    by_binary.into_values().collect()
}

fn doctor() -> Result<(), CliError> {
    let socket = default_socket_path();
    let hello = request(
        Method::HELLO,
        json!({"proto": 1, "build": "dirijor-cli/0.1.0"}),
        Duration::from_secs(3),
    );
    let mut daemon_ok = false;
    match hello {
        Ok(hello) => {
            println!(
                "✓ Rust Engine reachable (build {}, pid {}, proto {})",
                hello["build"].as_str().unwrap_or("unknown"),
                hello["pid"],
                hello["proto"]
            );
            daemon_ok = true;
        }
        Err(error) => println!(
            "✗ Engine unreachable at {} ({})",
            socket.display(),
            error.message
        ),
    }
    if daemon_ok {
        match request(Method::AGENT_READINESS, json!({}), Duration::from_secs(3)) {
            Ok(value) => match serde_json::from_value::<AgentReadinessResult>(value) {
                Ok(catalog) => {
                    for diagnostic in agent_binary_diagnostics(&catalog) {
                        let label = diagnostic.label();
                        if let Some(path) = which(&diagnostic.binary) {
                            println!("✓ {label} found at {}", path.display());
                        } else {
                            println!("✗ {label} not found on PATH");
                        }
                    }
                }
                Err(error) => println!("✗ Agent catalog unreadable ({error})"),
            },
            Err(error) => println!("✗ Agent catalog unavailable ({})", error.message),
        }
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    let state = if std::env::var_os(diri_proto::paths::ENV_SOCKET).is_some() {
        // An explicit socket normally denotes an isolated test or alternate
        // instance. Its state is conventionally colocated unless the caller
        // also selected an app-support root.
        socket
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("state.json")
    } else {
        diri_proto::paths::DirijorPaths::state_file(home)
    };
    if state.is_file() {
        println!("✓ state file present at {}", state.display());
    } else {
        println!("✗ state file missing at {}", state.display());
    }
    daemon_ok.then_some(()).ok_or_else(|| CliError {
        code: EXIT_UNREACHABLE,
        message: "Rust Engine is unavailable".into(),
    })
}

fn sessions() -> Result<Vec<SessionRecord>, CliError> {
    let value = request(Method::SESSION_LIST, json!({}), Duration::from_secs(10))?;
    let list: SessionListResult = serde_json::from_value(value)
        .map_err(|error| CliError::failure(format!("invalid session list: {error}")))?;
    Ok(list.sessions)
}

fn resolve_session<'a>(
    needle: &str,
    sessions: &'a [SessionRecord],
) -> Result<&'a SessionRecord, CliError> {
    if let Some(exact) = sessions.iter().find(|record| record.id.0 == needle) {
        return Ok(exact);
    }
    let mut prefixes = sessions
        .iter()
        .filter(|record| record.id.0.starts_with(needle));
    if let Some(first) = prefixes.next()
        && prefixes.next().is_none()
    {
        return Ok(first);
    }
    let needle = needle.to_ascii_lowercase();
    let matches: Vec<_> = sessions
        .iter()
        .filter(|record| record.title.to_ascii_lowercase().contains(&needle))
        .collect();
    match matches.as_slice() {
        [record] => Ok(record),
        [] => Err(CliError::not_found(format!(
            "no session matches {needle:?}"
        ))),
        _ => Err(CliError::failure(format!(
            "session target {needle:?} is ambiguous"
        ))),
    }
}

fn print_session_table(sessions: &[SessionRecord]) {
    if sessions.is_empty() {
        println!("No sessions.");
        return;
    }
    println!("ID          STATUS      KIND          TITLE");
    for record in sessions {
        println!(
            "{:<11} {:<11} {:<13} {}",
            record.id.0,
            status_label(&record.status),
            record.effective_kind().id(),
            record.title
        );
    }
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

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string(value).unwrap_or_else(|_| "null".into())
    );
}

fn has_flag(arguments: &[String], flag: &str) -> bool {
    arguments.iter().any(|argument| argument == flag)
}

fn option_value(arguments: &[String], flag: &str) -> Option<String> {
    arguments
        .iter()
        .position(|argument| argument == flag)
        .and_then(|index| arguments.get(index + 1))
        .cloned()
}

fn repeated_option(arguments: &[String], flag: &str) -> Vec<String> {
    arguments
        .iter()
        .enumerate()
        .filter(|(_, argument)| argument.as_str() == flag)
        .filter_map(|(index, _)| arguments.get(index + 1).cloned())
        .collect()
}

fn positionals(arguments: &[String]) -> Vec<String> {
    let options_with_values = [
        "--status",
        "--source",
        "--lines",
        "--until",
        "--timeout",
        "--cwd",
        "--prompt",
        "--name",
        "--title",
        "--host",
        "--branch",
        "--base",
        "--session",
        "--kind",
        "--since-seq",
        "--count",
        "--socket",
        "--port",
        "--token",
        "--tool",
    ];
    let mut result = Vec::new();
    let mut skip = false;
    for argument in arguments {
        if skip {
            skip = false;
            continue;
        }
        if options_with_values.contains(&argument.as_str()) {
            skip = true;
        } else if !argument.starts_with('-') {
            result.push(argument.clone());
        }
    }
    result
}

fn positional(arguments: &[String], index: usize) -> Option<String> {
    positionals(arguments).get(index).cloned()
}

fn which(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(binary))
        .find(|candidate| candidate.is_file())
}

fn map_control_error(error: ControlFailure) -> CliError {
    map_bridge_error(error.to_string())
}

#[cfg(unix)]
fn stdin_bytes(cap: usize, timeout: Duration) -> Vec<u8> {
    use std::os::fd::AsRawFd;

    let fd = io::stdin().as_raw_fd();
    // SAFETY: fcntl operates on stdin's live fd and the original flags are
    // restored before returning.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags >= 0 {
        // SAFETY: same valid fd, adding O_NONBLOCK only for this bounded read.
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    }
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    while bytes.len() < cap && Instant::now() < deadline {
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        // SAFETY: one initialized pollfd and a bounded timeout.
        let ready = unsafe {
            libc::poll(
                &mut poll,
                1,
                i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX),
            )
        };
        if ready <= 0 {
            break;
        }
        // SAFETY: chunk is a valid writable buffer and fd is stdin.
        let read = unsafe {
            libc::read(
                fd,
                chunk.as_mut_ptr().cast(),
                chunk.len().min(cap - bytes.len()),
            )
        };
        if read <= 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read as usize]);
    }
    if flags >= 0 {
        // SAFETY: restore the flags captured above.
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
    }
    bytes
}

#[cfg(not(unix))]
fn stdin_bytes(cap: usize, _: Duration) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = io::stdin().take(cap as u64).read_to_end(&mut bytes);
    bytes
}

fn stdin_json(cap: usize, timeout: Duration) -> Value {
    let bytes = stdin_bytes(cap, timeout);
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&bytes)}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::{
        AgentDescriptor, AgentKind, AgentReadinessItem, DateMillis, ProjectId, Resumability,
        SessionId, TitleSource,
    };

    fn record(id: &str, title: &str) -> SessionRecord {
        SessionRecord {
            id: SessionId::new(id),
            kind: AgentKind::CODEX,
            cwd: "/tmp".into(),
            project_id: ProjectId::new("p"),
            worktree_path: None,
            git_branch: None,
            title: title.into(),
            title_source: TitleSource::Placeholder,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status: SessionStatus::Idle,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::Live,
            parent: None,
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
    fn session_targets_resolve_by_id_prefix_and_title() {
        let sessions = vec![
            record("s_alpha1", "Refactor parser"),
            record("s_beta2", "Ship release"),
        ];
        assert_eq!(resolve_session("s_al", &sessions).unwrap().id.0, "s_alpha1");
        assert_eq!(
            resolve_session("RELEASE", &sessions).unwrap().id.0,
            "s_beta2"
        );
        assert_eq!(
            resolve_session("nothing", &sessions).unwrap_err().code,
            EXIT_NOT_FOUND
        );
    }

    #[test]
    fn parser_collects_message_text_without_option_values() {
        let args = vec![
            "s_a".into(),
            "--no-submit".into(),
            "run".into(),
            "the".into(),
            "tests".into(),
        ];
        assert_eq!(positionals(&args), vec!["s_a", "run", "the", "tests"]);
    }

    #[test]
    fn doctor_derives_distinct_deterministic_binary_checks_from_the_catalog() {
        let item = |id: &str, binary: &str, display_name: &str| AgentReadinessItem {
            kind: AgentKind::new(id),
            binary: binary.into(),
            path: None,
            descriptor: Some(AgentDescriptor {
                id: id.into(),
                display_name: display_name.into(),
                ..AgentDescriptor::default()
            }),
            ..AgentReadinessItem::default()
        };
        let catalog = AgentReadinessResult {
            agents: vec![
                item("zeta", "shared-agent", "Zeta Agent"),
                item("beta", "beta", "Beta"),
                item("alpha", "shared-agent", "Alpha Agent"),
                item("shell", "/bin/zsh", "Shell"),
                item("generic", "command", "Command"),
                item("empty", "", "Empty"),
            ],
            ..AgentReadinessResult::default()
        };

        let checks = agent_binary_diagnostics(&catalog);
        assert_eq!(
            checks,
            vec![
                AgentBinaryDiagnostic {
                    binary: "beta".into(),
                    display_name: "Beta".into(),
                },
                AgentBinaryDiagnostic {
                    binary: "shared-agent".into(),
                    display_name: "Alpha Agent".into(),
                },
            ]
        );
        assert_eq!(
            checks
                .iter()
                .map(AgentBinaryDiagnostic::label)
                .collect::<Vec<_>>(),
            vec!["Beta", "Alpha Agent (shared-agent)"]
        );
    }
}
