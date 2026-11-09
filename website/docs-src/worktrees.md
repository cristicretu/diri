---
title: Worktrees and review
description: Give each diri agent its own git worktree and branch, review its diff in the Review panel, commit, follow pull request checks, and clean up afterwards.
---
When several agents work in one repository, give each its own git worktree: a separate checkout on its own branch, so nothing one agent does touches another until you bring it back. diri then shows every change in one place to review.

## Give an agent its own worktree
Worktrees need a git repository. There are three ways to get one.
- **Ask the agent.** Agents in diri have tools to create worktrees and to start helpers in them. Say so in the task: "Use diri to do this in a new worktree so it does not touch my current files."
- **From the launcher.** A [recipe](/docs/scheduled-tasks/) can use a fresh worktree. Each run creates a new branch from the recipe's branch prefix plus a unique suffix. Fresh worktrees are local only.
- **From the command line.** `dirijor worktree list`, `create` and `remove` work from any shell.

Treat a worktree like any checkout. Its branch and commits belong to the repository, so commit or move changes before you delete one.

> [!NOTE]
> Worktrees separate files only. Agents still share your accounts, the network, and anything running on your machine.

## The Review panel
Press <kbd>⇧</kbd><kbd>⌘</kbd><kbd>D</kbd> to open the right panel and choose **Review**. It follows the selected session and lists every file changed in that session's checkout, with added and removed lines. A folder that is not a git repository shows "Not a Git repository".

### Choose what to compare
| Compare against | Shows |
| --- | --- |
| Default branch | Everything this session changed, committed or not |
| HEAD | Only changes that are not committed yet |

Choose the working or staged layer to act on individual hunks.

### Stage, discard and commit
| Action | What it does |
| --- | --- |
| Stage | Stage one file or hunk |
| Stage all | Stage every current change |
| Unstage | In the Staged layer, unstage a file or everything staged |
| Discard | Throw a hunk or your working changes away. Asks you to confirm, because it cannot be undone |
| Commit | Commit the staged changes with the message you type |

Changes on a remote host are view-only in the panel.

### Ask the agent to check itself
**Ask active agent** sends a ready-made request to the session's agent:

| Button | Request |
| --- | --- |
| Review | Review this for correctness, regressions, and missing tests |
| Find risks | Find the highest-risk behavior changes and explain why they matter |
| Suggest tests | Identify missing tests and propose concrete cases for this context |

You can also type your own follow-up.

## Pull requests and checks
When a session has a pull request, the session header links to it with its check status: passed, failed or still running. The panel shows checks, review state and comments, with a link to the conversation on GitHub. For an open pull request, **Merge pull request** opens GitHub so you can review and confirm the merge there. Next to it, the panel says "Ready to merge" when nothing blocks it.

To open file links in your editor, set **Settings → Appearance → Open file links in**.

## Bring work back
When helpers worked in separate worktrees, review each one in the Review panel and merge them one at a time. An agent that started helpers can do this itself with diri's `integrate` tool: it brings a helper's committed branch into the agent's own checkout as a merge, squash, or cherry-pick. Both checkouts must have no uncommitted tracked changes. On conflict nothing changes, and the conflicting paths are reported back. It works for local sessions in the same project only.

Try asking the lead agent: "Use diri to give each sub-task its own agent in a separate worktree, review their changes, and merge only the ones whose tests pass." See [Let agents work as a team](/guides/agent-teams/).

## Hand work to another session
Select the source session and press <kbd>⌃</kbd><kbd>⌘</kbd><kbd>D</kbd>, select the target, and press it again.

A handoff sheet opens with the generated context. You can edit it, remote targets carry a Remote badge, and nothing is sent until you click **Send handoff** or press Return. Esc cancels.

## Clean up
Open **Settings → Worktrees** to see the linked worktrees of your local projects, with their age and pull request state (Open, Merged or Closed). Filter by **All**, **Ready to clean** or **Older than 30 days**. Each worktree shows what keeps it from being cleaned up:

| Label | Meaning |
| --- | --- |
| Main checkout | The repository's own checkout |
| Default branch | It is on the repository's default branch |
| Session in use or status unknown | A diri session still runs in it |
| Local changes | It has uncommitted work |
| Open pull request | Its pull request is still open |
| Merge not verified | diri could not confirm its branch was merged |
| Ready to clean | Nothing stands in the way |

**Clean up…** asks first, then **Remove worktree** deletes the checkout and its ignored files, including build output. The git branch and its commits are kept. **Measure cleanup size** shows how much space you would get back. <kbd>⌥</kbd><kbd>⌘</kbd><kbd>W</kbd> opens the worktrees overview.
