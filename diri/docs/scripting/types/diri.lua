---@meta diri
-- Generated from the Engine's method catalog and the app's action catalog.
-- Do not edit: run `DIRI_UPDATE_DOCS=1 cargo test -p diri-app scripting_reference`.
-- Point your editor's Lua language server at this directory (workspace.library).

---@class diri
diri = {}

---@alias diri.Event
---| "session.created" # A session appeared: {session, kind}.
---| "session.status" # A session's status changed: {session, kind, from, to}.
---| "session.exited" # A session's process ended: {session, kind, code, signal, reason}.
---| "session.title" # A session's title changed: {session, kind, from, to}.
---| "session.removed" # A session was closed: {id, reason}.
---| "session.notification" # A program posted a notification: {session_id, title, body}.
---| "workspace.updated" # Workspaces, tabs or splits changed: the new snapshot.
---| "schedule.updated" # A schedule changed or ran: {id}.
---| "focus" # The window's selected session changed (in the app only): {session}.

diri.keymap = {}
diri.json = {}

---Run `fn(payload)` whenever `event` happens. Returns an id for `diri.off`.
---@param event diri.Event
---@param fn fun(payload: table)
---@return integer
function diri.on(event, fn) end

---Stop a handler or timer.
---@param id integer
function diri.off(id) end

---Call an Engine method by name. Parameters are checked against its schema.
---@param method string
---@param params? table
---@param opts? {timeout?: number}
---@return any
function diri.call(method, params, opts) end

---Run a window action by name (needs the Diri app).
---@param name string
---@param args? table
---@return any
function diri.action(name, args) end

---Bind keys to an action name, `{action=, args=}`, or a function. The
---shortcuts a person records in Settings still win over these.
---@param keys string  e.g. "cmd-shift-d" or "ctrl-a c"
---@param target string|table|fun()
---@param opts? {desc?: string}
---@return integer
function diri.keymap.set(keys, target, opts) end

---Free a key from its default command.
---@param keys string
function diri.keymap.del(keys) end

---Add a command to the palette.
---@param spec {name: string, title?: string, run: string|fun(), args?: table}
---@return integer
function diri.command(spec) end

---Show a toast in the window (stderr from `dirijor script`).
---@param message string
---@param opts? {level?: "info"|"success"|"warning"|"error", detail?: string}
function diri.notify(message, opts) end

---Write to the script log.
function diri.log(...) end

---Run `fn` once after `seconds`.
---@param seconds number
---@param fn fun()
---@return integer
function diri.after(seconds, fn) end

---Run `fn` every `seconds` (at least 0.5).
---@param seconds number
---@param fn fun()
---@return integer
function diri.every(seconds, fn) end

---End a `dirijor script` run.
function diri.stop() end

---Render a value as readable text.
---@return string
function diri.inspect(value) end

---@return string
function diri.json.encode(value) end
---@return any
function diri.json.decode(text) end

---Engine method names to one-line summaries.
---@type table<string, string>
diri.methods = {}

---Event names to one-line descriptions.
---@type table<string, string>
diri.events = {}

---"app" inside Diri, "cli" from `dirijor script`.
---@type string
diri.context = ""

diri.workspace = {}
diri.session = {}
diri.task = {}
diri.schedule = {}
diri.computer = {}
diri.host = {}
diri.activity = {}
diri.worktree = {}
diri.project = {}
diri.governor = {}
diri.terminal = {}
diri.agent = {}
diri.account = {}
diri.account.profiles = {}
diri.account.codex = {}
diri.account.claude = {}
diri.voice = {}
diri.test = {}
diri.browser = {}
diri.state = {}
diri.telemetry = {}
diri.app = {}
diri.app.actions = {}
diri.api = {}

---Workspaces, their tabs and each tab's split layout.
---@param params? table
---@param opts? {timeout?: number}
---@return table # WorkspaceSnapshot
function diri.workspace.snapshot(params, opts) end

---Change workspaces, tabs or splits at an expected revision; a stale revision is rejected.
---@param params {expectedRevision: integer, mutation: any}
---@param opts? {timeout?: number}
---@return table # WorkspaceSnapshot
function diri.workspace.mutate(params, opts) end

---Start a session on behalf of a sender, deduplicated by operation id.
---@param params {senderID: string, operationID: string, spawn: any}
---@param opts? {timeout?: number}
---@return table # SessionRecord
function diri.session.spawn_tracked(params, opts) end

---Assign a tracked task to an Agent.
---@param params {caller_id: string, request_id: string, session_id: string, text: string, result_schema?: any}
---@param opts? {timeout?: number}
---@return table # TaskRecord
function diri.task.submit(params, opts) end

---Read one task by id or request id.
---@param params {caller_id: string, task_id?: string, request_id?: string}
---@param opts? {timeout?: number}
---@return table # TaskRecord
function diri.task.get(params, opts) end

---Acknowledge or report a task you were assigned.
---@param params {caller_id: string, task_id: string, status: any, result?: string}
---@param opts? {timeout?: number}
---@return table # TaskRecord
function diri.task.report(params, opts) end

---Answer a task you submitted.
---@param params {caller_id: string, task_id: string, text: string}
---@param opts? {timeout?: number}
---@return table # TaskRecord
function diri.task.answer(params, opts) end

---Withdraw a task you submitted.
---@param params {caller_id: string, task_id: string, reason?: string}
---@param opts? {timeout?: number}
---@return table # TaskRecord
function diri.task.cancel(params, opts) end

---Tasks you sent or were assigned.
---@param params {caller_id: string, role?: string, include_terminal?: boolean, limit?: integer}
---@param opts? {timeout?: number}
---@return table # TaskListResult
function diri.task.list(params, opts) end

---Schedule Agent runs owned by Diri.
---@param params {title: string, when: any, spawn: any, catchUpWindowMs?: integer, keepAwake?: boolean, wakeMac?: boolean, enabled?: boolean}
---@param opts? {timeout?: number}
---@return table # ScheduleRecord
function diri.schedule.create(params, opts) end

---Change a schedule.
---@param params {id: string, title: string, when: any, spawn: any, catchUpWindowMs?: integer, keepAwake?: boolean, wakeMac?: boolean, enabled?: boolean}
---@param opts? {timeout?: number}
---@return table # ScheduleRecord
function diri.schedule.update(params, opts) end

---Delete a schedule.
---@param params {id: string}
---@param opts? {timeout?: number}
---@return table # {}
function diri.schedule.delete(params, opts) end

---Every schedule with its next due time.
---@param params? table
---@param opts? {timeout?: number}
---@return table # ScheduleListResult
function diri.schedule.list(params, opts) end

---Start one run now without moving the schedule.
---@param params {id: string}
---@param opts? {timeout?: number}
---@return table # ScheduleRecord
function diri.schedule.run_now(params, opts) end

---Start an Agent or shell session.
---@param params {accountProfileId?: string, kind: any, cwd: string, newWorktree?: boolean, worktreeBranch?: string, worktreeBase?: string, title?: string, initialPrompt?: string, parent?: string, initialCols?: integer, initialRows?: integer, host?: string, sameRepoAs?: string, startDirectory?: string, noteId?: string, appearance?: any, argv?: any[], keepOpen?: boolean}
---@param opts? {timeout?: number}
---@return table # SessionRecord
function diri.session.spawn(params, opts) end

---Every session record and project.
---@param params? table
---@param opts? {timeout?: number}
---@return table # SessionListResult
function diri.session.list(params, opts) end

---Stop a session's process; the record stays.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # {}
function diri.session.kill(params, opts) end

---Stop a session and delete its record.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # {}
function diri.session.remove(params, opts) end

---Rename a session.
---@param params {sessionID: string, title: string, by?: any}
---@param opts? {timeout?: number}
---@return table # SessionRecord
function diri.session.rename(params, opts) end

---Foreground program, working directory and process facts.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # SessionProcessInfo
function diri.session.process_info(params, opts) end

---Reconnect a remote session to its Holder.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # SessionRecord
function diri.session.reconnect(params, opts) end

---Resume an exited session's conversation.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # SessionRecord
function diri.session.resume(params, opts) end

---Fork a session's conversation into a new session.
---@param params {sessionID: string, parent?: string}
---@param opts? {timeout?: number}
---@return table # SessionRecord
function diri.session.fork(params, opts) end

---Deliver a message to an Agent with at-most-once identity.
---@param params {sessionID: string, senderID: string, messageID: string, text: string, submit: boolean}
---@param opts? {timeout?: number}
---@return table # DeliverMessageResult
function diri.session.deliver_message(params, opts) end

---Press one named key in a session's terminal.
---@param params {sessionID: string, key: any, modifiers?: any, action?: any}
---@param opts? {timeout?: number}
---@return table # {}
function diri.session.send_key(params, opts) end

---Type text into a session's terminal, optionally pressing Return.
---@param params {sessionID: string, text: string, submit: boolean}
---@param opts? {timeout?: number}
---@return table # {}
function diri.session.send_text(params, opts) end

---Resize a session's terminal.
---@param params {sessionID: string, cols: integer, rows: integer}
---@param opts? {timeout?: number}
---@return table # {}
function diri.session.resize(params, opts) end

---The visible screen as text.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # ReadScreenResult
function diri.session.read_screen(params, opts) end

---The terminal's current OSC title.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # TerminalTitleResult
function diri.session.terminal_title(params, opts) end

---Reset the emulator without touching the process.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # {}
function diri.session.reset_terminal(params, opts) end

---Drop the scrollback and clear the screen; the process keeps running.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # {}
function diri.session.clear_scrollback(params, opts) end

---List a directory as the session sees it, local or remote, relative to its working directory.
---@param params {sessionID: string, path?: string, includeFiles?: boolean, prefix?: string, limit?: integer}
---@param opts? {timeout?: number}
---@return table # SessionListDirResult
function diri.session.list_dir(params, opts) end

---Screen plus history as text, for find.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # CaptureFindResult
function diri.session.capture_find(params, opts) end

---One bounded page of a search over a session's whole history, by stable row.
---@param params {sessionID: string, query: string, beforeRow?: integer, cols?: integer, maxMatches: integer, budgetMs: integer}
---@param opts? {timeout?: number}
---@return table # FindHistoryResult
function diri.session.find_history(params, opts) end

---The terminal history as text.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # ReadScrollbackResult
function diri.session.read_scrollback(params, opts) end

---A window of history rows with styles.
---@param params {sessionID: string, firstRow: integer, maxRows: integer, styles?: boolean}
---@param opts? {timeout?: number}
---@return table # ScrollbackCells
function diri.session.read_scrollback_cells(params, opts) end

---The git diff of a session's working tree.
---@param params {sessionID: string, base?: any}
---@param opts? {timeout?: number}
---@return table # SessionDiff
function diri.session.read_diff(params, opts) end

---One chunk of an inline image the terminal placed.
---@param params {sessionID: string, key: string, offset?: integer}
---@param opts? {timeout?: number}
---@return table # InlineImageChunk
function diri.session.read_image(params, opts) end

---The latest conversation turns of an Agent.
---@param params {sessionID: string, turns?: integer}
---@param opts? {timeout?: number}
---@return table # ReadTranscriptResult
function diri.session.read_transcript(params, opts) end

---Mark a session's finished turn as seen.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # {}
function diri.session.mark_seen(params, opts) end

---Return a finished turn to unseen.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # {}
function diri.session.mark_unread(params, opts) end

---Put an idle Agent to sleep, freeing its memory.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # SessionRecord
function diri.session.hibernate(params, opts) end

---Wake a sleeping Agent.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # SessionRecord
function diri.session.wake(params, opts) end

---Stop a session and move it to the archive.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # {}
function diri.session.archive(params, opts) end

---Restore an archived session's record.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # {}
function diri.session.unarchive(params, opts) end

---Reopen the most recently closed session.
---@param params? table
---@param opts? {timeout?: number}
---@return table # SessionRecord
function diri.session.reopen_last(params, opts) end

---Ask the app to show a session without taking focus.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # {}
function diri.session.reveal(params, opts) end

---Open a prefilled HTTP request in a session's API tab.
---@param params {sessionID: string, request: any}
---@param opts? {timeout?: number}
---@return table # {}
function diri.session.open_api_request(params, opts) end

---One Cua Driver step under the caller's own cursor.
---@param params {sessionID: string, tool: string, args?: any}
---@param opts? {timeout?: number}
---@return table # Value
function diri.computer.act(params, opts) end

---Whether computer use is installed and allowed.
---@param params? {probe?: boolean}
---@param opts? {timeout?: number}
---@return table # ComputerStatus
function diri.computer.status(params, opts) end

---Install or restart computer use.
---@param params {action: any}
---@param opts? {timeout?: number}
---@return table # ComputerStatus
function diri.computer.setup(params, opts) end

---Move a Claude session between this Mac and a host.
---@param params {sessionID: string, targetHost?: string}
---@param opts? {timeout?: number}
---@return table # SessionRecord
function diri.session.migrate(params, opts) end

---Record that a session now works in another worktree.
---@param params {sessionID: string, projectRoot: string, worktreePath: string}
---@param opts? {timeout?: number}
---@return table # SessionRecord
function diri.session.reparent_worktree(params, opts) end

---Push Agent preferences to a remote host.
---@param params {host: string}
---@param opts? {timeout?: number}
---@return table # PrefsSyncReport
function diri.host.sync_prefs(params, opts) end

---Find a session's repository on a host.
---@param params? {host?: string, originURL?: string, sessionID?: string}
---@param opts? {timeout?: number}
---@return table # HostLocateRepoResult
function diri.host.locate_repo(params, opts) end

---Install or verify the remote Helper on a host.
---@param params {host: string, forceReinstall?: boolean}
---@param opts? {timeout?: number}
---@return table # HostInitializeResult
function diri.host.initialize(params, opts) end

---Agent usage on a host.
---@param params {host: string}
---@param opts? {timeout?: number}
---@return table # Value
function diri.host.usage(params, opts) end

---One level of folders on a host.
---@param params {host?: string, path: string, mode?: any}
---@param opts? {timeout?: number}
---@return table # HostListDirectoriesResult
function diri.host.list_directories(params, opts) end

---Configured hosts.
---@param params? table
---@param opts? {timeout?: number}
---@return table # HostsConfig
function diri.host.list(params, opts) end

---Past Agent conversations that can be resumed.
---@param params? table
---@param opts? {timeout?: number}
---@return table # SessionHistoryResult
function diri.session.history(params, opts) end

---Recent session activity, newest first.
---@param params? {limit?: integer}
---@param opts? {timeout?: number}
---@return table # ActivityListResult
function diri.activity.list(params, opts) end

---Open a session resuming a past conversation.
---@param params {entry: any, initialPrompt?: string}
---@param opts? {timeout?: number}
---@return table # SessionRecord
function diri.session.resume_from_history(params, opts) end

---Create a git worktree.
---@param params {repoPath: string, branch?: string, base?: string}
---@param opts? {timeout?: number}
---@return table # WorktreeInfo
function diri.worktree.create(params, opts) end

---A repository's worktrees.
---@param params {repoPath: string}
---@param opts? {timeout?: number}
---@return table # WorktreeListResult
function diri.worktree.list(params, opts) end

---Remove finished worktrees.
---@param params {repoPath: string, worktreePath: string, expectedHead: string}
---@param opts? {timeout?: number}
---@return table # WorktreeCleanupResult
function diri.worktree.cleanup(params, opts) end

---Remove one worktree.
---@param params {repoPath: string, worktreePath: string, force: boolean}
---@param opts? {timeout?: number}
---@return table # {}
function diri.worktree.remove(params, opts) end

---Scan for worktrees, a page at a time.
---@param params? {refresh?: boolean, measureDisk?: boolean, generation?: integer, cursor?: integer}
---@param opts? {timeout?: number}
---@return table # WorktreeScanPage
function diri.worktree.scan(params, opts) end

---Every worktree Diri knows with its sessions.
---@param params? table
---@param opts? {timeout?: number}
---@return table # WorktreeOverviewResult
function diri.worktree.overview(params, opts) end

---Merge one session's branch into another's.
---@param params {sourceSessionID: string, targetSessionID: string, strategy?: any, message?: string}
---@param opts? {timeout?: number}
---@return table # WorktreeIntegrateResult
function diri.worktree.integrate(params, opts) end

---Add a project folder.
---@param params {root: string}
---@param opts? {timeout?: number}
---@return table # Project
function diri.project.add(params, opts) end

---Idle and memory limits for sleeping Agents.
---@param params {idleThresholdSeconds: number, hardMemoryBytes: integer}
---@param opts? {timeout?: number}
---@return table # {}
function diri.governor.configure(params, opts) end

---Terminal settings the Engine applies.
---@param params {shellIntegration: boolean, palette?: any, shellCommand?: any[]}
---@param opts? {timeout?: number}
---@return table # {}
function diri.terminal.configure(params, opts) end

---Installed Agents and their descriptors.
---@param params? {host?: string, forceRefresh?: boolean}
---@param opts? {timeout?: number}
---@return table # AgentReadinessResult
function diri.agent.readiness(params, opts) end

---Paths, arguments and visibility of an Agent.
---@param params {host?: string, kind: any, executablePath?: string, showInQuickCreate: boolean, launchArgs?: any[]}
---@param opts? {timeout?: number}
---@return table # AgentReadinessResult
function diri.agent.configure(params, opts) end

---Saved Agent accounts (no secrets).
---@param params? table
---@param opts? {timeout?: number}
---@return table # AgentAccountCatalog
function diri.account.profiles.list(params, opts) end

---Save an account profile.
---@param params {id: string, label: string, agent: string, host?: string, configHome: string, isDefault?: boolean, loginStore?: string}
---@param opts? {timeout?: number}
---@return table # AgentAccountCatalog
function diri.account.profiles.save(params, opts) end

---Remove an account profile.
---@param params {id: string}
---@param opts? {timeout?: number}
---@return table # AgentAccountCatalog
function diri.account.profiles.remove(params, opts) end

---Start a Codex sign-in for a profile.
---@param params {id: string}
---@param opts? {timeout?: number}
---@return table # AgentAccountLogin
function diri.account.codex.login(params, opts) end

---Save the signed-in Codex account.
---@param params {id: string}
---@param opts? {timeout?: number}
---@return table # AgentAccountCatalog
function diri.account.codex.capture(params, opts) end

---Start a Claude sign-in for a profile.
---@param params {id: string}
---@param opts? {timeout?: number}
---@return table # AgentAccountLogin
function diri.account.claude.login(params, opts) end

---Save the signed-in Claude account.
---@param params {id: string}
---@param opts? {timeout?: number}
---@return table # AgentAccountCatalog
function diri.account.claude.capture(params, opts) end

---Switch every open tab of an Agent to a saved account.
---@param params {accountProfileId: string}
---@param opts? {timeout?: number}
---@return table # SwitchAccountResult
function diri.account.switch_all(params, opts) end

---Accounts in use and limits.
---@param params? table
---@param opts? {timeout?: number}
---@return table # AgentAccountOverview
function diri.account.overview(params, opts) end

---Save the Agent's current login as a profile.
---@param params {agent: string}
---@param opts? {timeout?: number}
---@return table # AgentAccountCatalog
function diri.account.adopt(params, opts) end

---Add another account for an Agent.
---@param params {agent: string}
---@param opts? {timeout?: number}
---@return table # AgentAccountLogin
function diri.account.add(params, opts) end

---Continue a conversation under another saved account.
---@param params {sessionID: string, accountProfileId: string}
---@param opts? {timeout?: number}
---@return table # SessionRecord
function diri.session.continue_with_account(params, opts) end

---Whether Talk with your agent is on.
---@param params? table
---@param opts? {timeout?: number}
---@return table # VoiceStateResult
function diri.voice.state(params, opts) end

---An Agent's spoken summary of its turn.
---@param params {sessionID: string, text: string}
---@param opts? {timeout?: number}
---@return table # {}
function diri.voice.speak(params, opts) end

---Collect a session's spoken summary.
---@param params {sessionID: string, turn: integer}
---@param opts? {timeout?: number}
---@return table # SpokenResult
function diri.voice.spoken(params, opts) end

---Read the latest spoken summary without collecting it.
---@param params {sessionID: string}
---@param opts? {timeout?: number}
---@return table # LatestSpokenResult
function diri.voice.latest_spoken(params, opts) end

---Get or set whether a phone wants spoken summaries.
---@param params? {readAloud?: boolean}
---@param opts? {timeout?: number}
---@return table # PhoneReaderResult
function diri.voice.phone_reader(params, opts) end

---Run a browser test step in the shared pool.
---@param params? table
---@param opts? {timeout?: number}
---@return table # Value
function diri.test.run(params, opts) end

---One step of the shared browser.
---@param params? table
---@param opts? {timeout?: number}
---@return table # Value
function diri.browser.act(params, opts) end

---Alias of session.list.
---@param params? table
---@param opts? {timeout?: number}
---@return table # SessionListResult
function diri.state.snapshot(params, opts) end

---Upload diagnostics now.
---@param params? table
---@param opts? {timeout?: number}
---@return table # TelemetryUploadNowResult
function diri.telemetry.upload_now(params, opts) end

---The connected app's actions with argument schemas.
---@param params? table
---@param opts? {timeout?: number}
---@return table # AppActionsListResult
function diri.app.actions.list(params, opts) end

---Run one app action in the window and wait for its answer.
---@param params {action: string, args?: any, actor?: string, timeoutMs?: integer}
---@param opts? {timeout?: number}
---@return table # AppPerformResult
function diri.app.perform(params, opts) end

---Every method and app action, one line each.
---@param params? table
---@param opts? {timeout?: number}
---@return table # ApiListResult
function diri.api.list(params, opts) end

---One method's or action's schema and example.
---@param params {name: string}
---@param opts? {timeout?: number}
---@return table # ApiDescribeResult
function diri.api.describe(params, opts) end

---The whole API as compact text for an Agent.
---@param params? table
---@param opts? {timeout?: number}
---@return table # ApiTextResult
function diri.api.llms(params, opts) end

---The window's actions (`diri.actions.split_pane_right()`).
---@class diri.actions
diri.actions = {}

---Close Diri and leave no windows open
---@param args? table
function diri.actions.quit(args) end

---Open another window for the current workspace
---@param args? table
function diri.actions.new_window(args) end

---Close the current Diri window
---@param args? table
function diri.actions.close_window(args) end

---Close the selected session
---@param args? {session_id?: string}
function diri.actions.close_session(args) end

---Restore the most recently closed session
---@param args? table
function diri.actions.reopen_session(args) end

---Open the new session picker
---@param args? table
function diri.actions.open_launcher(args) end

---Start a session with the default agent
---@param args? table
function diri.actions.new_default(args) end

---Start a standalone shell session
---@param args? table
function diri.actions.new_terminal(args) end

---Start a new Codex session
---@param args? table
function diri.actions.new_codex(args) end

---Search all available commands
---@param args? table
function diri.actions.command_palette(args) end

---Find and open a project folder
---@param args? table
function diri.actions.quick_open(args) end

---Open or close conversation history
---@param args? table
function diri.actions.history(args) end

---Open or close the session overview
---@param args? table
function diri.actions.session_overview(args) end

---Preview sessions across projects without changing work
---@param args? table
function diri.actions.tab_peek(args) end

---Move focus to the nearest pane in this direction
---@param args? table
function diri.actions.focus_pane_left(args) end

---Move focus to the nearest pane in this direction
---@param args? table
function diri.actions.focus_pane_right(args) end

---Move focus to the nearest pane in this direction
---@param args? table
function diri.actions.focus_pane_up(args) end

---Move focus to the nearest pane in this direction
---@param args? table
function diri.actions.focus_pane_down(args) end

---Open a terminal to the right of the focused pane
---@param args? table
function diri.actions.split_pane_right(args) end

---Open a terminal below the focused pane
---@param args? table
function diri.actions.split_pane_below(args) end

---Open a terminal to the left of the focused pane
---@param args? table
function diri.actions.split_pane_left(args) end

---Open a terminal above the focused pane
---@param args? table
function diri.actions.split_pane_above(args) end

---Open a terminal beside the focused pane, splitting its longer side
---@param args? table
function diri.actions.split_pane_best_fit(args) end

---Move focus to the previous pane of the tab
---@param args? table
function diri.actions.focus_previous_pane(args) end

---Move focus to the next pane of the tab
---@param args? table
function diri.actions.focus_next_pane(args) end

---Give every pane of the tab an equal share
---@param args? table
function diri.actions.equalize_panes(args) end

---Expand the focused pane or restore its saved layout
---@param args? table
function diri.actions.toggle_pane_zoom(args) end

---Remove the pane reference while keeping its session running
---@param args? table
function diri.actions.remove_focused_pane(args) end

---Resize the nearest matching split divider by five percent
---@param args? table
function diri.actions.pane_grow_width(args) end

---Resize the nearest matching split divider by five percent
---@param args? table
function diri.actions.pane_shrink_width(args) end

---Resize the nearest matching split divider by five percent
---@param args? table
function diri.actions.pane_grow_height(args) end

---Resize the nearest matching split divider by five percent
---@param args? table
function diri.actions.pane_shrink_height(args) end

---Swap the focused pane with its nearest neighbor
---@param args? table
function diri.actions.swap_pane_left(args) end

---Swap the focused pane with its nearest neighbor
---@param args? table
function diri.actions.swap_pane_right(args) end

---Swap the focused pane with its nearest neighbor
---@param args? table
function diri.actions.swap_pane_up(args) end

---Swap the focused pane with its nearest neighbor
---@param args? table
function diri.actions.swap_pane_down(args) end

---Move the focused pane with its nearest neighbor
---@param args? table
function diri.actions.move_pane_left(args) end

---Move the focused pane with its nearest neighbor
---@param args? table
function diri.actions.move_pane_right(args) end

---Move the focused pane with its nearest neighbor
---@param args? table
function diri.actions.move_pane_up(args) end

---Move the focused pane with its nearest neighbor
---@param args? table
function diri.actions.move_pane_down(args) end

---Give the focused pane a tab of its own; its program keeps running
---@param args? table
function diri.actions.move_pane_to_new_tab(args) end

---Move the focused pane into the next tab in the sidebar and follow it
---@param args? table
function diri.actions.move_pane_to_next_tab(args) end

---Move the focused pane into the previous tab in the sidebar and follow it
---@param args? table
function diri.actions.move_pane_to_previous_tab(args) end

---Open the current tab in a new window; the program keeps running
---@param args? table
function diri.actions.move_tab_to_new_window(args) end

---Show the current tab in the next window instead of this one
---@param args? table
function diri.actions.move_tab_to_next_window(args) end

---Start a note in the current project, beside its agents
---@param args? table
function diri.actions.new_note(args) end

---Every open to-do across your notes
---@param args? table
function diri.actions.todos(args) end

---Find any note, live or archived
---@param args? table
function diri.actions.search_notes(args) end

---See and restore earlier versions of the open note
---@param args? table
function diri.actions.note_version_history(args) end

---See how the open note links to your other notes
---@param args? table
function diri.actions.note_graph(args) end

---Open the Git worktrees overview
---@param args? table
function diri.actions.worktrees(args) end

---Open or close Diri settings
---@param args? table
function diri.actions.settings(args) end

---Show or hide the sidebar with vertical tabs, or the top bar with horizontal tabs
---@param args? table
function diri.actions.toggle_sidebar(args) end

---Place session tabs across the top or in the sidebar
---@param args? table
function diri.actions.toggle_tab_orientation(args) end

---Place session tabs across the top or in the sidebar
---@param args? table
function diri.actions.horizontal_tabs(args) end

---Place session tabs across the top or in the sidebar
---@param args? table
function diri.actions.vertical_tabs(args) end

---Show panes as rounded islands, or edge to edge with hairline borders
---@param args? table
function diri.actions.comfortable_density(args) end

---Show panes as rounded islands, or edge to edge with hairline borders
---@param args? table
function diri.actions.compact_density(args) end

---Move keyboard focus to the sessions sidebar
---@param args? table
function diri.actions.focus_sidebar(args) end

---Show or hide the session inspector
---@param args? table
function diri.actions.toggle_inspector(args) end

---Show or hide a terminal pane below the focused pane
---@param args? table
function diri.actions.toggle_auxiliary_terminal(args) end

---Archive the selected session
---@param args? {session_id?: string}
function diri.actions.archive_selected_session(args) end

---Rename the selected session
---@param args? table
function diri.actions.rename_selected_session(args) end

---Hand off work from the selected session
---@param args? table
function diri.actions.delegate_selected_session(args) end

---Jump to the next session waiting for you
---@param args? table
function diri.actions.select_next_attention_session(args) end

---Open the unread notification inbox
---@param args? table
function diri.actions.toggle_notifications(args) end

---Show only the sessions that are working, waiting on you or finished
---@param args? table
function diri.actions.toggle_activity(args) end

---Look for a newer version of Diri
---@param args? table
function diri.actions.check_for_updates(args) end

---Replay the highlights of recent releases
---@param args? table
function diri.actions.whats_new(args) end

---Turn Talk with your agent on or off for every agent
---@param args? table
function diri.actions.toggle_read_aloud(args) end

---Read this tab, only this tab, every tab but this one, or every tab
---@param args? table
function diri.actions.toggle_read_this_tab(args) end

---Read this tab, only this tab, every tab but this one, or every tab
---@param args? table
function diri.actions.read_only_this_tab(args) end

---Read this tab, only this tab, every tab but this one, or every tab
---@param args? table
function diri.actions.read_all_except_this_tab(args) end

---Read this tab, only this tab, every tab but this one, or every tab
---@param args? table
function diri.actions.read_all_tabs(args) end

---Show frame rate, frame times and memory over the window
---@param args? table
function diri.actions.toggle_perf_overlay(args) end

---Count how often each view renders
---@param args? table
function diri.actions.toggle_render_counters(args) end

---Run ~/.config/diri/init.lua again, or start it if it is new
---@param args? table
function diri.actions.reload_scripts(args) end

---Select the previous session in the sidebar
---@param args? table
function diri.actions.select_previous_session(args) end

---Select the next session in the sidebar
---@param args? table
function diri.actions.select_next_session(args) end

---Move the selected session up in the sidebar
---@param args? table
function diri.actions.move_selected_session_up(args) end

---Move the selected session down in the sidebar
---@param args? table
function diri.actions.move_selected_session_down(args) end

---Select the first session
---@param args? table
function diri.actions.select_session_1(args) end

---Select the second session
---@param args? table
function diri.actions.select_session_2(args) end

---Select the third session
---@param args? table
function diri.actions.select_session_3(args) end

---Select the fourth session
---@param args? table
function diri.actions.select_session_4(args) end

---Select the fifth session
---@param args? table
function diri.actions.select_session_5(args) end

---Select the sixth session
---@param args? table
function diri.actions.select_session_6(args) end

---Select the seventh session
---@param args? table
function diri.actions.select_session_7(args) end

---Select the eighth session
---@param args? table
function diri.actions.select_session_8(args) end

---Select the last session
---@param args? table
function diri.actions.select_last_session(args) end

---Search the active terminal output
---@param args? table
function diri.actions.terminal_find(args) end

---Move to the next terminal search result
---@param args? table
function diri.actions.terminal_find_next(args) end

---Move to the previous terminal search result
---@param args? table
function diri.actions.terminal_find_previous(args) end

---Make terminal text larger
---@param args? table
function diri.actions.terminal_zoom_in(args) end

---Make terminal text smaller
---@param args? table
function diri.actions.terminal_zoom_out(args) end

---Restore the default terminal text size
---@param args? table
function diri.actions.terminal_reset_zoom(args) end

---Select terminal output with the keyboard
---@param args? table
function diri.actions.terminal_copy_mode(args) end

---Search for the selected terminal text
---@param args? table
function diri.actions.terminal_find_selection(args) end

---Pick a file under the session's directory and type its path
---@param args? table
function diri.actions.terminal_insert_path(args) end

---Pick a folder, recent ones first, and cd the shell there
---@param args? table
function diri.actions.terminal_go_to_directory(args) end

---Jump to the preceding sent Agent message or marked shell prompt
---@param args? table
function diri.actions.terminal_previous_prompt(args) end

---Jump to the following sent Agent message or marked shell prompt
---@param args? table
function diri.actions.terminal_next_prompt(args) end

---Erase this terminal's history and screen, keeping the line you are typing on
---@param args? table
function diri.actions.terminal_clear_scrollback(args) end

---Blank a garbled terminal and restore its default modes; the program keeps running
---@param args? table
function diri.actions.terminal_reset(args) end

---Show the tab at this position in the sidebar: 1 is the first, -1 the last.
---@param args {index: integer}
function diri.actions.select_tab(args) end

---Select a session's tab in the window, without bringing Diri to the front.
---@param args {session_id: string}
function diri.actions.show_session(args) end

---Use this terminal color theme and stop following the system appearance.
---@param args {theme: string}
function diri.actions.set_theme(args) end

