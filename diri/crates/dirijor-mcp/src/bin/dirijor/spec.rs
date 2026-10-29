//! The CLI's command tree as data. `dirijor help COMMAND`, `dirijor COMMAND
//! --help`, the fish/zsh/bash completions and the website cheat sheet are all
//! generated from it, so a flag added here shows up in every one of them.

/// What a positional argument completes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Complete {
    Nothing,
    /// Live session ids, with their titles as descriptions.
    Target,
    /// Files and folders.
    Path,
    /// Shell names for `completion`.
    Shell,
}

#[derive(Debug)]
pub(super) struct Flag {
    pub(super) long: &'static str,
    pub(super) short: Option<char>,
    /// Placeholder for the flag's value; `None` for a switch.
    pub(super) value: Option<&'static str>,
    pub(super) choices: &'static [&'static str],
    pub(super) help: &'static str,
}

#[derive(Debug)]
pub(super) struct Command {
    pub(super) name: &'static str,
    pub(super) aliases: &'static [&'static str],
    /// Positional usage, for example `TARGET KEY...`.
    pub(super) args: &'static str,
    pub(super) summary: &'static str,
    pub(super) complete: Complete,
    pub(super) flags: &'static [Flag],
    pub(super) commands: &'static [Command],
    /// Called by agents and hooks, not people: left out of completions.
    pub(super) internal: bool,
}

impl Command {
    pub(super) fn find(&self, name: &str) -> Option<&Command> {
        self.commands
            .iter()
            .find(|command| command.name == name || command.aliases.contains(&name))
    }
}

const fn flag(long: &'static str, help: &'static str) -> Flag {
    Flag {
        long,
        short: None,
        value: None,
        choices: &[],
        help,
    }
}

const fn option(long: &'static str, value: &'static str, help: &'static str) -> Flag {
    Flag {
        long,
        short: None,
        value: Some(value),
        choices: &[],
        help,
    }
}

const fn choice(
    long: &'static str,
    value: &'static str,
    choices: &'static [&'static str],
    help: &'static str,
) -> Flag {
    Flag {
        long,
        short: None,
        value: Some(value),
        choices,
        help,
    }
}

const fn short(mut flag: Flag, short: char) -> Flag {
    flag.short = Some(short);
    flag
}

const fn cmd(
    name: &'static str,
    args: &'static str,
    summary: &'static str,
    complete: Complete,
    flags: &'static [Flag],
) -> Command {
    Command {
        name,
        aliases: &[],
        args,
        summary,
        complete,
        flags,
        commands: &[],
        internal: false,
    }
}

const fn group(name: &'static str, summary: &'static str, commands: &'static [Command]) -> Command {
    Command {
        name,
        aliases: &[],
        args: "",
        summary,
        complete: Complete::Nothing,
        flags: &[],
        commands,
        internal: false,
    }
}

const fn alias(mut command: Command, aliases: &'static [&'static str]) -> Command {
    command.aliases = aliases;
    command
}

const fn internal(mut command: Command) -> Command {
    command.internal = true;
    command
}

pub(super) const EDGES: &[&str] = &["right", "left", "up", "down"];
pub(super) const SHELLS: &[&str] = &["fish", "zsh", "bash"];
pub(super) const KEY_NAMES: &[&str] = &[
    "enter",
    "tab",
    "escape",
    "backspace",
    "space",
    "up",
    "down",
    "left",
    "right",
    "home",
    "end",
    "pageup",
    "pagedown",
    "insert",
    "delete",
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "f9",
    "f10",
    "f11",
    "f12",
    "ctrl+c",
    "ctrl+d",
    "ctrl+z",
    "ctrl+l",
    "ctrl+r",
    "shift+tab",
];
const STATUSES: &[&str] = &[
    "done",
    "idle",
    "working",
    "needsInput",
    "blocked",
    "exited",
    "starting",
    "unknown",
];

const JSON: Flag = flag("--json", "Print JSON");
const CWD: Flag = short(
    option("--cwd", "PATH", "Working directory (default: here)"),
    'c',
);
const LABEL: Flag = option("--label", "TEXT", "Session title (--name, --title)");
const HOST: Flag = option("--host", "ID", "Run on a remote host");
const FOCUS: Flag = choice(
    "--focus",
    "BOOL",
    &["true", "false"],
    "Show the new session in the app (default false)",
);
const RATIO: Flag = option("--ratio", "PERCENT", "Share of the split, 10 to 90");
const KEEP_OPEN: Flag = flag("--keep-open", "Keep the tab after the command exits");
const REVISION: Flag = option("--revision", "N", "Fail if the layout changed");

pub(super) const ROOT: Command = Command {
    name: "dirijor",
    aliases: &[],
    args: "",
    summary: "Diri automation CLI",
    complete: Complete::Nothing,
    flags: &[],
    commands: &[
        alias(
            cmd(
                "list",
                "",
                "List sessions",
                Complete::Nothing,
                &[flag("--all", "Include archived sessions"), JSON],
            ),
            &["ls"],
        ),
        cmd(
            "status",
            "",
            "List every session, archived ones included",
            Complete::Nothing,
            &[JSON],
        ),
        cmd(
            "new",
            "",
            "Open a shell (or --kind agent) tab and print its id",
            Complete::Nothing,
            &[
                option("--kind", "KIND", "Agent kind (default shell)"),
                CWD,
                LABEL,
                option("--prompt", "TEXT", "Initial prompt for an agent"),
                HOST,
                JSON,
            ],
        ),
        cmd(
            "run",
            "-- PROGRAM [ARG...]",
            "Run a command in a new tab, or a split with --split",
            Complete::Nothing,
            &[
                flag("--wait", "Wait, print its output, exit with its status"),
                option("--timeout", "DURATION", "Stop waiting after 30s, 2m, ..."),
                flag("--rm", "Remove the session after --wait"),
                CWD,
                LABEL,
                HOST,
                KEEP_OPEN,
                choice("--split", "EDGE", EDGES, "Split beside a pane instead"),
                option("--beside", "TARGET", "Pane to split (default: yours)"),
                RATIO,
                FOCUS,
                JSON,
            ],
        ),
        cmd(
            "split",
            "[TARGET] [-- PROGRAM [ARG...]]",
            "Open a shell or command beside a session's pane",
            Complete::Target,
            &[
                choice(
                    "--edge",
                    "EDGE",
                    EDGES,
                    "Side of the new pane (default right)",
                ),
                RATIO,
                CWD,
                LABEL,
                KEEP_OPEN,
                FOCUS,
                JSON,
            ],
        ),
        cmd(
            "send",
            "TARGET [--] TOKEN...",
            "Type text and tmux-style keys (Enter, C-c)",
            Complete::Target,
            &[
                short(flag("--literal", "Type every token as text"), 'l'),
                JSON,
            ],
        ),
        cmd(
            "send-key",
            "TARGET KEY...",
            "Press keys: enter, ctrl+c, shift+tab, alt+left, f5",
            Complete::Target,
            &[option("--repeat", "N", "Press each key N times"), JSON],
        ),
        cmd(
            "capture",
            "TARGET",
            "Print the screen or scrollback as text",
            Complete::Target,
            &[
                option("--lines", "N", "Keep the last N rows"),
                flag("--scrollback", "Include history"),
                JSON,
            ],
        ),
        cmd(
            "wait",
            "TARGET",
            "Wait until a session exits (or reaches --until)",
            Complete::Target,
            &[
                option("--for", "DURATION", "Give up after 30s, 2m, 1h (exit 2)"),
                choice(
                    "--until",
                    "STATUS",
                    STATUSES,
                    "Status to wait for (repeatable)",
                ),
                JSON,
            ],
        ),
        cmd(
            "kill",
            "TARGET",
            "Stop a session and close it",
            Complete::Target,
            &[flag("--keep", "Keep the stopped session's record"), JSON],
        ),
        cmd(
            "focus",
            "TARGET",
            "Show a session in the app and focus its pane",
            Complete::Target,
            &[JSON],
        ),
        cmd(
            "zoom",
            "TARGET",
            "Zoom a session's pane; toggles without --on/--off",
            Complete::Target,
            &[
                flag("--on", "Zoom"),
                flag("--off", "Restore the split"),
                JSON,
            ],
        ),
        cmd(
            "move",
            "TARGET EDGE ANCHOR",
            "Move a session's pane beside another session's",
            Complete::Target,
            &[RATIO, JSON],
        ),
        cmd(
            "swap",
            "TARGET OTHER",
            "Swap two sessions' panes",
            Complete::Target,
            &[JSON],
        ),
        cmd(
            "resize",
            "TARGET",
            "Set a pane's share of its split",
            Complete::Target,
            &[RATIO, JSON],
        ),
        cmd(
            "whoami",
            "",
            "This terminal's session, tab and pane",
            Complete::Nothing,
            &[JSON],
        ),
        cmd(
            "version",
            "",
            "CLI and Engine versions",
            Complete::Nothing,
            &[JSON],
        ),
        cmd(
            "completion",
            "SHELL",
            "Print a fish, zsh or bash completion script",
            Complete::Shell,
            &[],
        ),
        cmd(
            "activity",
            "",
            "Recent activity",
            Complete::Nothing,
            &[option("--limit", "N", "1 to 300, default 50"), JSON],
        ),
        group(
            "session",
            "List, read, drive, start and stop sessions",
            &[
                alias(
                    cmd(
                        "list",
                        "",
                        "List sessions",
                        Complete::Nothing,
                        &[
                            flag("--all", "Include archived sessions"),
                            option("--status", "PREFIX", "Filter by status"),
                            JSON,
                        ],
                    ),
                    &["ls"],
                ),
                cmd("get", "TARGET", "Show a session", Complete::Target, &[JSON]),
                cmd(
                    "inspect",
                    "TARGET",
                    "A session with its tabs, panes and process (JSON)",
                    Complete::Target,
                    &[JSON],
                ),
                cmd(
                    "rename",
                    "TARGET TITLE",
                    "Rename a session",
                    Complete::Target,
                    &[
                        flag("--for-user", "An agent passing on the user's name"),
                        JSON,
                    ],
                ),
                cmd(
                    "close",
                    "TARGET",
                    "Stop a session and remove it (same as kill)",
                    Complete::Target,
                    &[JSON],
                ),
                cmd(
                    "read",
                    "TARGET",
                    "Print the screen or scrollback",
                    Complete::Target,
                    &[
                        choice(
                            "--source",
                            "SOURCE",
                            &["screen", "scrollback"],
                            "What to read",
                        ),
                        option("--lines", "N", "Keep the last N lines"),
                        JSON,
                    ],
                ),
                cmd(
                    "send",
                    "TARGET [TEXT...]",
                    "Type text and press Return",
                    Complete::Target,
                    &[flag("--no-submit", "Do not press Return"), JSON],
                ),
                cmd(
                    "key",
                    "ID KEY",
                    "Press one key",
                    Complete::Target,
                    &[
                        flag("--ctrl", "Control"),
                        flag("--alt", "Alt"),
                        flag("--shift", "Shift"),
                        flag("--cmd", "Command"),
                        flag("--repeat", "Key repeat"),
                        JSON,
                    ],
                ),
                cmd(
                    "wait",
                    "TARGET",
                    "Wait for a status (default done)",
                    Complete::Target,
                    &[
                        choice("--until", "STATUS", STATUSES, "Status (repeatable)"),
                        option("--timeout", "SECONDS", "Default 600"),
                        JSON,
                    ],
                ),
                cmd(
                    "spawn",
                    "KIND",
                    "Start an agent",
                    Complete::Nothing,
                    &[
                        option("--cwd", "PATH", "Working directory"),
                        flag("--worktree", "Create a git worktree"),
                        option("--branch", "NAME", "Worktree branch"),
                        option("--prompt", "TEXT", "Initial prompt"),
                        option("--title", "TEXT", "Session title"),
                        HOST,
                        JSON,
                    ],
                ),
                cmd(
                    "run",
                    "-- PROGRAM [ARG...]",
                    "Run a command in a new session",
                    Complete::Nothing,
                    &[CWD, HOST, LABEL, KEEP_OPEN, JSON],
                ),
                cmd(
                    "fork",
                    "TARGET",
                    "Fork the conversation",
                    Complete::Target,
                    &[JSON],
                ),
                cmd(
                    "release",
                    "TARGET",
                    "Stop a session",
                    Complete::Target,
                    &[flag("--remove", "Also delete its record"), JSON],
                ),
                cmd(
                    "archive",
                    "TARGET",
                    "Archive a session",
                    Complete::Target,
                    &[flag("--undo", "Unarchive"), JSON],
                ),
                cmd(
                    "reconnect",
                    "ID",
                    "Reconnect a remote session",
                    Complete::Target,
                    &[JSON],
                ),
                cmd(
                    "process",
                    "ID",
                    "Process id, group, executable, folder",
                    Complete::Target,
                    &[JSON],
                ),
                cmd(
                    "terminal-title",
                    "ID",
                    "The title the program set",
                    Complete::Target,
                    &[JSON],
                ),
                cmd(
                    "reset-terminal",
                    "ID",
                    "Reset the emulator, not the process",
                    Complete::Target,
                    &[],
                ),
            ],
        ),
        group(
            "worktree",
            "List, create and remove git worktrees",
            &[
                cmd("list", "[REPO]", "List worktrees", Complete::Path, &[JSON]),
                cmd(
                    "create",
                    "[REPO]",
                    "Create a worktree",
                    Complete::Path,
                    &[
                        option("--branch", "NAME", "Branch"),
                        option("--base", "REF", "Starting ref"),
                        JSON,
                    ],
                ),
                cmd(
                    "remove",
                    "REPO PATH",
                    "Remove a worktree",
                    Complete::Path,
                    &[flag("--force", "Remove a dirty worktree"), JSON],
                ),
            ],
        ),
        group(
            "workspace",
            "Workspaces in the window layout",
            &[
                cmd(
                    "list",
                    "",
                    "Print the layout with ids",
                    Complete::Nothing,
                    &[],
                ),
                cmd(
                    "create",
                    "NAME",
                    "Add a workspace",
                    Complete::Nothing,
                    &[REVISION],
                ),
                cmd(
                    "rename",
                    "ID NAME",
                    "Rename",
                    Complete::Nothing,
                    &[REVISION],
                ),
                cmd("remove", "ID", "Remove", Complete::Nothing, &[REVISION]),
                cmd(
                    "move",
                    "ID INDEX",
                    "Reorder",
                    Complete::Nothing,
                    &[REVISION],
                ),
                cmd(
                    "apply",
                    "",
                    "Apply a mutation from stdin",
                    Complete::Nothing,
                    &[],
                ),
            ],
        ),
        group(
            "tab",
            "Tabs in the window layout",
            &[
                alias(
                    cmd("list", "", "One row per tab", Complete::Nothing, &[JSON]),
                    &["ls"],
                ),
                cmd(
                    "create",
                    "WORKSPACE SESSION",
                    "Open a session as a tab",
                    Complete::Nothing,
                    &[REVISION],
                ),
                cmd(
                    "rename",
                    "TAB TITLE",
                    "Rename",
                    Complete::Nothing,
                    &[REVISION],
                ),
                cmd(
                    "remove",
                    "TAB",
                    "Close the tab",
                    Complete::Nothing,
                    &[REVISION],
                ),
                cmd(
                    "move",
                    "TAB WORKSPACE INDEX",
                    "Move",
                    Complete::Nothing,
                    &[REVISION],
                ),
                cmd(
                    "select",
                    "WORKSPACE TAB",
                    "Select",
                    Complete::Nothing,
                    &[REVISION],
                ),
            ],
        ),
        group(
            "pane",
            "Split panes by tab and pane id",
            &[
                alias(
                    cmd(
                        "list",
                        "[TARGET]",
                        "One row per pane",
                        Complete::Target,
                        &[JSON],
                    ),
                    &["ls"],
                ),
                cmd(
                    "split",
                    "TAB PANE SESSION EDGE",
                    "Split",
                    Complete::Nothing,
                    &[REVISION],
                ),
                cmd(
                    "remove",
                    "TAB PANE",
                    "Close",
                    Complete::Nothing,
                    &[REVISION],
                ),
                cmd(
                    "move",
                    "SOURCE_TAB PANE DEST_TAB TARGET_PANE EDGE",
                    "Move",
                    Complete::Nothing,
                    &[REVISION],
                ),
                cmd(
                    "move-group",
                    "SOURCE_TAB SPLIT DEST_TAB TARGET_PANE EDGE",
                    "Move a split",
                    Complete::Nothing,
                    &[REVISION],
                ),
                cmd(
                    "swap",
                    "TAB PANE TAB PANE",
                    "Swap",
                    Complete::Nothing,
                    &[REVISION],
                ),
                cmd(
                    "resize",
                    "TAB SPLIT FRACTION",
                    "Resize",
                    Complete::Nothing,
                    &[REVISION],
                ),
                cmd("focus", "TAB PANE", "Focus", Complete::Nothing, &[REVISION]),
                cmd(
                    "zoom",
                    "TAB PANE|none",
                    "Zoom",
                    Complete::Nothing,
                    &[REVISION],
                ),
            ],
        ),
        group(
            "events",
            "Stream or wait for Engine events",
            &[
                cmd(
                    "subscribe",
                    "",
                    "Print events as they happen (the default)",
                    Complete::Nothing,
                    &[
                        short(option("--session", "TARGET", "Only this session"), 's'),
                        flag("--all", "Every session (the default)"),
                        option("--kind", "NAME", "Only this event (repeatable)"),
                        option("--since-seq", "N", "Replay from a sequence"),
                        option("--count", "N", "Stop after N events"),
                        option("--timeout", "SECONDS", "Stop after (default a day)"),
                        JSON,
                    ],
                ),
                cmd(
                    "wait",
                    "",
                    "Wait for one status or event",
                    Complete::Nothing,
                    &[
                        short(option("--session", "TARGET", "Session"), 's'),
                        choice("--until", "STATUS", STATUSES, "Status"),
                        option("--kind", "NAME", "Event"),
                        option("--timeout", "SECONDS", "Default 600"),
                        JSON,
                    ],
                ),
            ],
        ),
        cmd(
            "artifacts",
            "TARGET",
            "Links, pull requests and ports of a session",
            Complete::Target,
            &[JSON],
        ),
        cmd(
            "ports",
            "",
            "Listening ports across sessions",
            Complete::Nothing,
            &[JSON],
        ),
        alias(
            group(
                "note",
                "Write and edit notes (dirijor note --help)",
                &[
                    cmd(
                        "add",
                        "[TEXT]",
                        "Write a note",
                        Complete::Nothing,
                        &[
                            option("--title", "TEXT", "Title"),
                            option("--project", "FOLDER", "Project"),
                            flag("--inbox", "Inbox"),
                            flag("--pin", "Pin"),
                            flag("--open", "Show it"),
                        ],
                    ),
                    cmd(
                        "append",
                        "NOTE TEXT",
                        "Append Markdown",
                        Complete::Nothing,
                        &[],
                    ),
                    cmd(
                        "todo",
                        "TEXT",
                        "Add a to-do",
                        Complete::Nothing,
                        &[
                            option("--to", "NOTE", "Note"),
                            option("--project", "FOLDER", "Project"),
                            flag("--inbox", "Inbox"),
                        ],
                    ),
                    cmd(
                        "list",
                        "",
                        "List notes",
                        Complete::Nothing,
                        &[
                            option("--project", "FOLDER", "Project"),
                            option("--mentions", "SESSION", "Mentioning a session"),
                            flag("--all", "All"),
                            JSON,
                        ],
                    ),
                    cmd(
                        "check",
                        "NOTE TODO",
                        "Tick a to-do",
                        Complete::Nothing,
                        &[flag("--undo", "Untick")],
                    ),
                    cmd(
                        "link",
                        "NOTE TODO SESSION",
                        "Link a session",
                        Complete::Nothing,
                        &[],
                    ),
                    cmd("show", "NOTE", "Print a note", Complete::Nothing, &[]),
                    cmd(
                        "edit",
                        "NOTE",
                        "Replace text",
                        Complete::Nothing,
                        &[
                            option("--old", "TEXT", "Text to replace"),
                            option("--new", "TEXT", "Replacement"),
                            flag("--all", "Every match"),
                        ],
                    ),
                    cmd(
                        "replace-section",
                        "NOTE HEADING",
                        "Replace a section from stdin",
                        Complete::Nothing,
                        &[],
                    ),
                    cmd(
                        "history",
                        "NOTE [VERSION]",
                        "Earlier versions",
                        Complete::Nothing,
                        &[],
                    ),
                    cmd(
                        "restore",
                        "NOTE VERSION",
                        "Restore a version",
                        Complete::Nothing,
                        &[],
                    ),
                    cmd("path", "", "Print the notes folder", Complete::Nothing, &[]),
                ],
            ),
            &["notes"],
        ),
        cmd(
            "notify",
            "",
            "Post a notification from a terminal",
            Complete::Nothing,
            &[
                option("--title", "TEXT", "Title"),
                option("--body", "TEXT", "Body"),
            ],
        ),
        cmd(
            "doctor",
            "",
            "Check the Engine, agents and state file",
            Complete::Nothing,
            &[],
        ),
        cmd(
            "mcp-tools",
            "",
            "Print the MCP tool definitions",
            Complete::Nothing,
            &[],
        ),
        cmd(
            "mcp-call",
            "",
            "Call an MCP tool with JSON from stdin",
            Complete::Nothing,
            &[option("--tool", "NAME", "Tool")],
        ),
        cmd(
            "script",
            "[FILE]",
            "Run a Lua script against Diri, a line of Lua, or a prompt",
            Complete::Path,
            &[
                short(
                    option("--eval", "CODE", "Run this Lua instead of a file"),
                    'e',
                ),
                option(
                    "--for",
                    "SECONDS",
                    "Stop a script that listens after this long",
                ),
            ],
        ),
        cmd("help", "[COMMAND]", "Print usage", Complete::Nothing, &[]),
        internal(cmd("hook", "EVENT", "Agent hook", Complete::Nothing, &[])),
        internal(cmd("mcp-stdio", "", "MCP server", Complete::Nothing, &[])),
    ],
    internal: false,
};

/// The command a word path names, for example `["session", "rename"]`.
pub(super) fn find(path: &[&str]) -> Option<&'static Command> {
    let mut command = &ROOT;
    for word in path {
        command = command.find(word)?;
    }
    Some(command)
}

fn flag_usage(flag: &Flag) -> String {
    let mut text = String::new();
    if let Some(short) = flag.short {
        text.push_str(&format!("-{short}, "));
    }
    text.push_str(flag.long);
    if flag.long == "--focus" {
        // A bare `--focus` means true.
        text.push_str("[=BOOL]");
    } else if let Some(value) = flag.value {
        text.push(' ');
        text.push_str(value);
    }
    text
}

/// `dirijor COMMAND --help`.
pub(super) fn help(path: &[&str], command: &Command) -> String {
    let name = std::iter::once("dirijor")
        .chain(path.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = format!("{}\n\nUsage: {name}", command.summary);
    if !command.commands.is_empty() {
        out.push_str(" COMMAND");
    }
    if !command.flags.is_empty() {
        out.push_str(" [OPTIONS]");
    }
    if !command.args.is_empty() {
        out.push(' ');
        out.push_str(command.args);
    }
    out.push('\n');
    if !command.aliases.is_empty() {
        out.push_str(&format!("Alias: {}\n", command.aliases.join(", ")));
    }
    let width = command
        .commands
        .iter()
        .map(|sub| sub.name.len() + 1 + sub.args.len())
        .chain(command.flags.iter().map(|flag| flag_usage(flag).len()))
        .max()
        .unwrap_or(0);
    if !command.commands.is_empty() {
        out.push_str("\nCommands:\n");
        for sub in command.commands.iter().filter(|sub| !sub.internal) {
            let head = format!("{} {}", sub.name, sub.args);
            out.push_str(&format!("  {head:<width$}  {}\n", sub.summary));
        }
    }
    if !command.flags.is_empty() {
        out.push_str("\nOptions:\n");
        for flag in command.flags {
            let mut help = flag.help.to_owned();
            if !flag.choices.is_empty() {
                help.push_str(&format!(" [{}]", flag.choices.join(", ")));
            }
            out.push_str(&format!("  {:<width$}  {help}\n", flag_usage(flag)));
        }
    }
    out
}

/// The website's cheat sheet: one row per command people type.
pub(super) fn cheat_sheet() -> String {
    fn rows(prefix: &str, command: &Command, out: &mut String) {
        for sub in command.commands.iter().filter(|sub| !sub.internal) {
            let name = if prefix.is_empty() {
                sub.name.to_owned()
            } else {
                format!("{prefix} {}", sub.name)
            };
            if sub.commands.is_empty() || !sub.flags.is_empty() {
                let usage = if sub.args.is_empty() {
                    name.clone()
                } else {
                    format!("{name} {}", sub.args)
                };
                let flags = sub
                    .flags
                    .iter()
                    .map(|flag| format!("`{}`", flag_usage(flag)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let summary = if sub.aliases.is_empty() {
                    sub.summary.to_owned()
                } else {
                    format!("{} (alias `{}`)", sub.summary, sub.aliases.join("`, `"))
                };
                out.push_str(&format!(
                    "| `{}` | {} | {} |\n",
                    usage.replace('|', "\\|"),
                    flags.replace('|', "\\|"),
                    summary.replace('|', "\\|")
                ));
            }
            if name != "note" {
                rows(&name, sub, out);
            }
        }
    }
    let mut out = String::from("| Command | Options | What it does |\n| --- | --- | --- |\n");
    rows("", &ROOT, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_flags_are_unique_and_well_formed() {
        fn walk(command: &Command, path: &str) {
            let mut seen = std::collections::HashSet::new();
            for sub in command.commands {
                for name in std::iter::once(&sub.name).chain(sub.aliases) {
                    assert!(seen.insert(*name), "{path} {name} is listed twice");
                    assert!(
                        name.chars().all(|ch| ch.is_ascii_lowercase() || ch == '-'),
                        "{name}"
                    );
                }
                let mut flags = std::collections::HashSet::new();
                for flag in sub.flags {
                    assert!(flag.long.starts_with("--"), "{}", flag.long);
                    assert!(flags.insert(flag.long), "{path} {} {}", sub.name, flag.long);
                    assert!(!flag.help.is_empty());
                }
                walk(sub, &format!("{path} {}", sub.name));
            }
        }
        walk(&ROOT, "dirijor");
        assert!(find(&["session", "ls"]).is_some());
        assert!(find(&["notes", "add"]).is_some());
        assert!(find(&["nothing"]).is_none());
    }

    #[test]
    fn help_lists_options_with_choices() {
        let wait = find(&["wait"]).unwrap();
        let text = help(&["wait"], wait);
        assert!(text.starts_with("Wait until a session exits"));
        assert!(text.contains("Usage: dirijor wait [OPTIONS] TARGET"));
        assert!(text.contains("--for DURATION"));
        assert!(text.contains("[done, idle, working"));
        let session = help(&["session"], find(&["session"]).unwrap());
        assert!(session.contains("rename TARGET TITLE"));
    }
}
