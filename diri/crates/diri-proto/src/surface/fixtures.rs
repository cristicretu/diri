//! Example surfaces for tests and screenshots, as an agent might write them.
//! Compiled only for tests and the `fixtures` feature: Diri ships no
//! built-in surface, and nothing here is a preset an agent can ask for.

use super::action::Action;
use super::identity::Icon;
use super::node::{
    Binding, ButtonStyle, Cell, ChecklistItem, KvItem, Node, NodeKind, SelectOption, Tab, Tone,
    Trend,
};
use super::{Scope, SurfaceDoc};

fn text(t: &str) -> NodeKind {
    NodeKind::Text {
        text: t.into(),
        tone: None,
        mono: false,
        bind: None,
        size: None,
        weight: None,
        color: None,
        truncate: false,
    }
}

/// The agent picks each chip's glyph and words; Diri knows no vendor.
fn item(id: &str, t: &str, link: Option<(&str, &str, &str)>, checked: bool) -> ChecklistItem {
    ChecklistItem {
        id: id.into(),
        text: t.into(),
        icon: link.map(|(_, icon, _)| Icon::Symbol(icon.into())),
        link_label: link.map(|(_, _, label)| label.into()),
        checked,
        link: link.map(|(url, _, _)| url.to_owned()),
        detail: None,
    }
}

fn link(id: &str, url: &str, label: &str, detail: Option<&str>, icon: &str, badge: &str) -> Node {
    Node::with_id(
        id,
        NodeKind::Link {
            url: url.into(),
            label: Some(label.into()),
            detail: detail.map(str::to_owned),
            icon: Some(Icon::Symbol(icon.into())),
            badge: Some(badge.into()),
        },
    )
}

fn metric(
    id: &str,
    label: &str,
    value: &str,
    delta: Option<(&str, Trend)>,
    bind: Option<Binding>,
) -> Node {
    Node::with_id(
        id,
        NodeKind::Metric {
            label: label.into(),
            value: value.into(),
            delta: delta.map(|(d, _)| d.to_owned()),
            trend: delta.map(|(_, t)| t),
            caption: None,
            bind,
        },
    )
}

/// "What's for Today": the day's numbers, next steps as a live checklist,
/// the sessions doing the work, and the threads, PRs and issues behind it.
/// `sessions` become live session rows (up to four).
pub fn whats_for_today(id: &str, scope: Scope, sessions: &[String]) -> SurfaceDoc {
    let mut doc = SurfaceDoc::new(id, "What's for Today", scope);
    doc.subtitle = Some("Priorities, next steps and live agents".into());
    doc.pinned = true;

    let metrics = Node::with_id(
        "metrics",
        NodeKind::Grid {
            columns: 4,
            children: vec![
                metric(
                    "m-working",
                    "Agents working",
                    "0",
                    None,
                    Some(Binding::WorkingCount),
                ),
                metric(
                    "m-needs",
                    "Need you",
                    "0",
                    None,
                    Some(Binding::NeedsInputCount),
                ),
                metric(
                    "m-shipped",
                    "Shipped this week",
                    "7",
                    Some(("+3", Trend::Up)),
                    None,
                ),
                metric(
                    "m-review",
                    "Open reviews",
                    "4",
                    Some(("-2", Trend::Down)),
                    None,
                ),
            ],
        },
    );

    let next = Node::with_id(
        "next",
        NodeKind::Card {
            title: Some("Next up".into()),
            subtitle: Some("Tick as you go; your agent hears about it".into()),
            tone: None,
            children: vec![
                Node::with_id(
                    "today",
                    NodeKind::Checklist {
                        title: None,
                        items: vec![
                            item(
                                "standup",
                                "Post standup notes",
                                Some((
                                    "https://acme.slack.com/archives/C024BE91L/p1700000000000100",
                                    "slack",
                                    "Thread",
                                )),
                                true,
                            ),
                            item(
                                "review-812",
                                "Review the checkout retry PR",
                                Some((
                                    "https://github.com/acme/web/pull/812",
                                    "arrow.triangle.pull",
                                    "web#812",
                                )),
                                false,
                            ),
                            item(
                                "eng-142",
                                "Unblock ENG-142 rate limiter rollout",
                                Some((
                                    "https://linear.app/acme/issue/ENG-142/rate-limiter-rollout",
                                    "linear",
                                    "ENG-142",
                                )),
                                false,
                            ),
                            item(
                                "spec",
                                "Read the Q4 onboarding spec",
                                Some((
                                    "https://www.notion.so/acme/Q4-onboarding-spec-0a1b2c",
                                    "notion",
                                    "Spec",
                                )),
                                false,
                            ),
                            item("release", "Cut 2.4 release notes", None, false),
                        ],
                        notify: true,
                    },
                ),
                Node::with_id(
                    "progress",
                    NodeKind::Progress {
                        value: 1.0,
                        max: 5.0,
                        label: None,
                        tone: Some(Tone::Success),
                        track: Some("today".into()),
                    },
                ),
            ],
        },
    );

    let mut live: Vec<Node> = sessions
        .iter()
        .take(4)
        .enumerate()
        .map(|(index, session)| {
            Node::with_id(
                format!("session-{index}"),
                NodeKind::Session {
                    session: session.clone(),
                    label: None,
                },
            )
        })
        .collect();
    if live.is_empty() {
        live.push(Node::with_id(
            "no-sessions",
            NodeKind::Text {
                text: "Agents you mention here show their live status.".into(),
                tone: Some(Tone::Muted),
                mono: false,
                bind: None,
                size: None,
                weight: None,
                color: None,
                truncate: false,
            },
        ));
    }
    let agents = Node::with_id(
        "agents",
        NodeKind::Card {
            title: Some("Live agents".into()),
            subtitle: None,
            tone: None,
            children: live,
        },
    );

    let links = Node::with_id(
        "links",
        NodeKind::Card {
            title: Some("Threads & work".into()),
            subtitle: None,
            tone: None,
            children: vec![
                link(
                    "l-slack",
                    "https://acme.slack.com/archives/C024BE91L/p1700000000000200",
                    "#launch: pricing page copy",
                    Some("3 new replies"),
                    "slack",
                    "Thread",
                ),
                link(
                    "l-pr",
                    "https://github.com/acme/web/pull/812",
                    "Retry failed checkouts with backoff",
                    Some("Checks passing · 2 approvals"),
                    "arrow.triangle.pull",
                    "PR",
                ),
                link(
                    "l-linear",
                    "https://linear.app/acme/issue/ENG-142/rate-limiter-rollout",
                    "Rate limiter rollout",
                    Some("In progress · due today"),
                    "linear",
                    "Issue",
                ),
                link(
                    "l-notion",
                    "https://www.notion.so/acme/Q4-onboarding-spec-0a1b2c",
                    "Q4 onboarding spec",
                    None,
                    "notion",
                    "Doc",
                ),
            ],
        },
    );

    let week = Node::with_id(
        "week",
        NodeKind::Table {
            columns: vec!["Item".into(), "Owner".into(), "Status".into()],
            rows: vec![
                vec![
                    Cell::Text("Billing v2 migration".into()),
                    Cell::Text("Codex".into()),
                    Cell::Rich {
                        text: "On track".into(),
                        link: None,
                        tone: Some(Tone::Success),
                    },
                ],
                vec![
                    Cell::Text("Flaky e2e suite".into()),
                    Cell::Text("Claude".into()),
                    Cell::Rich {
                        text: "At risk".into(),
                        link: None,
                        tone: Some(Tone::Warning),
                    },
                ],
                vec![
                    Cell::Text("Design review".into()),
                    Cell::Text("You".into()),
                    Cell::Rich {
                        text: "Thursday".into(),
                        link: None,
                        tone: Some(Tone::Muted),
                    },
                ],
            ],
        },
    );

    let tabs = Node::with_id(
        "views",
        NodeKind::Tabs {
            tabs: vec![
                Tab {
                    id: "tab-today".into(),
                    label: "Today".into(),
                    children: vec![Node::with_id(
                        "columns",
                        NodeKind::Grid {
                            columns: 2,
                            children: vec![
                                next,
                                Node::with_id(
                                    "side",
                                    NodeKind::Stack {
                                        gap: Some(12.0),
                                        children: vec![agents, links],
                                    },
                                ),
                            ],
                        },
                    )],
                },
                Tab {
                    id: "tab-week".into(),
                    label: "This week".into(),
                    children: vec![week],
                },
            ],
        },
    );

    let actions = Node::with_id(
        "actions",
        NodeKind::Row {
            gap: Some(8.0),
            wrap: true,
            children: vec![
                Node::with_id(
                    "plan",
                    NodeKind::Button {
                        label: "Plan tomorrow".into(),
                        action: Action::SendPrompt {
                            session: None,
                            text: "Look at what's left on my What's for Today surface and draft tomorrow's priorities in it.".into(),
                        },
                        variant: ButtonStyle::Primary,
                        icon: None,
                    },
                ),
                Node::with_id(
                    "triage",
                    NodeKind::Button {
                        label: "Triage new threads".into(),
                        action: Action::Event {
                            name: "triage".into(),
                            payload: None,
                        },
                        variant: ButtonStyle::Secondary,
                        icon: None,
                    },
                ),
                Node::with_id(
                    "focus",
                    NodeKind::Select {
                        label: Some("Focus".into()),
                        options: vec![
                            SelectOption { value: "ship".into(), label: "Ship".into() },
                            SelectOption { value: "review".into(), label: "Review".into() },
                            SelectOption { value: "deep".into(), label: "Deep work".into() },
                        ],
                        default: Some("ship".into()),
                        notify: true,
                        value: None,
                    },
                ),
                Node::with_id(
                    "dnd",
                    NodeKind::Toggle {
                        label: "Hold pings until 2pm".into(),
                        default: false,
                        notify: true,
                        value: None,
                    },
                ),
            ],
        },
    );

    let summary = Node::with_id(
        "summary",
        NodeKind::KvList {
            items: vec![
                KvItem {
                    key: "Release".into(),
                    value: "2.4 freeze Friday".into(),
                    link: None,
                    tone: Some(Tone::Warning),
                },
                KvItem {
                    key: "On call".into(),
                    value: "You until 18:00".into(),
                    link: None,
                    tone: None,
                },
            ],
        },
    );

    doc.root = vec![
        metrics,
        Node::new(text(
            "Two reviews waiting and the rate limiter rollout is the one thing that can slip today.",
        )),
        summary,
        tabs,
        Node::new(NodeKind::Divider),
        actions,
    ];
    doc
}
