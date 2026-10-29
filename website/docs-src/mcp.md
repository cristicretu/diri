---
title: MCP server
description: diri's built-in MCP server lets agents start other agents, assign tracked tasks, wait for results, review diffs and merge branches. Set it up and use it well.
lead: Agents running in diri can start other agents, hand them tasks, wait for the results, review their diffs and merge their branches. They do it through diri's built-in MCP server, named `dirijor`.
---
The server ships inside the app. Claude Code, Codex and Cursor get it automatically when diri starts them, so most people never configure anything. This page covers what it can do, how to connect other agents, and the rules it enforces. Every tool and argument is listed in the [MCP tool reference](/docs/mcp-tools/).

## What agents can do

| Area | Tools | For example |
| --- | --- | --- |
| Start agents | `spawn_agent`, `spawn_agents`, `fork_agent` | Start three Codex agents, each in its own worktree, with one prompt each. |
| Track work | `submit_task`, `wait_any`, `report_task`, `answer_task` | Assign a task, get notified the moment it finishes or gets stuck, answer its question. |
| Read results | `read_output`, `get_status`, `summarize_children` | Read a helper's final answer from its transcript instead of scraping the screen. |
| Review and merge | `get_diff`, `integrate`, `get_artifacts` | See which files a helper changed, spot overlaps, merge its branch, find its PR. |
| Manage | `manage_agent`, `release_agent`, worktree tools | Hibernate an idle helper, resume an exited one, close it when done. |
| Panes | `spawn_agent` with `split`, `manage_layout`, `get_layout` | Open a dev server or a helper agent in a split beside the agent, zoom it, or move it back to its own tab. |
| Notes | `read_note`, `write_note`, `create_note`, `start_from_note` | Read a brief from a note, write findings back, tick its to-dos. |
| Browser | `browser` | Drive a real browser isolated to the session. |
| Window | `list_actions`, `perform_action` | Run any command the app's menus, keys and command palette run: split or zoom a pane, select a tab, toggle the sidebar, change the theme. The window names the agent while it changes. |

You do not need to name tools. Ask in plain words and the agent picks them:

> Split this refactor into three parts. Start a Codex agent for each in its own worktree, wait for them, review each diff, and merge the ones that pass the tests.

## Set up

### Agents started by diri

Nothing to do. When diri starts an agent whose manifest opts in, it passes the server along with the launch:

| Agent | How diri connects it |
| --- | --- |
| Claude Code | `--mcp-config` pointing at a file diri writes at startup. The Engine serves the tools itself over HTTP on `127.0.0.1`, so the session starts no extra process. |
| Codex | `-c mcp_servers.dirijor.url=…` and `bearer_token_env_var` overrides, served by the Engine like Claude Code |
| Cursor | A session-local plugin directory whose `mcp.json` lists the `dirijor-mcp` stdio server |

HTTP is used only from Claude Code 2.1.289 and Codex 0.160.0, the releases verified to work with it. Older or unrecognized versions, remote sessions, and an Engine whose HTTP endpoint can't start all keep the `dirijor-mcp` stdio server, exactly as before. Setting `DIRI_MCP_TRANSPORT=stdio` in the Engine's environment forces the stdio server everywhere.

Each agent session also gets these environment variables. The server uses them to know which session is calling.

| Variable | Meaning |
| --- | --- |
| `DIRIJOR_SESSION_ID` | The calling session. Required for anything that starts, messages or changes something. |
| `DIRIJOR_MCP_TOKEN` | The session's private key for the Engine's HTTP endpoint (Claude Code and Codex only). It identifies the session, stops working when the session ends, and is never written to a file. |
| `DIRIJOR_SOCKET` | The Engine's control socket. |
| `DIRIJOR_CLI` | Path to the bundled [`dirijor` CLI](/docs/cli/). |

To check, run `/mcp` in Claude Code. You should see `dirijor` with its tools.

### Other agents inside diri

Any agent that speaks MCP over stdio can use the same server. Add a server named `dirijor` to that agent's own MCP settings, with this command and no arguments:

```text
/Applications/diri.app/Contents/Resources/bin/dirijor-mcp
```

Started from diri, the agent inherits `DIRIJOR_SESSION_ID`, so every tool works with the same permissions as a built-in integration. For an agent that uses the common `mcpServers` JSON shape (Gemini CLI's `settings.json`, for example):

```json
{
  "mcpServers": {
    "dirijor": {
      "command": "/Applications/diri.app/Contents/Resources/bin/dirijor-mcp"
    }
  }
}
```

If you installed diri somewhere else, use that path. On Linux the binary sits next to the `diri` executable your package installed.

### Agents outside diri

You can connect an agent that runs in another terminal, or a desktop MCP client, to the same server. Without a diri session behind it, the caller has no identity, so only reads work: `list_agents`, `get_status`, `read_output`, `get_diff`, `get_artifacts`, `list_worktrees` and the note readers. Tools that start, message or change something fail with an error saying they need `DIRIJOR_SESSION_ID` and must run inside a live Diri session.

```sh
claude mcp add dirijor -- /Applications/diri.app/Contents/Resources/bin/dirijor-mcp
```

```toml ~/.codex/config.toml
[mcp_servers.dirijor]
command = "/Applications/diri.app/Contents/Resources/bin/dirijor-mcp"
```

diri must be running. The server talks to the Engine over its local socket and fails with `daemon socket: …` when the Engine is not up.

## Permissions

Reads are open: any agent can list sessions and read their status and output. Writes follow the session tree you see in the sidebar.

| Caller | May write to |
| --- | --- |
| A root agent (one you started) | Sessions in its own project, and its direct children on any host. |
| A delegated agent (started by another agent) | Only its parent and its direct children. |

- Agents can never target themselves, and the caller and its ancestors are protected from `release_agent`.
- Messages that cross lineages are delivered with a line saying who sent them.
- Every write is checked against the Engine's latest snapshot, so a stale MCP process fails closed.
- Delegation is capped at 3 levels deep and 16 live children per session. Set `DIRIJOR_MAX_SPAWN_DEPTH` and `DIRIJOR_MAX_LIVE_CHILDREN` in the environment diri starts with to change that.

Ask an agent to run `whoami` to see its identity, parent, children, worktree and the exact policy that applies to it.

> [!WARNING]
> Spawned agents run with your user's permissions, like any process you start. Only let agents you trust coordinate others, and review what they merge. See the [security model](/docs/security/).

## Delivery and waiting

Agents retry. The server is built so a retry never doubles the work.

- **Deduplicated.** Spawns, messages and tasks are delivered at most once. Identical arguments from the same caller return the original result. Reuse `operation_id`, `message_id` or `request_id` on a retry; pass a new one only when you really want a second copy.
- **Receipts, not guesses.** A spawn or message returns a receipt. `sent` means the input reached the terminal, not that the agent finished. An `unknown` outcome should be inspected with `get_task` or `get_status`, never resent under a new id.
- **Wait without polling.** `wait_any` blocks until at least one task or session needs attention, then returns everything that is `ready` and what is still `pending`. Pass `since_ms` from the spawn or send result so an agent that was idle before your message does not count as done.
- **Tasks have real results.** A tracked task completes only when the assigned agent calls `report_task` with `completed`. An agent going idle or exiting cannot fake it.

## Patterns

### Fan out and merge

The loop diri's own server instructions teach every agent:

1. `spawn_agents` with one entry per subtask: `worktree: true`, a `prompt`, and `task: true`.
2. `wait_any` with the returned `task_ids`.
3. For each ready task: `read_output` with `mode: "last_message"` for detail, `answer_task` if it is blocked, `get_diff` to review, `integrate` to bring its branch into your checkout.
4. Call `wait_any` again with the pending ids.
5. `release_agent` when a helper is done.

```json spawn_agents
{
  "agents": [
    { "kind": "codex", "cwd": "~/code/app", "worktree": true, "task": true, "name": "API client", "prompt": "Generate a typed client for openapi.yaml and add tests." },
    { "kind": "claude", "cwd": "~/code/app", "worktree": true, "task": true, "name": "Docs", "prompt": "Document every public endpoint in docs/api.md." }
  ]
}
```

### Structured results

Pass `result_schema` with a task and the helper must report JSON of that shape. Use it when a parent agent will act on the answer.

```json submit_task
{
  "session_id": "…",
  "text": "Find every call site of parseConfig and say whether it handles errors.",
  "result_schema": {
    "type": "object",
    "properties": {
      "call_sites": { "type": "array", "items": { "type": "string" } },
      "unhandled": { "type": "integer" }
    },
    "required": ["call_sites", "unhandled"]
  }
}
```

### Agents and plain terminals

To start another agent, use its own kind (`claude`, `codex`, `gemini`…) and pass the task as `prompt`. A `shell` child is a raw terminal in the parent's <kbd>⌘</kbd><kbd>J</kbd> pane: its prompt runs as shell commands, so never use it to launch an agent CLI.

### Notes as briefs

An agent started from a note sees `origin_note` in `whoami`. It reads the brief with `read_note`, adds short findings with `write_note`, ticks its own sub-tasks, and finishes with `report_to_parent`, which lands in the note. The person ticks the original to-do after reviewing. More in [Notes](/docs/notes/).

## Protocol details

| | |
| --- | --- |
| Transport | stdio, newline-delimited JSON-RPC 2.0 |
| Protocol versions | `2025-06-18` (default), `2025-03-26`, `2024-11-05` |
| Capabilities | `tools`. Results include `structuredContent` for clients that read it, plus a text block for the rest. |
| Server name | `dirijor` |
| Instructions | Sent on `initialize`: the fan-out loop, delivery rules and the notes contract. |

From a terminal, the [`dirijor` CLI](/docs/cli/) exposes the same catalog. `dirijor mcp-tools` prints every tool definition, and `dirijor mcp-call --tool <name>` calls one with JSON arguments on stdin:

```sh
echo '{}' | dirijor mcp-call --tool list_agents
```

## Docs MCP

These docs have their own small, read-only MCP server, separate from `dirijor`. It lets an agent search and read diri's documentation while it helps you. It runs at `https://diri.sh/mcp` over streamable HTTP, needs no account, and has three tools: `search_docs`, `read_doc` and `list_docs`.

```sh
claude mcp add --transport http diri-docs https://diri.sh/mcp
```

```toml ~/.codex/config.toml
[mcp_servers.diri-docs]
url = "https://diri.sh/mcp"
```

Prefer files? Every page is also plain Markdown: add `.md` to its path, or point your agent at [llms.txt](/llms.txt).
