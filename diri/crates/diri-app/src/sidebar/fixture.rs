use diri_proto::{
    AgentKind, ArtifactKind, DateMillis, ExitInfo, ExitReason, HibernationInfo, HibernationReason,
    NeedsInputDetail, NeedsInputKind, NeedsInputSource, PortInfo, PrCheck, PrDiscussionItem,
    Project, ProjectId, PullRequestStatus, Resumability, RiskHint, SessionArtifact, SessionId,
    SessionListResult, SessionRecord, SessionStatus, TitleSource,
};

use crate::store::{InspectorTab, Prefs, SessionStore};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PreviewScenario {
    #[default]
    Typical,
    Stress,
    Empty,
    Artifacts,
}

impl PreviewScenario {
    pub fn from_env(value: Option<&str>) -> Self {
        match value.map(str::to_ascii_lowercase).as_deref() {
            Some("stress") => Self::Stress,
            Some("empty") => Self::Empty,
            Some("artifacts") => Self::Artifacts,
            _ => Self::Typical,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SidebarPreviewFixture {
    pub list: SessionListResult,
    pub selected_session_id: Option<SessionId>,
    pub prefs: Prefs,
}

impl SidebarPreviewFixture {
    /// Deterministic sidebar contents for preview mode: mock dates, account
    /// identity, and usage values, with no daemon connection.
    pub fn make(scenario: PreviewScenario) -> Self {
        if scenario == PreviewScenario::Empty {
            return Self {
                list: SessionListResult {
                    sessions: Vec::new(),
                    projects: Vec::new(),
                },
                selected_session_id: None,
                prefs: Prefs::default(),
            };
        }

        // A stable clock makes screenshot output deterministic while retaining
        // the exact relative intervals used by the Swift fixture.
        let now = 1_750_000_000_000.0;
        let dirijor = project(
            "preview-dirijor",
            "/Users/preview/Projects/dirijor",
            "Dirijor",
        );
        let anara = project("preview-anara", "/Users/preview/Projects/anara", "Anara");
        let settings = project(
            "preview-settings-kit",
            "/Users/preview/Projects/settings-kit",
            "Settings Kit",
        );

        let mut codex: SessionRecord = session(
            "preview-codex",
            AgentKind::CODEX,
            &dirijor,
            "Polish the left sidebar hierarchy",
            SessionStatus::Working,
            Some("sidebar-craft"),
            now - minutes(18.0),
        )
        .memory(3_650_000_000)
        .into();
        if scenario == PreviewScenario::Artifacts {
            codex.title = "Move repository cloning into the background".into();
            let pull_request_url = "https://github.com/acme/dirijor/pull/63";
            codex.artifacts = Some(vec![SessionArtifact {
                kind: ArtifactKind::PullRequest,
                url: pull_request_url.into(),
                first_seen_at: DateMillis(now - minutes(14.0)),
            }]);
            codex.pull_requests = Some(vec![PullRequestStatus {
                url: pull_request_url.into(),
                number: 63,
                title: Some("Move repository cloning into the background".into()),
                author: Some("antfu".into()),
                body: Some(
                    r#"## What changed

Repository cloning now leaves the foreground path immediately while progress and failures remain visible to the active session.

- The checkout starts in a background worker.
- Progress stays attached to the active agent.
- Failures include a **retryable** explanation instead of blocking the UI.

> The foreground path no longer waits for network I/O.

### Verification

- [x] Existing repositories still open immediately
- [x] Clone failures remain visible
- [ ] Add a slow-network integration case

```rust
tokio::spawn(async move { clone_repository(request).await });
```

[Open the implementation notes](https://github.com/acme/dirijor/pull/63)"#
                        .into(),
                ),
                base_ref_name: Some("main".into()),
                head_ref_name: Some("refactor/repository-cloning".into()),
                state: "OPEN".into(),
                is_draft: false,
                review_decision: Some("APPROVED".into()),
                mergeable: Some("MERGEABLE".into()),
                merge_state_status: Some("CLEAN".into()),
                additions: 431,
                deletions: 381,
                changed_files: 8,
                comment_count: 2,
                review_count: 1,
                resolved_threads: Some(4),
                total_threads: Some(4),
                checks_passed: 2,
                checks_failed: 0,
                checks_pending: 1,
                checks: Some(vec![
                    PrCheck {
                        name: "CI / macOS 15".into(),
                        result: "pending".into(),
                        detail: Some("IN_PROGRESS".into()),
                        url: Some("https://github.com/acme/dirijor/actions/runs/103".into()),
                    },
                    PrCheck {
                        name: "Swift tests".into(),
                        result: "pass".into(),
                        detail: Some("SUCCESS".into()),
                        url: Some("https://github.com/acme/dirijor/actions/runs/101".into()),
                    },
                    PrCheck {
                        name: "Rust / clippy".into(),
                        result: "pass".into(),
                        detail: Some("SUCCESS".into()),
                        url: Some("https://github.com/acme/dirijor/actions/runs/102".into()),
                    },
                ]),
                discussion: Some(vec![
                    PrDiscussionItem {
                        kind: "comment".into(),
                        author: "ruru".into(),
                        body: "Please make sure the background task still reports clone failures in the active session.".into(),
                        state: None,
                        created_at: None,
                        url: Some(format!("{pull_request_url}#issuecomment-1")),
                    },
                    PrDiscussionItem {
                        kind: "comment".into(),
                        author: "antfu".into(),
                        body: "Added coverage for cancellation and for a failed remote fetch.".into(),
                        state: None,
                        created_at: None,
                        url: Some(format!("{pull_request_url}#issuecomment-2")),
                    },
                    PrDiscussionItem {
                        kind: "review".into(),
                        author: "yyx990803".into(),
                        body: "The lifecycle reads clearly now. Nice separation between scheduling and progress reporting.".into(),
                        state: Some("APPROVED".into()),
                        created_at: None,
                        url: Some(format!("{pull_request_url}#pullrequestreview-3")),
                    },
                ]),
                fetched_at: DateMillis(now),
            }]);
        }
        let claude: SessionRecord = session(
            "preview-claude",
            AgentKind::CLAUDE_CODE,
            &dirijor,
            "Prepare the signed 0.4.4 release",
            SessionStatus::NeedsInput(NeedsInputKind::Permission),
            Some("release/0.4.4"),
            now - minutes(42.0),
        )
        .needs_input(NeedsInputDetail {
            kind: NeedsInputKind::Permission,
            source: NeedsInputSource::ClaudePermissionHook,
            tool_name: Some("Bash".into()),
            summary: "Wants to publish the release tag".into(),
            prompt_excerpt: None,
            options: None,
            risk_hint: RiskHint::Network,
            occurred_at: DateMillis(now - 45_000.0),
        })
        .into();
        // Two agents the Codex session spawned for itself, so the preview
        // exercises the indent rail with one continuing and one terminating
        // segment, plus a second level under the first child.
        let cursor: SessionRecord = session(
            "preview-cursor",
            AgentKind::CURSOR,
            &dirijor,
            "Fix project switching focus",
            SessionStatus::Idle,
            Some("fix/focus-neighbor"),
            now - minutes(68.0),
        )
        .child_of(&codex.id)
        .completed(now - 90_000.0)
        .seen(now - minutes(12.0))
        .into();
        let spawned_review: SessionRecord = session(
            "preview-spawned-review",
            AgentKind::CLAUDE_CODE,
            &dirijor,
            "Review the projection tests",
            SessionStatus::Working,
            Some("sidebar-craft"),
            now - minutes(6.0),
        )
        .child_of(&codex.id)
        .into();
        let spawned_deep: SessionRecord = session(
            "preview-spawned-deep",
            AgentKind::CODEX,
            &dirijor,
            "Check the rail geometry",
            SessionStatus::Idle,
            Some("sidebar-craft"),
            now - minutes(3.0),
        )
        .child_of(&cursor.id)
        .seen(now - minutes(1.0))
        .into();
        let shell: SessionRecord = session(
            "preview-shell",
            AgentKind::SHELL,
            &dirijor,
            "Dev server · localhost:3000",
            SessionStatus::Idle,
            Some("main"),
            now - hours(2.2),
        )
        .seen(now - 60_000.0)
        .ports(vec![PortInfo {
            port: 3000,
            process_name: "node".into(),
        }])
        .into();
        let gemini: SessionRecord = session(
            "preview-gemini",
            AgentKind::GEMINI,
            &anara,
            "Trace PDF selection coordinate drift",
            SessionStatus::Working,
            Some("pdf-selection"),
            now - minutes(11.0),
        )
        .into();
        let question: SessionRecord = session(
            "preview-question",
            AgentKind::CLAUDE_CODE,
            &anara,
            "Rework the document import flow",
            SessionStatus::NeedsInput(NeedsInputKind::Question),
            Some("import-flow"),
            now - minutes(31.0),
        )
        .needs_input(NeedsInputDetail {
            kind: NeedsInputKind::Question,
            source: NeedsInputSource::ClaudeNotificationHook,
            tool_name: None,
            summary: "Which empty-state direction should I use?".into(),
            prompt_excerpt: None,
            options: Some(vec!["Editorial".into(), "Compact".into()]),
            risk_hint: RiskHint::Neutral,
            occurred_at: DateMillis(now - 120_000.0),
        })
        .into();
        let sleeping: SessionRecord = session(
            "preview-sleeping",
            AgentKind::CODEX,
            &settings,
            "Audit hydration-safe input behavior",
            SessionStatus::Idle,
            Some("headless/forms"),
            now - hours(5.4),
        )
        .seen(now - hours(3.0))
        .hibernation(HibernationInfo {
            since: DateMillis(now - minutes(35.0)),
            reason: HibernationReason::Idle,
            tree_pids: vec![4401],
            tree_start_times: None,
        })
        .into();
        let archived: SessionRecord = session(
            "preview-archived",
            AgentKind::CLAUDE_CODE,
            &settings,
            "Compare compound component APIs",
            SessionStatus::Exited(ExitInfo {
                reason: ExitReason::Archived,
                code: None,
                signal: None,
            }),
            Some("composition-notes"),
            now - hours(48.0),
        )
        .archived(now - hours(20.0))
        .into();

        let mut sessions = vec![
            codex.clone(),
            claude.clone(),
            cursor.clone(),
            spawned_review,
            spawned_deep,
            shell,
            gemini,
            question,
            sleeping,
            archived,
        ];
        if scenario == PreviewScenario::Stress {
            sessions.extend([
                session(
                    "preview-long",
                    AgentKind::CLAUDE_CODE,
                    &dirijor,
                    "Investigate why exceptionally long generated conversation titles truncate unpredictably",
                    SessionStatus::Starting,
                    Some("a-very-long-branch-name-for-layout-testing"),
                    now - minutes(4.0),
                )
                .into(),
                session(
                    "preview-ended",
                    AgentKind::CURSOR,
                    &anara,
                    "Cursor accessibility pass",
                    SessionStatus::Exited(ExitInfo {
                        reason: ExitReason::Exited,
                        code: Some(0),
                        signal: None,
                    }),
                    Some("accessibility"),
                    now - hours(7.0),
                )
                .into(),
                session(
                    "preview-memory",
                    AgentKind::CODEX,
                    &settings,
                    "Profile the dense session list",
                    SessionStatus::Working,
                    Some("perf/sidebar"),
                    now - minutes(9.0),
                )
                .memory(7_900_000_000)
                .into(),
            ]);
        }

        let mut prefs = Prefs {
            sidebar_project_order: vec![dirijor.id.clone(), anara.id.clone(), settings.id.clone()],
            sidebar_session_order: sessions.iter().map(|session| session.id.clone()).collect(),
            sidebar_pinned_sessions: vec![claude.id.clone()],
            sidebar_collapsed_projects: vec![anara.id.clone()],
            sidebar_expanded_archives: vec![settings.id.clone()],
            ..Prefs::default()
        };
        prefs.normalize();
        if scenario == PreviewScenario::Artifacts {
            prefs.inspector_open = true;
            prefs.inspector_width = 480.0;
            prefs.inspector_tab = InspectorTab::Artifacts;
        }
        Self {
            list: SessionListResult {
                sessions,
                projects: vec![dirijor, anara, settings],
            },
            selected_session_id: Some(codex.id),
            prefs,
        }
    }

    pub fn into_store(self) -> SessionStore {
        let (mut store, _effects) = SessionStore::headless(self.prefs);
        store.hydrate(self.list);
        if let Some(id) = self.selected_session_id {
            store.select(id);
        }
        store
    }
}

fn project(id: &str, root: &str, name: &str) -> Project {
    Project {
        id: ProjectId::new(id),
        root: root.into(),
        name: name.into(),
        pinned_order: None,
    }
}

struct SessionBuilder(SessionRecord);

impl SessionBuilder {
    fn memory(mut self, bytes: u64) -> Self {
        self.0.memory_bytes = Some(bytes);
        self
    }

    fn completed(mut self, millis: f64) -> Self {
        self.0.last_turn_completed_at = Some(DateMillis(millis));
        self
    }

    fn seen(mut self, millis: f64) -> Self {
        self.0.last_seen_at = Some(DateMillis(millis));
        self
    }

    fn needs_input(mut self, detail: NeedsInputDetail) -> Self {
        self.0.needs_input = Some(detail);
        self
    }

    fn hibernation(mut self, hibernation: HibernationInfo) -> Self {
        self.0.hibernation = Some(hibernation);
        self
    }

    fn archived(mut self, millis: f64) -> Self {
        self.0.archived_at = Some(DateMillis(millis));
        self
    }

    fn ports(mut self, ports: Vec<PortInfo>) -> Self {
        self.0.listening_ports = Some(ports);
        self
    }

    /// Marks this session as spawned by another through the MCP tools, which
    /// is what nests it under that row in the sidebar.
    fn child_of(mut self, parent: &SessionId) -> Self {
        self.0.parent = Some(parent.clone());
        self
    }
}

impl From<SessionBuilder> for SessionRecord {
    fn from(value: SessionBuilder) -> Self {
        value.0
    }
}

fn session(
    id: &str,
    kind: AgentKind,
    project: &Project,
    title: &str,
    status: SessionStatus,
    branch: Option<&str>,
    created: f64,
) -> SessionBuilder {
    let resumability = if matches!(
        kind.id(),
        AgentKind::CLAUDE_CODE_ID | AgentKind::CODEX_ID | AgentKind::GEMINI_ID
    ) {
        Resumability::Resumable
    } else {
        Resumability::NotResumable
    };
    SessionBuilder(SessionRecord {
        id: SessionId::new(id),
        kind,
        cwd: project.root.clone(),
        project_id: project.id.clone(),
        worktree_path: None,
        git_branch: branch.map(str::to_owned),
        title: title.into(),
        title_source: TitleSource::AgentProvided,
        agent_session_id: None,
        transcript_path: None,
        status,
        status_evidence: None,
        needs_input: None,
        resumability,
        parent: None,
        created_at: DateMillis(created),
        updated_at: DateMillis(created),
        last_turn_completed_at: None,
        last_seen_at: None,
        pinned: false,
        archived_at: None,
        host: None,
        remote_persistence: None,
        hibernation: None,
        memory_bytes: None,
        artifacts: None,
        pull_requests: None,
        listening_ports: None,
        foreground_agent: None,
    })
}

const fn minutes(value: f64) -> f64 {
    value * 60_000.0
}

const fn hours(value: f64) -> f64 {
    value * 3_600_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typical_fixture_matches_swift_counts_and_preferences() {
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        assert_eq!(fixture.list.projects.len(), 3);
        assert_eq!(fixture.list.sessions.len(), 10);
        assert_eq!(fixture.prefs.sidebar_pinned_sessions.len(), 1);
        assert_eq!(fixture.prefs.sidebar_collapsed_projects.len(), 1);
        assert_eq!(fixture.prefs.sidebar_expanded_archives.len(), 1);
    }

    #[test]
    fn stress_fixture_adds_three_edge_cases() {
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Stress);
        assert_eq!(fixture.list.sessions.len(), 13);
        assert!(
            fixture
                .list
                .sessions
                .iter()
                .any(|session| { session.title.starts_with("Investigate why exceptionally") })
        );
    }

    #[test]
    fn typical_fixture_carries_a_two_level_spawn_tree() {
        let mut store = SidebarPreviewFixture::make(PreviewScenario::Typical).into_store();
        let projection = store.sidebar_projection();
        let group = projection
            .projects
            .iter()
            .find(|group| group.project.id == ProjectId::new("preview-dirijor"))
            .expect("the dirijor group");
        let nested: Vec<_> = group
            .sessions
            .iter()
            .map(|row| (row.id().0.as_str(), row.depth))
            .collect();
        assert_eq!(
            nested,
            vec![
                ("preview-claude", 0),
                ("preview-codex", 0),
                ("preview-cursor", 1),
                ("preview-spawned-deep", 2),
                ("preview-spawned-review", 1),
                ("preview-shell", 0),
            ]
        );
    }

    #[test]
    fn artifacts_fixture_carries_a_rich_pull_request() {
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Artifacts);
        let session = fixture
            .list
            .sessions
            .iter()
            .find(|session| Some(&session.id) == fixture.selected_session_id.as_ref())
            .expect("selected session");
        let pull_request = session
            .pull_requests
            .as_deref()
            .and_then(|pull_requests| pull_requests.first())
            .expect("pull request fixture");
        assert_eq!(pull_request.checks.as_deref().map(<[_]>::len), Some(3));
        assert_eq!(pull_request.discussion.as_deref().map(<[_]>::len), Some(3));
        assert_eq!(fixture.prefs.inspector_tab, InspectorTab::Artifacts);
    }

    #[test]
    fn fixture_hydrates_store_with_swift_selection() {
        let store = SidebarPreviewFixture::make(PreviewScenario::Typical).into_store();
        assert_eq!(
            store.selected_session_id(),
            Some(&SessionId::new("preview-codex"))
        );
        assert_eq!(store.sessions().len(), 10);
    }
}
