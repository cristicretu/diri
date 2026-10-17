//! The MCP tool catalog shared by the Rust stdio frontend and CLI.

use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl ToolDefinition {
    fn new(name: &str, description: &str, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }

    pub fn wire_value(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
        })
    }
}

pub fn tool_definitions_for(kinds: &[String]) -> Vec<ToolDefinition> {
    let kind_enum: Vec<Value> = kinds.iter().map(|kind| json!(kind)).collect();
    let mut tools = vec![
        ToolDefinition::new(
            "get_skill",
            "Read one of Diri's skills: the detailed rules for a Diri capability, as Markdown. Read the matching skill before acting: scheduling (run anything later, at a time, or repeatedly; waking the Mac), notes (work started from or written to a Diri note), orchestration (parallel agents, tasks, waiting, retries), api (HTTP APIs and dev servers: show endpoints in the API tab). Claude Code sessions also have them as skills named diri:<name>.",
            json!({"type":"object","properties":{"name":{"type":"string","enum":["scheduling","notes","orchestration","api"]}},"required":["name"]}),
        ),
        ToolDefinition::new(
            "submit_task",
            "Assign a tracked task to an authorized Agent. Returns a durable task_id and delivery receipt, and tells the Agent to acknowledge and report that exact task. Reuse request_id on retries. Identical target/text defaults to one task; use a new request_id only for intentional additional work. Unknown delivery never permits a fresh copy. Pass result_schema to require a JSON result of that shape. Await it with wait_any (several tasks) or wait_for_task.",
            json!({"type":"object","properties":{"session_id":{"type":"string"},"text":{"type":"string","minLength":1,"maxLength":1048576},"request_id":message_id_schema(),"result_schema":result_schema()},"required":["session_id","text"]}),
        ),
        ToolDefinition::new(
            "get_task",
            "Read the durable receipt for one task you assigned or received. Provide exactly one of task_id or your original request_id; request_id recovers a lost submission reply even after the target disappears. Delivery, Agent acknowledgement, and task result are separate facts. Status survives Engine restarts; terminal idle is not task completion.",
            json!({"type":"object","properties":{"task_id":message_id_schema(),"request_id":message_id_schema()}}),
        ),
        ToolDefinition::new(
            "wait_for_task",
            "Wait for the explicit completed or failed result of this exact task. Already terminal tasks return immediately. timed_out means no terminal result was observed; completed is true only for a reported successful result. Agent idle/exit/removal cannot fabricate completion.",
            json!({"type":"object","properties":{"task_id":message_id_schema(),"timeout_s":{"type":"number","minimum":0,"maximum":600,"default":600}},"required":["task_id"]}),
        ),
        ToolDefinition::new(
            "report_task",
            "Acknowledge or report the exact Diri task assigned to you. Call acknowledged before starting; report completed only after verifying the requested outcome and include result evidence. Use blocked for a blocker and failed for terminal failure. Only the assigned Agent can report; terminal results are immutable and identical retries are safe.",
            json!({"type":"object","properties":{"task_id":message_id_schema(),"status":{"type":"string","enum":["acknowledged","blocked","completed","failed"]},"result":{"type":"string","maxLength":16384}},"required":["task_id","status"]}),
        ),
        ToolDefinition::new(
            "submit_tasks",
            "Assign several tracked tasks in one call (at most 16). Each entry behaves exactly like submit_task, including request_id deduplication; one failure does not stop the others. Returns one result per entry, in order, and the task_ids to pass to wait_any.",
            json!({"type":"object","properties":{"tasks":{"type":"array","items":{"type":"object","properties":{"session_id":{"type":"string","minLength":1},"text":{"type":"string","minLength":1,"maxLength":1048576},"request_id":message_id_schema(),"result_schema":result_schema()},"required":["session_id","text"],"additionalProperties":false}}},"required":["tasks"]}),
        ),
        ToolDefinition::new(
            "answer_task",
            "Answer a task you submitted, typically after it reported blocked with a question. The answer is recorded on the task, delivered to the assigned Agent, and a blocked task returns to acknowledged. Only the submitting session may answer.",
            json!({"type":"object","properties":{"task_id":message_id_schema(),"text":{"type":"string","minLength":1,"maxLength":65536}},"required":["task_id","text"]}),
        ),
        ToolDefinition::new(
            "cancel_task",
            "Withdraw a task you submitted. It becomes terminal (cancelled) immediately and the assigned Agent is told to stop. Only the submitting session may cancel; finished tasks cannot be cancelled.",
            json!({"type":"object","properties":{"task_id":message_id_schema(),"reason":{"type":"string","maxLength":16384}},"required":["task_id"]}),
        ),
        ToolDefinition::new(
            "list_tasks",
            "List tasks you submitted (role sent), were assigned (role assigned), or both, newest first. Open tasks only unless include_terminal is true.",
            json!({"type":"object","properties":{"role":{"type":"string","enum":["sent","assigned","all"]},"include_terminal":{"type":"boolean"},"limit":{"type":"number","minimum":1,"maximum":200}}}),
        ),
        ToolDefinition::new(
            "wait_any",
            "Wait until at least one of the given tasks or sessions needs attention, then return every one that does (ready) and the rest (pending). Tasks are ready when completed, failed, cancelled, or blocked. Sessions are ready per until: settled (default: turn done, needs input, or exited), done, needs_me, or exited. Pass since_ms from the spawn/send/submit result so a session that was already idle before your message does not count as done. Handle the ready items, then call again with only the pending ids. Returns immediately if something is already ready.",
            json!({
                "type": "object",
                "properties": {
                    "task_ids": {"type": "array", "items": {"type": "string", "minLength": 1}},
                    "session_ids": {"type": "array", "items": {"type": "string", "minLength": 1}},
                    "until": {"type": "string", "enum": ["settled", "done", "needs_me", "exited"]},
                    "since_ms": since_ms_schema(),
                    "timeout_s": {"type": "number", "default": 600, "minimum": 0, "maximum": 600}
                }
            }),
        ),
        ToolDefinition::new(
            "spawn_agent",
            "Open a new Diri session running an agent or shell, locally or on a configured remote host. Use this whenever the user asks to spawn another agent, session, or terminal. Pass split to open it as a pane beside your own (or beside_session_id's) instead of a separate tab: good for a dev server, test watcher, or log tail you want in view, or a child agent the user should watch work; leave it out for background work. Identical arguments are deduplicated for this caller; reuse operation_id on retries and supply a new operation_id only for an intentional additional session. Inspect spawn_receipt: unknown/failed must never be retried under a new identity blindly.",
            json!({
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": kind_enum},
                    "cwd": {"type": "string"},
                    "host": {"type": "string"},
                    "worktree": {"type": "boolean"},
                    "branch": {"type": "string"},
                    "base": {"type": "string", "description": "Starting ref for a new worktree, e.g. main. Omitted preserves HEAD behavior."},
                    "prompt": {"type": "string"},
                    "name": {"type": "string"},
                    "operation_id": message_id_schema(),
                    "task": {"type": "boolean", "description": "Deliver prompt as a tracked Diri task instead of an untracked initial prompt. The result then includes task.task_id for wait_any or wait_for_task."},
                    "result_schema": result_schema(),
                    "split": split_schema(),
                    "beside_session_id": beside_schema()
                },
                "required": ["kind", "cwd"]
            }),
        ),
        ToolDefinition::new(
            "schedule_agent",
            "Schedule an agent run owned by Diri, not by this session: it survives this session closing and Diri restarting. Use it whenever the user wants something done later, at a time, or repeatedly, instead of your own cron/loop tools. How runs work: at each due time Diri opens a NEW top-level session of `kind` in `cwd` and sends `prompt`. That session cannot see this conversation, so write a self-contained prompt: the goal, the repo and relevant context, what to produce, and where the result goes (open a PR, write a file, post a summary). For code changes pass worktree:true and base \"origin/main\" so each run starts clean. Give a short `name`. When: exactly one of cron (five fields, the Mac's local time, e.g. \"0 9 * * 1-5\" weekdays 09:00), in_minutes (relative one-off), or at_ms (epoch ms). Missed runs: if the Mac was asleep or Diri was closed, the newest missed run fires once when it is back, within catch_up_hours (default 12); older ones are recorded as missed. wake_mac:true (use when the user wants it to run even if the Mac is asleep): Diri wakes the Mac 2 minutes early (lid must be open), keeps it awake while the agent works, then puts it back to sleep if nobody used it. It needs the one-time \"Allow diri to wake the Mac\" approval in Settings > Schedules; if list_schedules reports wakeHelperError, tell the user to turn it on. After creating, confirm in words from the returned nextDue, e.g. \"weekdays at 09:00, next run Monday; it will wake the Mac\".",
            json!({
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": kind_enum},
                    "cwd": {"type": "string"},
                    "prompt": {"type": "string", "minLength": 1, "maxLength": 1048576},
                    "name": {"type": "string", "maxLength": 200},
                    "cron": {"type": "string"},
                    "at_ms": {"type": "number"},
                    "in_minutes": {"type": "number", "minimum": 0},
                    "worktree": {"type": "boolean", "description": "Start each run in a fresh worktree."},
                    "branch": {"type": "string"},
                    "base": {"type": "string", "description": "Starting ref for each run's worktree, e.g. origin/main."},
                    "catch_up_hours": {"type": "number", "minimum": 0, "maximum": 168, "description": "A run missed by up to this long still fires late; 0 never catches up. Default 12."},
                    "keep_awake": {"type": "boolean", "description": "Keep an awake Mac from idle-sleeping shortly before each run and while it works. Cannot wake a sleeping Mac."},
                    "wake_mac": {"type": "boolean", "description": "Wake a sleeping Mac (lid open) 2 minutes before each run, keep it awake while the run works, then let it sleep again. Needs the one-time wake approval in Settings > Schedules; list_schedules reports if it is missing."}
                },
                "required": ["kind", "cwd", "prompt"]
            }),
        ),
        ToolDefinition::new(
            "list_schedules",
            "List every Diri schedule with its next due time and recent runs (onTime, late with lateReason asleep/notRunning, missed, failed, manual) and the session each run opened.",
            json!({"type":"object","properties":{}}),
        ),
        ToolDefinition::new(
            "delete_schedule",
            "Delete a Diri schedule. Sessions its earlier runs opened are left alone.",
            json!({"type":"object","properties":{"schedule_id":{"type":"string","minLength":1}},"required":["schedule_id"]}),
        ),
        ToolDefinition::new(
            "spawn_agents",
            "Fan out: open several sessions in one call (at most 8), concurrently. Each entry takes the same fields as spawn_agent (kind, cwd, worktree, branch, base, host, prompt, name, operation_id, task, result_schema, split, beside_session_id) and is deduplicated the same way. Use worktree:true per entry for parallel edits. Returns one result per entry in order, plus the session_ids and task_ids to pass to wait_any.",
            json!({"type":"object","properties":{"agents":{"type":"array","items":spawn_entry_schema()}},"required":["agents"]}),
        ),
        ToolDefinition::new(
            "fork_agent",
            "Fork an authorized session's conversation into a new child of yours, keeping its context, for example to try an alternative approach. Optionally send the fork a prompt (as a tracked task with task:true). Supported for agents whose conversations can be forked (Claude Code, Codex).",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "prompt": {"type": "string"},
                    "task": {"type": "boolean"},
                    "result_schema": result_schema()
                },
                "required": ["session_id"]
            }),
        ),
        ToolDefinition::new(
            "manage_agent",
            "Park or revive an authorized session without losing it: hibernate freezes its whole process tree (no CPU) while keeping the conversation and terminal, wake resumes it, and resume restarts an exited Agent in its saved conversation. Same authorization as release_agent.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "action": {"type": "string", "enum": ["hibernate", "wake", "resume"]}
                },
                "required": ["session_id", "action"]
            }),
        ),
        ToolDefinition::new(
            "list_agents",
            "List every agent session with its id, kind, title, status, parent, host, and working directory.",
            json!({"type": "object", "properties": {}}),
        ),
        ToolDefinition::new(
            "get_status",
            "Read the current status, title, and working directory of one session.",
            session_id_schema(),
        ),
        ToolDefinition::new(
            "send_prompt",
            "Type into an authorized session and optionally press Enter. Delegated agents may message their parent or direct children; root agents may coordinate their project and message direct children on any host. Cross-lineage messages are attributed to their sender. Identical messages from the same sender to the same target are delivered at most once, including across retries and restarts. Reuse message_id on retries; use a new message_id only to intentionally repeat identical text. A receipt acknowledges input delivery, not agent completion. Inspect an unknown outcome; never resend it under a new identity.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "text": {"type": "string"},
                    "message_id": message_id_schema(),
                    "submit": {"type": "boolean", "description": "Press Enter after typing; defaults to true."}
                },
                "required": ["session_id", "text"]
            }),
        ),
        ToolDefinition::new(
            "wait_for_agent",
            "Wait for a session status without model polling. Already matching states return immediately unless since_ms is given: then done/idle require a turn that finished after that time (pass since_ms from your send_prompt/spawn result). This does not acknowledge completion of a particular message. Exit or removal also ends the wait; inspect matched, removed, and session before assuming success.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "until": {"type": "string", "enum": ["done", "needs_me", "idle", "exited"]},
                    "since_ms": since_ms_schema(),
                    "timeout_s": {"type": "number", "default": 600, "minimum": 0, "maximum": 600}
                },
                "required": ["session_id"]
            }),
        ),
        ToolDefinition::new(
            "read_output",
            "Read what a session produced. last_message (best for results) returns the Agent's final answer from its transcript; transcript returns the last N turns; since returns only terminal lines added after cursor (pass back the returned cursor next time); screen/tail return the rendered screen. Transcript modes fall back to the screen tail when a session has no readable transcript (remote, shells, other agents).",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "mode": {"type": "string", "enum": ["screen", "tail", "last_message", "transcript", "since"]},
                    "lines": {"type": "number", "default": 50},
                    "turns": {"type": "number", "default": 6, "minimum": 1, "maximum": 100},
                    "cursor": {"type": "number", "minimum": 0}
                },
                "required": ["session_id"]
            }),
        ),
        ToolDefinition::new(
            "get_artifacts",
            "Return PRs, issues, preview URLs, and listening ports discovered for a session.",
            session_id_schema(),
        ),
        ToolDefinition::new(
            "get_diff",
            "Summarize a session's code changes: changed files with +/- counts against its base (default branch merge-base, or HEAD for uncommitted work only), whether it is committed, and overlaps: files also changed by its live sibling sessions, which would conflict on integration. Set patch:true to include the unified diff (bounded by max_patch_bytes).",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "base": {"type": "string", "enum": ["default_branch", "head"]},
                    "patch": {"type": "boolean"},
                    "max_patch_bytes": {"type": "number", "default": 32768, "minimum": 0, "maximum": 1048576},
                    "overlaps": {"type": "boolean", "default": true}
                },
                "required": ["session_id"]
            }),
        ),
        ToolDefinition::new(
            "integrate",
            "Bring an authorized child session's committed branch into your own checkout (merge, squash, or cherry_pick). Both checkouts must have no uncommitted tracked changes. Conflicts abort cleanly, change nothing, and are returned as paths. Local sessions in the same project only.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "strategy": {"type": "string", "enum": ["merge", "squash", "cherry_pick"]},
                    "message": {"type": "string", "maxLength": 4096}
                },
                "required": ["session_id"]
            }),
        ),
        ToolDefinition::new(
            "create_worktree",
            "Create a git worktree in the calling session's project so parallel work does not collide in one checkout.",
            json!({
                "type": "object",
                "properties": {
                    "repo": {"type": "string"},
                    "branch": {"type": "string"},
                    "base": {"type": "string"}
                },
                "required": ["repo"]
            }),
        ),
        ToolDefinition::new(
            "list_worktrees",
            "List a repository's worktrees with their paths and branches.",
            json!({"type": "object", "properties": {"repo": {"type": "string"}}, "required": ["repo"]}),
        ),
        ToolDefinition::new(
            "remove_worktree",
            "Remove a git worktree from the calling session's project.",
            json!({
                "type": "object",
                "properties": {
                    "repo": {"type": "string"},
                    "path": {"type": "string"},
                    "force": {"type": "boolean"}
                },
                "required": ["repo", "path"]
            }),
        ),
        ToolDefinition::new(
            "release_agent",
            "Terminate an authorized agent session. Delegated agents may release direct children; root agents may release sessions in their project. The caller and its ancestors are protected.",
            session_id_schema(),
        ),
        ToolDefinition::new(
            "test_run",
            "Run a known web flow across real browser engines and return pass/fail evidence. Use browser instead for open-ended exploration.",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "engines": {"type": "array", "items": {"type": "string", "enum": ["chromium", "webkit", "firefox"]}},
                    "steps": {"type": "array", "items": {"type": "object"}},
                    "observe": {"type": "string", "enum": ["a11y", "screenshot"]},
                    "baseline": {"type": "string"},
                    "profile": {"type": "string"},
                    "auth": {"type": "object"}
                },
                "required": ["url", "steps"]
            }),
        ),
        ToolDefinition::new(
            "browser",
            "Drive a real browser isolated to this Diri session. Open a URL, inspect snapshot refs, act on those refs, and request a new snapshot after page changes.",
            browser_schema(),
        ),
        ToolDefinition::new(
            "computer",
            COMPUTER_DESCRIPTION,
            json!({
                "type": "object",
                "properties": {
                    "tool": {"type": "string", "minLength": 1, "description": "A computer tool, e.g. launch_app, list_windows, get_window_state, click, type_text, press_key, hotkey, scroll, run_steps; or \"tools\" for the list."},
                    "args": {"type": "object", "description": "That tool's arguments. Ask {\"tool\":\"tools\",\"args\":{\"name\":\"<tool>\"}} for one tool's full arguments."}
                },
                "required": ["tool"]
            }),
        ),
        ToolDefinition::new(
            "open_api_request",
            "Open an HTTP request in the API tab of YOUR session's right panel in Diri, prefilled, so the person can inspect, edit and send it. Use it whenever you build, run or debug an HTTP API: after starting a dev server, open the endpoint you just added or changed (e.g. GET http://localhost:3000/api/health) instead of only pasting a curl command. Put {{name}} placeholders in url/headers/body and pass their values in variables (marking tokens in secrets so they are masked); they land in the project's environment (environment names it, default the active one or \"Local\"). json sets a JSON body and its Content-Type; body sends text as written. auto_send:true sends a GET immediately and shows the response; any other method is never sent until the person presses Send. The call returns once the tab is open, without the response.",
            json!({
                "type": "object",
                "properties": {
                    "method": {"type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"], "default": "GET"},
                    "url": {"type": "string", "minLength": 1, "maxLength": 8192, "description": "http(s) URL; may use {{variables}}. No scheme means http:// for localhost."},
                    "headers": {"type": "object", "description": "Header name to value, e.g. {\"Authorization\": \"Bearer {{token}}\"}."},
                    "body": {"type": "string", "maxLength": 1048576, "description": "Raw body text."},
                    "json": {"description": "A JSON value sent as the body, indented, with Content-Type: application/json."},
                    "name": {"type": "string", "maxLength": 200, "description": "Tab and saved-request title."},
                    "variables": {"type": "object", "description": "Variable name to value for {{name}} placeholders."},
                    "secrets": {"type": "array", "items": {"type": "string"}, "description": "Names in variables to mask as secrets."},
                    "environment": {"type": "string", "maxLength": 200, "description": "Environment the variables go into (created if missing)."},
                    "auto_send": {"type": "boolean", "default": false, "description": "Send right away. Honoured for GET only."}
                },
                "required": ["url"]
            }),
        ),
        ToolDefinition::new(
            "quick_open_include",
            "Read or change ~/.diri-include, the gitignore-style extra folders Quick Open (Cmd+P) indexes even when hidden or skipped. action get returns the path, text, and patterns; add appends unique patterns (e.g. **/.worktrees/); set replaces the whole file with text (empty clears it).",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["get", "add", "set"]},
                    "patterns": {
                        "type": "array",
                        "items": {"type": "string", "minLength": 1, "maxLength": 256}
                    },
                    "text": {"type": "string", "maxLength": 65536}
                },
                "required": ["action"]
            }),
        ),
        ToolDefinition::new(
            "whoami",
            "Describe this session's identity, parent, ancestors, children, worktree, and cross-session write policy. origin_note, when present, is the note you were started from: read it first with read_note {\"note\":\"origin\"}.",
            json!({"type": "object", "properties": {}}),
        ),
        ToolDefinition::new(
            "get_layout",
            "Show the split-pane layout of the tab(s) that show a session (default yours): which sessions share the tab, side_by_side or stacked splits, the focused and zoomed session. Read it before manage_layout, or to check what the user sees beside you.",
            json!({"type": "object", "properties": {"session_id": {"type": "string", "minLength": 1}}}),
        ),
        ToolDefinition::new(
            "manage_layout",
            "Arrange split panes by session id; panes are views, so nothing is started or stopped. split: move session_id's pane beside beside_session_id's (default yours), e.g. bring a child agent or a running server into view next to you. unsplit: give session_id its own tab again. close: take its pane off the tab (the session keeps running and stays in the sidebar). focus, zoom/unzoom (fill the tab with one pane), equalize (even out the tab's splits). session_id defaults to you. Split only when the user benefits from seeing both at once; do not rearrange panes the user did not ask about.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["split", "unsplit", "close", "focus", "zoom", "unzoom", "equalize"]},
                    "session_id": {"type": "string", "minLength": 1, "description": "The session whose pane to act on; defaults to the calling session."},
                    "beside_session_id": {"type": "string", "minLength": 1, "description": "split only: the session to dock beside; defaults to the calling session."},
                    "split": {"type": "string", "enum": ["right", "down", "left", "up"], "description": "split only: which side of beside_session_id's pane; default right."}
                },
                "required": ["action"]
            }),
        ),
        ToolDefinition::new(
            "list_notes",
            "Find the person's Diri Notes (briefs, plans, to-do lists). Notes are kept by Diri, not in the project folder, so use this rather than searching files. Defaults to notes for your project; project:\"all\" lists every note, or pass a project folder. mentions:\"me\" returns notes that @-mention you or an ancestor that started you (mentioned_via says which). query filters title and body. links_to (a note's id or title) returns the notes that link to that note. session_id is the note's sidebar Session.",
            json!({"type":"object","properties":{"project":{"type":"string","minLength":1},"mentions":{"type":"string","minLength":1},"query":{"type":"string","minLength":1},"links_to":{"type":"string","minLength":1},"include_archived":{"type":"boolean"},"limit":{"type":"integer","minimum":1,"maximum":200}}}),
        ),
        ToolDefinition::new(
            "read_note",
            "Read one Diri note as Markdown. If you were started from a note, read note \"origin\" first: it is your brief. markdown is the note's canonical text (title first as a # line, tidy tables, normalised list markers): exactly what edit_note matches, so copy old_string from it. path is the note's .md file, which you may also edit with your own file tools; Diri keeps the change, a version before it, and the note's identity. Returns the text, its to-dos (block index, checked, linked sessions and their live status), its @-mentions resolved to sessions or notes, and its backlinks: other notes that link to this one, with the words around each link. Linked notes and backlinks are often the background a note was written against; read the relevant ones, and use note_links for the full picture. Mentioned sessions may be working on related things: inspect them with read_output/get_diff or wait on them with wait_for_agent. note is an id, a title, part of a title, a note Session id, or \"origin\": the note you were started from (whoami shows it as origin_note).",
            json!({"type":"object","properties":{"note":{"type":"string","minLength":1}},"required":["note"]}),
        ),
        ToolDefinition::new(
            "note_links",
            "How one Diri note connects to the others. links: the notes it links to. backlinks: every note linking to it, with the words around each link (backlinks_total counts them). unlinked_mentions: notes that write its title without linking it (unlinked:false skips them). depth (1-3) adds graph: the notes within that many links, either direction, and the links between them. Check backlinks before changing a note others depend on, and to find the plans, briefs and findings a note belongs to. note is an id, a title, part of a title, a note Session id, or \"origin\".",
            json!({"type":"object","properties":{"note":{"type":"string","minLength":1},"unlinked":{"type":"boolean"},"depth":{"type":"integer","minimum":1,"maximum":3}},"required":["note"]}),
        ),
        ToolDefinition::new(
            "write_note",
            "Add to a Diri note without rewriting it; never deletes the person's text. entry: one short line when something matters (a decision, a finding, a blocker, a result, a link), filed under your to-do (or the one you name) or in the note's Updates; keep entries sparing, no progress chatter. checked: tick a to-do, e.g. your own sub-tasks as you finish them (the to-do you were started from is the person's to tick). link_session: put a session's chip on a to-do. append: longer Markdown at the end, rarely needed. Pick the to-do by todo (its text or part of it) or todo_index (from read_note). Write [[Note title]] to link another note; it becomes a link by the note's id, so it survives renames. Delegated agents may write only to the note they were started from or notes that mention them.",
            json!({"type":"object","properties":{"note":{"type":"string","minLength":1},"entry":{"type":"string","minLength":1},"append":{"type":"string","minLength":1,"maxLength":65536},"todo":{"type":"string","minLength":1},"todo_index":{"type":"integer","minimum":0},"checked":{"type":"boolean"},"link_session":{"type":"string","minLength":1}},"required":["note"]}),
        ),
        ToolDefinition::new(
            "edit_note",
            "Change a Diri note in place, like editing a Markdown file: old_string is exact text from read_note's markdown (title line included), new_string replaces it; an empty new_string deletes it. old_string must appear once unless replace_all. Use it when the person asks for a change, or to keep your own entries current (tick a table row, change \"Fix\" to \"Done\"); otherwise prefer write_note. Every version is kept, so the person can restore one. Returns the changed lines and the new version. Diri tidies the text after each change (tables are re-aligned); a table row still matches if only its spacing differs, and the reply says so. For anything else, copy the next old_string from the changed lines or a fresh read_note. [[Note title]] in new_string links that note.",
            json!({"type":"object","properties":{"note":{"type":"string","minLength":1},"old_string":{"type":"string","minLength":1},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["note","old_string","new_string"]}),
        ),
        ToolDefinition::new(
            "replace_section",
            "Replace everything under one heading of a Diri note, up to the next heading of the same or a higher level; the heading itself stays. heading is its text, optionally with its # marks (\"## Status\") when two headings share a name. markdown is the new content. Same rules as edit_note: change the person's writing only when asked.",
            json!({"type":"object","properties":{"note":{"type":"string","minLength":1},"heading":{"type":"string","minLength":1},"markdown":{"type":"string","maxLength":65536}},"required":["note","heading","markdown"]}),
        ),
        ToolDefinition::new(
            "create_note",
            "Write a new Diri note for the person, e.g. an explanation (\"how sign-in works\") or a write-up. markdown is rich Markdown: headings, lists, to-dos, links, quotes, code. Link the notes it builds on or relates to by writing [[Note title]]: each becomes a link to that note (linked_notes lists them; unlinked_titles matched no single note), and the linked note shows this one among its backlinks. It appears under you in the sidebar, in your project (or project, a folder); open:true shows it to the person right away. Use this for anything longer than a write_note entry.",
            json!({"type":"object","properties":{"title":{"type":"string","minLength":1,"maxLength":200},"markdown":{"type":"string","maxLength":65536},"project":{"type":"string","minLength":1},"open":{"type":"boolean"}},"required":["title"]}),
        ),
        ToolDefinition::new(
            "start_from_note",
            "Start an agent on a Diri note or one of its to-dos. The note becomes the agent's parent in the sidebar, the agent gets the note as its brief (plus the sessions it mentions), and its chip is added to the to-do. kind defaults to your own kind. separate_copy gives it its own copy of the project folder (projects under git only) so its changes stay apart until merged. prompt adds your own instructions. task:true tracks it like submit_task.",
            json!({"type":"object","properties":{"note":{"type":"string","minLength":1},"todo":{"type":"string","minLength":1},"todo_index":{"type":"integer","minimum":0},"kind":{"type":"string","minLength":1},"separate_copy":{"type":"boolean"},"prompt":{"type":"string","minLength":1,"maxLength":65536},"task":{"type":"boolean"},"result_schema":result_schema(),"operation_id":message_id_schema()},"required":["note"]}),
        ),
        ToolDefinition::new(
            "note_history",
            "Earlier versions of a Diri note: when, by whom, and what changed, newest first. Pass version to read that version's text. Read-only: only the person restores a version.",
            json!({"type":"object","properties":{"note":{"type":"string","minLength":1},"version":{"type":"integer","minimum":0}},"required":["note"]}),
        ),
        ToolDefinition::new(
            "list_children",
            "List the sessions spawned by this one, optionally including the whole descendant tree.",
            json!({
                "type": "object",
                "properties": {
                    "recursive": {"type": "boolean"},
                    "include_exited": {"type": "boolean", "default": true}
                }
            }),
        ),
        ToolDefinition::new(
            "wait_for_children",
            "Wait until ALL selected child sessions settle, finish, or exit (use wait_any to handle each as soon as it is ready). Already matching states return immediately. Removed children are reported separately and cannot settle other working children. Omit session_ids for all direct children; an explicit empty array selects none.",
            json!({
                "type": "object",
                "properties": {
                    "session_ids": {"type": "array", "items": {"type": "string"}},
                    "until": {"type": "string", "enum": ["settled", "done", "exited"]},
                    "since_ms": since_ms_schema(),
                    "timeout_s": {"type": "number", "default": 600, "minimum": 0, "maximum": 600}
                }
            }),
        ),
        ToolDefinition::new(
            "summarize_children",
            "Collect compact screen tails, each child's last agent message (when its transcript is readable), status, and artifacts for this session's children without interpreting their output.",
            json!({
                "type": "object",
                "properties": {
                    "session_ids": {"type": "array", "items": {"type": "string"}},
                    "rows": {"type": "number", "default": 14}
                }
            }),
        ),
        ToolDefinition::new(
            "report_to_parent",
            "If your parent is a note, the report is added to the note: finish with status done and a one-paragraph result in summary (what you did, what changed, what is left), in plain words. Otherwise: deliver a structured update, result, blocker, or question to the session that delegated this work at most once. If your parent assigned you an open Diri task, the report is recorded on that task instead (update→progress, blocked→blocked, done→completed, failed→failed) and reaches the parent through wait_any/wait_for_task; set deliver:true to also type it into the parent's terminal. Identical reports are deduplicated. Reuse message_id on retries; choose a new one only for an intentional repeat. Inspect unknown outcomes without resending.",
            json!({
                "type": "object",
                "properties": {
                    "summary": {"type": "string"},
                    "message_id": message_id_schema(),
                    "status": {"type": "string", "enum": ["update", "done", "blocked", "failed"]},
                    "details": {"type": "string"},
                    "blockers": string_array(),
                    "questions": string_array(),
                    "next_steps": string_array(),
                    "changed_paths": string_array(),
                    "artifacts": string_array(),
                    "proof": string_array(),
                    "submit": {"type": "boolean"},
                    "deliver": {"type": "boolean"}
                },
                "required": ["summary"]
            }),
        ),
    ];

    if std::env::var_os("DIRIJOR_TEST_RUN_AVAILABLE").is_none() {
        tools.retain(|tool| tool.name != "test_run");
    }
    for tool in &mut tools {
        tool.input_schema["additionalProperties"] = json!(false);
        for key in [
            "kind",
            "cwd",
            "host",
            "branch",
            "base",
            "name",
            "repo",
            "path",
            "session_id",
        ] {
            if let Some(field) = tool.input_schema["properties"].get_mut(key) {
                field["minLength"] = json!(1);
            }
        }
        if let Some(field) = tool.input_schema["properties"].get_mut("session_ids") {
            field["items"]["minLength"] = json!(1);
        }
        if let Some(field) = tool.input_schema["properties"].get_mut("patterns") {
            field["items"]["minLength"] = json!(1);
        }
    }
    tools
}

/// Diri's `speak` tool, listed only while "Talk with your agent" is on.
pub fn speak_tool() -> ToolDefinition {
    ToolDefinition::new(
        diri_proto::voice::SPEAK_TOOL,
        diri_proto::voice::SPEAK_TOOL_DESCRIPTION,
        json!({"type":"object","properties":{"text":{"type":"string","minLength":1,"maxLength":diri_proto::voice::MAX_SPOKEN_CHARS}},"required":["text"]}),
    )
}

/// Validate the advertised argument contract before discovery, authorization,
/// or any Engine call. Wrong optional types must never silently select defaults
/// (especially submit, force, host, or the selection of child sessions).
pub(crate) fn validate_arguments(tool: &str, arguments: &Value) -> Result<(), String> {
    let mut definition = tool_definitions_for(&[])
        .into_iter()
        .chain(std::iter::once(speak_tool()))
        .find(|definition| definition.name == tool)
        .ok_or_else(|| format!("unknown or unavailable tool: {tool}"))?;
    // Kind aliases/custom commands are resolved against the live catalog by
    // spawn; the static validator must not use an empty discovery enum.
    if tool == "spawn_agent" || tool == "schedule_agent" {
        definition.input_schema["properties"]["kind"]
            .as_object_mut()
            .unwrap()
            .remove("enum");
    }
    validate_value(arguments, &definition.input_schema, "arguments")
}

pub(crate) fn validate_value(value: &Value, schema: &Value, path: &str) -> Result<(), String> {
    let expected = schema["type"].as_str().unwrap_or("any");
    let valid = match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "number" => value.as_f64().is_some_and(f64::is_finite),
        "integer" => value.is_i64() || value.is_u64(),
        "null" => value.is_null(),
        _ => true,
    };
    if !valid {
        return Err(format!("{path} must be {expected}"));
    }
    if let Some(allowed) = schema["enum"].as_array()
        && !allowed.contains(value)
    {
        return Err(format!("{path} is not a supported value"));
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema["required"].as_array() {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    return Err(format!("missing required argument: {key}"));
                }
            }
        }
        for (key, field) in object {
            if let Some(field_schema) = schema["properties"].get(key) {
                validate_value(field, field_schema, &format!("{path}.{key}"))?;
            } else if schema["additionalProperties"] == false {
                return Err(format!("unsupported argument: {key}"));
            }
        }
    }
    if let Some(entries) = value.as_array() {
        for (index, entry) in entries.iter().enumerate() {
            validate_value(entry, &schema["items"], &format!("{path}[{index}]"))?;
        }
    }
    if let Some(text) = value.as_str() {
        let length = text.chars().count() as u64;
        if schema["minLength"].as_u64().is_some_and(|min| length < min)
            || schema["maxLength"].as_u64().is_some_and(|max| length > max)
        {
            return Err(format!("{path} has an invalid length"));
        }
    }
    if let Some(number) = value.as_f64()
        && (schema["minimum"].as_f64().is_some_and(|min| number < min)
            || schema["maximum"].as_f64().is_some_and(|max| number > max))
    {
        return Err(format!("{path} is outside the supported range"));
    }
    Ok(())
}

fn session_id_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"session_id": {"type": "string"}},
        "required": ["session_id"]
    })
}

fn string_array() -> Value {
    json!({"type": "array", "items": {"type": "string"}})
}

const COMPUTER_DESCRIPTION: &str = "Use this Mac's apps the way a person would, in the background: \
any native app (Simulator, TextEdit, Xcode, Finder, the app you are building) and browser pages. \
The person's cursor, keyboard and focus stay theirs; your actions show as your own labelled cursor. \
Pass tool and args. Loop: (1) launch_app {bundle_id or name, creates_new_application_instance:true} \
returns your own pid and windows (or list_windows {pid}); (2) get_window_state {pid, window_id} returns \
accessibility rows [N] plus a screenshot; (3) act with element_token \"<snapshot_id>:N\" (click, \
type_text {text}, set_value, press_key {key}, hotkey {keys}, scroll) or with pixel x,y from that \
screenshot; (4) re-read with get_window_state {since:\"latest\"} and check the change before going on. \
run_steps runs several actions in one call. Act only on apps you launched or the person pointed you \
to: an app another agent is driving returns computer_app_busy. computer_unavailable or \
computer_permissions means only the person can finish setup (Diri Settings › Agents › Computer use): \
tell them, and don't try to install or grant anything yourself. tool \"tools\" lists every tool.";

fn browser_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {"type": "string", "enum": ["open", "snapshot", "click", "fill", "type", "press", "hover", "select", "check", "scroll", "get", "wait", "screenshot", "console", "back", "close", "list"]},
            "url": {"type": "string"},
            "ref": {"type": "string"},
            "selector": {"type": "string"},
            "text": {"type": "string"},
            "key": {"type": "string"},
            "value": {"type": "string"},
            "what": {"type": "string", "enum": ["url", "title", "text", "html", "value", "count"]},
            "ms": {"type": "number"},
            "state": {"type": "string"},
            "direction": {"type": "string", "enum": ["up", "down", "left", "right"]},
            "amount": {"type": "number"},
            "button": {"type": "string", "enum": ["left", "right", "middle"]},
            "double": {"type": "boolean"},
            "full": {"type": "boolean"},
            "annotate": {"type": "boolean"},
            "engine": {"type": "string", "enum": ["chromium", "webkit", "firefox"]},
            "profile": {"type": "string"}
        },
        "required": ["action"]
    })
}

fn result_schema() -> Value {
    json!({"type":"object",
        "description":"Optional JSON Schema (object) for the completed result. The Agent must then report result as JSON matching it (type, required, properties, enum, items are checked)."})
}

fn since_ms_schema() -> Value {
    json!({"type":"number", "minimum":0,
        "description":"Unix time in milliseconds, as returned in since_ms by spawn_agent, send_prompt, and submit_task. A session counts as done only after a turn that finished later."})
}

fn spawn_entry_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": {"type": "string", "minLength": 1},
            "cwd": {"type": "string", "minLength": 1},
            "host": {"type": "string", "minLength": 1},
            "worktree": {"type": "boolean"},
            "branch": {"type": "string", "minLength": 1},
            "base": {"type": "string", "minLength": 1},
            "prompt": {"type": "string"},
            "name": {"type": "string", "minLength": 1},
            "operation_id": message_id_schema(),
            "task": {"type": "boolean"},
            "result_schema": result_schema(),
            "split": split_schema(),
            "beside_session_id": beside_schema()
        },
        "required": ["kind", "cwd"],
        "additionalProperties": false
    })
}

fn split_schema() -> Value {
    json!({"type": "string", "enum": ["right", "down", "left", "up"],
        "description": "Open as a split pane on this side of beside_session_id's pane (default your own) instead of a new tab. A tab holds at most 8 panes."})
}

fn beside_schema() -> Value {
    json!({"type": "string", "minLength": 1,
        "description": "Session whose pane to split; defaults to the calling session. Implies split right when split is omitted."})
}

fn message_id_schema() -> Value {
    json!({"type":"string", "minLength":1, "maxLength":200,
        "description":"Stable identity for this logical message. Reuse on retries. If omitted, identical content is deduplicated for this sender/target. Use a new value only for an intentional repeat."})
}

#[cfg(test)]
mod tests {

    #[test]
    fn speak_takes_a_short_text() {
        assert!(validate_arguments("speak", &json!({"text": "I fixed it."})).is_ok());
        assert!(validate_arguments("speak", &json!({})).is_err());
        assert!(validate_arguments("speak", &json!({"text": ""})).is_err());
    }

    use super::*;

    #[test]
    fn schemas_are_valid_and_names_are_unique() {
        let mut names = Vec::new();
        for tool in tool_definitions_for(&["codex".into(), "shell".into()]) {
            assert!(!tool.name.is_empty());
            assert!(tool.description.len() > 20, "{}", tool.name);
            assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
            if let Some(required) = tool.input_schema.get("required").and_then(Value::as_array) {
                let properties = tool.input_schema["properties"].as_object().unwrap();
                for key in required {
                    assert!(
                        properties.contains_key(key.as_str().unwrap()),
                        "{}",
                        tool.name
                    );
                }
            }
            names.push(tool.name);
        }
        let total = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), total);
        for name in [
            "quick_open_include",
            "wait_any",
            "spawn_agents",
            "submit_tasks",
            "answer_task",
            "cancel_task",
            "list_tasks",
            "get_diff",
            "integrate",
            "fork_agent",
            "manage_agent",
            "get_layout",
            "manage_layout",
        ] {
            assert!(names.contains(&name.to_owned()), "{name}");
        }
    }

    #[test]
    fn quick_open_include_requires_an_action_and_pattern_strings() {
        assert!(validate_arguments("quick_open_include", &json!({})).is_err());
        assert!(
            validate_arguments(
                "quick_open_include",
                &json!({"action": "add", "patterns": ["**/.worktrees/"]})
            )
            .is_ok()
        );
        assert!(
            validate_arguments(
                "quick_open_include",
                &json!({"action": "add", "patterns": [7]})
            )
            .is_err()
        );
        assert!(
            validate_arguments("quick_open_include", &json!({"action": "set", "text": ""})).is_ok()
        );
    }

    #[test]
    fn batch_entries_reject_unknown_fields_and_bad_types() {
        assert!(
            validate_arguments(
                "spawn_agents",
                &json!({"agents": [{"kind": "claude", "cwd": "/tmp", "worktree": true}]})
            )
            .is_ok()
        );
        assert!(
            validate_arguments(
                "spawn_agents",
                &json!({"agents": [{"kind": "claude", "cwd": "/tmp", "bogus": 1}]})
            )
            .is_err()
        );
        assert!(
            validate_arguments(
                "submit_tasks",
                &json!({"tasks": [{"session_id": "s1", "text": ""}]})
            )
            .is_err()
        );
        assert!(validate_arguments("wait_any", &json!({"task_ids": [], "since_ms": -1})).is_err());
    }

    #[test]
    fn split_placement_is_validated_on_spawns_and_layout_calls() {
        assert!(
            validate_arguments(
                "spawn_agent",
                &json!({"kind": "shell", "cwd": "/tmp", "split": "down"})
            )
            .is_ok()
        );
        assert!(
            validate_arguments(
                "spawn_agent",
                &json!({"kind": "shell", "cwd": "/tmp", "split": "sideways"})
            )
            .is_err()
        );
        assert!(
            validate_arguments(
                "spawn_agents",
                &json!({"agents": [{"kind": "codex", "cwd": "/tmp", "split": "right", "beside_session_id": "s1"}]})
            )
            .is_ok()
        );
        assert!(validate_arguments("manage_layout", &json!({})).is_err());
        assert!(
            validate_arguments(
                "manage_layout",
                &json!({"action": "split", "session_id": "s2", "split": "up"})
            )
            .is_ok()
        );
        assert!(validate_arguments("manage_layout", &json!({"action": "shuffle"})).is_err());
        assert!(validate_arguments("get_layout", &json!({})).is_ok());
    }

    #[test]
    fn spawn_agents_come_from_the_runtime_catalog() {
        let tools = tool_definitions_for(&["opencode".into(), "shell".into()]);
        let spawn = tools
            .iter()
            .find(|tool| tool.name == "spawn_agent")
            .unwrap();
        assert_eq!(
            spawn.input_schema["properties"]["kind"]["enum"],
            json!(["opencode", "shell"])
        );
    }

    #[test]
    fn note_tools_teach_agents_to_keep_their_note_current() {
        let tools = tool_definitions_for(&[]);
        let describe = |name: &str| {
            tools
                .iter()
                .find(|tool| tool.name == name)
                .map(|tool| tool.description.to_owned())
                .unwrap_or_else(|| panic!("no tool {name}"))
        };
        assert!(describe("whoami").contains("read_note {\"note\":\"origin\"}"));
        assert!(describe("read_note").contains("read note \"origin\" first"));
        let write = describe("write_note");
        for phrase in [
            "a decision, a finding, a blocker, a result, a link",
            "no progress chatter",
            "your own sub-tasks",
            "never deletes the person's text",
        ] {
            assert!(write.contains(phrase), "write_note: {phrase}");
        }
        assert!(describe("report_to_parent").contains("one-paragraph result"));
        assert!(describe("create_note").contains("open:true"));
        assert!(describe("note_history").contains("only the person restores"));
        // Agents link related notes and read backlinks for context.
        assert!(describe("create_note").contains("[[Note title]]"));
        assert!(describe("write_note").contains("[[Note title]]"));
        assert!(describe("read_note").contains("backlinks"));
        assert!(describe("note_links").contains("Check backlinks"));
        assert!(describe("list_notes").contains("links_to"));
        // Plain language for people who are not developers.
        for name in [
            "list_notes",
            "read_note",
            "write_note",
            "create_note",
            "note_history",
            "edit_note",
            "replace_section",
            "note_links",
        ] {
            let text = describe(name).to_lowercase();
            for jargon in ["repo", "worktree", "branch", "commit"] {
                assert!(!text.contains(jargon), "{name} says {jargon}");
            }
        }
    }
}
