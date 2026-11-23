//! The composer's `/` menu: each Agent's own slash commands, plus the
//! user's custom ones (Claude `~/.claude/commands`, `<project>/.claude/commands`,
//! Codex `~/.codex/prompts`). Choosing one types it into the composer; the
//! CLI still runs it, so a picker it opens shows in the PTY portal.

use std::path::Path;

use diri_proto::AgentKind;

use crate::i18n::t;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashCommand {
    /// With its leading `/`.
    pub name: String,
    pub description: String,
    /// Commands that take an argument keep the menu's choice open for one.
    pub takes_args: bool,
}

/// Built-in commands as of Claude Code 2.1 and Codex 0.16x, most used first.
const CLAUDE: &[(&str, bool)] = &[
    ("model", true),
    ("compact", true),
    ("clear", false),
    ("context", false),
    ("cost", false),
    ("usage", false),
    ("status", false),
    ("review", true),
    ("init", false),
    ("memory", false),
    ("permissions", false),
    ("config", false),
    ("mcp", false),
    ("agents", false),
    ("hooks", false),
    ("add-dir", true),
    ("resume", false),
    ("rewind", false),
    ("export", false),
    ("fast", false),
    ("doctor", false),
    ("login", false),
    ("logout", false),
    ("help", false),
];

const CODEX: &[(&str, bool)] = &[
    ("model", false),
    ("approvals", false),
    ("review", true),
    ("diff", false),
    ("status", false),
    ("compact", false),
    ("new", false),
    ("init", false),
    ("mention", true),
    ("mcp", false),
    ("resume", false),
    ("logout", false),
    ("quit", false),
];

/// Everything `kind` accepts after `/`, built-ins first.
pub fn catalog(kind: &AgentKind, cwd: &Path, home: &Path) -> Vec<SlashCommand> {
    let (builtins, custom) = if kind == &AgentKind::CODEX {
        (CODEX, custom_codex(home))
    } else {
        (CLAUDE, custom_claude(cwd, home))
    };
    let mut commands: Vec<SlashCommand> = builtins
        .iter()
        .map(|(name, takes_args)| SlashCommand {
            name: format!("/{name}"),
            description: t(description_key(name)).to_owned(),
            takes_args: *takes_args,
        })
        .collect();
    for command in custom {
        if !commands.iter().any(|existing| existing.name == command.name) {
            commands.push(command);
        }
    }
    commands
}

fn description_key(name: &str) -> &'static str {
    match name {
        "model" => "thread_lens.slash.model",
        "compact" => "thread_lens.slash.compact",
        "clear" => "thread_lens.slash.clear",
        "context" => "thread_lens.slash.context",
        "cost" => "thread_lens.slash.cost",
        "usage" => "thread_lens.slash.usage",
        "status" => "thread_lens.slash.status",
        "review" => "thread_lens.slash.review",
        "init" => "thread_lens.slash.init",
        "memory" => "thread_lens.slash.memory",
        "permissions" => "thread_lens.slash.permissions",
        "config" => "thread_lens.slash.config",
        "mcp" => "thread_lens.slash.mcp",
        "agents" => "thread_lens.slash.agents",
        "hooks" => "thread_lens.slash.hooks",
        "add-dir" => "thread_lens.slash.add_dir",
        "resume" => "thread_lens.slash.resume",
        "rewind" => "thread_lens.slash.rewind",
        "export" => "thread_lens.slash.export",
        "fast" => "thread_lens.slash.fast",
        "doctor" => "thread_lens.slash.doctor",
        "login" => "thread_lens.slash.login",
        "logout" => "thread_lens.slash.logout",
        "help" => "thread_lens.slash.help",
        "approvals" => "thread_lens.slash.approvals",
        "diff" => "thread_lens.slash.diff",
        "new" => "thread_lens.slash.new",
        "mention" => "thread_lens.slash.mention",
        "quit" => "thread_lens.slash.quit",
        _ => "thread_lens.slash.custom",
    }
}

/// Commands whose name starts with `query` (after its `/`), then those that
/// merely contain it; at most `limit`.
pub fn matching<'a>(commands: &'a [SlashCommand], query: &str, limit: usize) -> Vec<&'a SlashCommand> {
    let query = query.trim_start_matches('/').to_ascii_lowercase();
    let name = |command: &SlashCommand| command.name[1..].to_ascii_lowercase();
    let mut out: Vec<&SlashCommand> = commands.iter().filter(|c| name(c).starts_with(&query)).collect();
    out.extend(
        commands
            .iter()
            .filter(|c| !name(c).starts_with(&query) && name(c).contains(&query)),
    );
    out.truncate(limit);
    out
}

/// The `/query` being typed: the composer holds only a slash word so far.
pub fn query(text: &str) -> Option<&str> {
    let text = text.strip_prefix('/')?;
    (!text.contains(char::is_whitespace)).then_some(text)
}

fn custom_claude(cwd: &Path, home: &Path) -> Vec<SlashCommand> {
    let mut out = Vec::new();
    for dir in [cwd.join(".claude/commands"), home.join(".claude/commands")] {
        collect_markdown(&dir, "", &mut out, 0);
    }
    out
}

fn custom_codex(home: &Path) -> Vec<SlashCommand> {
    let mut out = Vec::new();
    collect_markdown(&home.join(".codex/prompts"), "prompts:", &mut out, 1);
    out
}

/// `name.md` files become `/prefix name`; one level of folders namespaces
/// them (`/frontend:component`), as Claude does.
fn collect_markdown(dir: &Path, prefix: &str, out: &mut Vec<SlashCommand>, depth: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries.into_iter().take(200) {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if path.is_dir() {
            if depth == 0 {
                collect_markdown(&path, &format!("{prefix}{stem}:"), out, 1);
            }
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        out.push(SlashCommand {
            name: format!("/{prefix}{stem}"),
            description: front_matter_description(&path).unwrap_or_default(),
            takes_args: true,
        });
    }
}

fn front_matter_description(path: &Path) -> Option<String> {
    use std::io::Read as _;
    let mut head = String::new();
    std::fs::File::open(path).ok()?.take(4096).read_to_string(&mut head).ok()?;
    let body = head.strip_prefix("---")?;
    body.lines()
        .take_while(|line| line.trim() != "---")
        .find_map(|line| line.strip_prefix("description:"))
        .map(|value| value.trim().trim_matches('"').to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_lists_builtins_then_custom_commands_with_descriptions() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude/commands/git")).unwrap();
        std::fs::write(
            home.path().join(".claude/commands/git/ship.md"),
            "---\ndescription: \"Commit, push and open a PR\"\n---\nShip $ARGUMENTS",
        )
        .unwrap();
        std::fs::create_dir_all(project.path().join(".claude/commands")).unwrap();
        std::fs::write(project.path().join(".claude/commands/bench.md"), "Run benches").unwrap();
        let commands = catalog(&AgentKind::CLAUDE_CODE, project.path(), home.path());
        assert_eq!(commands[0].name, "/model");
        let ship = commands.iter().find(|c| c.name == "/git:ship").unwrap();
        assert_eq!(ship.description, "Commit, push and open a PR");
        assert!(commands.iter().any(|c| c.name == "/bench"));
        let found: Vec<&str> = matching(&commands, "/co", 8).iter().map(|c| c.name.as_str()).collect();
        assert_eq!(&found[..4], ["/compact", "/context", "/cost", "/config"]);
    }

    #[test]
    fn codex_has_its_own_commands_and_prompts() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".codex/prompts")).unwrap();
        std::fs::write(home.path().join(".codex/prompts/triage.md"), "x").unwrap();
        let commands = catalog(&AgentKind::CODEX, home.path(), home.path());
        assert!(commands.iter().any(|c| c.name == "/approvals"));
        assert!(commands.iter().any(|c| c.name == "/prompts:triage"));
        assert!(!commands.iter().any(|c| c.name == "/cost"));
    }

    #[test]
    fn the_menu_opens_only_on_a_lone_slash_word() {
        assert_eq!(query("/mo"), Some("mo"));
        assert_eq!(query("/"), Some(""));
        assert_eq!(query("/model opus"), None);
        assert_eq!(query("fix /model"), None);
    }
}
