//! Command-palette actions, ranking, and filtering.
//!
//! The ordering and labels mirror `CommandPaletteView.swift`; UI code only
//! renders these specs and dispatches the associated command.

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;

use diri_proto::{AgentKind, AgentReadinessResult, HostEntry, Project, SessionRecord};

use crate::agent_catalog::{
    AgentOption, display_name, quick_agent_options, resolved_default_agent, resolved_target_agent,
    system_image, title_case_id,
};
use crate::commands::{self, CommandId};
use crate::fuzzy::{FuzzyMatcher, FuzzyQuery, PreparedText, Score};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteCommand {
    /// A static application command. The palette dispatches the same typed
    /// action used by key bindings, menus, and toolbar controls.
    Action(CommandId),
    SpawnAgent {
        agent: AgentKind,
        cwd: Option<PathBuf>,
        /// `HostEntry.id` — spawn on that remote host (cwd then comes from the
        /// host's defaultCwd unless overridden).
        host: Option<String>,
    },
    /// `session.migrate` the SELECTED session; None = back to local.
    MigrateSelected { target_host: Option<String> },
    /// `host.sync_prefs` to one configured host.
    SyncPrefs { host: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteAction {
    pub id: String,
    pub title: String,
    pub system_image: &'static str,
    pub shortcut: Option<String>,
    /// Availability copy displayed in the trailing chip.
    pub detail: Option<String>,
    /// Disabled rows remain searchable as setup guidance, but cannot be
    /// activated when the manifest provides no safe setup destination.
    pub enabled: bool,
    pub command: PaletteCommand,
    /// Scored alongside the title but never rendered: the folder path behind
    /// "New Claude Code in anara", a host's ssh target, and the synonyms people
    /// actually type ("shell" for New Terminal, "preferences" for Settings).
    pub keywords: String,
}

#[derive(Clone, Debug)]
pub struct ProjectTarget {
    pub project: Project,
    pub host: Option<String>,
}

/// Catalog-aware action builder used by the live app. The older wrappers below
/// remain deterministic fixtures for palette ranking tests.
pub fn actions_for_catalogs(
    default_agent: AgentKind,
    projects: &[ProjectTarget],
    hosts: &[HostEntry],
    selected: Option<&SessionRecord>,
    default_host_id: Option<&str>,
    catalogs: &HashMap<String, AgentReadinessResult>,
) -> Vec<PaletteAction> {
    // Readiness is target-specific. Missing remote facts must not inherit the
    // local machine's PATH: that would advertise Agents the remote cannot
    // launch. `quick_agent_options(None)` deliberately leaves only Terminal
    // until that target's scan completes.
    let catalog = |host: Option<&str>| catalogs.get(host.unwrap_or("local"));
    let default_host = default_host_id.and_then(|id| hosts.iter().find(|host| host.id == id));
    let default_host_id = default_host.map(|host| host.id.as_str());
    let mut result = Vec::new();
    let default_catalog = catalog(default_host_id);
    let target_default = resolved_target_agent(&default_agent, default_catalog);
    let default_agents = quick_agent_options(default_catalog);
    let mut default_is_listed = false;
    for agent in default_agents {
        if agent.kind == AgentKind::SHELL {
            continue;
        }
        let is_default = agent.kind == target_default;
        default_is_listed |= is_default;
        result.push(new_dynamic_agent_action(
            agent.kind,
            agent.display_name,
            is_default,
            default_host,
            None,
        ));
    }
    let terminal_title = default_host.map_or_else(
        || "New Terminal".to_owned(),
        |host| format!("New Terminal on {}", host.display_name()),
    );
    // ⌘T needs exactly one row, and it must describe what ⌘T will actually do.
    // The quick-create loop above carries the badge whenever it lists the
    // resolved Agent; it cannot when that Agent is hidden from quick create,
    // when resolution lands on Terminal, or when this target has no readiness
    // facts at all. In that last case the shortcut still belongs to the saved
    // preference — it resolves against real facts, or opens the launcher, at
    // press time — so labelling the row "Terminal" would promise a session the
    // user never chose.
    if !default_is_listed {
        let pending = default_catalog.is_none();
        let kind = if pending {
            default_agent.clone()
        } else {
            target_default.clone()
        };
        if kind.is_terminal() {
            result.push(default_action(
                terminal_title.clone(),
                "terminal",
                "shell console zsh bash tty default",
            ));
        } else {
            let name = default_catalog.map_or_else(
                || title_case_id(kind.id()),
                |catalog| display_name(&kind, catalog),
            );
            result.push(default_action(
                default_host.map_or_else(
                    || format!("New {name} Session"),
                    |host| format!("New {name} on {}", host.display_name()),
                ),
                system_image(&kind),
                &format!("{} agent spawn start create default", kind.id()),
            ));
        }
    }
    result.extend([
        registered_action_with_title(CommandId::NewTerminal, terminal_title),
        registered_action(CommandId::ToggleQuickOpen),
        registered_action(CommandId::ToggleOverview),
    ]);

    for target in projects {
        let agents = quick_agent_options(catalog(target.host.as_deref()));
        let preferred = agents
            .iter()
            .filter(|agent| agent.kind != AgentKind::SHELL)
            .find(|agent| agent.kind == default_agent)
            .or_else(|| agents.iter().find(|agent| agent.kind != AgentKind::SHELL));
        if let Some(agent) = preferred {
            let target_host = target
                .host
                .as_deref()
                .and_then(|id| hosts.iter().find(|host| host.id == id));
            let mut action = new_dynamic_agent_action(
                agent.kind.clone(),
                agent.display_name.clone(),
                false,
                target_host,
                Some(PathBuf::from(&target.project.root)),
            );
            action.title = target_host.map_or_else(
                || format!("New {} in {}", agent.display_name, target.project.name),
                |host| {
                    format!(
                        "New {} in {} on {}",
                        agent.display_name,
                        target.project.name,
                        host.display_name()
                    )
                },
            );
            result.push(action);
        }
    }

    for host in hosts {
        if Some(host.id.as_str()) == default_host_id {
            continue;
        }
        for agent in quick_agent_options(catalog(Some(&host.id))) {
            if agent.kind == AgentKind::SHELL {
                continue;
            }
            result.push(new_dynamic_agent_action(
                agent.kind,
                agent.display_name,
                false,
                Some(host),
                None,
            ));
        }
    }

    append_management_actions(&mut result, hosts, selected);
    result
}

fn new_dynamic_agent_action(
    kind: AgentKind,
    label: String,
    shortcut: bool,
    host: Option<&HostEntry>,
    cwd: Option<PathBuf>,
) -> PaletteAction {
    let host_id = host.map(|host| host.id.clone());
    let id = format!(
        "new-{}-{}-{}",
        kind.id(),
        host_id.as_deref().unwrap_or("local"),
        cwd.as_ref()
            .map_or("default".into(), |path| path.to_string_lossy())
    );
    PaletteAction {
        id,
        title: host.map_or_else(
            || format!("New {label} Session"),
            |host| format!("New {label} on {}", host.display_name()),
        ),
        system_image: system_image_for_kind(&kind),
        shortcut: shortcut
            .then(|| commands::command(CommandId::NewDefaultSession).shortcut_label())
            .flatten()
            .or_else(|| {
                (kind == AgentKind::CODEX)
                    .then(|| commands::command(CommandId::NewCodexSession).shortcut_label())
                    .flatten()
            }),
        command: PaletteCommand::SpawnAgent {
            agent: kind.clone(),
            cwd,
            host: host_id,
        },
        detail: None,
        enabled: true,
        keywords: format!("{} {label} agent spawn start create", kind.id()),
    }
}

fn system_image_for_kind(kind: &AgentKind) -> &'static str {
    match kind.id() {
        AgentKind::CLAUDE_CODE_ID => "sparkle",
        AgentKind::CODEX_ID => "chevron.left.forwardslash.chevron.right",
        AgentKind::CURSOR_ID => "cube",
        AgentKind::GEMINI_ID => "sparkles",
        _ => "terminal",
    }
}

fn append_management_actions(
    result: &mut Vec<PaletteAction>,
    hosts: &[HostEntry],
    selected: Option<&SessionRecord>,
) {
    result.extend(
        actions_for_default_host(
            AgentKind::SHELL,
            &AgentReadinessResult::default(),
            &[],
            hosts,
            selected,
            None,
        )
        .into_iter()
        .filter(|action| {
            matches!(
                action.command,
                PaletteCommand::MigrateSelected { .. }
                    | PaletteCommand::SyncPrefs { .. }
                    | PaletteCommand::Action(
                        CommandId::OpenWorktrees
                            | CommandId::ToggleSidebar
                            | CommandId::OpenSettings
                            | CommandId::CheckForUpdates
                    )
            )
        }),
    );
}

pub fn actions(
    default_agent: AgentKind,
    catalog: &AgentReadinessResult,
    projects: &[Project],
    hosts: &[HostEntry],
    selected: Option<&SessionRecord>,
) -> Vec<PaletteAction> {
    actions_for_default_host(default_agent, catalog, projects, hosts, selected, None)
}

pub fn actions_for_default_host(
    default_agent: AgentKind,
    catalog: &AgentReadinessResult,
    projects: &[Project],
    hosts: &[HostEntry],
    selected: Option<&SessionRecord>,
    default_host_id: Option<&str>,
) -> Vec<PaletteAction> {
    let default_host = default_host_id.and_then(|id| hosts.iter().find(|host| host.id == id));
    let default_agent = resolved_default_agent(&default_agent, catalog);
    let options = quick_agent_options(Some(catalog));
    let mut result = Vec::new();
    if default_agent == AgentKind::SHELL {
        result.push(default_action(
            "New Terminal".into(),
            "terminal",
            "shell console zsh bash tty default",
        ));
    } else if let Some(option) = options.iter().find(|option| option.kind == default_agent) {
        result.push(new_agent_action(option, true, default_host));
    } else {
        // Installed, so still what ⌘T launches, but hidden from quick create —
        // menu visibility must not leave the shortcut without a row.
        let name = display_name(&default_agent, catalog);
        result.push(default_action(
            default_host.map_or_else(
                || format!("New {name} Session"),
                |host| format!("New {name} on {}", host.display_name()),
            ),
            system_image(&default_agent),
            &format!("{} agent spawn start create default", default_agent.id()),
        ));
    }
    result.extend(
        options
            .iter()
            .filter(|option| option.kind != default_agent)
            .map(|option| new_agent_action(option, false, default_host)),
    );
    let terminal_title = default_host.map_or_else(
        || "New Terminal".to_owned(),
        |host| format!("New Terminal on {}", host.display_name()),
    );
    result.extend([
        registered_action_with_title(CommandId::NewTerminal, terminal_title),
        registered_action(CommandId::ToggleQuickOpen),
        registered_action(CommandId::ToggleOverview),
    ]);

    let default_name = display_name(&default_agent, catalog);
    for project in projects {
        result.push(PaletteAction {
            id: format!("new-default-in-{}", project.root),
            title: format!("New {default_name} in {}", project.name),
            system_image: "folder",
            shortcut: None,
            detail: None,
            enabled: true,
            command: PaletteCommand::SpawnAgent {
                agent: default_agent.clone(),
                cwd: Some(PathBuf::from(&project.root)),
                host: None,
            },
            keywords: format!("{} project folder spawn", project.root),
        });
    }

    // Remote spawns: one entry per agent per configured host, in the host's
    // default cwd (hosts.json).
    for host in hosts {
        for option in &options {
            result.push(PaletteAction {
                id: format!("new-{}-on-{}", option.kind.id(), host.id),
                title: format!("New {} on {}", option.display_name, host.display_name()),
                system_image: "network",
                shortcut: None,
                detail: None,
                enabled: true,
                command: PaletteCommand::SpawnAgent {
                    agent: option.kind.clone(),
                    cwd: None,
                    host: Some(host.id.clone()),
                },
                keywords: format!("{} {} remote host ssh spawn", host.id, host.ssh),
            });
        }
    }

    // Session handoff: move the SELECTED Claude session across hosts (v1 is
    // Claude-only — other kinds have no reliable resume, so no entries).
    if let Some(session) = selected
        && session.kind == AgentKind::CLAUDE_CODE
        && !session.is_archived()
    {
        if let Some(current) = &session.host {
            if hosts.iter().any(|host| &host.id == current) {
                result.push(PaletteAction {
                    id: "migrate-to-local".into(),
                    title: "Move Session to Local".into(),
                    system_image: "arrow.left.arrow.right",
                    shortcut: None,
                    detail: None,
                    enabled: true,
                    command: PaletteCommand::MigrateSelected { target_host: None },
                    keywords: "migrate handoff move back local".into(),
                });
            }
        } else {
            for host in hosts {
                result.push(PaletteAction {
                    id: format!("migrate-to-{}", host.id),
                    title: format!("Move Session to {}", host.display_name()),
                    system_image: "arrow.left.arrow.right",
                    shortcut: None,
                    detail: None,
                    enabled: true,
                    command: PaletteCommand::MigrateSelected {
                        target_host: Some(host.id.clone()),
                    },
                    keywords: format!("{} {} migrate handoff move remote", host.id, host.ssh),
                });
            }
        }
    }

    // Prefs push: make remote agents behave like local ones.
    for host in hosts {
        result.push(PaletteAction {
            id: format!("sync-prefs-{}", host.id),
            title: format!("Sync Prefs to {}", host.display_name()),
            system_image: "arrow.triangle.2.circlepath",
            shortcut: None,
            detail: None,
            enabled: true,
            command: PaletteCommand::SyncPrefs {
                host: host.id.clone(),
            },
            keywords: format!("{} {} preferences push remote", host.id, host.ssh),
        });
    }

    result.extend([
        registered_action(CommandId::OpenWorktrees),
        registered_action(CommandId::ToggleSidebar),
        registered_action(CommandId::OpenSettings),
        registered_action(CommandId::CheckForUpdates),
    ]);
    result
}

fn registered_action(id: CommandId) -> PaletteAction {
    let command = commands::command(id);
    let title = command
        .palette
        .expect("palette commands must carry palette metadata")
        .title
        .to_owned();
    registered_action_with_title(id, title)
}

fn registered_action_with_title(id: CommandId, title: String) -> PaletteAction {
    let command = commands::command(id);
    let palette = command
        .palette
        .expect("palette commands must carry palette metadata");
    PaletteAction {
        id: command.stable_id.into(),
        title,
        system_image: palette.system_image,
        shortcut: command.shortcut_label(),
        detail: None,
        enabled: true,
        command: PaletteCommand::Action(id),
        keywords: palette.keywords.into(),
    }
}

fn default_action(title: String, system_image: &'static str, keywords: &str) -> PaletteAction {
    let command = commands::command(CommandId::NewDefaultSession);
    PaletteAction {
        id: command.stable_id.into(),
        title,
        system_image,
        shortcut: command.shortcut_label(),
        detail: None,
        enabled: true,
        command: PaletteCommand::Action(CommandId::NewDefaultSession),
        keywords: keywords.into(),
    }
}

/// Builds a row for an Agent the target's readiness reports as installed.
/// Callers pass `quick_agent_options` output, so unavailable Agents never reach
/// here — Settings is the one surface that lists them, with their setup links.
fn new_agent_action(
    option: &AgentOption,
    is_default: bool,
    host: Option<&HostEntry>,
) -> PaletteAction {
    let registered = is_default.then_some(CommandId::NewDefaultSession);
    PaletteAction {
        id: if is_default {
            "new-default".into()
        } else {
            format!("new-{}", option.kind.id())
        },
        title: host.map_or_else(
            || format!("New {} Session", option.display_name),
            |host| format!("New {} on {}", option.display_name, host.display_name()),
        ),
        system_image: system_image(&option.kind),
        shortcut: registered
            .or((option.kind == AgentKind::CODEX).then_some(CommandId::NewCodexSession))
            .and_then(|id| commands::command(id).shortcut_label()),
        detail: None,
        enabled: true,
        command: registered.map_or_else(
            || PaletteCommand::SpawnAgent {
                agent: option.kind.clone(),
                cwd: None,
                host: host.map(|host| host.id.clone()),
            },
            PaletteCommand::Action,
        ),
        keywords: format!(
            "{} {} agent spawn start create tab",
            option.kind.id(),
            option.binary
        ),
    }
}

/// Matching a keyword instead of the visible title costs two characters, so a
/// title hit always sorts above a synonym hit.
const KEYWORD_PENALTY: Score = 32;

/// A palette entry that survived filtering, with the byte ranges of its title
/// to highlight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ranked<T> {
    pub item: T,
    pub title_matches: Vec<Range<usize>>,
    pub score: Score,
}

/// Score `title` (highlighted) against `keywords` (invisible, penalized) and
/// keep whichever wins. `None` means the row is filtered out.
fn rank_text(
    query: &FuzzyQuery,
    title: &str,
    keywords: &str,
    matcher: &mut FuzzyMatcher,
) -> Option<(Score, Vec<Range<usize>>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let title_score = query.highlights(&PreparedText::new(title), title, matcher);
    let keyword_score = (!keywords.is_empty())
        .then(|| query.score(&PreparedText::new(keywords), matcher))
        .flatten()
        .map(|score| score.saturating_sub(KEYWORD_PENALTY));

    match (title_score, keyword_score) {
        (Some((score, ranges)), Some(keyword)) if keyword > score => Some((keyword, ranges)),
        (Some((score, ranges)), _) => Some((score, ranges)),
        (None, Some(keyword)) => Some((keyword, Vec::new())),
        (None, None) => None,
    }
}

/// Rank in place: filter out non-matches, then sort by score with the original
/// (curated) order as the tiebreak so an empty query renders unchanged.
fn rank_by<T>(
    items: Vec<T>,
    query: &FuzzyQuery,
    matcher: &mut FuzzyMatcher,
    text: impl Fn(&T) -> (String, String),
) -> Vec<Ranked<T>> {
    let mut ranked: Vec<_> = items
        .into_iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let (title, keywords) = text(&item);
            rank_text(query, &title, &keywords, matcher).map(|(score, title_matches)| {
                (
                    index,
                    Ranked {
                        item,
                        title_matches,
                        score,
                    },
                )
            })
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .1
            .score
            .cmp(&left.1.score)
            .then_with(|| left.0.cmp(&right.0))
    });
    ranked.into_iter().map(|(_, ranked)| ranked).collect()
}

pub fn rank_actions(
    actions: Vec<PaletteAction>,
    query: &FuzzyQuery,
    matcher: &mut FuzzyMatcher,
) -> Vec<Ranked<PaletteAction>> {
    rank_by(actions, query, matcher, |action| {
        (action.title.clone(), action.keywords.clone())
    })
}

pub fn rank_sessions(
    sessions: Vec<SessionRecord>,
    query: &FuzzyQuery,
    matcher: &mut FuzzyMatcher,
) -> Vec<Ranked<SessionRecord>> {
    rank_by(sessions, query, matcher, |session| {
        (session.title.clone(), session_keywords(session))
    })
}

/// Everything about a session that is true but not printed on its row: where it
/// runs, on which branch, and which agent drives it.
fn session_keywords(session: &SessionRecord) -> String {
    let mut keywords = session.cwd.clone();
    if let Some(branch) = &session.git_branch {
        keywords.push(' ');
        keywords.push_str(branch);
    }
    if let Some(host) = &session.host {
        keywords.push(' ');
        keywords.push_str(host);
    }
    keywords.push(' ');
    keywords.push_str(agent_keyword(&session.kind));
    keywords
}

/// Extra fuzzy-search terms for a session's agent. The manifest id is already a
/// searchable kebab-case name ("claude-code", "opencode"), so this only adds the
/// spellings a user is likely to type that the id doesn't cover.
fn agent_keyword(kind: &AgentKind) -> &str {
    match kind.id() {
        AgentKind::CLAUDE_CODE_ID => "claude code",
        AgentKind::SHELL_ID => "shell terminal",
        AgentKind::GENERIC_ID => "terminal",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use diri_proto::{AgentDescriptor, AgentPathSource, AgentReadinessItem, AgentSetup, ProjectId};

    use super::*;

    fn catalog() -> AgentReadinessResult {
        AgentReadinessResult {
            agents: vec![
                installed_agent("claude-code", "Claude Code", true),
                installed_agent("codex", "Codex", true),
                installed_agent("cursor", "Cursor", true),
                installed_agent("gemini", "Gemini", true),
            ],
            ..AgentReadinessResult::default()
        }
    }

    fn catalog_item(
        id: &str,
        display_name: &str,
        available: bool,
        setup_url: Option<&str>,
        sign_in_hint: Option<&str>,
    ) -> AgentReadinessItem {
        let path = available.then(|| format!("/bin/{id}"));
        AgentReadinessItem {
            kind: AgentKind::new(id),
            binary: format!("{id}-bin"),
            path: path.clone(),
            detected_path: path,
            configured_path: None,
            path_source: available.then_some(AgentPathSource::SystemPath),
            show_in_quick_create: available,
            error: None,
            descriptor: Some(AgentDescriptor {
                id: id.into(),
                display_name: display_name.into(),
                setup: setup_url.map(|url| AgentSetup {
                    url: Some(url.into()),
                    install_hint: Some(format!("Install {display_name}.")),
                    sign_in_hint: sign_in_hint.map(str::to_owned),
                }),
                ..AgentDescriptor::default()
            }),
        }
    }

    fn installed_agent(id: &str, label: &str, show: bool) -> AgentReadinessItem {
        let mut item = catalog_item(id, label, true, None, None);
        item.show_in_quick_create = show;
        item
    }

    #[test]
    fn manifest_only_agents_get_typed_default_and_contextual_remote_actions() {
        let catalog = AgentReadinessResult {
            agents: vec![catalog_item("amp", "Amp", true, None, None)],
            ..AgentReadinessResult::default()
        };
        let host = HostEntry {
            id: "forge".into(),
            name: Some("Forge".into()),
            ssh: "forge.local".into(),
            default_cwd: None,
            node: None,
        };
        let actions = actions(
            AgentKind::new("amp"),
            &catalog,
            &[],
            std::slice::from_ref(&host),
            None,
        );
        assert_eq!(actions[0].title, "New Amp Session");
        assert_eq!(
            actions[0].shortcut,
            commands::command(CommandId::NewDefaultSession).shortcut_label()
        );
        assert_eq!(
            actions[0].command,
            PaletteCommand::Action(CommandId::NewDefaultSession)
        );
        assert!(actions.iter().any(|action| {
            action.id == "new-amp-on-forge"
                && action.command
                    == PaletteCommand::SpawnAgent {
                        agent: AgentKind::new("amp"),
                        cwd: None,
                        host: Some("forge".into()),
                    }
        }));
    }

    #[test]
    fn unavailable_catalog_agent_is_not_a_quick_palette_action() {
        let catalog = AgentReadinessResult {
            agents: vec![catalog_item(
                "amp",
                "Amp",
                false,
                Some("https://ampcode.com/manual"),
                Some("Sign in at ampcode.com, then run amp."),
            )],
            ..AgentReadinessResult::default()
        };
        let actions = actions(AgentKind::new("amp"), &catalog, &[], &[], None);
        assert!(!actions.iter().any(|action| action.id == "new-amp"));
    }

    #[test]
    fn unavailable_catalog_agent_without_setup_is_not_a_quick_palette_action() {
        let catalog = AgentReadinessResult {
            agents: vec![catalog_item("private", "Private", false, None, None)],
            ..AgentReadinessResult::default()
        };
        let actions = actions(AgentKind::new("private"), &catalog, &[], &[], None);
        assert!(!actions.iter().any(|action| action.id == "new-private"));
    }

    #[test]
    fn live_palette_uses_target_catalog_without_duplicate_default_host_actions() {
        let host = HostEntry {
            id: "forge".into(),
            name: Some("Forge".into()),
            ssh: "forge".into(),
            default_cwd: None,
            node: None,
        };
        let catalogs = HashMap::from([(
            "forge".into(),
            AgentReadinessResult {
                host: Some("forge".into()),
                scanned_at: None,
                agents: vec![
                    installed_agent("codex", "Codex", true),
                    installed_agent("aider", "Aider", false),
                ],
            },
        )]);
        let actions = actions_for_catalogs(
            AgentKind::CLAUDE_CODE,
            &[],
            std::slice::from_ref(&host),
            None,
            Some("forge"),
            &catalogs,
        );
        let codex = actions
            .iter()
            .filter(|action| action.id.starts_with("new-codex-forge"))
            .collect::<Vec<_>>();
        assert_eq!(codex.len(), 1);
        assert_eq!(
            codex[0].shortcut,
            commands::command(CommandId::NewDefaultSession).shortcut_label()
        );
        assert!(!actions.iter().any(|action| action.id.contains("aider")));
    }

    #[test]
    fn a_host_whose_catalog_has_not_been_fetched_does_not_borrow_local_actions() {
        let host = HostEntry {
            id: "forge".into(),
            name: Some("Forge".into()),
            ssh: "forge".into(),
            default_cwd: None,
            node: None,
        };
        // Only the local catalog is warmed at connect; forge has never been
        // scanned. Local availability says nothing about the remote target,
        // so Forge must not claim that Codex can be launched there.
        let catalogs = HashMap::from([(
            "local".to_owned(),
            AgentReadinessResult {
                agents: vec![installed_agent("codex", "Codex", true)],
                ..AgentReadinessResult::default()
            },
        )]);
        let actions = actions_for_catalogs(
            AgentKind::CODEX,
            &[ProjectTarget {
                project: Project {
                    id: ProjectId::new("p1"),
                    root: "/srv/app".into(),
                    name: "app".into(),
                    pinned_order: None,
                    host: Some("forge".into()),
                },
                host: Some("forge".into()),
            }],
            std::slice::from_ref(&host),
            None,
            None,
            &catalogs,
        );
        assert!(!actions.iter().any(|action| {
            action.command
                == PaletteCommand::SpawnAgent {
                    agent: AgentKind::new("codex"),
                    cwd: None,
                    host: Some("forge".into()),
                }
        }));
        assert!(
            actions
                .iter()
                .all(|action| action.title != "New Codex in app on Forge")
        );
    }

    #[test]
    fn an_unscanned_default_target_keeps_the_shortcut_on_the_saved_preference() {
        let host = HostEntry {
            id: "forge".into(),
            name: Some("Forge".into()),
            ssh: "forge".into(),
            default_cwd: None,
            node: None,
        };
        let actions = actions_for_catalogs(
            AgentKind::new("saved-agent"),
            &[],
            std::slice::from_ref(&host),
            None,
            Some("forge"),
            &HashMap::new(),
        );

        // Forge has never been scanned, so no Agent may be advertised as
        // launchable there. ⌘T still belongs to the saved preference — it
        // resolves against real facts, or opens the launcher, at press time —
        // so the row must name it rather than promise an unrequested Terminal.
        let default = actions
            .iter()
            .find(|action| action.id == "new-default")
            .expect("default action");
        assert_eq!(default.title, "New Saved Agent on Forge");
        assert_eq!(
            default.shortcut,
            commands::command(CommandId::NewDefaultSession).shortcut_label()
        );
        assert_eq!(
            default.command,
            PaletteCommand::Action(CommandId::NewDefaultSession)
        );
        assert!(
            !actions.iter().any(|action| matches!(
                &action.command,
                PaletteCommand::SpawnAgent { agent, .. } if !agent.is_terminal()
            )),
            "an unscanned target must not advertise a direct Agent spawn"
        );
    }

    #[test]
    fn a_default_hidden_from_quick_create_still_owns_the_shortcut() {
        // Installed, so ⌘T launches it, but toggled out of the quick-create
        // menus in Settings. Menu visibility must not leave the shortcut
        // without any row to sit on.
        let mut hidden = catalog_item("codex", "Codex", true, None, None);
        hidden.show_in_quick_create = false;
        let catalog = AgentReadinessResult {
            agents: vec![hidden, catalog_item("amp", "Amp", true, None, None)],
            ..AgentReadinessResult::default()
        };

        let actions = actions(AgentKind::CODEX, &catalog, &[], &[], None);

        let default = actions
            .iter()
            .find(|action| action.id == "new-default")
            .expect("default action");
        assert_eq!(default.title, "New Codex Session");
        assert_eq!(
            default.shortcut,
            commands::command(CommandId::NewDefaultSession).shortcut_label()
        );
    }

    #[test]
    fn action_list_matches_swift_order_and_dynamic_default() {
        let project = Project {
            id: ProjectId::new("p1"),
            root: "/work/diri".into(),
            name: "diri".into(),
            pinned_order: None,
            host: None,
        };
        let result = actions(AgentKind::CODEX, &catalog(), &[project], &[], None);
        let ids: Vec<_> = result.iter().map(|action| action.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "new-default",
                "new-claude-code",
                "new-cursor",
                "new-gemini",
                "new-terminal",
                "quick-open",
                "session-overview",
                "new-default-in-/work/diri",
                "worktrees",
                "toggle-sidebar",
                "settings",
                "check-for-updates",
            ]
        );
        assert_eq!(result[0].title, "New Codex Session");
        assert_eq!(
            result[0].shortcut,
            commands::command(CommandId::NewDefaultSession).shortcut_label()
        );
        assert_eq!(result[1].title, "New Claude Code Session");
        assert_eq!(result[7].title, "New Codex in diri");
    }

    #[test]
    fn configured_hosts_add_remote_spawn_entries_per_agent() {
        let hosts = [
            HostEntry {
                id: "forge".into(),
                name: Some("Forge".into()),
                ssh: "cristi@forge".into(),
                default_cwd: Some("~/code".into()),
                node: None,
            },
            HostEntry {
                id: "studio".into(),
                name: Some("Studio Mac".into()),
                ssh: "studio.local".into(),
                default_cwd: None,
                node: None,
            },
        ];
        let result = actions(AgentKind::CLAUDE_CODE, &catalog(), &[], &hosts, None);
        let forge_actions: Vec<_> = result
            .iter()
            .filter(|action| action.id.ends_with("-on-forge"))
            .collect();
        let studio_actions: Vec<_> = result
            .iter()
            .filter(|action| action.id.ends_with("-on-studio"))
            .collect();
        assert_eq!(forge_actions.len(), 4);
        assert_eq!(studio_actions.len(), 4);
        assert_eq!(forge_actions[0].title, "New Claude Code on Forge");
        assert_eq!(forge_actions[0].system_image, "network");
        assert_eq!(studio_actions[0].title, "New Claude Code on Studio Mac");
        assert_eq!(studio_actions[0].system_image, "network");
        assert_eq!(
            forge_actions[0].command,
            PaletteCommand::SpawnAgent {
                agent: AgentKind::CLAUDE_CODE,
                cwd: None,
                host: Some("forge".into()),
            }
        );
        // Remote entries slot between the per-project block and the tail.
        let first_remote = result
            .iter()
            .position(|action| action.id == "new-claude-code-on-forge")
            .unwrap();
        let worktrees = result
            .iter()
            .position(|action| action.id == "worktrees")
            .unwrap();
        assert!(first_remote < worktrees);
    }

    #[test]
    fn global_palette_shortcuts_follow_the_selected_default_host() {
        let host = HostEntry {
            id: "forge".into(),
            name: Some("Forge".into()),
            ssh: "cristi@forge".into(),
            default_cwd: None,
            node: None,
        };
        let result = actions_for_default_host(
            AgentKind::CLAUDE_CODE,
            &catalog(),
            &[],
            std::slice::from_ref(&host),
            None,
            Some("forge"),
        );

        assert_eq!(result[0].title, "New Claude Code on Forge");
        assert_eq!(
            result[0].shortcut,
            commands::command(CommandId::NewDefaultSession).shortcut_label()
        );
        assert_eq!(
            result[0].command,
            PaletteCommand::Action(CommandId::NewDefaultSession)
        );
        let terminal = result
            .iter()
            .find(|action| action.id == "new-terminal")
            .expect("terminal action");
        assert_eq!(terminal.title, "New Terminal on Forge");
        assert_eq!(
            terminal.shortcut,
            commands::command(CommandId::NewTerminal).shortcut_label()
        );
        assert_eq!(
            terminal.command,
            PaletteCommand::Action(CommandId::NewTerminal)
        );
    }

    fn claude_session(host: Option<&str>) -> SessionRecord {
        use diri_proto::{DateMillis, ProjectId, Resumability, SessionId, TitleSource};
        SessionRecord {
            id: SessionId::new("s_1"),
            kind: AgentKind::CLAUDE_CODE,
            cwd: "/work/app".into(),
            project_id: ProjectId::new("p1"),
            worktree_path: None,
            git_branch: None,
            title: "Refactor".into(),
            title_source: TitleSource::AgentProvided,
            originating_prompt: None,
            agent_session_id: Some("uuid".into()),
            transcript_path: None,
            status: diri_proto::SessionStatus::Idle,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::Live,
            parent: None,
            created_at: DateMillis(1.0),
            updated_at: DateMillis(2.0),
            last_turn_completed_at: None,
            last_seen_at: None,
            pinned: false,
            archived_at: None,
            host: host.map(str::to_owned),
            remote_persistence: None,
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
        }
    }

    #[test]
    fn selected_claude_session_gets_migration_and_sync_entries() {
        let host = HostEntry {
            id: "forge".into(),
            name: Some("Forge".into()),
            ssh: "cristi@forge".into(),
            default_cwd: Some("~/code".into()),
            node: None,
        };
        let hosts = std::slice::from_ref(&host);

        // Local Claude session → one "Move Session to <host>" per host.
        let local = claude_session(None);
        let result = actions(AgentKind::CLAUDE_CODE, &catalog(), &[], hosts, Some(&local));
        let migrate = result
            .iter()
            .find(|action| action.id == "migrate-to-forge")
            .expect("move entry");
        assert_eq!(migrate.title, "Move Session to Forge");
        assert_eq!(
            migrate.command,
            PaletteCommand::MigrateSelected {
                target_host: Some("forge".into())
            }
        );

        // Remote Claude session → a single "Move Session to Local".
        let remote = claude_session(Some("forge"));
        let result = actions(
            AgentKind::CLAUDE_CODE,
            &catalog(),
            &[],
            hosts,
            Some(&remote),
        );
        let back = result
            .iter()
            .find(|action| action.id == "migrate-to-local")
            .expect("move-to-local entry");
        assert_eq!(
            back.command,
            PaletteCommand::MigrateSelected { target_host: None }
        );
        assert!(!result.iter().any(|action| action.id == "migrate-to-forge"));

        // Non-Claude selections get no move entries; sync entries always show.
        let mut shell = claude_session(None);
        shell.kind = AgentKind::SHELL;
        let result = actions(AgentKind::CLAUDE_CODE, &catalog(), &[], hosts, Some(&shell));
        assert!(
            !result
                .iter()
                .any(|action| action.id.starts_with("migrate-"))
        );
        let sync = result
            .iter()
            .find(|action| action.id == "sync-prefs-forge")
            .expect("sync entry");
        assert_eq!(sync.title, "Sync Prefs to Forge");
        assert_eq!(
            sync.command,
            PaletteCommand::SyncPrefs {
                host: "forge".into()
            }
        );

        // No hosts configured → neither family appears.
        let result = actions(AgentKind::CLAUDE_CODE, &catalog(), &[], &[], Some(&local));
        assert!(!result.iter().any(|action| {
            action.id.starts_with("migrate-") || action.id.starts_with("sync-prefs-")
        }));
    }

    fn project(root: &str, name: &str) -> Project {
        Project {
            id: ProjectId::new(root),
            root: root.into(),
            name: name.into(),
            pinned_order: None,
            host: None,
        }
    }

    #[test]
    fn empty_query_keeps_every_action_in_curated_order() {
        let all = actions(
            AgentKind::CLAUDE_CODE,
            &catalog(),
            &[project("/work/diri", "diri")],
            &[],
            None,
        );
        let ranked = rank_actions(all.clone(), &FuzzyQuery::new(""), &mut FuzzyMatcher::text());
        let ids: Vec<_> = ranked.iter().map(|entry| entry.item.id.clone()).collect();
        let expected: Vec<_> = all.iter().map(|action| action.id.clone()).collect();
        assert_eq!(ids, expected);
        assert!(ranked.iter().all(|entry| entry.title_matches.is_empty()));
    }

    #[test]
    fn actions_are_found_by_title_acronym_synonym_and_project_path() {
        let all = actions(
            AgentKind::CLAUDE_CODE,
            &catalog(),
            &[project("/work/anara", "anara")],
            &[],
            None,
        );
        let top = |query: &str| {
            rank_actions(
                all.clone(),
                &FuzzyQuery::new(query),
                &mut FuzzyMatcher::text(),
            )
            .first()
            .map(|entry| entry.item.id.clone())
        };

        assert_eq!(top("term").as_deref(), Some("new-terminal"));
        assert_eq!(top("ncc").as_deref(), Some("new-default"));
        assert_eq!(top("anara").as_deref(), Some("new-default-in-/work/anara"));
        // Synonyms nobody put in a title: "preferences" is only a keyword.
        assert_eq!(top("preferences").as_deref(), Some("settings"));
        assert_eq!(top("shell").as_deref(), Some("new-terminal"));
        assert_eq!(top("zzq"), None);
    }

    #[test]
    fn title_matches_outrank_keyword_matches_and_carry_highlights() {
        let all = actions(AgentKind::CLAUDE_CODE, &catalog(), &[], &[], None);
        let ranked = rank_actions(all, &FuzzyQuery::new("terminal"), &mut FuzzyMatcher::text());
        assert_eq!(ranked[0].item.id, "new-terminal");
        // "Terminal" starts at byte 4 of "New Terminal".
        assert_eq!(ranked[0].title_matches.len(), 1);
        assert_eq!(ranked[0].title_matches[0], 4..12);
        assert!(
            ranked
                .iter()
                .skip(1)
                .all(|entry| entry.score < ranked[0].score)
        );
    }

    #[test]
    fn sessions_match_their_directory_and_branch_as_well_as_their_title() {
        let mut titled = claude_session(None);
        titled.title = "Refactor tokens".into();
        let mut untitled = claude_session(None);
        untitled.id = diri_proto::SessionId::new("s_2");
        untitled.title = "Untitled".into();
        untitled.cwd = "/work/dirijor".into();
        untitled.git_branch = Some("perf/palette".into());
        let pool = vec![titled, untitled];

        let ranked = rank_sessions(
            pool.clone(),
            &FuzzyQuery::new("dirijor"),
            &mut FuzzyMatcher::text(),
        );
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].item.cwd, "/work/dirijor");
        assert!(ranked[0].title_matches.is_empty());

        let ranked = rank_sessions(
            pool.clone(),
            &FuzzyQuery::new("palette"),
            &mut FuzzyMatcher::text(),
        );
        assert_eq!(ranked.len(), 1);

        let ranked = rank_sessions(
            pool,
            &FuzzyQuery::new("refactor"),
            &mut FuzzyMatcher::text(),
        );
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].title_matches.len(), 1);
        assert_eq!(ranked[0].title_matches[0], 0..8);
    }
}
