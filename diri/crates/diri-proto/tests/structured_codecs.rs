//! Structured-protocol codecs, checked against lines recorded from the
//! installed `claude` 2.1.331 and `codex-cli` 0.171.0. The fixtures are
//! sanitized (no home paths, MCP or hook payloads); regenerate them from a
//! fresh probe when either CLI changes its output.
use diri_proto::structured::{
    CodexInbound, CodexModel, StructuredEvent, TokenUsage, claude_user_message_line,
    codex_notification_line, codex_request_line, codex_response_line, codex_thread_id,
    codex_turn_id, codex_turn_start_params, parse_claude_line, parse_codex_line,
    parse_codex_models,
};
use serde_json::{Value, json};

const CLAUDE_TEXT: &str = include_str!("fixtures/structured/claude-text-stream.jsonl");
const CLAUDE_TOOL: &str = include_str!("fixtures/structured/claude-tool-use.jsonl");
const CODEX_TURN: &str = include_str!("fixtures/structured/codex-turn.jsonl");
const CODEX_MODELS: &str = include_str!("fixtures/structured/codex-model-list.json");

fn claude_events(fixture: &str) -> Vec<StructuredEvent> {
    fixture
        .lines()
        .flat_map(|line| parse_claude_line(line).expect("recorded line is valid JSON"))
        .collect()
}

fn codex_events(fixture: &str) -> Vec<StructuredEvent> {
    fixture
        .lines()
        .flat_map(
            |line| match parse_codex_line(line).expect("recorded line is valid JSON") {
                CodexInbound::Events(events) => events,
                _ => Vec::new(),
            },
        )
        .collect()
}

#[test]
fn claude_text_stream_emits_session_and_ordered_text_deltas() {
    let events = claude_events(CLAUDE_TEXT);
    assert_eq!(
        events.first(),
        Some(&StructuredEvent::SessionStarted {
            conversation_id: "11111111-2222-4333-8444-555555555555".to_owned(),
            model: Some("claude-haiku-5-5".to_owned()),
        })
    );
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            StructuredEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text, "pong",
        "deltas concatenate to the final assistant text"
    );
    let items: std::collections::BTreeSet<_> = events
        .iter()
        .filter_map(|event| match event {
            StructuredEvent::TextDelta { item, .. } => Some(item.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(items.len(), 1, "one content block keeps one item key");
}

#[test]
fn claude_turn_completion_reports_usage_and_cost() {
    let completed = claude_events(CLAUDE_TEXT)
        .into_iter()
        .find_map(|event| match event {
            StructuredEvent::TurnCompleted {
                is_error,
                subtype,
                usage,
                cost_usd,
            } => Some((is_error, subtype, usage, cost_usd)),
            _ => None,
        })
        .expect("result line completes the turn");
    assert!(!completed.0);
    assert_eq!(completed.1, "success");
    assert_eq!(
        completed.2,
        TokenUsage {
            input_tokens: 2,
            output_tokens: 4,
            cache_read_tokens: 11844,
            cache_write_tokens: 20924,
        }
    );
    assert!(completed.3.unwrap() > 0.0);
}

#[test]
fn claude_tool_use_pairs_start_and_finish_by_id() {
    let events = claude_events(CLAUDE_TOOL);
    let started: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            StructuredEvent::ToolStarted { id, name, input } => Some((id, name, input)),
            _ => None,
        })
        .collect();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].1, "Bash");
    assert_eq!(started[0].2["command"], "echo structured-ok");

    let finished: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            StructuredEvent::ToolFinished { id, is_error } => Some((id, *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(finished, vec![(started[0].0, false)]);
}

#[test]
fn claude_ignores_hook_and_rate_limit_lines() {
    for line in [
        r#"{"type":"system","subtype":"hook_started","hook_id":"h"}"#,
        r#"{"type":"rate_limit_event","rate_limit_info":{}}"#,
        r#"{"type":"system","subtype":"status","status":"requesting"}"#,
    ] {
        assert_eq!(parse_claude_line(line).unwrap(), Vec::new(), "{line}");
    }
}

#[test]
fn malformed_lines_are_errors_not_silent_drops() {
    assert!(parse_claude_line("{not json").is_err());
    assert!(parse_codex_line("").is_err());
}

#[test]
fn claude_user_message_is_one_escaped_line() {
    let line = claude_user_message_line("line one\nline \"two\"");
    assert!(!line.contains('\n'), "stdin frames are newline-delimited");
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["type"], "user");
    assert_eq!(value["message"]["role"], "user");
    assert_eq!(
        value["message"]["content"][0]["text"],
        "line one\nline \"two\""
    );
}

#[test]
fn codex_turn_streams_text_and_idle_completes_it() {
    let events = codex_events(CODEX_TURN);
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            StructuredEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "pong");

    // Recorded on app-server 0.171.0: no turn/completed arrives, so the
    // idle thread status is the end of the turn.
    assert!(events.iter().any(|event| matches!(
        event,
        StructuredEvent::TurnCompleted { subtype, .. } if subtype == "idle"
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StructuredEvent::ToolStarted { .. }))
    );
}

#[test]
fn codex_usage_splits_cached_input_from_fresh_input() {
    let usage = codex_events(CODEX_TURN)
        .into_iter()
        .find_map(|event| match event {
            StructuredEvent::UsageUpdated { usage } => Some(usage),
            _ => None,
        })
        .expect("tokenUsage notification");
    assert_eq!(usage.input_tokens, 22022);
    assert_eq!(usage.output_tokens, 5);
    assert_eq!(usage.cache_read_tokens, 0);
}

#[test]
fn codex_responses_correlate_by_id() {
    let lines: Vec<&str> = CODEX_TURN.lines().collect();
    let CodexInbound::Response { id, result } = parse_codex_line(lines[1]).unwrap() else {
        panic!("second recorded line answers thread/start");
    };
    assert_eq!(id, 2);
    assert_eq!(
        codex_thread_id(&result).as_deref(),
        Some("01a20cde-b4db-7943-96e7-07ff3d31860e")
    );
    let CodexInbound::Response { result, .. } = parse_codex_line(lines[2]).unwrap() else {
        panic!("third recorded line answers turn/start");
    };
    assert_eq!(
        codex_turn_id(&result).as_deref(),
        Some("01a20cde-b59a-7fa3-a2ee-aeaec411e0c6")
    );
}

#[test]
fn codex_server_requests_are_not_dropped() {
    let line = r#"{"jsonrpc":"2.0","id":"req-9","method":"item/commandExecution/requestApproval","params":{"itemId":"x"}}"#;
    match parse_codex_line(line).unwrap() {
        CodexInbound::ServerRequest { id, method, params } => {
            assert_eq!(id, json!("req-9"));
            assert_eq!(method, "item/commandExecution/requestApproval");
            assert_eq!(params["itemId"], "x");
            let reply: Value =
                serde_json::from_str(&codex_response_line(&id, json!({"decision":"accept"})))
                    .unwrap();
            assert_eq!(reply["id"], "req-9");
            assert_eq!(reply["result"]["decision"], "accept");
        }
        other => panic!("expected a server request, got {other:?}"),
    }
}

#[test]
fn codex_failure_responses_surface_the_message() {
    let line = r#"{"id":4,"error":{"code":-32600,"message":"thread not found"}}"#;
    assert_eq!(
        parse_codex_line(line).unwrap(),
        CodexInbound::Failure {
            id: Some(4),
            message: "thread not found".to_owned()
        }
    );
}

#[test]
fn codex_requests_and_handshake_encode_as_single_json_lines() {
    let init = codex_request_line(1, "initialize", json!({"clientInfo": {"name": "diri"}}));
    let value: Value = serde_json::from_str(&init).unwrap();
    assert_eq!(value["jsonrpc"], "2.0");
    assert_eq!(value["method"], "initialize");
    assert!(!init.contains('\n'));

    let initialized: Value = serde_json::from_str(&codex_notification_line("initialized")).unwrap();
    assert!(initialized.get("id").is_none(), "notifications carry no id");

    let params = codex_turn_start_params("thread-1", "hi", Some("low"));
    assert_eq!(params["threadId"], "thread-1");
    assert_eq!(params["input"][0]["text"], "hi");
    assert_eq!(params["effort"], "low");
    assert!(
        codex_turn_start_params("t", "hi", None)
            .get("effort")
            .is_none()
    );
}

#[test]
fn codex_model_catalog_keeps_efforts_and_default() {
    let result: Value = serde_json::from_str(CODEX_MODELS).unwrap();
    let models = parse_codex_models(&result);
    assert_eq!(models.len(), 10);
    let default: Vec<&CodexModel> = models.iter().filter(|m| m.is_default).collect();
    assert_eq!(default.len(), 1);
    assert_eq!(default[0].model, "gpt-6.2-sol");
    assert_eq!(default[0].default_effort.as_deref(), Some("low"));
    assert!(default[0].supported_efforts.contains(&"xhigh".to_owned()));
    let luna = models.iter().find(|m| m.model == "gpt-6-luna").unwrap();
    assert!(!luna.supported_efforts.contains(&"ultra".to_owned()));
}

const CLAUDE_PERMISSION: &str = include_str!("fixtures/structured/claude-permission.jsonl");
const CODEX_APPROVAL: &str = include_str!("fixtures/structured/codex-approval.jsonl");

#[test]
fn claude_can_use_tool_becomes_a_permission_request() {
    let events = claude_events(CLAUDE_PERMISSION);
    let [
        StructuredEvent::PermissionRequested {
            request_id,
            tool,
            input,
            ..
        },
    ] = events.as_slice()
    else {
        panic!("one permission request, got {events:?}");
    };
    assert!(!request_id.is_empty());
    assert_eq!(tool, "Bash");
    assert_eq!(input["command"], "rm perm-probe-file.txt");

    let allow: Value = serde_json::from_str(&diri_proto::structured::claude_permission_allow_line(
        request_id, input,
    ))
    .unwrap();
    assert_eq!(allow["type"], "control_response");
    assert_eq!(allow["response"]["request_id"], json!(request_id));
    assert_eq!(allow["response"]["response"]["behavior"], "allow");
    assert_eq!(allow["response"]["response"]["updatedInput"], *input);

    let deny: Value = serde_json::from_str(&diri_proto::structured::claude_permission_deny_line(
        request_id, "no",
    ))
    .unwrap();
    assert_eq!(deny["response"]["response"]["behavior"], "deny");
}

#[test]
fn codex_command_approval_becomes_a_permission_request_and_answers_by_id() {
    let line = CODEX_APPROVAL.lines().next().unwrap();
    let value: Value = serde_json::from_str(line).unwrap();
    let event = diri_proto::structured::codex_permission_request(
        &value["id"],
        value["method"].as_str().unwrap(),
        &value["params"],
    )
    .expect("command approvals are rendered");
    let StructuredEvent::PermissionRequested {
        request_id,
        tool,
        input,
        reason,
    } = event
    else {
        panic!("expected a permission request");
    };
    assert_eq!(tool, "Bash");
    assert!(
        input["command"]
            .as_str()
            .unwrap()
            .contains("codex-approve-target.txt")
    );
    assert!(reason.unwrap().contains("Allow deletion"));

    let reply: Value = serde_json::from_str(&diri_proto::structured::codex_approval_line(
        &value["id"],
        "accept",
    ))
    .unwrap();
    assert_eq!(reply["id"], value["id"]);
    assert_eq!(reply["result"]["decision"], "accept");
    assert!(!request_id.is_empty());
}

#[test]
fn unknown_codex_server_requests_are_not_rendered() {
    assert!(
        diri_proto::structured::codex_permission_request(
            &json!(1),
            "mcpServer/elicitation/request",
            &json!({})
        )
        .is_none()
    );
}

const AGY_PROMPT: &str = include_str!("fixtures/structured/agy-prompt.jsonl");

#[test]
fn agy_prompt_streams_text_and_reports_usage_on_its_reply() {
    use diri_proto::structured::{AcpInbound, acp_usage, parse_acp_line, parse_acp_models};
    let mut text = String::new();
    let mut usage = None;
    let mut models = Vec::new();
    for line in AGY_PROMPT.lines() {
        match parse_acp_line(line).expect("recorded line is valid JSON") {
            AcpInbound::Events(events) => {
                for event in events {
                    if let StructuredEvent::TextDelta { text: chunk, .. } = event {
                        text.push_str(&chunk);
                    }
                }
            }
            AcpInbound::Response { id: 2, result } => models = parse_acp_models(&result),
            AcpInbound::Response { id: 3, result } => usage = Some(acp_usage(&result)),
            _ => {}
        }
    }
    assert_eq!(text, "pong");
    assert_eq!(models.len(), 3, "the fixture keeps the first three models");
    assert_eq!(models[0].id, "gemini-4-neon-high");
    let usage = usage.expect("prompt reply carries usage");
    assert_eq!(usage.output_tokens, 20);
    assert_eq!(usage.input_tokens, 9890);
}
