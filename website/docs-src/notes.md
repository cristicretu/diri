---
title: Notes
description: Write plans and to-dos in diri Notes, hand any to-do to an agent with the note as context, follow its progress live, and let agents write back.
---
Notes are plain Markdown documents that live next to your agents. Write a plan, break it into to-dos, and send any to-do to an agent. The agent gets the note as context, its progress shows under the to-do, and it can write its findings back into the note.

## Create a note
- Press <kbd>⌥</kbd><kbd>⌘</kbd><kbd>N</kbd>, or run **New Note** from the command palette.
- Or choose **Note** in the sidebar's new-session menu.

A note is a session. It sits in the sidebar with your agents and terminals, and it sorts, pins, archives and nests the same way. An agent started from a note appears as the note's child. The first line is the title; a note with no title shows as "Untitled".

## Close or delete a note
Closing a note's tab (<kbd>⌘</kbd><kbd>W</kbd> or the sidebar ✕) only hides it: the file stays, and [Search](#search) still finds it and reopens it. To delete a note, open it and run **Delete Note** from the command palette. Its tab closes and its file moves to `.trash/` (see [Where notes live](#where-notes-live)). <kbd>⇧</kbd><kbd>⌘</kbd><kbd>T</kbd> brings back a note you just closed or deleted.

## Write
Type <kbd>/</kbd> for the block menu or <kbd>@</kbd> to mention something. Markdown shortcuts also work as you type.

| Block | Type this | Or turn into |
| --- | --- | --- |
| Heading 1, 2, 3 | `# `, `## `, `### ` | <kbd>⌥</kbd><kbd>⌘</kbd><kbd>1</kbd> … <kbd>3</kbd> |
| Bulleted list | `- `, `* ` or `+ ` | <kbd>⇧</kbd><kbd>⌘</kbd><kbd>8</kbd> |
| Numbered list | `1. ` or `1) ` | <kbd>⇧</kbd><kbd>⌘</kbd><kbd>7</kbd> |
| To-do | `[] `, `[ ] ` or `[x] ` | <kbd>⇧</kbd><kbd>⌘</kbd><kbd>9</kbd> |
| Quote | `> ` | <kbd>⌥</kbd><kbd>⌘</kbd><kbd>Q</kbd> |
| Code | three backticks | <kbd>⌥</kbd><kbd>⌘</kbd><kbd>C</kbd> |
| Divider | `---` | |

The `/` menu adds **Callout**, **Table** and **Image**. Callouts are stored as GitHub alerts such as `> [!NOTE]`. Tables are GFM tables; <kbd>⌃</kbd><kbd>↵</kbd> opens the table menu for inserting rows and columns, alignment and deleting. Pasted or dropped images are saved next to the note, up to 25 MB each.

**Diagram** adds a [Mermaid](https://mermaid.js.org) diagram: a code block fenced ` ```mermaid `, drawn in place as flowcharts, sequence, class, state, ER, pie, Gantt, mindmap and git graphs. Click a diagram to edit its source, with the drawing updating below it as you type. Notes written by agents or pasted from elsewhere draw their ` ```mermaid ` blocks the same way.

Tick a to-do with <kbd>⌘</kbd><kbd>↵</kbd>. Move a block with <kbd>⌥</kbd><kbd>⇧</kbd><kbd>↑</kbd> / <kbd>↓</kbd>. The full list is in [Keyboard shortcuts](/docs/keyboard-shortcuts/#notes).

### Mentions and links
Type <kbd>@</kbd> to mention a diri session or another note. A mention is stored as an ordinary Markdown link, for example `[@Release plan](diri://note/<id>)`, and shown as a chip. Pasted links to tools such as GitHub pull requests, Linear, Figma, Notion, Google Docs and Slack show as chips too. diri does not fetch their contents.

## To-dos
Press <kbd>⌃</kbd><kbd>⌘</kbd><kbd>T</kbd>, or click **To-dos** in the sidebar, to see every open to-do across your notes. Ticking one there writes it back to its note.

## Start an agent from a to-do
1. Put the caret on an open to-do, or hover it, and click **Start** (<kbd>⌃</kbd><kbd>⌘</kbd><kbd>↵</kbd>).
2. Under **Start with**, choose an agent. The default is marked.
3. **What the agent gets** shows the exact brief before you send it.

The brief contains the to-do, the indented lines under it, the heading section it sits in, short excerpts of notes it mentions, the sessions it mentions with their status, and any earlier attempt's report. It is capped at 24 KB. The agent is told to report back and not to tick the to-do itself.

### Follow it live
The new session's chip is added to the to-do, and a status line appears under it:

| Status | Meaning |
| --- | --- |
| Starting… | The session is launching |
| Working on it | The agent is busy |
| Needs you | It asked a question; **Answer in session** opens it |
| Ready to review | It finished a turn or exited cleanly; shows an open PR if there is one |
| Stopped | The session ended; **Start again** tries once more |
| Couldn't start | The launch failed; **Try again** |

This status comes live from the Engine and is never written into the file. Each attempt adds one more chip, newest last. The agent's session shows the note's title above its own; click it to jump back to the to-do.

When you are happy with the work, tick the to-do. If the agent is still running, diri asks whether to **Tick and stop the agent** or **Tick, keep it running**.

## How agents write back
diri's MCP server gives agents tools for notes. Agents are asked to add short entries (decisions, findings, blockers, results, links) rather than progress chatter, to prefer adding over rewriting, and never to delete silently.

| Tool | What it does |
| --- | --- |
| `list_notes` | Find notes in this project, all projects, or ones that mention the agent |
| `read_note` | Read a note as Markdown, with to-dos, linked sessions and their live status |
| `write_note` | Add an entry, tick a to-do, link a session, or append Markdown |
| `edit_note` | Replace an exact piece of text |
| `replace_section` | Replace everything under one heading |
| `create_note` | Write a new note; it appears under the agent in the sidebar |
| `delete_note` | Move a note the agent created to the trash (restorable) |
| `start_from_note` | Start another agent from a note or one of its to-dos |
| `note_history` | List or read earlier versions (read-only) |

A report from a to-do's own agent lands as a bullet under that to-do. Anything else goes into an `## Updates` section, created if missing, with the agent's chip and a timestamp. An agent started from a note may only write to that note or to notes that mention it. An agent may delete only notes it or its sub-agents created, never yours.

### From the command line
The `dirijor note` command does the same from any shell:

```sh
dirijor note "Ship the beta"                    # quick note; first line is the title
dirijor note todo "Write release notes"         # add to this project's To-dos note
dirijor note list --all
dirijor note show "Release plan"
dirijor note check "Release plan" "Write release notes"
dirijor note append "Release plan" "Beta is out"
dirijor note history "Release plan"
dirijor note path                               # print the notes folder
```

Run `dirijor note help` for every subcommand and flag, including `add --pin --open`, `edit --old --new`, `replace-section` and `link`.

## Search
Press <kbd>⇧</kbd><kbd>⌘</kbd><kbd>F</kbd> for **Search notes**. It searches titles and the whole body, including to-dos, table cells and mention labels, across live, closed and archived notes. Every word you type must appear. Archived notes show dimmed, and opening one unarchives it. The command palette also shows up to three matching notes while you type.

## Version history
diri keeps earlier versions of every note. With a note open, run **Version History…** from the command palette. Pick a version to preview it, then **Restore This Version…**. The text you replace stays in history.

| When | Versions kept |
| --- | --- |
| While you type | At most one per minute |
| Before an agent, the CLI or another editor changes the note | Always |
| Last hour | All of them |
| Last day | One per 10 minutes |
| Last month | One per day |
| Older | One per week |

Each note keeps up to 200 versions or 20 MB. Only you can restore a version; agents can read history but not restore it.

## Edits from elsewhere
Agents, the CLI and other editors can change a note while you have it open. With no unsaved typing, the note simply reloads. Otherwise diri merges block by block: additions from both sides are kept, and if both changed the same block your text wins while an outside tick or chip is kept. The losing edit stays in version history.

## Where notes live
Each note is one Markdown file with a small front matter block.

| Platform | Folder |
| --- | --- |
| macOS | `~/Library/Application Support/Dirijor/notes` |
| Linux | `~/.local/share/diri/notes` |

You can edit the files with any editor; diri notices and records a version. Images live in `assets/`, history in `.history/`, and deleted notes in `.trash/` inside the same folder.
