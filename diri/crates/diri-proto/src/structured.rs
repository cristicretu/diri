//! Native structured-protocol codecs for Agent CLIs (Claude Code stream-json,
//! Codex app-server JSON-RPC). Pure: no process, no IO. The Engine's
//! structured driver owns the child process, feeds each stdout line through
//! these parsers, and writes the encoded lines produced here to stdin.
//!
//! Every parser is checked against lines recorded from the installed CLIs
//! (`tests/fixtures/structured`). Unknown message types decode to no events,
//! so a newer CLI adds nothing until a codec opts in; malformed JSON is an
//! error the driver must surface, never a silent drop.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Normalized events both drivers emit. `item` keys are stable per content
/// block within one turn, so the Engine can route deltas to one segment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum StructuredEvent {
    /// The CLI bound its conversation identity (Claude `system/init`).
    SessionStarted {
        conversation_id: String,
        model: Option<String>,
    },
    TextDelta {
        item: String,
        text: String,
    },
    ThinkingDelta {
        item: String,
        text: String,
    },
    /// Emitted from the complete `tool_use` block, so `input` is whole.
    /// The same `id` may arrive more than once; the Engine deduplicates.
    ToolStarted {
        id: String,
        name: String,
        input: Value,
    },
    ToolFinished {
        id: String,
        is_error: bool,
    },
    TurnCompleted {
        is_error: bool,
        subtype: String,
        usage: TokenUsage,
        cost_usd: Option<f64>,
    },
    /// Codex reports usage separately from turn completion (`thread/tokenUsage/updated`).
    UsageUpdated {
        usage: TokenUsage,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodecError(pub String);

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "structured protocol line is not valid JSON: {}", self.0)
    }
}

impl std::error::Error for CodecError {}

fn parse_line(line: &str) -> Result<Value, CodecError> {
    serde_json::from_str(line).map_err(|error| CodecError(error.to_string()))
}

fn usage_from(value: Option<&Value>) -> TokenUsage {
    let field = |name: &str| {
        value
            .and_then(|usage| usage.get(name))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    TokenUsage {
        input_tokens: field("input_tokens"),
        output_tokens: field("output_tokens"),
        cache_read_tokens: field("cache_read_input_tokens"),
        cache_write_tokens: field("cache_creation_input_tokens"),
    }
}

// ---------------------------------------------------------------- Claude Code

/// Decodes one line of `claude -p --output-format stream-json
/// --include-partial-messages --verbose` output.
pub fn parse_claude_line(line: &str) -> Result<Vec<StructuredEvent>, CodecError> {
    let value = parse_line(line)?;
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut events = Vec::new();
    match kind {
        "system" => {
            if value.get("subtype").and_then(Value::as_str) == Some("init") {
                events.push(StructuredEvent::SessionStarted {
                    conversation_id: str_field(&value, "session_id"),
                    model: value
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                });
            }
        }
        "stream_event" => {
            let message = value
                .get("api_message_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let index = value
                .pointer("/event/index")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let item = format!("{message}:{index}");
            if value.pointer("/event/type").and_then(Value::as_str) == Some("content_block_delta") {
                let delta = value.pointer("/event/delta");
                match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                    Some("text_delta") => events.push(StructuredEvent::TextDelta {
                        item,
                        text: str_field_of(delta, "text"),
                    }),
                    Some("thinking_delta") => events.push(StructuredEvent::ThinkingDelta {
                        item,
                        text: str_field_of(delta, "thinking"),
                    }),
                    _ => {}
                }
            }
        }
        "assistant" => {
            for block in value
                .pointer("/message/content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    events.push(StructuredEvent::ToolStarted {
                        id: str_field_of(Some(block), "id"),
                        name: str_field_of(Some(block), "name"),
                        input: block.get("input").cloned().unwrap_or(Value::Null),
                    });
                }
            }
        }
        "user" => {
            for block in value
                .pointer("/message/content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                    events.push(StructuredEvent::ToolFinished {
                        id: str_field_of(Some(block), "tool_use_id"),
                        is_error: block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    });
                }
            }
        }
        "result" => {
            let subtype = str_field(&value, "subtype");
            events.push(StructuredEvent::TurnCompleted {
                is_error: value
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                subtype,
                usage: usage_from(value.get("usage")),
                cost_usd: value.get("total_cost_usd").and_then(Value::as_f64),
            });
        }
        // hook_*, rate_limit_event, status and other diagnostics carry no
        // conversation state the GUI renders.
        _ => {}
    }
    Ok(events)
}

/// One stream-json user turn for Claude's stdin. The returned line has no
/// trailing newline; the driver appends `\n` and flushes.
pub fn claude_user_message_line(text: &str) -> String {
    json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{ "type": "text", "text": text }],
        },
    })
    .to_string()
}

// ----------------------------------------------------------- Codex app-server

/// One decoded line of `codex app-server` stdout (JSON-RPC 2.0).
#[derive(Clone, Debug, PartialEq)]
pub enum CodexInbound {
    /// Reply to a request this client sent; `id` is the request id.
    Response {
        id: u64,
        result: Value,
    },
    /// Error reply to a request (`id` absent when the server could not tell).
    Failure {
        id: Option<u64>,
        message: String,
    },
    Events(Vec<StructuredEvent>),
    /// The server asks the client something (an approval). The driver must
    /// answer with `codex_response_line(id, ...)` or the turn stalls.
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    /// Notifications with no GUI-visible effect.
    Ignored,
}

/// Item types whose start/complete pair is one tool call in the GUI.
const CODEX_TOOL_ITEMS: &[&str] = &[
    "commandExecution",
    "fileChange",
    "mcpToolCall",
    "dynamicToolCall",
    "webSearch",
    "imageView",
    "collabAgentToolCall",
];

/// Decodes one stdout line from `codex app-server` (JSON-RPC notifications,
/// responses and server-initiated requests).
pub fn parse_codex_line(line: &str) -> Result<CodexInbound, CodecError> {
    let value = parse_line(line)?;
    let method = value.get("method").and_then(Value::as_str);
    let id = value.get("id");

    if let Some(method) = method {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        return match id {
            // A request from the server: it expects a reply.
            Some(id) => Ok(CodexInbound::ServerRequest {
                id: id.clone(),
                method: method.to_owned(),
                params,
            }),
            None => Ok(CodexInbound::Events(codex_notification_events(
                method, &params,
            ))),
        };
    }

    if let Some(error) = value.get("error") {
        return Ok(CodexInbound::Failure {
            id: id.and_then(Value::as_u64),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("app-server error")
                .to_owned(),
        });
    }

    match id.and_then(Value::as_u64) {
        Some(id) => Ok(CodexInbound::Response {
            id,
            result: value.get("result").cloned().unwrap_or(Value::Null),
        }),
        None => Ok(CodexInbound::Ignored),
    }
}

fn codex_notification_events(method: &str, params: &Value) -> Vec<StructuredEvent> {
    match method {
        "item/agentMessage/delta" => vec![StructuredEvent::TextDelta {
            item: str_field(params, "itemId"),
            text: str_field(params, "delta"),
        }],
        "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
            vec![StructuredEvent::ThinkingDelta {
                item: str_field(params, "itemId"),
                text: str_field(params, "delta"),
            }]
        }
        "item/started" | "item/completed" => {
            let item = params.get("item").unwrap_or(&Value::Null);
            let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
            if !CODEX_TOOL_ITEMS.contains(&kind) {
                return Vec::new();
            }
            let id = str_field(item, "id");
            if method == "item/started" {
                vec![StructuredEvent::ToolStarted {
                    id,
                    name: codex_tool_name(kind, item),
                    input: item.clone(),
                }]
            } else {
                let status = item
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let failed = matches!(status, "failed" | "declined")
                    || item.get("success").and_then(Value::as_bool) == Some(false);
                vec![StructuredEvent::ToolFinished {
                    id,
                    is_error: failed,
                }]
            }
        }
        // Observed on app-server 0.171.0: a completed turn reports idle
        // thread status rather than turn/completed; both end the turn.
        "turn/completed" => {
            let turn = params.get("turn").unwrap_or(&Value::Null);
            let status = turn
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            vec![StructuredEvent::TurnCompleted {
                is_error: status == "failed",
                subtype: status.to_owned(),
                usage: TokenUsage::default(),
                cost_usd: None,
            }]
        }
        "thread/status/changed" => {
            let idle = params.pointer("/status/type").and_then(Value::as_str) == Some("idle");
            if idle {
                vec![StructuredEvent::TurnCompleted {
                    is_error: false,
                    subtype: "idle".to_owned(),
                    usage: TokenUsage::default(),
                    cost_usd: None,
                }]
            } else {
                Vec::new()
            }
        }
        "thread/tokenUsage/updated" => {
            let last = params.pointer("/tokenUsage/last");
            let input = last
                .and_then(|u| u.get("inputTokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cached = last
                .and_then(|u| u.get("cachedInputTokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            vec![StructuredEvent::UsageUpdated {
                usage: TokenUsage {
                    // Codex counts cached tokens inside inputTokens; Claude does not.
                    input_tokens: input.saturating_sub(cached),
                    output_tokens: last
                        .and_then(|u| u.get("outputTokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    cache_read_tokens: cached,
                    cache_write_tokens: last
                        .and_then(|u| u.get("cacheWriteInputTokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                },
            }]
        }
        "error" => vec![StructuredEvent::Error {
            message: params
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("app-server error")
                .to_owned(),
        }],
        _ => Vec::new(),
    }
}

fn codex_tool_name(kind: &str, item: &Value) -> String {
    match kind {
        "commandExecution" => "Bash".to_owned(),
        "fileChange" => "Edit".to_owned(),
        "mcpToolCall" => {
            let server = str_field(item, "server");
            if server.is_empty() {
                "mcp".to_owned()
            } else {
                format!("mcp:{server}")
            }
        }
        "webSearch" => "WebSearch".to_owned(),
        "imageView" => "Read".to_owned(),
        other => other.to_owned(),
    }
}

/// JSON-RPC request line for `codex app-server` stdin (no trailing newline).
pub fn codex_request_line(id: u64, method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
}

/// JSON-RPC notification line (the `initialized` handshake).
pub fn codex_notification_line(method: &str) -> String {
    json!({ "jsonrpc": "2.0", "method": method }).to_string()
}

/// Reply to a server-initiated request.
pub fn codex_response_line(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

/// `turn/start` params for one user message in an existing thread.
pub fn codex_turn_start_params(thread_id: &str, text: &str, effort: Option<&str>) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        "input": [{ "type": "text", "text": text }],
    });
    if let Some(effort) = effort {
        params["effort"] = Value::String(effort.to_owned());
    }
    params
}

/// Thread id from a `thread/start` or `thread/resume` reply.
pub fn codex_thread_id(result: &Value) -> Option<String> {
    result
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Turn id from a `turn/start` reply.
pub fn codex_turn_id(result: &Value) -> Option<String> {
    result
        .pointer("/turn/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// One model from `model/list`, as the picker shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexModel {
    pub model: String,
    pub display_name: String,
    pub default_effort: Option<String>,
    pub supported_efforts: Vec<String>,
    pub is_default: bool,
}

/// Decodes a `model/list` reply, dropping entries without a model id.
pub fn parse_codex_models(result: &Value) -> Vec<CodexModel> {
    result
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let model = entry.get("model").and_then(Value::as_str)?.to_owned();
            let supported_efforts = entry
                .get("supportedReasoningEfforts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|effort| effort.get("reasoningEffort").and_then(Value::as_str))
                .map(str::to_owned)
                .collect();
            Some(CodexModel {
                display_name: entry
                    .get("displayName")
                    .and_then(Value::as_str)
                    .unwrap_or(&model)
                    .to_owned(),
                default_effort: entry
                    .get("defaultReasoningEffort")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                supported_efforts,
                is_default: entry
                    .get("isDefault")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                model,
            })
        })
        .collect()
}

fn str_field(value: &Value, name: &str) -> String {
    str_field_of(Some(value), name)
}

fn str_field_of(value: Option<&Value>, name: &str) -> String {
    value
        .and_then(|v| v.get(name))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}
