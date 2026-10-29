//! The Engine's self-description: every control method with a JSON Schema
//! for its parameters, the name of its result type and an example, plus the
//! connected app's actions. `api.list`, `api.describe` and `api.llms` serve
//! it; `dirijor api` prints it.
//!
//! The schemas are generated from the same Rust types the Engine decodes
//! (`schemars`, behind this crate's `schema` feature, so the remote Helper
//! never builds it). Tests keep [`crate::Method::ALL`], the method constants
//! and this catalog in step, and decode every example into its real type.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app_actions::AppActionSpec;

/// One Engine method.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiMethod {
    pub name: String,
    pub summary: String,
    /// JSON Schema for `params`.
    pub params: Value,
    /// The Rust type the result decodes into (`SessionRecord`, `{}`).
    pub result: String,
    pub example: Value,
}

/// One line of `api.list`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ApiEntry {
    pub name: String,
    pub summary: String,
}

/// `api.list`: names and one-line summaries; `api.describe` has the rest.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiListResult {
    pub methods: Vec<ApiEntry>,
    /// The connected app's actions, run with `app.perform`. Empty, with
    /// `app_connected` false, while no app is connected.
    pub actions: Vec<ApiEntry>,
    pub app_connected: bool,
}

/// `api.describe`: a method name, or an app action's name.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApiDescribeParams {
    pub name: String,
}

/// Exactly one of the two is set.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ApiDescribeResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<ApiMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<AppActionSpec>,
}

/// `api.llms`: the whole surface as compact text.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ApiTextResult {
    pub text: String,
}

/// The compact text an Agent reads: one line per method with its required
/// and optional parameters, then one line per app action.
pub fn llms_text(methods: &[ApiMethod], actions: &[AppActionSpec], app_connected: bool) -> String {
    let mut text = String::from(
        "# Diri Engine API\n\n\
         Newline-delimited JSON over the Engine's Unix socket: send {\"id\":N,\"method\":M,\"params\":{...}}, \
         read {\"id\":N,\"ok\":...} or {\"id\":N,\"err\":{\"code\",\"message\"}}. Start with \"hello\". \
         From a shell: `dirijor api describe METHOD` for the full JSON Schema and an example.\n\n\
         ## Methods (name(required; optional?) -> result: summary)\n",
    );
    for method in methods {
        text.push_str(&format!(
            "- {}({}) -> {}: {}\n",
            method.name,
            signature(&method.params),
            method.result,
            method.summary
        ));
    }
    text.push_str(
        "\n## App actions (run with app.perform {\"action\":NAME,\"args\":{...}}, \
         or `dirijor do NAME key=value`)\n",
    );
    if !app_connected {
        text.push_str("No Diri app is connected, so no actions can run now.\n");
    }
    for action in actions {
        let destructive = if action.risk == crate::app_actions::ActionRisk::Destructive {
            " [destructive]"
        } else {
            ""
        };
        text.push_str(&format!(
            "- {}({}): {}{}\n",
            action.name,
            signature(&action.args),
            if action.description.is_empty() {
                &action.title
            } else {
                &action.description
            },
            destructive
        ));
    }
    text
}

/// `a, b, c?` for an object schema's properties.
fn signature(schema: &Value) -> String {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return String::new();
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|keys| keys.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    // Required first, so the line reads like a call; then by name, so the
    // text is the same whether or not serde_json keeps insertion order (a
    // workspace build can unify `preserve_order` on).
    let mut keys: Vec<&String> = properties.keys().collect();
    keys.sort_by(|a, b| {
        (!required.contains(&a.as_str()), a.as_str())
            .cmp(&(!required.contains(&b.as_str()), b.as_str()))
    });
    keys.into_iter()
        .map(|key| {
            if required.contains(&key.as_str()) {
                key.clone()
            } else {
                format!("{key}?")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(feature = "schema")]
pub use catalog::{example_for, methods};

#[cfg(feature = "schema")]
mod catalog {
    use schemars::{JsonSchema, generate::SchemaSettings};
    use serde::de::DeserializeOwned;
    use serde_json::{Map, Value, json};

    use super::ApiMethod;
    use crate::Method;

    struct Entry {
        name: &'static str,
        summary: &'static str,
        params: fn() -> Value,
        #[cfg_attr(not(test), allow(dead_code))]
        check: fn(Value) -> Result<(), String>,
        result: &'static str,
        example: Option<fn() -> Value>,
    }

    fn schema<T: JsonSchema>() -> Value {
        let generator = SchemaSettings::draft2020_12().into_generator();
        let schema = generator.into_root_schema_for::<T>();
        serde_json::to_value(schema).unwrap_or(Value::Null)
    }

    fn check<T: DeserializeOwned>(value: Value) -> Result<(), String> {
        serde_json::from_value::<T>(value)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    macro_rules! entry {
        ($name:expr, $params:ty, $result:literal, $summary:literal) => {
            Entry {
                name: $name,
                summary: $summary,
                params: schema::<$params>,
                check: check::<$params>,
                result: $result,
                example: None,
            }
        };
        ($name:expr, $params:ty, $result:literal, $summary:literal, $example:expr) => {
            Entry {
                name: $name,
                summary: $summary,
                params: schema::<$params>,
                check: check::<$params>,
                result: $result,
                example: Some($example),
            }
        };
    }

    use crate::{
        accounts as acc, app_actions as app, schedules as sch, tasks as task, workspace as ws,
    };

    /// `session.spawn_tracked`'s envelope, decoded by the Engine itself.
    #[derive(serde::Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct SessionSpawnTrackedParams {
        #[serde(rename = "senderID")]
        sender_id: String,
        #[serde(rename = "operationID")]
        operation_id: String,
        spawn: crate::SessionSpawnParams,
    }

    /// A browser or test step, passed through to the browser pool as is.
    #[derive(serde::Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct PassThrough(Map<String, Value>);

    fn entries() -> Vec<Entry> {
        use crate::*;
        vec![
            entry!(
                Method::HELLO,
                HelloParams,
                "HelloResult",
                "Open a control session: protocol version and client build; answers the Engine's identity."
            ),
            entry!(
                Method::WORKSPACE_SNAPSHOT,
                EmptyParams,
                "WorkspaceSnapshot",
                "Workspaces, their tabs and each tab's split layout."
            ),
            entry!(
                Method::WORKSPACE_MUTATE,
                ws::WorkspaceMutationParams,
                "WorkspaceSnapshot",
                "Change workspaces, tabs or splits at an expected revision; a stale revision is rejected.",
                || json!({"expectedRevision": 3, "mutation": {"type": "zoomPane", "tabId": "<tabId>", "paneId": "<paneId>"}})
            ),
            entry!(
                Method::SESSION_SPAWN,
                SessionSpawnParams,
                "SessionRecord",
                "Start an Agent or shell session.",
                || json!({"kind": "shell", "cwd": "/Users/me/project"})
            ),
            entry!(
                Method::SESSION_SPAWN_TRACKED,
                SessionSpawnTrackedParams,
                "SessionRecord",
                "Start a session on behalf of a sender, deduplicated by operation id."
            ),
            entry!(
                Method::SESSION_LIST,
                EmptyParams,
                "SessionListResult",
                "Every session record and project."
            ),
            entry!(
                Method::STATE_SNAPSHOT,
                EmptyParams,
                "SessionListResult",
                "Alias of session.list."
            ),
            entry!(
                Method::SESSION_KILL,
                SessionIdParams,
                "{}",
                "Stop a session's process; the record stays."
            ),
            entry!(
                Method::SESSION_REMOVE,
                SessionIdParams,
                "{}",
                "Stop a session and delete its record."
            ),
            entry!(
                Method::SESSION_RENAME,
                SessionRenameParams,
                "SessionRecord",
                "Rename a session."
            ),
            entry!(
                Method::SESSION_PROCESS_INFO,
                SessionIdParams,
                "SessionProcessInfo",
                "Foreground program, working directory and process facts."
            ),
            entry!(
                Method::SESSION_RECONNECT,
                SessionReconnectParams,
                "SessionRecord",
                "Reconnect a remote session to its Holder."
            ),
            entry!(
                Method::SESSION_RESUME,
                SessionIdParams,
                "SessionRecord",
                "Resume an exited session's conversation."
            ),
            entry!(
                Method::SESSION_FORK,
                SessionForkParams,
                "SessionRecord",
                "Fork a session's conversation into a new session."
            ),
            entry!(
                Method::SESSION_DELIVER_MESSAGE,
                DeliverMessageParams,
                "DeliverMessageResult",
                "Deliver a message to an Agent with at-most-once identity."
            ),
            entry!(
                Method::SESSION_SEND_KEY,
                SendKeyParams,
                "{}",
                "Press one named key in a session's terminal.",
                || json!({"sessionID": "<sessionID>", "key": {"named": "enter"}})
            ),
            entry!(
                Method::SESSION_SEND_TEXT,
                SendTextParams,
                "{}",
                "Type text into a session's terminal, optionally pressing Return.",
                || json!({"sessionID": "<sessionID>", "text": "npm test", "submit": true})
            ),
            entry!(
                Method::SESSION_RESIZE,
                ResizeParams,
                "{}",
                "Resize a session's terminal."
            ),
            entry!(
                Method::SESSION_READ_SCREEN,
                SessionIdParams,
                "ReadScreenResult",
                "The visible screen as text."
            ),
            entry!(
                Method::SESSION_TERMINAL_TITLE,
                SessionTerminalTitleParams,
                "TerminalTitleResult",
                "The terminal's current OSC title."
            ),
            entry!(
                Method::SESSION_RESET_TERMINAL,
                SessionIdParams,
                "{}",
                "Reset the emulator without touching the process."
            ),
            entry!(
                Method::SESSION_CLEAR_SCROLLBACK,
                SessionIdParams,
                "{}",
                "Drop the scrollback and clear the screen; the process keeps running."
            ),
            entry!(
                Method::SESSION_LIST_DIR,
                SessionListDirParams,
                "SessionListDirResult",
                "List a directory as the session sees it, local or remote, relative to its working directory."
            ),
            entry!(
                Method::SESSION_CAPTURE_FIND,
                SessionIdParams,
                "CaptureFindResult",
                "Screen plus history as text, for find."
            ),
            entry!(
                Method::SESSION_FIND_HISTORY,
                crate::find::FindHistoryParams,
                "FindHistoryResult",
                "One bounded page of a search over a session's whole history, by stable row."
            ),
            entry!(
                Method::SESSION_READ_SCROLLBACK,
                SessionIdParams,
                "ReadScrollbackResult",
                "The terminal history as text."
            ),
            entry!(
                Method::SESSION_READ_SCROLLBACK_CELLS,
                ReadScrollbackCellsParams,
                "ScrollbackCells",
                "A window of history rows with styles."
            ),
            entry!(
                Method::SESSION_READ_DIFF,
                SessionReadDiffParams,
                "SessionDiff",
                "The git diff of a session's working tree."
            ),
            entry!(
                Method::SESSION_READ_IMAGE,
                SessionReadImageParams,
                "InlineImageChunk",
                "One chunk of an inline image the terminal placed."
            ),
            entry!(
                Method::SESSION_READ_TRANSCRIPT,
                ReadTranscriptParams,
                "ReadTranscriptResult",
                "The latest conversation turns of an Agent."
            ),
            entry!(
                Method::SESSION_MARK_SEEN,
                SessionIdParams,
                "{}",
                "Mark a session's finished turn as seen."
            ),
            entry!(
                Method::SESSION_MARK_UNREAD,
                SessionIdParams,
                "{}",
                "Return a finished turn to unseen."
            ),
            entry!(
                Method::SESSION_HIBERNATE,
                SessionIdParams,
                "SessionRecord",
                "Put an idle Agent to sleep, freeing its memory."
            ),
            entry!(
                Method::SESSION_WAKE,
                SessionIdParams,
                "SessionRecord",
                "Wake a sleeping Agent."
            ),
            entry!(
                Method::SESSION_ARCHIVE,
                SessionIdParams,
                "{}",
                "Stop a session and move it to the archive."
            ),
            entry!(
                Method::SESSION_UNARCHIVE,
                SessionIdParams,
                "{}",
                "Restore an archived session's record."
            ),
            entry!(
                Method::SESSION_REOPEN_LAST,
                EmptyParams,
                "SessionRecord",
                "Reopen the most recently closed session."
            ),
            entry!(
                Method::SESSION_REVEAL,
                SessionIdParams,
                "{}",
                "Ask the app to show a session without taking focus."
            ),
            entry!(
                Method::SESSION_OPEN_API_REQUEST,
                SessionOpenApiRequestParams,
                "{}",
                "Open a prefilled HTTP request in a session's API tab."
            ),
            entry!(
                Method::SESSION_MIGRATE,
                SessionMigrateParams,
                "SessionRecord",
                "Move a Claude session between this Mac and a host."
            ),
            entry!(
                Method::SESSION_REPARENT_WORKTREE,
                SessionReparentWorktreeParams,
                "SessionRecord",
                "Record that a session now works in another worktree."
            ),
            entry!(
                Method::SESSION_HISTORY,
                EmptyParams,
                "SessionHistoryResult",
                "Past Agent conversations that can be resumed."
            ),
            entry!(
                Method::SESSION_RESUME_FROM_HISTORY,
                ResumeFromHistoryParams,
                "SessionRecord",
                "Open a session resuming a past conversation."
            ),
            entry!(
                Method::SESSION_CONTINUE_ACCOUNT,
                ContinueAccountParams,
                "SessionRecord",
                "Continue a conversation under another saved account."
            ),
            entry!(
                Method::ACTIVITY_LIST,
                ActivityListParams,
                "ActivityListResult",
                "Recent session activity, newest first."
            ),
            entry!(
                Method::TASK_SUBMIT,
                task::TaskSubmitParams,
                "TaskRecord",
                "Assign a tracked task to an Agent."
            ),
            entry!(
                Method::TASK_GET,
                task::TaskGetParams,
                "TaskRecord",
                "Read one task by id or request id."
            ),
            entry!(
                Method::TASK_REPORT,
                task::TaskReportParams,
                "TaskRecord",
                "Acknowledge or report a task you were assigned."
            ),
            entry!(
                Method::TASK_ANSWER,
                task::TaskAnswerParams,
                "TaskRecord",
                "Answer a task you submitted."
            ),
            entry!(
                Method::TASK_CANCEL,
                task::TaskCancelParams,
                "TaskRecord",
                "Withdraw a task you submitted."
            ),
            entry!(
                Method::TASK_LIST,
                task::TaskListParams,
                "TaskListResult",
                "Tasks you sent or were assigned."
            ),
            entry!(
                Method::SCHEDULE_CREATE,
                sch::ScheduleSpec,
                "ScheduleRecord",
                "Schedule Agent runs owned by Diri."
            ),
            entry!(
                Method::SCHEDULE_UPDATE,
                sch::ScheduleUpdateParams,
                "ScheduleRecord",
                "Change a schedule."
            ),
            entry!(
                Method::SCHEDULE_DELETE,
                sch::ScheduleIdParams,
                "{}",
                "Delete a schedule."
            ),
            entry!(
                Method::SCHEDULE_LIST,
                EmptyParams,
                "ScheduleListResult",
                "Every schedule with its next due time."
            ),
            entry!(
                Method::SCHEDULE_RUN_NOW,
                sch::ScheduleIdParams,
                "ScheduleRecord",
                "Start one run now without moving the schedule."
            ),
            entry!(
                Method::COMPUTER_ACT,
                ComputerActParams,
                "Value",
                "One Cua Driver step under the caller's own cursor."
            ),
            entry!(
                Method::COMPUTER_STATUS,
                ComputerStatusParams,
                "ComputerStatus",
                "Whether computer use is installed and allowed."
            ),
            entry!(
                Method::COMPUTER_SETUP,
                ComputerSetupParams,
                "ComputerStatus",
                "Install or restart computer use."
            ),
            entry!(
                Method::HOST_SYNC_PREFS,
                HostSyncPrefsParams,
                "PrefsSyncReport",
                "Push Agent preferences to a remote host."
            ),
            entry!(
                Method::HOST_LOCATE_REPO,
                HostLocateRepoParams,
                "HostLocateRepoResult",
                "Find a session's repository on a host."
            ),
            entry!(
                Method::HOST_INITIALIZE,
                HostInitializeParams,
                "HostInitializeResult",
                "Install or verify the remote Helper on a host."
            ),
            entry!(
                Method::HOST_USAGE,
                HostUsageParams,
                "Value",
                "Agent usage on a host."
            ),
            entry!(
                Method::HOST_LIST_DIRECTORIES,
                HostListDirectoriesParams,
                "HostListDirectoriesResult",
                "One level of folders on a host."
            ),
            entry!(
                Method::HOST_LIST,
                EmptyParams,
                "HostsConfig",
                "Configured hosts."
            ),
            entry!(
                Method::WORKTREE_CREATE,
                WorktreeCreateParams,
                "WorktreeInfo",
                "Create a git worktree."
            ),
            entry!(
                Method::WORKTREE_LIST,
                WorktreeListParams,
                "WorktreeListResult",
                "A repository's worktrees."
            ),
            entry!(
                Method::WORKTREE_CLEANUP,
                WorktreeCleanupParams,
                "WorktreeCleanupResult",
                "Remove finished worktrees."
            ),
            entry!(
                Method::WORKTREE_REMOVE,
                WorktreeRemoveParams,
                "{}",
                "Remove one worktree."
            ),
            entry!(
                Method::WORKTREE_SCAN,
                WorktreeScanParams,
                "WorktreeScanPage",
                "Scan for worktrees, a page at a time."
            ),
            entry!(
                Method::WORKTREE_OVERVIEW,
                EmptyParams,
                "WorktreeOverviewResult",
                "Every worktree Diri knows with its sessions."
            ),
            entry!(
                Method::WORKTREE_INTEGRATE,
                WorktreeIntegrateParams,
                "WorktreeIntegrateResult",
                "Merge one session's branch into another's."
            ),
            entry!(
                Method::PROJECT_ADD,
                ProjectAddParams,
                "Project",
                "Add a project folder."
            ),
            entry!(
                Method::CLIENT_SET_ACTIVE,
                ClientActiveParams,
                "{}",
                "Tell the Engine whether the app is in front."
            ),
            entry!(
                Method::GOVERNOR_CONFIGURE,
                GovernorSettingsParams,
                "{}",
                "Idle and memory limits for sleeping Agents."
            ),
            entry!(
                Method::TERMINAL_CONFIGURE,
                TerminalConfigureParams,
                "{}",
                "Terminal settings the Engine applies."
            ),
            entry!(
                Method::AGENT_READINESS,
                AgentReadinessParams,
                "AgentReadinessResult",
                "Installed Agents and their descriptors."
            ),
            entry!(
                Method::AGENT_CONFIGURE,
                AgentConfigureParams,
                "AgentReadinessResult",
                "Paths, arguments and visibility of an Agent."
            ),
            entry!(
                Method::ACCOUNT_PROFILES_LIST,
                EmptyParams,
                "AgentAccountCatalog",
                "Saved Agent accounts (no secrets)."
            ),
            entry!(
                Method::ACCOUNT_PROFILES_SAVE,
                acc::AgentAccountProfile,
                "AgentAccountCatalog",
                "Save an account profile."
            ),
            entry!(
                Method::ACCOUNT_PROFILES_REMOVE,
                acc::AgentAccountId,
                "AgentAccountCatalog",
                "Remove an account profile."
            ),
            entry!(
                Method::ACCOUNT_CODEX_LOGIN,
                acc::AgentAccountId,
                "AgentAccountLogin",
                "Start a Codex sign-in for a profile."
            ),
            entry!(
                Method::ACCOUNT_CODEX_CAPTURE,
                acc::AgentAccountId,
                "AgentAccountCatalog",
                "Save the signed-in Codex account."
            ),
            entry!(
                Method::ACCOUNT_CLAUDE_LOGIN,
                acc::AgentAccountId,
                "AgentAccountLogin",
                "Start a Claude sign-in for a profile."
            ),
            entry!(
                Method::ACCOUNT_CLAUDE_CAPTURE,
                acc::AgentAccountId,
                "AgentAccountCatalog",
                "Save the signed-in Claude account."
            ),
            entry!(
                Method::ACCOUNT_SWITCH_ALL,
                acc::SwitchAccountParams,
                "SwitchAccountResult",
                "Switch every open tab of an Agent to a saved account."
            ),
            entry!(
                Method::ACCOUNT_OVERVIEW,
                EmptyParams,
                "AgentAccountOverview",
                "Accounts in use and limits."
            ),
            entry!(
                Method::ACCOUNT_ADOPT,
                acc::AgentAccountAgent,
                "AgentAccountCatalog",
                "Save the Agent's current login as a profile."
            ),
            entry!(
                Method::ACCOUNT_ADD,
                acc::AgentAccountAgent,
                "AgentAccountLogin",
                "Add another account for an Agent."
            ),
            entry!(
                Method::EVENTS_SUBSCRIBE,
                EventsSubscribeParams,
                "{subscribed}",
                "Stream events on this connection, replaying from sinceSeq."
            ),
            entry!(
                Method::EVENTS_WAIT,
                EventsWaitParams,
                "EventsWaitResult",
                "Wait for a session to reach a status."
            ),
            entry!(
                Method::HOOK_REPORT,
                HookReportParams,
                "{}",
                "An Agent hook or notify callback."
            ),
            entry!(
                Method::VOICE_STATE,
                EmptyParams,
                "VoiceStateResult",
                "Whether Talk with your agent is on."
            ),
            entry!(
                Method::VOICE_SPEAK,
                voice::SpeakParams,
                "{}",
                "An Agent's spoken summary of its turn."
            ),
            entry!(
                Method::VOICE_SPOKEN,
                voice::SpokenParams,
                "SpokenResult",
                "Collect a session's spoken summary."
            ),
            entry!(
                Method::VOICE_LATEST_SPOKEN,
                voice::LatestSpokenParams,
                "LatestSpokenResult",
                "Read the latest spoken summary without collecting it."
            ),
            entry!(
                Method::VOICE_PHONE_READER,
                voice::PhoneReaderParams,
                "PhoneReaderResult",
                "Get or set whether a phone wants spoken summaries."
            ),
            entry!(
                Method::TEST_RUN,
                PassThrough,
                "Value",
                "Run a browser test step in the shared pool."
            ),
            entry!(
                Method::BROWSER_ACT,
                PassThrough,
                "Value",
                "One step of the shared browser."
            ),
            entry!(
                Method::DAEMON_PREPARE_SHUTDOWN,
                EmptyParams,
                "{}",
                "Flush state before the Engine stops."
            ),
            entry!(
                Method::DAEMON_SHUTDOWN_IF_IDLE,
                EmptyParams,
                "DaemonShutdownIfIdleResult",
                "Stop the Engine if nothing needs it."
            ),
            entry!(
                Method::DAEMON_SHUTDOWN,
                EmptyParams,
                "{}",
                "Stop the Engine; sessions in Holders keep running."
            ),
            entry!(
                Method::TELEMETRY_UPLOAD_NOW,
                EmptyParams,
                "TelemetryUploadNowResult",
                "Upload diagnostics now."
            ),
            entry!(
                Method::APP_ACTIONS_PUBLISH,
                app::AppActionsPublishParams,
                "{registered}",
                "The app's exported actions; this connection performs them."
            ),
            entry!(
                Method::APP_ACTIONS_LIST,
                EmptyParams,
                "AppActionsListResult",
                "The connected app's actions with argument schemas."
            ),
            entry!(
                Method::APP_PERFORM,
                app::AppPerformParams,
                "AppPerformResult",
                "Run one app action in the window and wait for its answer.",
                || json!({"action": "split-pane-right", "args": {}})
            ),
            entry!(
                Method::APP_REPLY,
                app::AppReplyParams,
                "{}",
                "The app's answer to app.perform_requested."
            ),
            entry!(
                Method::API_LIST,
                EmptyParams,
                "ApiListResult",
                "Every method and app action, one line each."
            ),
            entry!(
                Method::API_DESCRIBE,
                super::ApiDescribeParams,
                "ApiDescribeResult",
                "One method's or action's schema and example.",
                || json!({"name": "session.send_text"})
            ),
            entry!(
                Method::API_LLMS,
                EmptyParams,
                "ApiTextResult",
                "The whole API as compact text for an Agent."
            ),
        ]
    }

    /// Every method, in [`Method::ALL`] order, with schema and example.
    pub fn methods() -> Vec<ApiMethod> {
        let mut entries = entries();
        entries.sort_by_key(|entry| {
            Method::ALL
                .iter()
                .position(|name| *name == entry.name)
                .unwrap_or(usize::MAX)
        });
        entries
            .into_iter()
            .map(|entry| {
                let params = (entry.params)();
                let example = entry
                    .example
                    .map_or_else(|| example_for(&params), |example| example());
                ApiMethod {
                    name: entry.name.into(),
                    summary: entry.summary.into(),
                    params,
                    result: entry.result.into(),
                    example,
                }
            })
            .collect()
    }

    /// Decodes `example` into `method`'s real params type.
    #[cfg(test)]
    fn check_example(method: &str, example: Value) -> Result<(), String> {
        let entry = entries()
            .into_iter()
            .find(|entry| entry.name == method)
            .ok_or_else(|| format!("no entry for {method}"))?;
        (entry.check)(example)
    }

    /// The smallest params object the schema allows: required fields only,
    /// filled with placeholders (`"<sessionID>"`, `0`, the first enum value).
    pub fn example_for(schema: &Value) -> Value {
        instance(schema, schema, None, 0)
    }

    fn instance(schema: &Value, root: &Value, name: Option<&str>, depth: usize) -> Value {
        if depth > 16 {
            return Value::Null;
        }
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str)
            && let Some(target) = reference.strip_prefix('#').and_then(|p| root.pointer(p))
        {
            return instance(target, root, name, depth + 1);
        }
        if let Some(value) = schema.get("const") {
            return value.clone();
        }
        if let Some(first) = schema
            .get("enum")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
        {
            return first.clone();
        }
        for keyword in ["oneOf", "anyOf"] {
            if let Some(branches) = schema.get(keyword).and_then(Value::as_array) {
                let branch = branches
                    .iter()
                    .find(|branch| branch.get("type") != Some(&json!("null")))
                    .or_else(|| branches.first());
                if let Some(branch) = branch {
                    return instance(branch, root, name, depth + 1);
                }
            }
        }
        let kind = match schema.get("type") {
            Some(Value::String(kind)) => kind.as_str(),
            Some(Value::Array(kinds)) => kinds
                .iter()
                .filter_map(Value::as_str)
                .find(|kind| *kind != "null")
                .unwrap_or("null"),
            _ if schema.get("properties").is_some() => "object",
            _ => "null",
        };
        match kind {
            "object" => {
                let mut object = Map::new();
                let required: Vec<&str> = schema
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|keys| keys.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                    for (key, property) in properties {
                        if required.contains(&key.as_str()) {
                            object.insert(
                                key.clone(),
                                instance(property, root, Some(key), depth + 1),
                            );
                        }
                    }
                }
                Value::Object(object)
            }
            "array" => json!([]),
            "string" => json!(format!("<{}>", name.unwrap_or("text"))),
            "integer" | "number" => {
                json!(schema.get("minimum").and_then(Value::as_u64).unwrap_or(0))
            }
            "boolean" => json!(false),
            _ => Value::Null,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_method_constant_is_listed_and_described_once() {
            // The constants as written in the source, so a new method cannot
            // be added without listing it in `Method::ALL` and describing it.
            let source = include_str!("methods.rs");
            let block = &source[source.find("impl Method {").unwrap()..];
            let block = &block[..block.find("pub const ALL").unwrap()];
            let declared: Vec<&str> = block
                .lines()
                .filter_map(|line| line.trim().strip_prefix("pub const "))
                .filter_map(|line| line.split('"').nth(1))
                .collect();
            let mut all: Vec<&str> = Method::ALL.to_vec();
            all.sort_unstable();
            let mut sorted = declared.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, all, "Method::ALL must list every method constant");
            let described: Vec<String> = methods().into_iter().map(|m| m.name).collect();
            for name in Method::ALL {
                assert_eq!(
                    described.iter().filter(|d| d.as_str() == *name).count(),
                    1,
                    "{name} must be described exactly once"
                );
            }
            assert_eq!(described.len(), Method::ALL.len());
        }

        #[test]
        fn every_method_has_an_object_schema_a_summary_and_a_valid_example() {
            let mut failures = Vec::new();
            for method in methods() {
                assert!(!method.summary.is_empty(), "{}", method.name);
                assert!(!method.result.is_empty(), "{}", method.name);
                if method.params.get("type") != Some(&json!("object"))
                    && method.params.get("oneOf").is_none()
                    && method.params.get("anyOf").is_none()
                {
                    failures.push(format!("{} params: {}", method.name, method.params));
                }
                if let Err(error) =
                    crate::json_schema::validate(&method.example, &method.params, "example")
                {
                    failures.push(format!(
                        "{} schema: {error} ({})",
                        method.name, method.example
                    ));
                }
                if let Err(error) = check_example(&method.name, method.example.clone()) {
                    failures.push(format!(
                        "{} decode: {error} ({})",
                        method.name, method.example
                    ));
                }
            }
            assert!(failures.is_empty(), "{}", failures.join("\n"));
        }
    }
}
