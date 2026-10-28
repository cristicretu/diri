//! `dirijor completion fish|zsh|bash` prints a shell completion script.
//!
//! The scripts are generated from one table ([`COMMANDS`]), so the three
//! shells always offer the same commands, subcommands and flags. Session
//! targets are completed live through the hidden `dirijor __complete
//! sessions`, which asks the running Engine and prints nothing when it is
//! not running, so a completion never hangs or prints an error mid-line.

use std::fmt::Write as _;
use std::time::Duration;

use diri_proto::{Method, SessionListResult};
use serde_json::json;

use super::{CliError, request};

pub(super) const HELP: &str = "\
Shell completion:
  dirijor completion fish > ~/.config/fish/completions/dirijor.fish
  dirijor completion zsh  > \"${fpath[1]}/_dirijor\"
  dirijor completion bash > ~/.local/share/bash-completion/completions/dirijor";

/// One command or subcommand: its name, a short description, the flags it
/// takes, whether its first argument is a session, and its subcommands.
pub(super) struct Spec {
    pub name: &'static str,
    pub about: &'static str,
    pub flags: &'static [&'static str],
    pub takes_session: bool,
    pub subcommands: &'static [Spec],
}

const fn leaf(name: &'static str, about: &'static str, flags: &'static [&'static str]) -> Spec {
    Spec {
        name,
        about,
        flags,
        takes_session: false,
        subcommands: &[],
    }
}

const fn on_session(
    name: &'static str,
    about: &'static str,
    flags: &'static [&'static str],
) -> Spec {
    Spec {
        name,
        about,
        flags,
        takes_session: true,
        subcommands: &[],
    }
}

const fn group(name: &'static str, about: &'static str, subcommands: &'static [Spec]) -> Spec {
    Spec {
        name,
        about,
        flags: &[],
        takes_session: false,
        subcommands,
    }
}

pub(super) const COMMANDS: &[Spec] = &[
    leaf("status", "List every session with its status", &["--json"]),
    leaf("list", "List sessions", &["--all", "--json"]),
    leaf(
        "new",
        "Start a session",
        &["--kind", "--cwd", "--name", "--prompt", "--host", "--json"],
    ),
    on_session("send", "Type text or keys into a session", &["-l"]),
    on_session(
        "capture",
        "Print a session's screen",
        &["--lines", "--scrollback", "--json"],
    ),
    leaf(
        "run",
        "Run one command in a new session",
        &["--wait", "--timeout", "--rm", "--cwd", "--name", "--json"],
    ),
    leaf("activity", "Recent activity", &["--limit", "--json"]),
    group(
        "session",
        "Inspect and drive sessions",
        &[
            leaf("list", "List sessions", &["--all", "--status", "--json"]),
            on_session("get", "Show one session", &["--json"]),
            on_session("process", "Show a session's processes", &["--json"]),
            on_session(
                "terminal-title",
                "Read the terminal's own title",
                &["--json"],
            ),
            on_session("reset-terminal", "Reset the terminal emulator", &[]),
            on_session("read", "Read a session's screen", &["--json"]),
            on_session("send", "Send text to a session", &["--json"]),
            on_session("key", "Send keys to a session", &["--json"]),
            on_session(
                "wait",
                "Wait for a session state",
                &["--until", "--timeout", "--json"],
            ),
            leaf(
                "spawn",
                "Start a session",
                &["--kind", "--cwd", "--host", "--title", "--json"],
            ),
            leaf(
                "run",
                "Run one command in a session",
                &["--cwd", "--host", "--title", "--keep-open", "--json"],
            ),
            on_session("fork", "Fork a session", &["--json"]),
            on_session("reconnect", "Reconnect a remote session", &["--json"]),
            on_session("release", "Release a session", &["--remove", "--json"]),
            on_session("archive", "Archive a session", &["--json"]),
        ],
    ),
    group(
        "worktree",
        "Manage worktrees",
        &[
            leaf("list", "List worktrees", &["--json"]),
            leaf("create", "Create a worktree", &["--json"]),
            leaf("remove", "Remove a worktree", &["--json"]),
        ],
    ),
    group(
        "workspace",
        "Edit workspaces",
        &[
            leaf("list", "List workspaces", &[]),
            leaf("create", "Create a workspace", &["--revision"]),
            leaf("rename", "Rename a workspace", &["--revision"]),
            leaf("remove", "Remove a workspace", &["--revision"]),
            leaf("move", "Move a workspace", &["--revision"]),
            leaf("apply", "Apply a mutation from stdin", &["--revision"]),
        ],
    ),
    group(
        "tab",
        "Edit tabs",
        &[
            leaf("create", "Create a tab", &["--revision"]),
            leaf("rename", "Rename a tab", &["--revision"]),
            leaf("remove", "Remove a tab", &["--revision"]),
            leaf("move", "Move a tab", &["--revision"]),
            leaf("select", "Select a tab", &["--revision"]),
        ],
    ),
    group(
        "pane",
        "Edit panes",
        &[
            leaf("split", "Split a pane", &["--revision"]),
            leaf("remove", "Remove a pane", &["--revision"]),
            leaf("move", "Move a pane", &["--revision"]),
            leaf("move-group", "Move a split", &["--revision"]),
            leaf("swap", "Swap two panes", &["--revision"]),
            leaf("resize", "Resize a split", &["--revision"]),
            leaf("focus", "Focus a pane", &["--revision"]),
            leaf("zoom", "Zoom a pane", &["--revision"]),
        ],
    ),
    on_session("artifacts", "A session's artifacts", &["--json"]),
    group(
        "events",
        "Follow Engine events",
        &[
            leaf("subscribe", "Print events as they happen", &[]),
            leaf("wait", "Wait for one event", &[]),
        ],
    ),
    leaf("ports", "Ports sessions listen on", &["--json"]),
    group(
        "note",
        "Write and read notes",
        &[
            leaf("add", "Write a note", &[]),
            leaf("list", "List notes", &[]),
            leaf("show", "Print a note", &[]),
            leaf("edit", "Edit a note", &[]),
            leaf("append", "Append to a note", &[]),
            leaf("replace-section", "Replace a note section", &[]),
            leaf("todo", "Add a to-do", &[]),
            leaf("check", "Tick a to-do", &[]),
            leaf("link", "Link two notes", &[]),
            leaf("history", "A note's versions", &[]),
            leaf("restore", "Restore a version", &[]),
            leaf("path", "Where notes are stored", &[]),
        ],
    ),
    leaf("doctor", "Check the Diri setup", &[]),
    group(
        "completion",
        "Print a shell completion script",
        &[
            leaf("fish", "fish completion", &[]),
            leaf("zsh", "zsh completion", &[]),
            leaf("bash", "bash completion", &[]),
        ],
    ),
    leaf("help", "Show help", &[]),
    leaf("hook", "Report an Agent hook event (used by Agents)", &[]),
    leaf("notify", "Show a notification", &["--title", "--body"]),
    leaf("mcp-tools", "List the MCP tools", &[]),
    leaf(
        "mcp-call",
        "Call an MCP tool with JSON on stdin",
        &["--tool"],
    ),
    leaf("mcp-stdio", "Serve MCP over stdio", &[]),
];

pub(super) fn run(arguments: &[String]) -> Result<(), CliError> {
    let script = match arguments.first().map(String::as_str) {
        Some("fish") => fish(),
        Some("zsh") => zsh(),
        Some("bash") => bash(),
        Some("--help" | "-h") | None => {
            println!("{HELP}");
            return Ok(());
        }
        Some(other) => {
            return Err(CliError::failure(format!(
                "unknown shell: {other} (fish, zsh or bash)"
            )));
        }
    };
    print!("{script}");
    Ok(())
}

/// `dirijor __complete sessions`: one `id<TAB>title` line per live session.
/// Silent and quick on any failure: it runs on every Tab press.
pub(super) fn complete(arguments: &[String]) -> Result<(), CliError> {
    if arguments.first().map(String::as_str) != Some("sessions") {
        return Ok(());
    }
    let Ok(value) = request(Method::SESSION_LIST, json!({}), Duration::from_secs(1)) else {
        return Ok(());
    };
    let Ok(list) = serde_json::from_value::<SessionListResult>(value) else {
        return Ok(());
    };
    let mut out = String::new();
    for record in list.sessions.iter().filter(|record| !record.is_archived()) {
        let title: String = record
            .title
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect();
        let _ = writeln!(out, "{}\t{}", record.id.0, title.trim());
    }
    print!("{out}");
    Ok(())
}

#[cfg(test)]
fn find<'a>(specs: &'a [Spec], name: &str) -> Option<&'a Spec> {
    specs.iter().find(|spec| spec.name == name)
}

/// Words a shell can quote safely inside single quotes.
fn quoted(text: &str) -> String {
    text.replace('\'', "'\\''")
}

pub(super) fn fish() -> String {
    let mut out = String::from(
        "# dirijor completion for fish. Generated by `dirijor completion fish`.\n\
         complete -c dirijor -f\n\
         function __dirijor_sessions\n    dirijor __complete sessions 2>/dev/null\nend\n",
    );
    let top: Vec<&str> = COMMANDS.iter().map(|spec| spec.name).collect();
    let top = top.join(" ");
    for spec in COMMANDS {
        let _ = writeln!(
            out,
            "complete -c dirijor -n 'not __fish_seen_subcommand_from {top}' -a {} -d '{}'",
            spec.name,
            quoted(spec.about)
        );
        let names: Vec<&str> = spec.subcommands.iter().map(|sub| sub.name).collect();
        let names = names.join(" ");
        for sub in spec.subcommands {
            let _ = writeln!(
                out,
                "complete -c dirijor -n '__fish_seen_subcommand_from {} && not __fish_seen_subcommand_from {names}' -a {} -d '{}'",
                spec.name,
                sub.name,
                quoted(sub.about)
            );
            fish_leaf(
                &mut out,
                &format!(
                    "__fish_seen_subcommand_from {} && __fish_seen_subcommand_from {}",
                    spec.name, sub.name
                ),
                sub,
            );
        }
        if spec.subcommands.is_empty() {
            fish_leaf(
                &mut out,
                &format!("__fish_seen_subcommand_from {}", spec.name),
                spec,
            );
        }
    }
    out
}

fn fish_leaf(out: &mut String, condition: &str, spec: &Spec) {
    for flag in spec.flags {
        let (kind, name) = match flag.strip_prefix("--") {
            Some(long) => ("-l", long),
            None => ("-s", flag.trim_start_matches('-')),
        };
        let _ = writeln!(out, "complete -c dirijor -n '{condition}' {kind} {name}");
    }
    if spec.takes_session {
        let _ = writeln!(
            out,
            "complete -c dirijor -n '{condition}' -a '(__dirijor_sessions)'"
        );
    }
    if spec.flags.contains(&"--cwd") {
        let _ = writeln!(out, "complete -c dirijor -n '{condition}' -l cwd -r -F");
    }
}

pub(super) fn zsh() -> String {
    let mut out = String::from(
        "#compdef dirijor\n\
         # dirijor completion for zsh. Generated by `dirijor completion zsh`.\n\n\
         _dirijor_sessions() {\n  \
           local -a sessions\n  \
           sessions=(${(f)\"$(dirijor __complete sessions 2>/dev/null | sed 's/:/\\\\:/g; s/\\t/:/')\"})\n  \
           _describe -t sessions 'session' sessions\n\
         }\n\n\
         _dirijor() {\n  \
           local -a commands\n",
    );
    out.push_str("  commands=(\n");
    for spec in COMMANDS {
        let _ = writeln!(out, "    '{}:{}'", spec.name, quoted(spec.about));
    }
    out.push_str("  )\n");
    out.push_str(
        "  if (( CURRENT == 2 )); then\n    _describe -t commands 'dirijor command' commands\n    return\n  fi\n",
    );
    out.push_str("  case $words[2] in\n");
    for spec in COMMANDS {
        let _ = writeln!(out, "    ({})", spec.name);
        if spec.subcommands.is_empty() {
            zsh_leaf(&mut out, spec, "      ");
        } else {
            out.push_str("      if (( CURRENT == 3 )); then\n        local -a subcommands\n        subcommands=(\n");
            for sub in spec.subcommands {
                let _ = writeln!(out, "          '{}:{}'", sub.name, quoted(sub.about));
            }
            let _ = writeln!(
                out,
                "        )\n        _describe -t subcommands '{} command' subcommands\n        return\n      fi\n      case $words[3] in",
                spec.name
            );
            for sub in spec.subcommands {
                let _ = writeln!(out, "        ({})", sub.name);
                zsh_leaf(&mut out, sub, "          ");
                out.push_str("          ;;\n");
            }
            out.push_str("      esac\n");
        }
        out.push_str("      ;;\n");
    }
    out.push_str("  esac\n}\n\n_dirijor \"$@\"\n");
    out
}

fn zsh_leaf(out: &mut String, spec: &Spec, indent: &str) {
    if spec.takes_session {
        let _ = writeln!(
            out,
            "{indent}if [[ $words[CURRENT] != -* ]]; then _dirijor_sessions; fi"
        );
    }
    if !spec.flags.is_empty() {
        let _ = writeln!(
            out,
            "{indent}if [[ $words[CURRENT] == -* ]]; then compadd -- {}; fi",
            spec.flags.join(" ")
        );
    }
}

pub(super) fn bash() -> String {
    let mut out = String::from(
        "# dirijor completion for bash. Generated by `dirijor completion bash`.\n\
         _dirijor() {\n  \
           local cur=${COMP_WORDS[COMP_CWORD]}\n  \
           local words=\"\"\n  \
           local sessions=0\n",
    );
    let top: Vec<&str> = COMMANDS.iter().map(|spec| spec.name).collect();
    let _ = writeln!(
        out,
        "  if (( COMP_CWORD == 1 )); then\n    COMPREPLY=($(compgen -W \"{}\" -- \"$cur\"))\n    return\n  fi",
        top.join(" ")
    );
    out.push_str("  case \"${COMP_WORDS[1]}\" in\n");
    for spec in COMMANDS {
        let _ = writeln!(out, "    {})", spec.name);
        if spec.subcommands.is_empty() {
            bash_leaf(&mut out, spec, "      ");
        } else {
            let names: Vec<&str> = spec.subcommands.iter().map(|sub| sub.name).collect();
            let _ = writeln!(
                out,
                "      if (( COMP_CWORD == 2 )); then\n        words=\"{}\"\n      else\n        case \"${{COMP_WORDS[2]}}\" in",
                names.join(" ")
            );
            for sub in spec.subcommands {
                let _ = writeln!(out, "          {})", sub.name);
                bash_leaf(&mut out, sub, "            ");
                out.push_str("            ;;\n");
            }
            out.push_str("        esac\n      fi\n");
        }
        out.push_str("      ;;\n");
    }
    out.push_str(
        "  esac\n  \
         if [[ $sessions == 1 && $cur != -* ]]; then\n    \
           words=\"$words $(dirijor __complete sessions 2>/dev/null | cut -f1)\"\n  \
         fi\n  \
         COMPREPLY=($(compgen -W \"$words\" -- \"$cur\"))\n\
         }\n\
         complete -o default -F _dirijor dirijor\n",
    );
    out
}

fn bash_leaf(out: &mut String, spec: &Spec, indent: &str) {
    let _ = writeln!(out, "{indent}words=\"{}\"", spec.flags.join(" "));
    if spec.takes_session {
        let _ = writeln!(out, "{indent}sessions=1");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command the help text documents can be completed.
    #[test]
    fn the_table_covers_every_documented_command() {
        let help = format!(
            "{}\n{}\n{}",
            super::super::HELP_USAGE,
            super::super::script::HELP,
            HELP
        );
        for line in help.lines() {
            // Usage lines start with the program name; prose mentioning it
            // ("when dirijor itself fails") does not.
            let mut words = line.split_whitespace();
            if words.next() != Some("dirijor") {
                continue;
            }
            let Some(command) = words.next() else {
                continue;
            };
            // Not part of the Rust Engine; it only explains that.
            if command == "forward"
                || !command
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch == '-')
            {
                continue;
            }
            let spec = find(COMMANDS, command)
                .unwrap_or_else(|| panic!("`dirijor {command}` has no completion"));
            if let Some(sub) = words.next()
                && !spec.subcommands.is_empty()
            {
                for sub in sub.trim_matches(|c| c == '<' || c == '>').split('|') {
                    if sub.chars().all(|ch| ch.is_ascii_lowercase() || ch == '-') {
                        assert!(
                            find(spec.subcommands, sub).is_some(),
                            "`dirijor {command} {sub}` has no completion"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn every_shell_script_names_every_command_and_subcommand() {
        for (shell, script) in [("fish", fish()), ("zsh", zsh()), ("bash", bash())] {
            for spec in COMMANDS {
                assert!(script.contains(spec.name), "{shell}: {}", spec.name);
                for sub in spec.subcommands {
                    assert!(
                        script.contains(sub.name),
                        "{shell}: {} {}",
                        spec.name,
                        sub.name
                    );
                }
            }
            assert!(script.contains("__complete sessions"), "{shell}: sessions");
        }
    }

    /// The generated scripts parse in the real shells when they are
    /// installed (CI has bash and zsh; fish is optional).
    #[test]
    fn scripts_parse_in_their_shells() {
        for (shell, flag, script) in [
            ("bash", "-n", bash()),
            ("zsh", "-n", zsh()),
            ("fish", "--no-execute", fish()),
        ] {
            let Ok(mut child) = std::process::Command::new(shell)
                .arg(flag)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
            else {
                continue;
            };
            {
                use std::io::Write as _;
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(script.as_bytes())
                    .unwrap();
            }
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{shell} rejected its script: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
