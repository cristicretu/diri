//! Control-RPC state for structured conversations. Each conversation is
//! addressed by an opaque id returned from `structured.open`; the GUI holds
//! that id, the Engine holds the process. Kept apart from `control.rs` so the
//! dispatch stays a forwarding table.
use std::collections::BTreeMap;
use std::path::PathBuf;

use diri_proto::ControlError;
use diri_proto::structured::{AgentModel, CodexModel};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::inject::uuid_v4;
use crate::structured::{
    PermissionDecision, SequencedEvent, StructuredConversation, StructuredError, StructuredHarness,
    probe_antigravity_models, probe_codex_models,
};

/// Claude has no catalog command; these are the model ids the Agent SDK and
/// CLI accept, with the effort levels `claude --effort` takes.
const CLAUDE_MODELS: &[&str] = &[
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "claude-fable-5-2",
];
const CLAUDE_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// One conversation and whether the PTY Holder currently holds its native
/// conversation. Mirrors `formal/StructuredGui.lean`: the lease is taken only
/// when free, and the structured driver is parked before the PTY attaches.
struct Entry {
    conversation: StructuredConversation,
    pty_attached: bool,
}

#[derive(Default)]
pub struct StructuredRegistry {
    conversations: BTreeMap<String, Entry>,
    /// Catalogs are probed once per Engine, not per picker open.
    codex_models: Option<Vec<CodexModel>>,
    antigravity_models: Option<Vec<AgentModel>>,
    /// Program overrides for tests; `None` uses the installed binaries.
    programs: BTreeMap<&'static str, PathBuf>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenParams {
    harness: StructuredHarness,
    cwd: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConversationParams {
    conversation_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendParams {
    conversation_id: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigureParams {
    conversation_id: String,
    model: Option<String>,
    effort: Option<String>,
    /// Switching agent keeps the conversation and records a divider.
    harness: Option<StructuredHarness>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnswerParams {
    conversation_id: String,
    request_id: String,
    decision: String,
    message: Option<String>,
}

/// Where a conversation's native session runs, as the PTY side must start it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeCommand {
    pub program: String,
    pub args: Vec<String>,
    pub native_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelsParams {
    harness: StructuredHarness,
}

fn parse<T: for<'de> Deserialize<'de>>(params: Option<Value>) -> Result<T, ControlError> {
    serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|error| ControlError::bad_request(format!("invalid structured params: {error}")))
}

fn rpc_error(error: StructuredError) -> ControlError {
    match error {
        StructuredError::Unsupported { .. } => {
            ControlError::new("structured_unsupported", error.to_string())
        }
        other => ControlError::new("structured_failed", other.to_string()),
    }
}

fn event_json(item: &SequencedEvent) -> Value {
    let mut value = serde_json::to_value(&item.event).unwrap_or(Value::Null);
    value["sequence"] = json!(item.sequence);
    value
}

impl StructuredRegistry {
    /// Test hook: run `program` for `harness` instead of the installed CLI.
    pub fn with_program(mut self, harness: &'static str, program: PathBuf) -> Self {
        self.programs.insert(harness, program);
        self
    }

    fn conversation(&mut self, id: &str) -> Result<&mut StructuredConversation, ControlError> {
        let entry = self
            .conversations
            .get_mut(id)
            .ok_or_else(|| ControlError::not_found(format!("no structured conversation {id}")))?;
        if entry.pty_attached {
            return Err(ControlError::new(
                "pty_attached",
                "the PTY holds this conversation; switch back to the GUI first",
            ));
        }
        Ok(&mut entry.conversation)
    }

    /// Parks the structured driver and returns the argv that resumes the same
    /// native conversation in a PTY. Refused for Antigravity until its
    /// `session/load` path is verified.
    pub fn switch_to_pty(&mut self, params: Option<Value>) -> Result<ResumeCommand, ControlError> {
        let p: ConversationParams = parse(params)?;
        let entry = self
            .conversations
            .get_mut(&p.conversation_id)
            .ok_or_else(|| {
                ControlError::not_found(format!("no structured conversation {}", p.conversation_id))
            })?;
        if entry.pty_attached {
            return Err(ControlError::new(
                "pty_attached",
                "the PTY already holds this conversation",
            ));
        }
        let harness = entry.conversation.harness();
        let native = entry
            .conversation
            .native_id()
            .map(str::to_owned)
            .ok_or_else(|| {
                ControlError::bad_request("the conversation has no native session to resume yet")
            })?;
        let (program, args) = match harness {
            StructuredHarness::Claude => ("claude", vec!["--resume".to_owned(), native.clone()]),
            StructuredHarness::Codex => ("codex", vec!["resume".to_owned(), native.clone()]),
            StructuredHarness::Antigravity => {
                return Err(ControlError::new(
                    "structured_unsupported",
                    "Antigravity cannot hand off to the PTY yet",
                ));
            }
        };
        entry.conversation.stop();
        entry.pty_attached = true;
        Ok(ResumeCommand {
            program: program.to_owned(),
            args,
            native_id: native,
        })
    }

    /// Returns the conversation to the structured driver after the PTY detaches.
    pub fn switch_to_gui(&mut self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: ConversationParams = parse(params)?;
        let entry = self
            .conversations
            .get_mut(&p.conversation_id)
            .ok_or_else(|| {
                ControlError::not_found(format!("no structured conversation {}", p.conversation_id))
            })?;
        entry.pty_attached = false;
        Ok(json!({ "owner": "structured" }))
    }

    /// Creates a conversation. Starts no process.
    pub fn open(&mut self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: OpenParams = parse(params)?;
        let id = uuid_v4();
        let mut conversation = StructuredConversation::new(p.harness, PathBuf::from(&p.cwd));
        if let Some(program) = self.programs.get(harness_key(p.harness)) {
            conversation = conversation.with_program(program.clone());
        }
        self.conversations.insert(
            id.clone(),
            Entry {
                conversation,
                pty_attached: false,
            },
        );
        Ok(json!({ "conversationId": id, "harness": p.harness, "spawnCount": 0 }))
    }

    pub fn send(&mut self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: SendParams = parse(params)?;
        let conversation = self.conversation(&p.conversation_id)?;
        conversation.send_turn(&p.text).map_err(rpc_error)?;
        Ok(json!({ "turnInFlight": true, "spawnCount": conversation.spawn_count() }))
    }

    /// Model, effort, or agent. Before turn 1 this is a field write.
    pub fn configure(&mut self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: ConfigureParams = parse(params)?;
        let conversation = self.conversation(&p.conversation_id)?;
        if let Some(harness) = p.harness {
            conversation.switch_harness(harness);
        }
        if p.model.is_some() || p.effort.is_some() {
            let model = p.model.or_else(|| conversation.model().map(str::to_owned));
            let effort = p
                .effort
                .or_else(|| conversation.effort().map(str::to_owned));
            conversation.set_model(model, effort);
        }
        Ok(json!({
            "harness": conversation.harness(),
            "model": conversation.model(),
            "effort": conversation.effort(),
            "spawnCount": conversation.spawn_count(),
        }))
    }

    /// Events since the last poll, each with its sequence number.
    pub fn poll(&mut self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: ConversationParams = parse(params)?;
        let conversation = self.conversation(&p.conversation_id)?;
        let events: Vec<Value> = conversation.poll().iter().map(event_json).collect();
        let pending: Vec<Value> = conversation
            .pending_permissions()
            .into_iter()
            .map(|pending| {
                json!({
                    "requestId": pending.request_id,
                    "tool": pending.tool,
                    "input": pending.input,
                    "reason": pending.reason,
                })
            })
            .collect();
        Ok(json!({
            "events": events,
            "pendingPermissions": pending,
            "turnInFlight": conversation.is_turn_in_flight(),
        }))
    }

    pub fn answer_permission(&mut self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: AnswerParams = parse(params)?;
        let decision = match p.decision.as_str() {
            "allow" => PermissionDecision::Allow,
            "allowForSession" => PermissionDecision::AllowForSession,
            "deny" => PermissionDecision::Deny,
            other => {
                return Err(ControlError::bad_request(format!(
                    "decision must be allow, allowForSession or deny, not {other}"
                )));
            }
        };
        self.conversation(&p.conversation_id)?
            .answer_permission(&p.request_id, decision, p.message.as_deref())
            .map_err(rpc_error)?;
        Ok(json!({ "answered": p.request_id }))
    }

    /// Ends the running Agent, keeping its native conversation for the next turn.
    pub fn stop(&mut self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: ConversationParams = parse(params)?;
        self.conversation(&p.conversation_id)?.stop();
        Ok(json!({ "stopped": true }))
    }

    pub fn close(&mut self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: ConversationParams = parse(params)?;
        self.conversations.remove(&p.conversation_id);
        Ok(json!({ "closed": true }))
    }

    /// The model and effort choices for the picker, from the CLI where it can
    /// be asked (cached after the first call), else a fixed list.
    pub fn models(&mut self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: ModelsParams = parse(params)?;
        match p.harness {
            StructuredHarness::Claude => Ok(json!({
                "models": CLAUDE_MODELS.iter().map(|id| json!({"id": id, "name": id})).collect::<Vec<_>>(),
                "efforts": CLAUDE_EFFORTS,
                "source": "static",
            })),
            StructuredHarness::Codex => {
                if self.codex_models.is_none() {
                    let program = self
                        .programs
                        .get("codex")
                        .map(|p| p.clone().into_os_string())
                        .unwrap_or_else(|| "codex".into());
                    self.codex_models = Some(probe_codex_models(program).map_err(rpc_error)?);
                }
                let models = self.codex_models.as_deref().unwrap_or_default();
                Ok(json!({
                    "models": models,
                    "source": "probed",
                }))
            }
            StructuredHarness::Antigravity => {
                if self.antigravity_models.is_none() {
                    let program = self
                        .programs
                        .get("antigravity")
                        .map(|p| p.clone().into_os_string())
                        .unwrap_or_else(|| "agy".into());
                    self.antigravity_models =
                        Some(probe_antigravity_models(program).map_err(rpc_error)?);
                }
                Ok(json!({
                    "models": self.antigravity_models.as_deref().unwrap_or_default(),
                    "efforts": [],
                    "source": "probed",
                }))
            }
        }
    }
}

fn harness_key(harness: StructuredHarness) -> &'static str {
    match harness {
        StructuredHarness::Claude => "claude",
        StructuredHarness::Codex => "codex",
        StructuredHarness::Antigravity => "antigravity",
    }
}
