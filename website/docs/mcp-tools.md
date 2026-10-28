# MCP tool reference

> Every tool the dirijor MCP server exposes, with its arguments, types and defaults. Generated from the server's own catalog, so it always matches the app.

For setup, permissions and patterns, read [MCP server](/docs/mcp/) first. Descriptions below are the exact text agents see. `kind` accepts the short label of any [supported agent](/docs/agents/) installed on your machine, or `shell`.

> [!TIP]
> On your own machine, `dirijor mcp-tools` prints this catalog as JSON, with `kind` limited to the agents you have installed.

## Start and manage agents

### `spawn_agent`

Open a new Diri session running an agent or shell, locally or on a configured remote host. Use this whenever the user asks to spawn another agent, session, or terminal. Pass split to open it as a pane beside your own (or beside_session_id's) instead of a separate tab: good for a dev server, test watcher, or log tail you want in view, or a child agent the user should watch work; leave it out for background work. Identical arguments are deduplicated for this caller; reuse operation_id on retries and supply a new operation_id only for an intentional additional session. Inspect spawn_receipt: unknown/failed must never be retried under a new identity blindly.

| Argument | Type | Notes |
| --- | --- | --- |
| `cwd` | string | **Required.** |
| `kind` | agent label or `shell` ([list](/docs/agents/)) | **Required.** |
| `base` | string | Starting ref for a new worktree, e.g. main. Omitted preserves HEAD behavior. |
| `beside_session_id` | string | Session whose pane to split; defaults to the calling session. Implies split right when split is omitted. |
| `branch` | string |  |
| `host` | string |  |
| `name` | string |  |
| `operation_id` | string | Stable identity for this logical message. Reuse on retries. If omitted, identical content is deduplicated for this sender/target. Use a new value only for an intentional repeat. |
| `prompt` | string |  |
| `result_schema` | object | Optional JSON Schema (object) for the completed result. The Agent must then report result as JSON matching it (type, required, properties, enum, items are checked). |
| `split` | `right`, `down`, `left`, `up` | Open as a split pane on this side of beside_session_id's pane (default your own) instead of a new tab. A tab holds at most 8 panes. |
| `task` | boolean | Deliver prompt as a tracked Diri task instead of an untracked initial prompt. The result then includes task.task_id for wait_any or wait_for_task. |
| `worktree` | boolean |  |

### `spawn_agents`

Fan out: open several sessions in one call (at most 8), concurrently. Each entry takes the same fields as spawn_agent (kind, cwd, worktree, branch, base, host, prompt, name, operation_id, task, result_schema, split, beside_session_id) and is deduplicated the same way. Use worktree:true per entry for parallel edits. Returns one result per entry in order, plus the session_ids and task_ids to pass to wait_any.

| Argument | Type | Notes |
| --- | --- | --- |
| `agents` | array of objects | **Required.** Each item: `base`, `beside_session_id`, `branch`, `cwd`, `host`, `kind`, `name`, `operation_id`, `prompt`, `result_schema`, `split`, `task`, `worktree`. |

### `fork_agent`

Fork an authorized session's conversation into a new child of yours, keeping its context, for example to try an alternative approach. Optionally send the fork a prompt (as a tracked task with task:true). Supported for agents whose conversations can be forked (Claude Code, Codex).

| Argument | Type | Notes |
| --- | --- | --- |
| `session_id` | string | **Required.** |
| `prompt` | string |  |
| `result_schema` | object | Optional JSON Schema (object) for the completed result. The Agent must then report result as JSON matching it (type, required, properties, enum, items are checked). |
| `task` | boolean |  |

### `manage_agent`

Park or revive an authorized session without losing it: hibernate freezes its whole process tree (no CPU) while keeping the conversation and terminal, wake resumes it, and resume restarts an exited Agent in its saved conversation. Same authorization as release_agent.

| Argument | Type | Notes |
| --- | --- | --- |
| `action` | `hibernate`, `wake`, `resume` | **Required.** |
| `session_id` | string | **Required.** |

### `release_agent`

Terminate an authorized agent session. Delegated agents may release direct children; root agents may release sessions in their project and their direct children anywhere. The caller and its ancestors are protected.

| Argument | Type | Notes |
| --- | --- | --- |
| `session_id` | string | **Required.** |

### `list_agents`

List every agent session with its id, kind, title, status, parent, host, and working directory.

No arguments.

### `get_status`

Read the current status, title, and working directory of one session.

| Argument | Type | Notes |
| --- | --- | --- |
| `session_id` | string | **Required.** |


## Talk to agents and wait

### `send_prompt`

Type into an authorized session and optionally press Enter. Delegated agents may message their parent or direct children; root agents may coordinate their project and message direct children on any host. Cross-lineage messages are attributed to their sender. Identical messages from the same sender to the same target are delivered at most once, including across retries and restarts. Reuse message_id on retries; use a new message_id only to intentionally repeat identical text. A receipt acknowledges input delivery, not agent completion. Inspect an unknown outcome; never resend it under a new identity.

| Argument | Type | Notes |
| --- | --- | --- |
| `session_id` | string | **Required.** |
| `text` | string | **Required.** |
| `message_id` | string | Stable identity for this logical message. Reuse on retries. If omitted, identical content is deduplicated for this sender/target. Use a new value only for an intentional repeat. |
| `submit` | boolean | Press Enter after typing; defaults to true. |

### `wait_any`

Wait until at least one of the given tasks or sessions needs attention, then return every one that does (ready) and the rest (pending). Tasks are ready when completed, failed, cancelled, or blocked. Sessions are ready per until: settled (default: turn done, needs input, or exited), done, needs_me, or exited. Pass since_ms from the spawn/send/submit result so a session that was already idle before your message does not count as done. Handle the ready items, then call again with only the pending ids. Returns immediately if something is already ready.

| Argument | Type | Notes |
| --- | --- | --- |
| `session_ids` | string array |  |
| `since_ms` | number | Unix time in milliseconds, as returned in since_ms by spawn_agent, send_prompt, and submit_task. A session counts as done only after a turn that finished later. |
| `task_ids` | string array |  |
| `timeout_s` | number | Default `600`. |
| `until` | `settled`, `done`, `needs_me`, `exited` |  |

### `wait_for_agent`

Wait for a session status without model polling. Already matching states return immediately unless since_ms is given: then done/idle require a turn that finished after that time (pass since_ms from your send_prompt/spawn result). This does not acknowledge completion of a particular message. Exit or removal also ends the wait; inspect matched, removed, and session before assuming success.

| Argument | Type | Notes |
| --- | --- | --- |
| `session_id` | string | **Required.** |
| `since_ms` | number | Unix time in milliseconds, as returned in since_ms by spawn_agent, send_prompt, and submit_task. A session counts as done only after a turn that finished later. |
| `timeout_s` | number | Default `600`. |
| `until` | `done`, `needs_me`, `idle`, `exited` |  |

### `read_output`

Read what a session produced. last_message (best for results) returns the Agent's final answer from its transcript; transcript returns the last N turns; since returns only terminal lines added after cursor (pass back the returned cursor next time); screen/tail return the rendered screen. Transcript modes fall back to the screen tail when a session has no readable transcript (remote, shells, other agents).

| Argument | Type | Notes |
| --- | --- | --- |
| `session_id` | string | **Required.** |
| `cursor` | number |  |
| `lines` | number | Default `50`. |
| `mode` | `screen`, `tail`, `last_message`, `transcript`, `since` |  |
| `turns` | number | Default `6`. |


## Tracked tasks

### `submit_task`

Assign a tracked task to an authorized Agent. Returns a durable task_id and delivery receipt, and tells the Agent to acknowledge and report that exact task. Reuse request_id on retries. Identical target/text defaults to one task; use a new request_id only for intentional additional work. Unknown delivery never permits a fresh copy. Pass result_schema to require a JSON result of that shape. Await it with wait_any (several tasks) or wait_for_task.

| Argument | Type | Notes |
| --- | --- | --- |
| `session_id` | string | **Required.** |
| `text` | string | **Required.** |
| `request_id` | string | Stable identity for this logical message. Reuse on retries. If omitted, identical content is deduplicated for this sender/target. Use a new value only for an intentional repeat. |
| `result_schema` | object | Optional JSON Schema (object) for the completed result. The Agent must then report result as JSON matching it (type, required, properties, enum, items are checked). |

### `submit_tasks`

Assign several tracked tasks in one call (at most 16). Each entry behaves exactly like submit_task, including request_id deduplication; one failure does not stop the others. Returns one result per entry, in order, and the task_ids to pass to wait_any.

| Argument | Type | Notes |
| --- | --- | --- |
| `tasks` | array of objects | **Required.** Each item: `request_id`, `result_schema`, `session_id`, `text`. |

### `get_task`

Read the durable receipt for one task you assigned or received. Provide exactly one of task_id or your original request_id; request_id recovers a lost submission reply even after the target disappears. Delivery, Agent acknowledgement, and task result are separate facts. Status survives Engine restarts; terminal idle is not task completion.

| Argument | Type | Notes |
| --- | --- | --- |
| `request_id` | string | Stable identity for this logical message. Reuse on retries. If omitted, identical content is deduplicated for this sender/target. Use a new value only for an intentional repeat. |
| `task_id` | string | Stable identity for this logical message. Reuse on retries. If omitted, identical content is deduplicated for this sender/target. Use a new value only for an intentional repeat. |

### `wait_for_task`

Wait for the explicit completed or failed result of this exact task. Already terminal tasks return immediately. timed_out means no terminal result was observed; completed is true only for a reported successful result. Agent idle/exit/removal cannot fabricate completion.

| Argument | Type | Notes |
| --- | --- | --- |
| `task_id` | string | **Required.** Stable identity for this logical message. Reuse on retries. If omitted, identical content is deduplicated for this sender/target. Use a new value only for an intentional repeat. |
| `timeout_s` | number | Default `600`. |

### `report_task`

Acknowledge or report the exact Diri task assigned to you. Call acknowledged before starting; report completed only after verifying the requested outcome and include result evidence. Use blocked for a blocker and failed for terminal failure. Only the assigned Agent can report; terminal results are immutable and identical retries are safe.

| Argument | Type | Notes |
| --- | --- | --- |
| `status` | `acknowledged`, `blocked`, `completed`, `failed` | **Required.** |
| `task_id` | string | **Required.** Stable identity for this logical message. Reuse on retries. If omitted, identical content is deduplicated for this sender/target. Use a new value only for an intentional repeat. |
| `result` | string |  |

### `answer_task`

Answer a task you submitted, typically after it reported blocked with a question. The answer is recorded on the task, delivered to the assigned Agent, and a blocked task returns to acknowledged. Only the submitting session may answer.

| Argument | Type | Notes |
| --- | --- | --- |
| `task_id` | string | **Required.** Stable identity for this logical message. Reuse on retries. If omitted, identical content is deduplicated for this sender/target. Use a new value only for an intentional repeat. |
| `text` | string | **Required.** |

### `cancel_task`

Withdraw a task you submitted. It becomes terminal (cancelled) immediately and the assigned Agent is told to stop. Only the submitting session may cancel; finished tasks cannot be cancelled.

| Argument | Type | Notes |
| --- | --- | --- |
| `task_id` | string | **Required.** Stable identity for this logical message. Reuse on retries. If omitted, identical content is deduplicated for this sender/target. Use a new value only for an intentional repeat. |
| `reason` | string |  |

### `list_tasks`

List tasks you submitted (role sent), were assigned (role assigned), or both, newest first. Open tasks only unless include_terminal is true.

| Argument | Type | Notes |
| --- | --- | --- |
| `include_terminal` | boolean |  |
| `limit` | number |  |
| `role` | `sent`, `assigned`, `all` |  |


## Split panes

### `get_layout`

Show the split-pane layout of the tab(s) that show a session (default yours): which sessions share the tab, side_by_side or stacked splits, the focused and zoomed session. Read it before manage_layout, or to check what the user sees beside you.

| Argument | Type | Notes |
| --- | --- | --- |
| `session_id` | string |  |

### `manage_layout`

Arrange split panes by session id; panes are views, so nothing is started or stopped. split: move session_id's pane beside beside_session_id's (default yours), e.g. bring a child agent or a running server into view next to you. unsplit: give session_id its own tab again. close: take its pane off the tab (the session keeps running and stays in the sidebar). focus, zoom/unzoom (fill the tab with one pane), equalize (even out the tab's splits). session_id defaults to you. Split only when the user benefits from seeing both at once; do not rearrange panes the user did not ask about.

| Argument | Type | Notes |
| --- | --- | --- |
| `action` | `split`, `unsplit`, `close`, `focus`, `zoom`, `unzoom`, `equalize` | **Required.** |
| `beside_session_id` | string | split only: the session to dock beside; defaults to the calling session. |
| `session_id` | string | The session whose pane to act on; defaults to the calling session. |
| `split` | `right`, `down`, `left`, `up` | split only: which side of beside_session_id's pane; default right. |


## Lineage

### `whoami`

Describe this session's identity, parent, ancestors, children, worktree, and cross-session write policy. origin_note, when present, is the note you were started from: read it first with read_note {"note":"origin"}.

No arguments.

### `list_children`

List the sessions spawned by this one, optionally including the whole descendant tree.

| Argument | Type | Notes |
| --- | --- | --- |
| `include_exited` | boolean | Default `true`. |
| `recursive` | boolean |  |

### `wait_for_children`

Wait until ALL selected child sessions settle, finish, or exit (use wait_any to handle each as soon as it is ready). Already matching states return immediately. Removed children are reported separately and cannot settle other working children. Omit session_ids for all direct children; an explicit empty array selects none.

| Argument | Type | Notes |
| --- | --- | --- |
| `session_ids` | string array |  |
| `since_ms` | number | Unix time in milliseconds, as returned in since_ms by spawn_agent, send_prompt, and submit_task. A session counts as done only after a turn that finished later. |
| `timeout_s` | number | Default `600`. |
| `until` | `settled`, `done`, `exited` |  |

### `summarize_children`

Collect compact screen tails, each child's last agent message (when its transcript is readable), status, and artifacts for this session's children without interpreting their output.

| Argument | Type | Notes |
| --- | --- | --- |
| `rows` | number | Default `14`. |
| `session_ids` | string array |  |

### `report_to_parent`

If your parent is a note, the report is added to the note: finish with status done and a one-paragraph result in summary (what you did, what changed, what is left), in plain words. Otherwise: deliver a structured update, result, blocker, or question to the session that delegated this work at most once. If your parent assigned you an open Diri task, the report is recorded on that task instead (update→progress, blocked→blocked, done→completed, failed→failed) and reaches the parent through wait_any/wait_for_task; set deliver:true to also type it into the parent's terminal. Identical reports are deduplicated. Reuse message_id on retries; choose a new one only for an intentional repeat. Inspect unknown outcomes without resending.

| Argument | Type | Notes |
| --- | --- | --- |
| `summary` | string | **Required.** |
| `artifacts` | string array |  |
| `blockers` | string array |  |
| `changed_paths` | string array |  |
| `deliver` | boolean |  |
| `details` | string |  |
| `message_id` | string | Stable identity for this logical message. Reuse on retries. If omitted, identical content is deduplicated for this sender/target. Use a new value only for an intentional repeat. |
| `next_steps` | string array |  |
| `proof` | string array |  |
| `questions` | string array |  |
| `status` | `update`, `done`, `blocked`, `failed` |  |
| `submit` | boolean |  |


## Code and worktrees

### `get_diff`

Summarize a session's code changes: changed files with +/- counts against its base (default branch merge-base, or HEAD for uncommitted work only), whether it is committed, and overlaps: files also changed by its live sibling sessions, which would conflict on integration. Set patch:true to include the unified diff (bounded by max_patch_bytes).

| Argument | Type | Notes |
| --- | --- | --- |
| `session_id` | string | **Required.** |
| `base` | `default_branch`, `head` |  |
| `max_patch_bytes` | number | Default `32768`. |
| `overlaps` | boolean | Default `true`. |
| `patch` | boolean |  |

### `integrate`

Bring an authorized child session's committed branch into your own checkout (merge, squash, or cherry_pick). Both checkouts must have no uncommitted tracked changes. Conflicts abort cleanly, change nothing, and are returned as paths. Local sessions in the same project only.

| Argument | Type | Notes |
| --- | --- | --- |
| `session_id` | string | **Required.** |
| `message` | string |  |
| `strategy` | `merge`, `squash`, `cherry_pick` |  |

### `create_worktree`

Create a git worktree in the calling session's project so parallel work does not collide in one checkout.

| Argument | Type | Notes |
| --- | --- | --- |
| `repo` | string | **Required.** |
| `base` | string |  |
| `branch` | string |  |

### `list_worktrees`

List a repository's worktrees with their paths and branches.

| Argument | Type | Notes |
| --- | --- | --- |
| `repo` | string | **Required.** |

### `remove_worktree`

Remove a git worktree from the calling session's project.

| Argument | Type | Notes |
| --- | --- | --- |
| `path` | string | **Required.** |
| `repo` | string | **Required.** |
| `force` | boolean |  |

### `get_artifacts`

Return PRs, issues, preview URLs, and listening ports discovered for a session.

| Argument | Type | Notes |
| --- | --- | --- |
| `session_id` | string | **Required.** |

### `quick_open_include`

Read or change ~/.diri-include, the gitignore-style extra folders Quick Open (Cmd+P) indexes even when hidden or skipped. action get returns the path, text, and patterns; add appends unique patterns (e.g. **/.worktrees/); set replaces the whole file with text (empty clears it).

| Argument | Type | Notes |
| --- | --- | --- |
| `action` | `get`, `add`, `set` | **Required.** |
| `patterns` | string array |  |
| `text` | string |  |


## Schedules

### `schedule_agent`

Schedule an agent run owned by Diri, not by this session: it survives this session closing and Diri restarting. Use it whenever the user wants something done later, at a time, or repeatedly, instead of your own cron/loop tools. How runs work: at each due time Diri opens a NEW top-level session of `kind` in `cwd` and sends `prompt`. That session cannot see this conversation, so write a self-contained prompt: the goal, the repo and relevant context, what to produce, and where the result goes (open a PR, write a file, post a summary). For code changes pass worktree:true and base "origin/main" so each run starts clean. Give a short `name`. When: exactly one of cron (five fields, the Mac's local time, e.g. "0 9 * * 1-5" weekdays 09:00), in_minutes (relative one-off), or at_ms (epoch ms). Missed runs: if the Mac was asleep or Diri was closed, the newest missed run fires once when it is back, within catch_up_hours (default 12); older ones are recorded as missed. wake_mac:true (use when the user wants it to run even if the Mac is asleep): Diri wakes the Mac 2 minutes early (lid must be open), keeps it awake while the agent works, then puts it back to sleep if nobody used it. It needs the one-time "Allow diri to wake the Mac" approval in Settings > Schedules; if list_schedules reports wakeHelperError, tell the user to turn it on. After creating, confirm in words from the returned nextDue, e.g. "weekdays at 09:00, next run Monday; it will wake the Mac".

| Argument | Type | Notes |
| --- | --- | --- |
| `cwd` | string | **Required.** |
| `kind` | agent label or `shell` ([list](/docs/agents/)) | **Required.** |
| `prompt` | string | **Required.** |
| `at_ms` | number |  |
| `base` | string | Starting ref for each run's worktree, e.g. origin/main. |
| `branch` | string |  |
| `catch_up_hours` | number | A run missed by up to this long still fires late; 0 never catches up. Default 12. |
| `cron` | string |  |
| `in_minutes` | number |  |
| `keep_awake` | boolean | Keep an awake Mac from idle-sleeping shortly before each run and while it works. Cannot wake a sleeping Mac. |
| `name` | string |  |
| `wake_mac` | boolean | Wake a sleeping Mac (lid open) 2 minutes before each run, keep it awake while the run works, then let it sleep again. Needs the one-time wake approval in Settings > Schedules; list_schedules reports if it is missing. |
| `worktree` | boolean | Start each run in a fresh worktree. |

### `list_schedules`

List every Diri schedule with its next due time and recent runs (onTime, late with lateReason asleep/notRunning, missed, failed, manual) and the session each run opened.

No arguments.

### `delete_schedule`

Delete a Diri schedule. Sessions its earlier runs opened are left alone.

| Argument | Type | Notes |
| --- | --- | --- |
| `schedule_id` | string | **Required.** |


## Browser

### `browser`

Drive a real browser isolated to this Diri session. Open a URL, inspect snapshot refs, act on those refs, and request a new snapshot after page changes.

| Argument | Type | Notes |
| --- | --- | --- |
| `action` | `open`, `snapshot`, `click`, `fill`, `type`, `press`, `hover`, `select`, `check`, `scroll`, `get`, `wait`, `screenshot`, `console`, `back`, `close`, `list` | **Required.** |
| `amount` | number |  |
| `annotate` | boolean |  |
| `button` | `left`, `right`, `middle` |  |
| `direction` | `up`, `down`, `left`, `right` |  |
| `double` | boolean |  |
| `engine` | `chromium`, `webkit`, `firefox` |  |
| `full` | boolean |  |
| `key` | string |  |
| `ms` | number |  |
| `profile` | string |  |
| `ref` | string |  |
| `selector` | string |  |
| `state` | string |  |
| `text` | string |  |
| `url` | string |  |
| `value` | string |  |
| `what` | `url`, `title`, `text`, `html`, `value`, `count` |  |


## Notes

### `list_notes`

Find the person's Diri Notes (briefs, plans, to-do lists). Notes are kept by Diri, not in the project folder, so use this rather than searching files. Defaults to notes for your project; project:"all" lists every note, or pass a project folder. mentions:"me" returns notes that @-mention you or an ancestor that started you (mentioned_via says which). query filters title and body. links_to (a note's id or title) returns the notes that link to that note. session_id is the note's sidebar Session.

| Argument | Type | Notes |
| --- | --- | --- |
| `include_archived` | boolean |  |
| `limit` | integer |  |
| `links_to` | string |  |
| `mentions` | string |  |
| `project` | string |  |
| `query` | string |  |

### `read_note`

Read one Diri note as Markdown. If you were started from a note, read note "origin" first: it is your brief. markdown is the note's canonical text (title first as a # line, tidy tables, normalised list markers): exactly what edit_note matches, so copy old_string from it. path is the note's .md file, which you may also edit with your own file tools; Diri keeps the change, a version before it, and the note's identity. Returns the text, its to-dos (block index, checked, linked sessions and their live status), its @-mentions resolved to sessions or notes, and its backlinks: other notes that link to this one, with the words around each link. Linked notes and backlinks are often the background a note was written against; read the relevant ones, and use note_links for the full picture. Mentioned sessions may be working on related things: inspect them with read_output/get_diff or wait on them with wait_for_agent. note is an id, a title, part of a title, a note Session id, or "origin": the note you were started from (whoami shows it as origin_note).

| Argument | Type | Notes |
| --- | --- | --- |
| `note` | string | **Required.** |

### `write_note`

Add to a Diri note without rewriting it; never deletes the person's text. entry: one short line when something matters (a decision, a finding, a blocker, a result, a link), filed under your to-do (or the one you name) or in the note's Updates; keep entries sparing, no progress chatter. checked: tick a to-do, e.g. your own sub-tasks as you finish them (the to-do you were started from is the person's to tick). link_session: put a session's chip on a to-do. append: longer Markdown at the end, rarely needed. Pick the to-do by todo (its text or part of it) or todo_index (from read_note). Write [[Note title]] to link another note; it becomes a link by the note's id, so it survives renames. Delegated agents may write only to the note they were started from or notes that mention them.

| Argument | Type | Notes |
| --- | --- | --- |
| `note` | string | **Required.** |
| `append` | string |  |
| `checked` | boolean |  |
| `entry` | string |  |
| `link_session` | string |  |
| `todo` | string |  |
| `todo_index` | integer |  |

### `edit_note`

Change a Diri note in place, like editing a Markdown file: old_string is exact text from read_note's markdown (title line included), new_string replaces it; an empty new_string deletes it. old_string must appear once unless replace_all. Use it when the person asks for a change, or to keep your own entries current (tick a table row, change "Fix" to "Done"); otherwise prefer write_note. Every version is kept, so the person can restore one. Returns the changed lines and the new version. Diri tidies the text after each change (tables are re-aligned); a table row still matches if only its spacing differs, and the reply says so. For anything else, copy the next old_string from the changed lines or a fresh read_note. [[Note title]] in new_string links that note.

| Argument | Type | Notes |
| --- | --- | --- |
| `new_string` | string | **Required.** |
| `note` | string | **Required.** |
| `old_string` | string | **Required.** |
| `replace_all` | boolean |  |

### `replace_section`

Replace everything under one heading of a Diri note, up to the next heading of the same or a higher level; the heading itself stays. heading is its text, optionally with its # marks ("## Status") when two headings share a name. markdown is the new content. Same rules as edit_note: change the person's writing only when asked.

| Argument | Type | Notes |
| --- | --- | --- |
| `heading` | string | **Required.** |
| `markdown` | string | **Required.** |
| `note` | string | **Required.** |

### `create_note`

Write a new Diri note for the person, e.g. an explanation ("how sign-in works") or a write-up. markdown is rich Markdown: headings, lists, to-dos, links, quotes, code. Link the notes it builds on or relates to by writing [[Note title]]: each becomes a link to that note (linked_notes lists them; unlinked_titles matched no single note), and the linked note shows this one among its backlinks. It appears under you in the sidebar, in your project (or project, a folder); open:true shows it to the person right away. Use this for anything longer than a write_note entry.

| Argument | Type | Notes |
| --- | --- | --- |
| `title` | string | **Required.** |
| `markdown` | string |  |
| `open` | boolean |  |
| `project` | string |  |

### `start_from_note`

Start an agent on a Diri note or one of its to-dos. The note becomes the agent's parent in the sidebar, the agent gets the note as its brief (plus the sessions it mentions), and its chip is added to the to-do. kind defaults to your own kind. separate_copy gives it its own copy of the project folder (projects under git only) so its changes stay apart until merged. prompt adds your own instructions. task:true tracks it like submit_task.

| Argument | Type | Notes |
| --- | --- | --- |
| `note` | string | **Required.** |
| `kind` | string |  |
| `operation_id` | string | Stable identity for this logical message. Reuse on retries. If omitted, identical content is deduplicated for this sender/target. Use a new value only for an intentional repeat. |
| `prompt` | string |  |
| `result_schema` | object | Optional JSON Schema (object) for the completed result. The Agent must then report result as JSON matching it (type, required, properties, enum, items are checked). |
| `separate_copy` | boolean |  |
| `task` | boolean |  |
| `todo` | string |  |
| `todo_index` | integer |  |

### `note_history`

Earlier versions of a Diri note: when, by whom, and what changed, newest first. Pass version to read that version's text. Read-only: only the person restores a version.

| Argument | Type | Notes |
| --- | --- | --- |
| `note` | string | **Required.** |
| `version` | integer |  |


## Other

### `get_skill`

Read one of Diri's skills: the detailed rules for a Diri capability, as Markdown. Read the matching skill before acting: scheduling (run anything later, at a time, or repeatedly; waking the Mac), notes (work started from or written to a Diri note), orchestration (parallel agents, tasks, waiting, retries), api (HTTP APIs and dev servers: show endpoints in the API tab). Claude Code sessions also have them as skills named diri:<name>.

| Argument | Type | Notes |
| --- | --- | --- |
| `name` | `scheduling`, `notes`, `orchestration`, `api` | **Required.** |

### `computer`

Use this Mac's apps the way a person would, in the background: any native app (Simulator, TextEdit, Xcode, Finder, the app you are building) and browser pages. The person's cursor, keyboard and focus stay theirs; your actions show as your own labelled cursor. Pass tool and args. Loop: (1) launch_app {bundle_id or name, creates_new_application_instance:true} returns your own pid and windows (or list_windows {pid}); (2) get_window_state {pid, window_id} returns accessibility rows [N] plus a screenshot; (3) act with element_token "<snapshot_id>:N" (click, type_text {text}, set_value, press_key {key}, hotkey {keys}, scroll) or with pixel x,y from that screenshot; (4) re-read with get_window_state {since:"latest"} and check the change before going on. run_steps runs several actions in one call. Act only on apps you launched or the person pointed you to: an app another agent is driving returns computer_app_busy. computer_unavailable or computer_permissions means only the person can finish setup (Diri Settings › Agents › Computer use): tell them, and don't try to install or grant anything yourself. tool "tools" lists every tool.

| Argument | Type | Notes |
| --- | --- | --- |
| `tool` | string | **Required.** A computer tool, e.g. launch_app, list_windows, get_window_state, click, type_text, press_key, hotkey, scroll, run_steps; or "tools" for the list. |
| `args` | object | That tool's arguments. Ask {"tool":"tools","args":{"name":"<tool>"}} for one tool's full arguments. |

### `open_api_request`

Open an HTTP request in the API tab of YOUR session's right panel in Diri, prefilled, so the person can inspect, edit and send it. Use it whenever you build, run or debug an HTTP API: after starting a dev server, open the endpoint you just added or changed (e.g. GET http://localhost:3000/api/health) instead of only pasting a curl command. Put {{name}} placeholders in url/headers/body and pass their values in variables (marking tokens in secrets so they are masked); they land in the project's environment (environment names it, default the active one or "Local"). json sets a JSON body and its Content-Type; body sends text as written. auto_send:true sends a GET immediately and shows the response; any other method is never sent until the person presses Send. The call returns once the tab is open, without the response.

| Argument | Type | Notes |
| --- | --- | --- |
| `url` | string | **Required.** http(s) URL; may use {{variables}}. No scheme means http:// for localhost. |
| `auto_send` | boolean | Send right away. Honoured for GET only. Default `false`. |
| `body` | string | Raw body text. |
| `environment` | string | Environment the variables go into (created if missing). |
| `headers` | object | Header name to value, e.g. {"Authorization": "Bearer {{token}}"}. |
| `json` | any | A JSON value sent as the body, indented, with Content-Type: application/json. |
| `method` | `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS` | Default `"GET"`. |
| `name` | string | Tab and saved-request title. |
| `secrets` | string array | Names in variables to mask as secrets. |
| `variables` | object | Variable name to value for {{name}} placeholders. |

### `name_session`

Name your own Diri session (its tab and sidebar row) after the task: 2-4 lowercase words joined by hyphens naming the task or project, e.g. fix-login-redirect or checkout-flaky-tests, never generic ones like claude or session. Call it unasked while handling the first request that identifies the work (a greeting is not enough), keep a current name that still describes the work (whoami shows it and who set it), and call it again when the task changes. A name the user or Diri gave always wins: the call leaves it and answers applied:false, reason userNamed. When the user asks for a specific name, pass it with user_requested:true; it is kept as written and later agent names never replace it. Mention a rename briefly, if at all. If it fails, carry on with the task.

| Argument | Type | Notes |
| --- | --- | --- |
| `name` | string | **Required.** |
| `user_requested` | boolean |  |

### `note_links`

How one Diri note connects to the others. links: the notes it links to. backlinks: every note linking to it, with the words around each link (backlinks_total counts them). unlinked_mentions: notes that write its title without linking it (unlinked:false skips them). depth (1-3) adds graph: the notes within that many links, either direction, and the links between them. Check backlinks before changing a note others depend on, and to find the plans, briefs and findings a note belongs to. note is an id, a title, part of a title, a note Session id, or "origin".

| Argument | Type | Notes |
| --- | --- | --- |
| `note` | string | **Required.** |
| `depth` | integer |  |
| `unlinked` | boolean |  |
