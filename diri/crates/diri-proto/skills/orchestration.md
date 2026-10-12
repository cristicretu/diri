# Working with other Diri sessions

## Parallel work

Prefer `spawn_agents` with one entry per subtask (`worktree: true`, `prompt`, `task: true`), then `wait_any(task_ids)`. For each ready task: read its result (`read_output` with `mode: "last_message"` for detail), `answer_task` if it is blocked, `get_diff` to review, `integrate` to bring its branch into your checkout. Call `wait_any` again with the pending ids, and `release_agent` when done. `wait_any` returns as soon as **any** target needs you; do not wait for all of them at once.

## Agents versus terminals

To spawn another agent, select its native kind (for example `claude` or `codex`) and pass its task as `prompt`. If no agent is named, use your own native kind when available. Never use `shell` to launch an agent CLI such as `claude`, `codex`, `cursor`, or `gemini`: a child `shell` is a raw terminal in the parent's Cmd+J pane whose prompt runs as shell commands.

## Split panes

Pass `split: "right"` or `"down"` to `spawn_agent` / `spawn_agents` to open the session as a pane beside yours (or beside `beside_session_id`) instead of in its own tab. Do it when the user benefits from seeing both at once: a dev server, test watcher, or log tail next to your work, or a child agent they want to watch. Leave background work in its own tab. `get_layout` shows who shares your tab; `manage_layout` moves an existing session beside another (`split`), gives it its own tab again (`unsplit`), takes its pane off the tab (`close`, the session keeps running), or focuses, zooms, and equalizes. Do not rearrange panes the user did not ask about.

## Tasks

- When you receive a Diri task: `report_task` acknowledged before starting, then completed or failed for that exact `task_id` after verifying (JSON matching `result_schema` when the task has one), or blocked with your question.
- `report_to_parent` is recorded on your open task automatically.

## Delivery

Messages, spawns, and tasks are deduplicated and delivered at most once. Reuse `message_id` / `operation_id` / `request_id` on retries; never resend under a new identity because an agent is slow or its screen is unchanged. A delivery receipt does not mean the work is done. For untracked prompts, pass the returned `since_ms` to `wait_for_agent` / `wait_any` so an agent that was already idle does not count as finished.

## Also

- `get_artifacts` returns PR and preview URLs and ports; PRs include live GitHub status.
- `fork_agent` branches a conversation to try an alternative.
- `manage_agent` hibernates idle children instead of killing them.
- `quick_open_include` edits the folders Cmd+P indexes (for example `**/.worktrees/`).
