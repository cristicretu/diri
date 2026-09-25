//! Bring a herdr setup over: every pane herdr would restore becomes a Diri
//! session in the same folder.
//!
//! herdr keeps its layout in `session.json` (one per named session). A pane
//! whose agent reported its conversation through herdr's integration hooks
//! carries that id; Diri resumes exactly that conversation through the same
//! Engine path as conversation history, so nothing is replayed or re-typed.
//! A pane that only knows its agent starts that agent fresh, and a plain pane
//! becomes a Terminal. herdr's files are only read, never changed.
//!
//! The on-disk format is herdr's persist snapshot (`src/persist/snapshot.rs`
//! upstream): version 3 with tabs, and the older tabless workspaces that
//! herdr itself still migrates.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use diri_proto::{AgentKind, HistoryEntry};
use serde::Deserialize;

use crate::history::HistoryRoots;

/// Largest `session.json` read. herdr's own files are a few kilobytes; the
/// cap only keeps a corrupt or hostile file from being slurped.
const SNAPSHOT_CAP: u64 = 8 << 20;
/// herdr refuses newer snapshots than it understands; so does the importer.
const NEWEST_SNAPSHOT_VERSION: u32 = 3;

/// Where herdr keeps its state on this Mac.
#[derive(Clone, Debug)]
pub struct HerdrRoots {
    pub config_dir: PathBuf,
    pub history: HistoryRoots,
}

impl HerdrRoots {
    pub fn in_home(home: &Path, xdg_config_home: Option<&Path>) -> Self {
        let config = xdg_config_home
            .filter(|path| path.is_absolute())
            .map_or_else(|| home.join(".config"), Path::to_path_buf);
        Self {
            config_dir: config.join("herdr"),
            history: HistoryRoots::in_home(home),
        }
    }

    pub fn current_user() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/nonexistent"));
        let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
        Self::in_home(&home, xdg.as_deref())
    }
}

/// What one herdr pane becomes in Diri.
#[derive(Clone, Debug, PartialEq)]
pub enum HerdrAction {
    /// Re-enter the exact conversation the pane was running.
    Resume(HistoryEntry),
    /// The pane ran this agent but left no conversation Diri can re-enter.
    Start(AgentKind),
    /// A plain terminal pane.
    Terminal,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HerdrItem {
    /// Stable identity of the pane across scans, remembered once imported so
    /// a second import never opens the same pane twice.
    pub key: String,
    pub cwd: String,
    pub title: Option<String>,
    /// The herdr session and workspace the pane came from, for counting
    /// workspaces in the confirmation summary.
    pub workspace: String,
    pub action: HerdrAction,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HerdrPlan {
    pub items: Vec<HerdrItem>,
    /// herdr has saved state here at all, imported or not.
    pub found: bool,
    /// Panes whose folder no longer exists; herdr cannot restore them either.
    pub missing_folders: usize,
    /// Panes already brought over, or whose conversation Diri already has open.
    pub already_in_diri: usize,
    /// A herdr server answers on one of the session sockets: its agents are
    /// still running and would share their conversation with the import.
    pub herdr_running: bool,
}

impl HerdrPlan {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn resumed(&self) -> usize {
        self.count(|action| matches!(action, HerdrAction::Resume(_)))
    }

    pub fn started(&self) -> usize {
        self.count(|action| matches!(action, HerdrAction::Start(_)))
    }

    pub fn terminals(&self) -> usize {
        self.count(|action| matches!(action, HerdrAction::Terminal))
    }

    fn count(&self, matches: impl Fn(&HerdrAction) -> bool) -> usize {
        self.items
            .iter()
            .filter(|item| matches(&item.action))
            .count()
    }

    /// "5 sessions from herdr" — the label on the import controls.
    pub fn headline(&self) -> String {
        let count = self.items.len();
        format!(
            "{count} {} from herdr",
            if count == 1 { "session" } else { "sessions" }
        )
    }

    /// "2 conversations, 1 agent and 3 terminals in 2 workspaces": what the
    /// import holds, in the words the Settings row uses.
    pub fn summary(&self) -> String {
        format!(
            "{} in {} {}.",
            join_list(&self.parts(false)),
            self.workspaces(),
            plural(self.workspaces(), "workspace", "workspaces")
        )
    }

    /// The confirmation sheet's detail: what will happen, then the one
    /// caveat that matters.
    pub fn confirmation_detail(&self) -> String {
        let workspaces = self.workspaces();
        let mut detail = format!(
            "From {workspaces} herdr {}: {}.",
            plural(workspaces, "workspace", "workspaces"),
            join_list(&self.parts(true))
        );
        if self.herdr_running {
            detail.push_str(
                "\n\nherdr is still running. Quit it first so no conversation is open in both apps.",
            );
        }
        detail
    }

    fn workspaces(&self) -> usize {
        self.items
            .iter()
            .map(|item| item.workspace.as_str())
            .collect::<HashSet<_>>()
            .len()
    }

    fn parts(&self, explain: bool) -> Vec<String> {
        let mut parts = Vec::new();
        let resumed = self.resumed();
        if resumed > 0 {
            let noun = plural(resumed, "conversation", "conversations");
            parts.push(if explain {
                format!(
                    "{resumed} {noun} resumed where {} left off",
                    if resumed == 1 { "it" } else { "they" }
                )
            } else {
                format!("{resumed} {noun}")
            });
        }
        let started = self.started();
        if started > 0 {
            let noun = plural(started, "agent", "agents");
            parts.push(if explain {
                format!("{started} {noun} started fresh")
            } else {
                format!("{started} {noun}")
            });
        }
        let terminals = self.terminals();
        if terminals > 0 {
            parts.push(format!(
                "{terminals} {}",
                plural(terminals, "terminal", "terminals")
            ));
        }
        parts
    }
}

/// The consent step: the system alert sheet saying exactly what arrives and
/// whether herdr still has these agents open. Nothing runs on any answer but
/// Import.
pub(crate) fn confirm(
    plan: &HerdrPlan,
    window: &mut gpui::Window,
    cx: &mut gpui::App,
    on_import: impl FnOnce(&mut gpui::App) + 'static,
) {
    let answer = window.prompt(
        gpui::PromptLevel::Info,
        &format!("Import {}?", plan.headline()),
        Some(&plan.confirmation_detail()),
        &[
            gpui::PromptButton::ok("Import"),
            gpui::PromptButton::cancel("Cancel"),
        ],
        cx,
    );
    cx.spawn(async move |cx| {
        if answer.await.ok() == Some(0) {
            cx.update(on_import);
        }
    })
    .detach();
}

fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

fn join_list(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [only] => only.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// Read every herdr session on this Mac and decide what each pane becomes.
///
/// `tracked` holds the conversation ids Diri sessions already own and
/// `imported` the pane keys an earlier import brought over.
pub fn plan(
    roots: &HerdrRoots,
    tracked: &HashSet<String>,
    imported: &HashSet<String>,
) -> HerdrPlan {
    let mut plan = HerdrPlan::default();
    let mut seen_conversations = HashSet::new();
    for session in sessions(&roots.config_dir) {
        let Some(snapshot) = read_snapshot(&session.dir.join("session.json")) else {
            continue;
        };
        plan.found = true;
        plan.herdr_running |= socket_answers(&session.dir.join("herdr.sock"));
        for (index, workspace) in snapshot.workspaces.into_iter().enumerate() {
            let workspace_id = workspace.id.clone().unwrap_or_else(|| format!("#{index}"));
            let workspace_name = workspace
                .custom_name
                .as_ref()
                .filter(|name| !name.trim().is_empty());
            for tab in workspace.tabs() {
                for (pane_id, pane) in tab.ordered_panes() {
                    let cwd = pane.cwd.to_string_lossy().into_owned();
                    let key = format!("{}/{workspace_id}/{pane_id}/{cwd}", session.name);
                    let title = [
                        pane.label.as_ref(),
                        pane.agent_name.as_ref(),
                        tab.custom_name,
                        workspace_name,
                    ]
                    .into_iter()
                    .flatten()
                    .find(|name| !name.trim().is_empty())
                    .cloned();
                    let action = action(&roots.history, pane);
                    let conversation = match &action {
                        HerdrAction::Resume(entry) => Some(entry.id.clone()),
                        _ => None,
                    };
                    if imported.contains(&key)
                        || conversation.as_ref().is_some_and(|id| {
                            tracked.contains(id) || imported.contains(&conversation_key(id))
                        })
                    {
                        plan.already_in_diri += 1;
                        continue;
                    }
                    // Two panes on one conversation would fight over its
                    // transcript; herdr resumes it once as well.
                    if let Some(id) = &conversation
                        && !seen_conversations.insert(id.clone())
                    {
                        continue;
                    }
                    let folder = match &action {
                        HerdrAction::Resume(entry) => entry.cwd.as_str(),
                        _ => cwd.as_str(),
                    };
                    if !Path::new(folder).is_dir() {
                        plan.missing_folders += 1;
                        continue;
                    }
                    plan.items.push(HerdrItem {
                        key,
                        cwd: folder.to_owned(),
                        title,
                        workspace: format!("{}/{workspace_id}", session.name),
                        action,
                    });
                }
            }
        }
    }
    plan
}

/// The key remembered for a resumed conversation, so re-running the import
/// after closing that session in Diri does not bring it back.
pub fn conversation_key(id: &str) -> String {
    format!("conversation/{id}")
}

/// Keys to remember once an item has been imported.
pub fn remembered_keys(item: &HerdrItem) -> Vec<String> {
    let mut keys = vec![item.key.clone()];
    if let HerdrAction::Resume(entry) = &item.action {
        keys.push(conversation_key(&entry.id));
    }
    keys
}

fn action(history: &HistoryRoots, pane: &PaneSnapshot) -> HerdrAction {
    let reported = pane.agent_session.as_ref();
    let agent = reported
        .map(|session| session.agent.as_str())
        .or(pane.managed_agent_kind.as_deref())
        .and_then(diri_kind);
    let Some(kind) = agent else {
        return HerdrAction::Terminal;
    };
    if let Some(session) = reported
        && session.kind == "id"
        && diri_kind(&session.agent).as_ref() == Some(&kind)
        && let Some(entry) = crate::history::conversation(history, &kind, &session.value)
    {
        return HerdrAction::Resume(entry);
    }
    HerdrAction::Start(kind)
}

/// herdr's agent names (its `detect::Agent` labels and hook `agent` field)
/// mapped to the Diri manifest that runs the same CLI. Agents Diri has no
/// manifest for come over as Terminals rather than as a guess.
fn diri_kind(agent: &str) -> Option<AgentKind> {
    let id = match agent {
        "claude" => AgentKind::CLAUDE_CODE_ID,
        "codex" => AgentKind::CODEX_ID,
        "cursor" | "cursor-agent" => "cursor",
        "gemini" => "gemini",
        "opencode" => "opencode",
        "copilot" => "copilot",
        "devin" => "devin",
        "droid" => "droid",
        "kimi" => "kimi",
        "kilo" => "kilo",
        "pi" => "pi",
        "hermes" => "hermes",
        "grok" => "grok",
        "amp" => "amp",
        "cline" => "cline",
        "aider" => "aider",
        "qodercli" | "qoder" => "qoder",
        "agy" | "antigravity_cli" | "antigravity" => "antigravity",
        "kiro" | "kiro-cli" => "kiro",
        _ => return None,
    };
    Some(AgentKind::new(id))
}

struct HerdrSession {
    name: String,
    dir: PathBuf,
}

/// The default session lives in the config folder itself; named sessions
/// (`herdr --session <name>`) each get `sessions/<name>/`.
fn sessions(config_dir: &Path) -> Vec<HerdrSession> {
    let mut sessions = vec![HerdrSession {
        name: "default".to_owned(),
        dir: config_dir.to_path_buf(),
    }];
    let mut named = fs::read_dir(config_dir.join("sessions"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_owned();
            (name != "default" && !name.starts_with('.')).then(|| HerdrSession {
                name,
                dir: entry.path(),
            })
        })
        .collect::<Vec<_>>();
    named.sort_by(|left, right| left.name.cmp(&right.name));
    sessions.extend(named);
    sessions
}

fn read_snapshot(path: &Path) -> Option<SessionSnapshot> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > SNAPSHOT_CAP {
        return None;
    }
    let snapshot: SessionSnapshot = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    (snapshot.version <= NEWEST_SNAPSHOT_VERSION).then_some(snapshot)
}

/// A live herdr server accepts on its API socket; a stale socket file left
/// by a crash refuses.
fn socket_answers(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

#[derive(Deserialize)]
struct SessionSnapshot {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    workspaces: Vec<WorkspaceSnapshot>,
}

#[derive(Deserialize)]
struct WorkspaceSnapshot {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    custom_name: Option<String>,
    #[serde(default)]
    tabs: Vec<TabSnapshot>,
    /// Tabless workspaces (herdr's pre-tabs format) keep one layout inline.
    #[serde(default)]
    layout: Option<LayoutSnapshot>,
    #[serde(default)]
    panes: HashMap<u32, PaneSnapshot>,
}

impl WorkspaceSnapshot {
    fn tabs(&self) -> Vec<TabView<'_>> {
        if !self.tabs.is_empty() {
            return self
                .tabs
                .iter()
                .map(|tab| TabView {
                    custom_name: tab.custom_name.as_ref(),
                    layout: Some(&tab.layout),
                    panes: &tab.panes,
                })
                .collect();
        }
        vec![TabView {
            custom_name: None,
            layout: self.layout.as_ref(),
            panes: &self.panes,
        }]
    }
}

#[derive(Deserialize)]
struct TabSnapshot {
    #[serde(default)]
    custom_name: Option<String>,
    layout: LayoutSnapshot,
    #[serde(default)]
    panes: HashMap<u32, PaneSnapshot>,
}

struct TabView<'a> {
    custom_name: Option<&'a String>,
    layout: Option<&'a LayoutSnapshot>,
    panes: &'a HashMap<u32, PaneSnapshot>,
}

impl<'a> TabView<'a> {
    /// Panes in reading order (the layout tree left to right, top to
    /// bottom), then any the layout forgot, so the sidebar matches herdr.
    fn ordered_panes(&self) -> Vec<(u32, &'a PaneSnapshot)> {
        let mut order = Vec::new();
        if let Some(layout) = self.layout {
            layout.collect(&mut order);
        }
        let mut seen = HashSet::new();
        let mut panes = order
            .into_iter()
            .filter(|id| seen.insert(*id))
            .filter_map(|id| self.panes.get(&id).map(|pane| (id, pane)))
            .collect::<Vec<_>>();
        let rest = self
            .panes
            .iter()
            .filter(|(id, _)| !seen.contains(id))
            .collect::<BTreeMap<_, _>>();
        panes.extend(rest.into_iter().map(|(id, pane)| (*id, pane)));
        panes
    }
}

#[derive(Deserialize)]
enum LayoutSnapshot {
    Pane(u32),
    Split {
        first: Box<LayoutSnapshot>,
        second: Box<LayoutSnapshot>,
    },
}

impl LayoutSnapshot {
    fn collect(&self, out: &mut Vec<u32>) {
        match self {
            Self::Pane(id) => out.push(*id),
            Self::Split { first, second } => {
                first.collect(out);
                second.collect(out);
            }
        }
    }
}

#[derive(Deserialize)]
struct PaneSnapshot {
    cwd: PathBuf,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    agent_name: Option<String>,
    #[serde(default)]
    managed_agent_kind: Option<String>,
    #[serde(default)]
    agent_session: Option<PaneAgentSession>,
}

#[derive(Deserialize)]
struct PaneAgentSession {
    agent: String,
    kind: String,
    value: String,
}

/// A plausible plan for screenshots: two workspaces with conversations,
/// a fresh agent, and terminals.
#[cfg(test)]
pub(crate) fn preview_plan() -> HerdrPlan {
    let item = |workspace: &str, pane: u32, cwd: &str, action: HerdrAction| HerdrItem {
        key: format!("default/{workspace}/{pane}/{cwd}"),
        cwd: cwd.to_owned(),
        title: None,
        workspace: format!("default/{workspace}"),
        action,
    };
    let resume = |kind: AgentKind, id: &str, cwd: &str| {
        HerdrAction::Resume(HistoryEntry {
            id: id.to_owned(),
            kind,
            cwd: cwd.to_owned(),
            title: None,
            transcript_path: String::new(),
            last_active_at: diri_proto::DateMillis(0.0),
            created_at: None,
            cwd_exists: true,
        })
    };
    HerdrPlan {
        items: vec![
            item(
                "w1",
                1,
                "/Users/you/checkout",
                resume(AgentKind::CLAUDE_CODE, "a", "/Users/you/checkout"),
            ),
            item(
                "w1",
                2,
                "/Users/you/checkout",
                resume(AgentKind::CODEX, "b", "/Users/you/checkout"),
            ),
            item("w1", 3, "/Users/you/checkout", HerdrAction::Terminal),
            item(
                "w2",
                4,
                "/Users/you/api",
                resume(AgentKind::CLAUDE_CODE, "c", "/Users/you/api"),
            ),
            item(
                "w2",
                5,
                "/Users/you/api",
                HerdrAction::Start(AgentKind::new("opencode")),
            ),
            item("w2", 6, "/Users/you/api", HerdrAction::Terminal),
        ],
        found: true,
        missing_folders: 0,
        already_in_diri: 0,
        herdr_running: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct Fixture {
        home: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                home: TempDir::new().expect("temp home"),
            }
        }

        fn roots(&self) -> HerdrRoots {
            HerdrRoots::in_home(self.home.path(), None)
        }

        fn folder(&self, name: &str) -> String {
            let path = self.home.path().join(name);
            fs::create_dir_all(&path).expect("folder");
            path.to_string_lossy().into_owned()
        }

        fn session(&self, name: Option<&str>, json: &str) {
            let config = self.home.path().join(".config/herdr");
            let dir = name.map_or(config.clone(), |name| config.join("sessions").join(name));
            fs::create_dir_all(&dir).expect("session dir");
            fs::write(dir.join("session.json"), json).expect("session.json");
        }

        fn claude_transcript(&self, cwd: &str, id: &str) {
            let slug = cwd.replace(['/', '.'], "-");
            let dir = self.home.path().join(".claude/projects").join(slug);
            fs::create_dir_all(&dir).expect("claude project");
            fs::write(
                dir.join(format!("{id}.jsonl")),
                format!(
                    "{}\n",
                    serde_json::json!({
                        "type": "user",
                        "cwd": cwd,
                        "message": {"role": "user", "content": "fix the flaky login test"}
                    })
                ),
            )
            .expect("claude transcript");
        }

        fn codex_transcript(&self, cwd: &str, id: &str) {
            let dir = self.home.path().join(".codex/sessions/2026/09/24");
            fs::create_dir_all(&dir).expect("codex day");
            fs::write(
                dir.join(format!("rollout-2026-09-24T10-00-00-{id}.jsonl")),
                format!(
                    "{}\n",
                    serde_json::json!({
                        "type": "session_meta",
                        "payload": {"id": id, "cwd": cwd, "source": "cli"}
                    })
                ),
            )
            .expect("codex transcript");
        }
    }

    const CLAUDE_ID: &str = "70cc4d7a-c6ae-48fc-a185-c9d9e411dbe7";
    const CODEX_ID: &str = "0199a1b2-7c3d-7e4f-8a9b-0c1d2e3f4a5b";

    fn current_snapshot(app: &str, api: &str) -> String {
        serde_json::json!({
            "version": 3,
            "workspaces": [
                {
                    "id": "w1",
                    "custom_name": "checkout",
                    "identity_cwd": app,
                    "tabs": [{
                        "custom_name": null,
                        "layout": {"Split": {
                            "direction": "Vertical",
                            "ratio": 0.5,
                            "first": {"Pane": 2},
                            "second": {"Split": {
                                "direction": "Horizontal",
                                "ratio": 0.5,
                                "first": {"Pane": 1},
                                "second": {"Pane": 3}
                            }}
                        }},
                        "panes": {
                            "1": {"cwd": app, "agent_session": {
                                "source": "herdr:codex", "agent": "codex",
                                "kind": "id", "value": CODEX_ID
                            }},
                            "2": {"cwd": app, "label": "login bug", "agent_session": {
                                "source": "herdr:claude", "agent": "claude",
                                "kind": "id", "value": CLAUDE_ID
                            }},
                            "3": {"cwd": app}
                        },
                        "zoomed": false,
                        "focused": 2
                    }],
                    "active_tab": 0
                },
                {
                    "id": "w2",
                    "identity_cwd": api,
                    "tabs": [{
                        "layout": {"Pane": 1},
                        "panes": {"1": {
                            "cwd": api,
                            "agent_name": "reviewer",
                            "managed_agent_kind": "opencode"
                        }},
                        "zoomed": false
                    }]
                }
            ],
            "selected": 0,
            "agent_panel_scope": "AllWorkspaces"
        })
        .to_string()
    }

    fn kinds(plan: &HerdrPlan) -> Vec<String> {
        plan.items
            .iter()
            .map(|item| match &item.action {
                HerdrAction::Resume(entry) => format!("resume {} {}", entry.kind.id(), entry.id),
                HerdrAction::Start(kind) => format!("start {}", kind.id()),
                HerdrAction::Terminal => "terminal".to_owned(),
            })
            .collect()
    }

    #[test]
    fn current_snapshot_resumes_conversations_in_layout_order() {
        let fixture = Fixture::new();
        let app = fixture.folder("app");
        let api = fixture.folder("api");
        fixture.claude_transcript(&app, CLAUDE_ID);
        fixture.codex_transcript(&app, CODEX_ID);
        fixture.session(None, &current_snapshot(&app, &api));

        let plan = plan(&fixture.roots(), &HashSet::new(), &HashSet::new());

        assert!(plan.found);
        assert_eq!(
            kinds(&plan),
            [
                format!("resume claude-code {CLAUDE_ID}"),
                format!("resume codex {CODEX_ID}"),
                "terminal".to_owned(),
                "start opencode".to_owned(),
            ]
        );
        assert_eq!(plan.items[0].title.as_deref(), Some("login bug"));
        assert_eq!(plan.items[3].title.as_deref(), Some("reviewer"));
        assert_eq!(plan.items[0].cwd, app);
        let HerdrAction::Resume(entry) = &plan.items[0].action else {
            panic!("expected a resume");
        };
        assert!(
            entry
                .transcript_path
                .ends_with(&format!("{CLAUDE_ID}.jsonl"))
        );
        assert!(entry.cwd_exists);
        assert_eq!(
            plan.confirmation_detail(),
            "From 2 herdr workspaces: 2 conversations resumed where they left off, \
             1 agent started fresh and 1 terminal."
        );
        assert_eq!(plan.headline(), "4 sessions from herdr");
    }

    #[test]
    fn legacy_tabless_snapshot_and_named_sessions_are_read() {
        let fixture = Fixture::new();
        let app = fixture.folder("app");
        let site = fixture.folder("site");
        fixture.session(
            None,
            &serde_json::json!({
                "workspaces": [{
                    "custom_name": null,
                    "layout": {"Pane": 0},
                    "panes": {"0": {"cwd": app}},
                    "zoomed": false
                }],
                "selected": 0
            })
            .to_string(),
        );
        fixture.session(
            Some("work"),
            &serde_json::json!({
                "version": 3,
                "workspaces": [{
                    "id": "w1",
                    "identity_cwd": site,
                    "tabs": [{"layout": {"Pane": 4}, "panes": {"4": {"cwd": site}}, "zoomed": false}]
                }],
                "selected": 0
            })
            .to_string(),
        );

        let plan = plan(&fixture.roots(), &HashSet::new(), &HashSet::new());

        assert_eq!(kinds(&plan), ["terminal", "terminal"]);
        assert_eq!(plan.items[0].cwd, app);
        assert_eq!(plan.items[1].cwd, site);
        assert_eq!(plan.items[1].workspace, "work/w1");
    }

    #[test]
    fn a_reported_conversation_without_a_transcript_starts_the_agent_fresh() {
        let fixture = Fixture::new();
        let app = fixture.folder("app");
        let api = fixture.folder("api");
        fixture.session(None, &current_snapshot(&app, &api));

        let plan = plan(&fixture.roots(), &HashSet::new(), &HashSet::new());

        assert_eq!(
            kinds(&plan)[..2],
            ["start claude-code".to_owned(), "start codex".to_owned()]
        );
    }

    #[test]
    fn already_open_imported_duplicate_and_missing_panes_are_left_out() {
        let fixture = Fixture::new();
        let app = fixture.folder("app");
        let api = fixture
            .home
            .path()
            .join("gone")
            .to_string_lossy()
            .into_owned();
        fixture.claude_transcript(&app, CLAUDE_ID);
        fixture.codex_transcript(&app, CODEX_ID);
        fixture.session(None, &current_snapshot(&app, &api));
        // The same Claude conversation open in a second named session.
        fixture.session(Some("twin"), &current_snapshot(&app, &api));

        let tracked = HashSet::from([CODEX_ID.to_owned()]);
        let first = plan(&fixture.roots(), &tracked, &HashSet::new());
        assert_eq!(
            kinds(&first),
            [
                format!("resume claude-code {CLAUDE_ID}"),
                "terminal".to_owned(),
                "terminal".to_owned(),
            ]
        );
        assert_eq!(first.missing_folders, 2);
        assert_eq!(first.already_in_diri, 2);

        let imported = first
            .items
            .iter()
            .flat_map(remembered_keys)
            .collect::<HashSet<_>>();
        let second = plan(&fixture.roots(), &tracked, &imported);
        assert!(second.is_empty(), "{:?}", kinds(&second));
        assert!(second.found);
    }

    #[test]
    fn unsafe_ids_newer_formats_and_unknown_agents_never_resume() {
        let fixture = Fixture::new();
        let app = fixture.folder("app");
        fixture.claude_transcript(&app, CLAUDE_ID);
        fixture.session(
            None,
            &serde_json::json!({
                "version": 3,
                "workspaces": [{"id": "w1", "tabs": [{
                    "layout": {"Split": {"direction": "Vertical", "ratio": 0.5,
                        "first": {"Pane": 1}, "second": {"Pane": 2}}},
                    "panes": {
                        "1": {"cwd": app, "agent_session": {"source": "herdr:claude",
                            "agent": "claude", "kind": "id", "value": "../../etc/passwd"}},
                        "2": {"cwd": app, "agent_session": {"source": "herdr:letta",
                            "agent": "letta", "kind": "id", "value": CLAUDE_ID}}
                    },
                    "zoomed": false
                }]}],
                "selected": 0
            })
            .to_string(),
        );
        fixture.session(
            Some("future"),
            &serde_json::json!({"version": 4, "workspaces": [], "selected": 0}).to_string(),
        );

        let plan = plan(&fixture.roots(), &HashSet::new(), &HashSet::new());
        assert_eq!(kinds(&plan), ["start claude-code", "terminal"]);
    }

    #[test]
    fn no_herdr_means_nothing_found() {
        let fixture = Fixture::new();
        let plan = plan(&fixture.roots(), &HashSet::new(), &HashSet::new());
        assert!(!plan.found);
        assert!(plan.is_empty());
        assert!(!plan.herdr_running);
    }

    /// Read-only: prints what an import would do with this Mac's herdr.
    #[test]
    #[ignore = "reads the current user's herdr state"]
    fn live_herdr_plan() {
        let plan = plan(
            &HerdrRoots::current_user(),
            &HashSet::new(),
            &HashSet::new(),
        );
        for item in &plan.items {
            eprintln!("{:?} {} {:?}", item.action, item.cwd, item.title);
        }
        eprintln!(
            "found={} missing={} running={}\n{}",
            plan.found,
            plan.missing_folders,
            plan.herdr_running,
            plan.confirmation_detail()
        );
    }

    #[test]
    fn xdg_config_home_moves_the_herdr_folder() {
        let home = Path::new("/Users/someone");
        assert_eq!(
            HerdrRoots::in_home(home, Some(Path::new("/xdg"))).config_dir,
            Path::new("/xdg/herdr")
        );
        assert_eq!(
            HerdrRoots::in_home(home, Some(Path::new("relative"))).config_dir,
            Path::new("/Users/someone/.config/herdr")
        );
    }
}
