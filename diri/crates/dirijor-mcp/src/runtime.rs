//! Bounded stdio dispatch. Reads may overlap; mutations retain arrival order.
//! The input loop stays available for ping and cancellation while tools wait.

use std::collections::HashMap;
use std::io::{self, BufRead, BufWriter, Read, Write};
use std::sync::mpsc::{self, TrySendError};
use std::sync::{Arc, Mutex};

use dirijor_mcp::cancellation::Cancellation;
use serde_json::Value;

use crate::{DirectBackend, error, handle_message, success, tool_content};

const MAX_READS: usize = 8;
const MAX_QUEUED_MUTATIONS: usize = 8;
const MAX_LINE: usize = diri_proto::control::MAX_CONTROL_LINE_BYTES;

type Output = Arc<Mutex<BufWriter<io::Stdout>>>;
type Active = Arc<Mutex<HashMap<String, ActiveRequest>>>;

struct ActiveRequest {
    cancellation: Cancellation,
    read_only: bool,
    started: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    New,
    Initializing,
    Ready,
}

struct Task {
    message: Value,
    key: String,
    cancellation: Cancellation,
}

fn respond(output: &Output, response: &Value) {
    let mut writer = output.lock().unwrap();
    let _ = serde_json::to_writer(&mut *writer, response)
        .map_err(io::Error::other)
        .and_then(|()| writer.write_all(b"\n"))
        .and_then(|()| writer.flush());
}

fn execute(task: Task, output: &Output, active: &Active) {
    let Task {
        message,
        key,
        cancellation,
    } = task;
    let response_id = message["id"].clone();
    // Starting a mutation and cancelling a queued one are ordered by this lock.
    if let Some(request) = active.lock().unwrap().get_mut(&key) {
        request.started = true;
    }
    let mut backend = DirectBackend {
        bridge: dirijor_mcp::Bridge::default().with_cancellation(cancellation.clone()),
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if cancellation.is_cancelled() {
            None
        } else {
            handle_message(message, &mut backend)
        }
    }));
    let response = result.unwrap_or_else(|_| Some(success(response_id, tool_content(Err(
        "Tool failed unexpectedly. An action may already have reached the Engine; inspect its state before repeating it.".into()
    )))));
    if !cancellation.is_cancelled()
        && let Some(response) = response
    {
        respond(output, &response);
    }
    active.lock().unwrap().remove(&key);
}

fn read_only(message: &Value) -> bool {
    message["method"] == "tools/list"
        || matches!(
            message["params"]["name"].as_str(),
            Some(
                "list_agents"
                    | "get_status"
                    | "wait_for_agent"
                    | "read_output"
                    | "get_artifacts"
                    | "list_worktrees"
                    | "whoami"
                    | "list_children"
                    | "wait_for_children"
                    | "summarize_children"
            )
        )
}

pub(super) fn serve() {
    let output: Output = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    let active: Active = Default::default();
    let (mutations, receiver) = mpsc::sync_channel::<Task>(MAX_QUEUED_MUTATIONS);
    let worker_output = output.clone();
    let worker_active = active.clone();
    let mutation_worker = std::thread::spawn(move || {
        for task in receiver {
            execute(task, &worker_output, &worker_active);
        }
    });
    let mut readers: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut line = Vec::new();
    let mut immediate = DirectBackend::new();
    let mut lifecycle = Lifecycle::New;
    loop {
        line.clear();
        match input
            .by_ref()
            .take((MAX_LINE + 1) as u64)
            .read_until(b'\n', &mut line)
        {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.len() > MAX_LINE {
            if line.last() != Some(&b'\n') {
                // Drain the rejected frame in bounded chunks, then resynchronize.
                while let Ok(buffer) = input.fill_buf() {
                    if buffer.is_empty() {
                        break;
                    }
                    let end = buffer.iter().position(|byte| *byte == b'\n');
                    let count = end.map_or(buffer.len(), |end| end + 1);
                    input.consume(count);
                    if end.is_some() {
                        break;
                    }
                }
            }
            respond(
                &output,
                &error(Value::Null, -32600, "MCP message exceeds the frame limit"),
            );
            continue;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let message: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(_) => {
                respond(&output, &error(Value::Null, -32700, "Parse error"));
                continue;
            }
        };
        let valid = message["jsonrpc"] == "2.0"
            && message["method"].is_string()
            && message
                .get("id")
                .is_none_or(|id| id.is_string() || id.as_i64().is_some() || id.as_u64().is_some())
            && message.get("params").is_none_or(Value::is_object);
        if valid && message["method"] == "initialize" && message.get("id").is_some() {
            if lifecycle != Lifecycle::New {
                respond(
                    &output,
                    &error(message["id"].clone(), -32600, "MCP is already initialized"),
                );
                continue;
            }
            let params = &message["params"];
            if !params["protocolVersion"].is_string()
                || !params["capabilities"].is_object()
                || !params["clientInfo"]["name"].is_string()
                || !params["clientInfo"]["version"].is_string()
            {
                respond(
                    &output,
                    &error(
                        message["id"].clone(),
                        -32602,
                        "initialize requires protocolVersion, capabilities, and clientInfo",
                    ),
                );
                continue;
            }
            if let Some(response) = handle_message(message, &mut immediate) {
                respond(&output, &response);
            }
            lifecycle = Lifecycle::Initializing;
            continue;
        }
        if valid && message["method"] == "notifications/initialized" && message.get("id").is_none()
        {
            if lifecycle == Lifecycle::Initializing {
                lifecycle = Lifecycle::Ready;
            }
            continue;
        }
        if valid
            && matches!(
                message["method"].as_str(),
                Some("tools/list" | "tools/call")
            )
            && lifecycle != Lifecycle::Ready
        {
            if let Some(id) = message.get("id") {
                respond(
                    &output,
                    &error(
                        id.clone(),
                        -32000,
                        "MCP initialization must complete before tool requests",
                    ),
                );
            }
            continue;
        }
        if valid && message["method"] == "notifications/cancelled" && message.get("id").is_none() {
            if let Some(id) = message["params"].get("requestId")
                && let Some(request) = active.lock().unwrap().get(&id.to_string())
                && (request.read_only || !request.started)
            {
                request.cancellation.cancel();
            }
            continue;
        }
        if !valid
            || !matches!(
                message["method"].as_str(),
                Some("tools/call" | "tools/list")
            )
            || message.get("id").is_none()
        {
            if let Some(response) = handle_message(message, &mut immediate) {
                respond(&output, &response);
            }
            continue;
        }
        let key = message["id"].to_string();
        let cancellable = read_only(&message);
        let cancellation = Cancellation::default();
        let mut running = active.lock().unwrap();
        if running.contains_key(&key) {
            respond(
                &output,
                &error(
                    message["id"].clone(),
                    -32600,
                    "request ID is already in progress; duplicate was not dispatched",
                ),
            );
            continue;
        }
        if cancellable && running.values().filter(|request| request.read_only).count() >= MAX_READS
        {
            respond(
                &output,
                &success(
                    message["id"].clone(),
                    tool_content(Err("MCP is busy; this request was not dispatched".into())),
                ),
            );
            continue;
        }
        running.insert(
            key.clone(),
            ActiveRequest {
                cancellation: cancellation.clone(),
                read_only: cancellable,
                started: false,
            },
        );
        drop(running);
        let task = Task {
            message,
            key: key.clone(),
            cancellation,
        };
        if cancellable {
            readers.retain(|reader| !reader.is_finished());
            let worker_output = output.clone();
            let worker_active = active.clone();
            readers.push(std::thread::spawn(move || {
                execute(task, &worker_output, &worker_active)
            }));
        } else if let Err(TrySendError::Full(task) | TrySendError::Disconnected(task)) =
            mutations.try_send(task)
        {
            active.lock().unwrap().remove(&key);
            respond(
                &output,
                &success(
                    task.message["id"].clone(),
                    tool_content(Err("MCP is busy; this request was not dispatched".into())),
                ),
            );
        }
    }
    // EOF closes read-only subscriptions promptly. Already accepted mutations
    // finish in order, preserving the one-shot CLI/MCP request contract.
    for request in active.lock().unwrap().values() {
        if request.read_only {
            request.cancellation.cancel();
        }
    }
    drop(mutations);
    let _ = mutation_worker.join();
    for reader in readers {
        let _ = reader.join();
    }
}
