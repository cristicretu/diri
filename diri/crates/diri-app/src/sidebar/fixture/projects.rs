//! Six projects with a working day's mix of sessions, interleaved in time, so
//! previews show what telling projects apart looks like.

use super::*;

pub(super) fn make(now: f64) -> SidebarPreviewFixture {
    let projects = [
        project("preview-api", "/Users/preview/Projects/api", "api"),
        project("preview-web", "/Users/preview/Projects/web", "web"),
        project("preview-ios", "/Users/preview/Projects/ios", "ios"),
        project("preview-infra", "/Users/preview/Projects/infra", "infra"),
        project("preview-docs", "/Users/preview/Projects/docs", "docs"),
        project(
            "preview-dirijor",
            "/Users/preview/Projects/dirijor",
            "Dirijor",
        ),
    ];
    let [api, web, ios, infra, docs, dirijor] = &projects;
    let permission = |summary: &str, risk_hint| NeedsInputDetail {
        kind: NeedsInputKind::Permission,
        source: NeedsInputSource::ClaudePermissionHook,
        tool_name: Some("Bash".into()),
        summary: summary.into(),
        prompt_excerpt: None,
        options: None,
        risk_hint,
        occurred_at: DateMillis(now - 45_000.0),
    };
    let idle = SessionStatus::Idle;
    let working = SessionStatus::Working;
    let asking = SessionStatus::NeedsInput(NeedsInputKind::Permission);
    let sessions: Vec<SessionRecord> = vec![
        session(
            "hue-api-1",
            AgentKind::CLAUDE_CODE,
            api,
            "Paginate the invoices endpoint",
            working.clone(),
            Some("invoices-cursor"),
            now - minutes(4.0),
        )
        .into(),
        session(
            "hue-web-1",
            AgentKind::CODEX,
            web,
            "Move checkout to server actions",
            asking.clone(),
            Some("checkout"),
            now - minutes(9.0),
        )
        .needs_input(permission(
            "Wants to run the migration",
            RiskHint::Destructive,
        ))
        .into(),
        session(
            "hue-ios-1",
            AgentKind::CLAUDE_CODE,
            ios,
            "Fix the keyboard inset on iPad",
            idle.clone(),
            Some("fix/inset"),
            now - minutes(16.0),
        )
        .completed(now - minutes(2.0))
        .into(),
        session(
            "hue-api-2",
            AgentKind::CODEX,
            api,
            "Rate-limit the webhook receiver",
            idle.clone(),
            Some("webhooks"),
            now - minutes(27.0),
        )
        .completed(now - minutes(20.0))
        .seen(now - minutes(12.0))
        .into(),
        session(
            "hue-infra-1",
            AgentKind::CLAUDE_CODE,
            infra,
            "Rotate the staging certificates",
            working.clone(),
            Some("certs"),
            now - minutes(33.0),
        )
        .into(),
        session(
            "hue-dirijor-1",
            AgentKind::CURSOR,
            dirijor,
            "Polish the left sidebar hierarchy",
            working,
            Some("sidebar-craft"),
            now - minutes(41.0),
        )
        .into(),
        session(
            "hue-web-2",
            AgentKind::GEMINI,
            web,
            "Audit bundle size after the upgrade",
            idle.clone(),
            Some("perf/bundle"),
            now - minutes(58.0),
        )
        .completed(now - minutes(30.0))
        .into(),
        session(
            "hue-docs-1",
            AgentKind::CLAUDE_CODE,
            docs,
            "Rewrite the quickstart for 2.0",
            asking,
            Some("quickstart"),
            now - hours(1.4),
        )
        .needs_input(permission(
            "Wants to fetch the published site",
            RiskHint::Network,
        ))
        .into(),
        session(
            "hue-api-3",
            AgentKind::SHELL,
            api,
            "Dev server · localhost:8080",
            idle.clone(),
            Some("main"),
            now - hours(2.0),
        )
        .seen(now - minutes(50.0))
        .into(),
        session(
            "hue-ios-2",
            AgentKind::CODEX,
            ios,
            "Adopt the new navigation stack",
            idle.clone(),
            Some("nav-stack"),
            now - hours(3.1),
        )
        .seen(now - hours(2.0))
        .hibernation(HibernationInfo {
            since: DateMillis(now - minutes(35.0)),
            reason: HibernationReason::Idle,
            tree_pids: vec![4401],
            tree_start_times: None,
        })
        .into(),
        session(
            "hue-infra-2",
            AgentKind::CODEX,
            infra,
            "Trim the CI cache keys",
            idle.clone(),
            Some("ci-cache"),
            now - hours(4.5),
        )
        .completed(now - hours(4.0))
        .seen(now - hours(3.9))
        .into(),
        session(
            "hue-dirijor-2",
            AgentKind::CLAUDE_CODE,
            dirijor,
            "Review the projection tests",
            idle,
            Some("sidebar-craft"),
            now - hours(6.0),
        )
        .completed(now - hours(5.0))
        .seen(now - hours(4.8))
        .into(),
    ];
    let mut prefs = Prefs {
        sidebar_visible: true,
        sidebar_project_order: projects.iter().map(|project| project.id.clone()).collect(),
        sidebar_session_order: sessions.iter().map(|session| session.id.clone()).collect(),
        ..Prefs::default()
    };
    prefs.normalize();
    SidebarPreviewFixture {
        selected_session_id: Some(sessions[3].id.clone()),
        list: SessionListResult {
            sessions,
            projects: projects.to_vec(),
        },
        prefs,
    }
}
