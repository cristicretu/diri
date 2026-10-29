<!-- Generated from the Engine's method catalog and the app's action catalog. Do not edit: run `DIRI_UPDATE_DOCS=1 cargo test -p diri-app scripting_reference` -->

# Diri Lua API reference

The design and the core functions are in [scripting.md](scripting.md). This page lists what is generated: the events `diri.on` accepts, every Engine method a script can call, and every window action.

## Events

| Event | Payload |
| --- | --- |
| `session.created` | A session appeared: {session, kind}. |
| `session.status` | A session's status changed: {session, kind, from, to}. |
| `session.exited` | A session's process ended: {session, kind, code, signal, reason}. |
| `session.title` | A session's title changed: {session, kind, from, to}. |
| `session.removed` | A session was closed: {id, reason}. |
| `session.cwd` | A shell's live working directory changed: {session, kind, from, to}. |
| `session.program` | A shell's foreground program changed (to is nil back at the prompt): {session, kind, from, to}. |
| `session.bell` | A program rang the terminal bell: {session_id}. |
| `session.notification` | A program posted a notification: {session_id, title, body}. |
| `workspace.updated` | Workspaces, tabs or splits changed: the new snapshot. |
| `schedule.updated` | A schedule changed or ran: {id}. |
| `focus` | The window's selected session changed (in the app only): {session}. |

## Engine methods

Call one as `diri.call("session.list")` or through its generated function, `diri.session.list()`. Parameters are checked against the method's JSON Schema before they are sent; `dirijor api describe NAME` prints the schema and an example. Required parameters come first; `?` marks an optional one.

| Function | Parameters | Returns | |
| --- | --- | --- | --- |
| `diri.workspace.snapshot` |  | `WorkspaceSnapshot` | Workspaces, their tabs and each tab's split layout. |
| `diri.workspace.mutate` | `expectedRevision, mutation` | `WorkspaceSnapshot` | Change workspaces, tabs or splits at an expected revision; a stale revision is rejected. |
| `diri.session.spawn_tracked` | `senderID, operationID, spawn` | `SessionRecord` | Start a session on behalf of a sender, deduplicated by operation id. |
| `diri.task.submit` | `caller_id, request_id, session_id, text, result_schema?` | `TaskRecord` | Assign a tracked task to an Agent. |
| `diri.task.get` | `caller_id, task_id?, request_id?` | `TaskRecord` | Read one task by id or request id. |
| `diri.task.report` | `caller_id, task_id, status, result?` | `TaskRecord` | Acknowledge or report a task you were assigned. |
| `diri.task.answer` | `caller_id, task_id, text` | `TaskRecord` | Answer a task you submitted. |
| `diri.task.cancel` | `caller_id, task_id, reason?` | `TaskRecord` | Withdraw a task you submitted. |
| `diri.task.list` | `caller_id, role?, include_terminal?, limit?` | `TaskListResult` | Tasks you sent or were assigned. |
| `diri.schedule.create` | `title, when, spawn, catchUpWindowMs?, keepAwake?, wakeMac?, enabled?` | `ScheduleRecord` | Schedule Agent runs owned by Diri. |
| `diri.schedule.update` | `id, title, when, spawn, catchUpWindowMs?, keepAwake?, wakeMac?, enabled?` | `ScheduleRecord` | Change a schedule. |
| `diri.schedule.delete` | `id` | `{}` | Delete a schedule. |
| `diri.schedule.list` |  | `ScheduleListResult` | Every schedule with its next due time. |
| `diri.schedule.run_now` | `id` | `ScheduleRecord` | Start one run now without moving the schedule. |
| `diri.session.spawn` | `kind, cwd, accountProfileId?, newWorktree?, worktreeBranch?, worktreeBase?, title?, initialPrompt?, parent?, initialCols?, initialRows?, host?, sameRepoAs?, startDirectory?, noteId?, appearance?, argv?, keepOpen?` | `SessionRecord` | Start an Agent or shell session. |
| `diri.session.list` |  | `SessionListResult` | Every session record and project. |
| `diri.session.kill` | `sessionID` | `{}` | Stop a session's process; the record stays. |
| `diri.session.remove` | `sessionID` | `{}` | Stop a session and delete its record. |
| `diri.session.rename` | `sessionID, title, by?` | `SessionRecord` | Rename a session. |
| `diri.session.process_info` | `sessionID` | `SessionProcessInfo` | Foreground program, working directory and process facts. |
| `diri.session.reconnect` | `sessionID` | `SessionRecord` | Reconnect a remote session to its Holder. |
| `diri.session.resume` | `sessionID` | `SessionRecord` | Resume an exited session's conversation. |
| `diri.session.fork` | `sessionID, parent?` | `SessionRecord` | Fork a session's conversation into a new session. |
| `diri.session.deliver_message` | `sessionID, senderID, messageID, text, submit` | `DeliverMessageResult` | Deliver a message to an Agent with at-most-once identity. |
| `diri.session.send_key` | `sessionID, key, modifiers?, action?` | `{}` | Press one named key in a session's terminal. |
| `diri.session.send_text` | `sessionID, text, submit` | `{}` | Type text into a session's terminal, optionally pressing Return. |
| `diri.session.resize` | `sessionID, cols, rows` | `{}` | Resize a session's terminal. |
| `diri.session.read_screen` | `sessionID` | `ReadScreenResult` | The visible screen as text. |
| `diri.session.terminal_title` | `sessionID` | `TerminalTitleResult` | The terminal's current OSC title. |
| `diri.session.reset_terminal` | `sessionID` | `{}` | Reset the emulator without touching the process. |
| `diri.session.clear_scrollback` | `sessionID` | `{}` | Drop the scrollback and clear the screen; the process keeps running. |
| `diri.session.list_dir` | `sessionID, path?, includeFiles?, prefix?, limit?` | `SessionListDirResult` | List a directory as the session sees it, local or remote, relative to its working directory. |
| `diri.session.capture_find` | `sessionID` | `CaptureFindResult` | Screen plus history as text, for find. |
| `diri.session.find_history` | `sessionID, query, maxMatches, budgetMs, beforeRow?, cols?` | `FindHistoryResult` | One bounded page of a search over a session's whole history, by stable row. |
| `diri.session.read_scrollback` | `sessionID` | `ReadScrollbackResult` | The terminal history as text. |
| `diri.session.read_scrollback_cells` | `sessionID, firstRow, maxRows, styles?` | `ScrollbackCells` | A window of history rows with styles. |
| `diri.session.read_diff` | `sessionID, base?` | `SessionDiff` | The git diff of a session's working tree. |
| `diri.session.read_image` | `sessionID, key, offset?` | `InlineImageChunk` | One chunk of an inline image the terminal placed. |
| `diri.session.read_transcript` | `sessionID, turns?` | `ReadTranscriptResult` | The latest conversation turns of an Agent. |
| `diri.session.mark_seen` | `sessionID` | `{}` | Mark a session's finished turn as seen. |
| `diri.session.mark_unread` | `sessionID` | `{}` | Return a finished turn to unseen. |
| `diri.session.hibernate` | `sessionID` | `SessionRecord` | Put an idle Agent to sleep, freeing its memory. |
| `diri.session.wake` | `sessionID` | `SessionRecord` | Wake a sleeping Agent. |
| `diri.session.archive` | `sessionID` | `{}` | Stop a session and move it to the archive. |
| `diri.session.unarchive` | `sessionID` | `{}` | Restore an archived session's record. |
| `diri.session.reopen_last` |  | `SessionRecord` | Reopen the most recently closed session. |
| `diri.session.reveal` | `sessionID` | `{}` | Ask the app to show a session without taking focus. |
| `diri.session.open_api_request` | `sessionID, request` | `{}` | Open a prefilled HTTP request in a session's API tab. |
| `diri.computer.act` | `sessionID, tool, args?` | `Value` | One Cua Driver step under the caller's own cursor. |
| `diri.computer.status` | `probe?` | `ComputerStatus` | Whether computer use is installed and allowed. |
| `diri.computer.setup` | `action` | `ComputerStatus` | Install or restart computer use. |
| `diri.session.migrate` | `sessionID, targetHost?` | `SessionRecord` | Move a Claude session between this Mac and a host. |
| `diri.session.reparent_worktree` | `sessionID, projectRoot, worktreePath` | `SessionRecord` | Record that a session now works in another worktree. |
| `diri.host.sync_prefs` | `host` | `PrefsSyncReport` | Push Agent preferences to a remote host. |
| `diri.host.locate_repo` | `host?, originURL?, sessionID?` | `HostLocateRepoResult` | Find a session's repository on a host. |
| `diri.host.initialize` | `host, forceReinstall?` | `HostInitializeResult` | Install or verify the remote Helper on a host. |
| `diri.host.usage` | `host` | `Value` | Agent usage on a host. |
| `diri.host.list_directories` | `path, host?, mode?` | `HostListDirectoriesResult` | One level of folders on a host. |
| `diri.host.list` |  | `HostsConfig` | Configured hosts. |
| `diri.session.history` |  | `SessionHistoryResult` | Past Agent conversations that can be resumed. |
| `diri.activity.list` | `limit?` | `ActivityListResult` | Recent session activity, newest first. |
| `diri.session.resume_from_history` | `entry, initialPrompt?` | `SessionRecord` | Open a session resuming a past conversation. |
| `diri.worktree.create` | `repoPath, branch?, base?` | `WorktreeInfo` | Create a git worktree. |
| `diri.worktree.list` | `repoPath` | `WorktreeListResult` | A repository's worktrees. |
| `diri.worktree.cleanup` | `repoPath, worktreePath, expectedHead` | `WorktreeCleanupResult` | Remove finished worktrees. |
| `diri.worktree.remove` | `repoPath, worktreePath, force` | `{}` | Remove one worktree. |
| `diri.worktree.scan` | `refresh?, measureDisk?, generation?, cursor?` | `WorktreeScanPage` | Scan for worktrees, a page at a time. |
| `diri.worktree.overview` |  | `WorktreeOverviewResult` | Every worktree Diri knows with its sessions. |
| `diri.worktree.integrate` | `sourceSessionID, targetSessionID, strategy?, message?` | `WorktreeIntegrateResult` | Merge one session's branch into another's. |
| `diri.project.add` | `root` | `Project` | Add a project folder. |
| `diri.governor.configure` | `idleThresholdSeconds, hardMemoryBytes` | `{}` | Idle and memory limits for sleeping Agents. |
| `diri.terminal.configure` | `shellIntegration, palette?, shellCommand?` | `{}` | Terminal settings the Engine applies. |
| `diri.agent.readiness` | `host?, forceRefresh?` | `AgentReadinessResult` | Installed Agents and their descriptors. |
| `diri.agent.configure` | `kind, showInQuickCreate, host?, executablePath?, launchArgs?` | `AgentReadinessResult` | Paths, arguments and visibility of an Agent. |
| `diri.account.profiles.list` |  | `AgentAccountCatalog` | Saved Agent accounts (no secrets). |
| `diri.account.profiles.save` | `id, label, agent, configHome, host?, isDefault?, loginStore?` | `AgentAccountCatalog` | Save an account profile. |
| `diri.account.profiles.remove` | `id` | `AgentAccountCatalog` | Remove an account profile. |
| `diri.account.codex.login` | `id` | `AgentAccountLogin` | Start a Codex sign-in for a profile. |
| `diri.account.codex.capture` | `id` | `AgentAccountCatalog` | Save the signed-in Codex account. |
| `diri.account.claude.login` | `id` | `AgentAccountLogin` | Start a Claude sign-in for a profile. |
| `diri.account.claude.capture` | `id` | `AgentAccountCatalog` | Save the signed-in Claude account. |
| `diri.account.switch_all` | `accountProfileId` | `SwitchAccountResult` | Switch every open tab of an Agent to a saved account. |
| `diri.account.overview` |  | `AgentAccountOverview` | Accounts in use and limits. |
| `diri.account.adopt` | `agent` | `AgentAccountCatalog` | Save the Agent's current login as a profile. |
| `diri.account.add` | `agent` | `AgentAccountLogin` | Add another account for an Agent. |
| `diri.session.continue_with_account` | `sessionID, accountProfileId` | `SessionRecord` | Continue a conversation under another saved account. |
| `diri.voice.state` |  | `VoiceStateResult` | Whether Talk with your agent is on. |
| `diri.voice.speak` | `sessionID, text` | `{}` | An Agent's spoken summary of its turn. |
| `diri.voice.spoken` | `sessionID, turn` | `SpokenResult` | Collect a session's spoken summary. |
| `diri.voice.latest_spoken` | `sessionID` | `LatestSpokenResult` | Read the latest spoken summary without collecting it. |
| `diri.voice.phone_reader` | `readAloud?` | `PhoneReaderResult` | Get or set whether a phone wants spoken summaries. |
| `diri.test.run` |  | `Value` | Run a browser test step in the shared pool. |
| `diri.browser.act` |  | `Value` | One step of the shared browser. |
| `diri.state.snapshot` |  | `SessionListResult` | Alias of session.list. |
| `diri.telemetry.upload_now` |  | `TelemetryUploadNowResult` | Upload diagnostics now. |
| `diri.app.actions.list` |  | `AppActionsListResult` | The connected app's actions with argument schemas. |
| `diri.app.perform` | `action, args?, actor?, timeoutMs?` | `AppPerformResult` | Run one app action in the window and wait for its answer. |
| `diri.api.list` |  | `ApiListResult` | Every method and app action, one line each. |
| `diri.api.describe` | `name` | `ApiDescribeResult` | One method's or action's schema and example. |
| `diri.api.llms` |  | `ApiTextResult` | The whole API as compact text for an Agent. |

Not callable from scripts:

- `hello`: the connection says hello itself.
- `events.subscribe`: listen with diri.on instead.
- `events.wait`: listen with diri.on instead.
- `app.actions.publish`: only the window publishes its actions.
- `app.reply`: only the window answers its actions.
- `client.set_active`: only the window reports its focus.
- `hook.report`: only Agents' hooks report.
- `daemon.prepare_shutdown`: stopping the Engine ends every session.
- `daemon.shutdown`: stopping the Engine ends every session.
- `daemon.shutdown_if_idle`: stopping the Engine ends every session.

## Window actions

Run one with `diri.action("split-pane-right")` or `diri.actions.split_pane_right()`, bind it with `diri.keymap.set("cmd-shift-d", "split-pane-right")`. Actions need the Diri app to be running. Destructive ones close, quit, archive or clear something.

| Action | Arguments | Default key | |
| --- | --- | --- | --- |
| `quit` |  | ⌘Q | Close Diri and leave no windows open **Destructive.** |
| `new-window` |  | ⇧⌘N | Open another window for the current workspace |
| `close-window` |  | ⇧⌘W | Close the current Diri window **Destructive.** |
| `close-session` | `session_id?` | ⌘W | Close the selected session **Destructive.** |
| `reopen-session` |  | ⇧⌘T | Restore the most recently closed session |
| `open-launcher` |  | ⌘N | Open the new session picker |
| `new-default` |  | ⌘T | Start a session with the default agent |
| `new-terminal` |  | ⌥⌘T | Start a standalone shell session |
| `new-codex` |  | ⌥⇧⌘N | Start a new Codex session |
| `command-palette` |  | ⌘K | Search all available commands |
| `quick-open` |  | ⌘P | Find and open a project folder |
| `history` |  | ⇧⌘H | Open or close conversation history |
| `session-overview` |  | ⇧⌘O | Open or close the session overview |
| `tab-peek` |  | ⌃⇧Space | Preview sessions across projects without changing work |
| `focus-pane-left` |  | ⌥⌘← | Move focus to the nearest pane in this direction |
| `focus-pane-right` |  | ⌥⌘→ | Move focus to the nearest pane in this direction |
| `focus-pane-up` |  | ⌥⌘↑ | Move focus to the nearest pane in this direction |
| `focus-pane-down` |  | ⌥⌘↓ | Move focus to the nearest pane in this direction |
| `split-pane-right` |  | ⌘D | Open a terminal to the right of the focused pane |
| `split-pane-below` |  | ⇧⌘D | Open a terminal below the focused pane |
| `split-pane-left` |  |  | Open a terminal to the left of the focused pane |
| `split-pane-above` |  |  | Open a terminal above the focused pane |
| `split-pane-best-fit` |  | ⌘Return | Open a terminal beside the focused pane, splitting its longer side |
| `focus-previous-pane` |  | ⌘[ | Move focus to the previous pane of the tab |
| `focus-next-pane` |  | ⌘] | Move focus to the next pane of the tab |
| `equalize-panes` |  | ⌃⌘= | Give every pane of the tab an equal share |
| `toggle-pane-zoom` |  | ⇧⌘Return | Expand the focused pane or restore its saved layout |
| `remove-focused-pane` |  |  | Remove the pane reference while keeping its session running **Destructive.** |
| `pane-grow-width` |  | ⌥⇧⌘→ | Resize the nearest matching split divider by five percent |
| `pane-shrink-width` |  | ⌥⇧⌘← | Resize the nearest matching split divider by five percent |
| `pane-grow-height` |  | ⌥⇧⌘↓ | Resize the nearest matching split divider by five percent |
| `pane-shrink-height` |  | ⌥⇧⌘↑ | Resize the nearest matching split divider by five percent |
| `swap-pane-left` |  |  | Swap the focused pane with its nearest neighbor |
| `swap-pane-right` |  |  | Swap the focused pane with its nearest neighbor |
| `swap-pane-up` |  |  | Swap the focused pane with its nearest neighbor |
| `swap-pane-down` |  |  | Swap the focused pane with its nearest neighbor |
| `move-pane-left` |  |  | Move the focused pane with its nearest neighbor |
| `move-pane-right` |  |  | Move the focused pane with its nearest neighbor |
| `move-pane-up` |  |  | Move the focused pane with its nearest neighbor |
| `move-pane-down` |  |  | Move the focused pane with its nearest neighbor |
| `move-pane-to-new-tab` |  |  | Give the focused pane a tab of its own; its program keeps running |
| `move-pane-to-next-tab` |  |  | Move the focused pane into the next tab in the sidebar and follow it |
| `move-pane-to-previous-tab` |  |  | Move the focused pane into the previous tab in the sidebar and follow it |
| `move-tab-to-new-window` |  |  | Open the current tab in a new window; the program keeps running |
| `move-tab-to-next-window` |  |  | Show the current tab in the next window instead of this one |
| `new-note` |  | ⌥⌘N | Start a note in the current project, beside its agents |
| `todos` |  | ⌃⌘T | Every open to-do across your notes |
| `search-notes` |  | ⇧⌘F | Find any note, live or archived |
| `note-version-history` |  |  | See and restore earlier versions of the open note |
| `note-graph` |  |  | See how the open note links to your other notes |
| `worktrees` |  | ⌥⌘W | Open the Git worktrees overview |
| `settings` |  | ⌘, | Open or close Diri settings |
| `toggle-sidebar` |  | ⌘B | Show or hide the sidebar with vertical tabs, or the top bar with horizontal tabs |
| `toggle-tab-orientation` |  | ⇧⌘S | Place session tabs across the top or in the sidebar |
| `horizontal-tabs` |  |  | Place session tabs across the top or in the sidebar |
| `vertical-tabs` |  |  | Place session tabs across the top or in the sidebar |
| `comfortable-density` |  |  | Show panes as rounded islands, or edge to edge with hairline borders |
| `compact-density` |  |  | Show panes as rounded islands, or edge to edge with hairline borders |
| `focus-sidebar` |  | ⇧⌘B | Move keyboard focus to the sessions sidebar |
| `toggle-inspector` |  | ⌥⌘B | Show or hide the session inspector |
| `toggle-auxiliary-terminal` |  | ⌘J | Show or hide a terminal pane below the focused pane |
| `archive-selected-session` | `session_id?` | ⌥⇧⌘W | Archive the selected session **Destructive.** |
| `rename-selected-session` |  | ⌘R | Rename the selected session |
| `delegate-selected-session` |  | ⌃⌘D | Hand off work from the selected session |
| `select-next-attention-session` |  | ⇧⌘J | Jump to the next session waiting for you |
| `toggle-notifications` |  | ⇧⌘I | Open the unread notification inbox |
| `toggle-activity` |  | ⌘. | Show only the sessions that are working, waiting on you or finished |
| `check-for-updates` |  |  | Look for a newer version of Diri **Destructive.** |
| `whats-new` |  |  | Replay the highlights of recent releases |
| `toggle-read-aloud` |  | ⌥⌘S | Turn Talk with your agent on or off for every agent |
| `toggle-read-this-tab` |  |  | Read this tab, only this tab, every tab but this one, or every tab |
| `read-only-this-tab` |  |  | Read this tab, only this tab, every tab but this one, or every tab |
| `read-all-except-this-tab` |  |  | Read this tab, only this tab, every tab but this one, or every tab |
| `read-all-tabs` |  |  | Read this tab, only this tab, every tab but this one, or every tab |
| `toggle-perf-overlay` |  | ⌥⌘P | Show frame rate, frame times and memory over the window |
| `toggle-render-counters` |  | ⌥⌘R | Count how often each view renders |
| `reload-scripts` |  |  | Run ~/.config/diri/init.lua again, or start it if it is new |
| `select-previous-session` |  | ⇧⌘[ | Select the previous session in the sidebar |
| `select-next-session` |  | ⇧⌘] | Select the next session in the sidebar |
| `move-selected-session-up` |  | ⌃⌘↑ | Move the selected session up in the sidebar |
| `move-selected-session-down` |  | ⌃⌘↓ | Move the selected session down in the sidebar |
| `select-session-1` |  | ⌘1 | Select the first session |
| `select-session-2` |  | ⌘2 | Select the second session |
| `select-session-3` |  | ⌘3 | Select the third session |
| `select-session-4` |  | ⌘4 | Select the fourth session |
| `select-session-5` |  | ⌘5 | Select the fifth session |
| `select-session-6` |  | ⌘6 | Select the sixth session |
| `select-session-7` |  | ⌘7 | Select the seventh session |
| `select-session-8` |  | ⌘8 | Select the eighth session |
| `select-last-session` |  | ⌘9 | Select the last session |
| `terminal-find` |  | ⌘F | Search the active terminal output |
| `terminal-find-next` |  | ⌘G | Move to the next terminal search result |
| `terminal-find-previous` |  | ⇧⌘G | Move to the previous terminal search result |
| `terminal-zoom-in` |  | ⌘+ | Make terminal text larger |
| `terminal-zoom-out` |  | ⌘− | Make terminal text smaller |
| `terminal-reset-zoom` |  | ⌘0 | Restore the default terminal text size |
| `terminal-copy-mode` |  | ⌘⌥C | Select terminal output with the keyboard |
| `terminal-find-selection` |  | ⌘⌥F | Search for the selected terminal text |
| `terminal-insert-path` |  | ⌘E | Pick a file under the session's directory and type its path |
| `terminal-go-to-directory` |  | ⌥⌘E | Pick a folder, recent ones first, and cd the shell there |
| `terminal-previous-prompt` |  | ⌘⇧↑ | Jump to the preceding sent Agent message or marked shell prompt |
| `terminal-next-prompt` |  | ⌘⇧↓ | Jump to the following sent Agent message or marked shell prompt |
| `terminal-clear-scrollback` |  | ⇧⌘K | Erase this terminal's history and screen, keeping the line you are typing on **Destructive.** |
| `terminal-reset` |  |  | Blank a garbled terminal and restore its default modes; the program keeps running **Destructive.** |
| `select-tab` | `index` |  | Show the tab at this position in the sidebar: 1 is the first, -1 the last. |
| `show-session` | `session_id` |  | Select a session's tab in the window, without bringing Diri to the front. |
| `set-theme` | `theme` |  | Use this terminal color theme and stop following the system appearance. |
| `list-keys` |  |  | Answer every key binding: its keys, what it runs, and where it came from (default, recorded in Settings, or the script's file:line), with the script's key modes. |
