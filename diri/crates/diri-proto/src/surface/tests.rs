use serde_json::json;

use super::action::{Action, Authority, Denied, Effect, Gesture, authorize};
use super::link::{LinkKind, classify, is_allowed};
use super::node::{Node, NodeKind};
use super::patch::{Patch, apply, replace_all};
use super::*;

fn today() -> SurfaceDoc {
    template::whats_for_today("today", Scope::Global, &["sess-1".into()])
}

fn parse(value: serde_json::Value) -> Node {
    serde_json::from_value(value).expect("node")
}

fn patch(ops: serde_json::Value) -> Patch {
    serde_json::from_value(json!({ "ops": ops })).expect("patch")
}

#[test]
fn template_is_valid_and_round_trips() {
    let doc = today();
    let count = doc.validate().expect("valid template");
    assert!(
        count > 20,
        "the template exercises many components: {count}"
    );
    let json = serde_json::to_string(&doc).unwrap();
    let back: SurfaceDoc = serde_json::from_str(&json).unwrap();
    assert_eq!(back, doc);
    assert_eq!(doc.checklist_progress(), (1, 5));
}

#[test]
fn agent_json_reads_naturally() {
    let node = parse(json!({
        "type": "card", "id": "c", "title": "Hi",
        "children": [
            {"type": "metric", "label": "PRs", "value": "3", "delta": "+1", "trend": "up"},
            {"type": "checklist", "id": "todo", "items": [{"id": "a", "text": "A"}]},
            {"type": "button", "id": "go", "label": "Go", "action": {"kind": "open_url", "url": "https://x.dev"}},
            {"type": "progress", "value": 2, "max": 4},
            {"type": "text", "bind": {"source": "session_status", "session": "s1"}}
        ]
    }));
    let mut doc = SurfaceDoc::new("s", "T", Scope::Global);
    doc.root = vec![node];
    doc.validate().expect("valid");
}

#[test]
fn unknown_components_are_kept_as_placeholders() {
    let node = parse(json!({"type": "hologram", "id": "h", "beams": 3}));
    assert_eq!(node.kind, NodeKind::Unsupported);
    let mut doc = SurfaceDoc::new("s", "T", Scope::Global);
    doc.root = vec![node];
    doc.validate()
        .expect("a newer component never breaks the surface");
}

#[test]
fn newer_schema_is_refused_for_writing() {
    let mut doc = today();
    doc.schema = SCHEMA_VERSION + 1;
    assert_eq!(doc.validate().unwrap_err().code, "unsupported_schema");
}

fn nested(depth: usize) -> Node {
    let mut node = Node::new(NodeKind::Divider);
    for _ in 1..depth {
        node = Node::new(NodeKind::Stack {
            gap: None,
            children: vec![node],
        });
    }
    node
}

#[test]
fn depth_is_bounded() {
    let mut doc = SurfaceDoc::new("s", "T", Scope::Global);
    doc.root = vec![nested(limits::MAX_DEPTH)];
    doc.validate().expect("at the limit");
    doc.root = vec![nested(limits::MAX_DEPTH + 1)];
    assert_eq!(doc.validate().unwrap_err().code, "too_deep");
}

#[test]
fn node_count_is_bounded() {
    let mut doc = SurfaceDoc::new("s", "T", Scope::Global);
    let row = |n: usize| {
        Node::new(NodeKind::Stack {
            gap: None,
            children: (0..n).map(|_| Node::new(NodeKind::Divider)).collect(),
        })
    };
    // 8 stacks of 199 dividers = 1600 components > MAX_NODES.
    doc.root = (0..8).map(|_| row(199)).collect();
    assert_eq!(doc.validate().unwrap_err().code, "too_many_nodes");
    doc.root = vec![row(limits::MAX_CHILDREN + 1)];
    assert_eq!(doc.validate().unwrap_err().code, "too_many_children");
}

#[test]
fn ids_are_unique_and_required_for_state() {
    let mut doc = SurfaceDoc::new("s", "T", Scope::Global);
    doc.root = vec![
        Node::with_id("a", NodeKind::Divider),
        Node::with_id("a", NodeKind::Divider),
    ];
    assert_eq!(doc.validate().unwrap_err().code, "duplicate_id");
    doc.root = vec![parse(json!({"type": "toggle", "label": "x"}))];
    assert_eq!(doc.validate().unwrap_err().code, "missing_id");
    doc.root = vec![parse(json!({"type": "divider", "id": "../etc"}))];
    assert_eq!(doc.validate().unwrap_err().code, "invalid_id");
}

#[test]
fn unsafe_links_and_images_are_refused() {
    for url in [
        "javascript:alert(1)",
        "file:///etc/passwd",
        "data:text/html,hi",
        "https://",
        "https://ok.dev/with space",
        "vscode://open",
    ] {
        assert!(!is_allowed(url), "{url}");
        let mut doc = SurfaceDoc::new("s", "T", Scope::Global);
        doc.root = vec![parse(json!({"type": "link", "url": url}))];
        assert_eq!(doc.validate().unwrap_err().code, "invalid_url", "{url}");
    }
    let mut doc = SurfaceDoc::new("s", "T", Scope::Global);
    doc.root = vec![parse(json!({"type": "image", "src": "/etc/hosts"}))];
    assert_eq!(doc.validate().unwrap_err().code, "invalid_url");
    doc.root = vec![parse(json!({"type": "image", "src": "assets/../x.png"}))];
    assert_eq!(doc.validate().unwrap_err().code, "invalid_url");
    doc.root = vec![parse(json!({"type": "image", "src": "assets/chart.png"}))];
    doc.validate().expect("an asset beside the surfaces");
}

#[test]
fn links_are_classified_for_badges() {
    assert_eq!(
        classify("https://github.com/acme/web/pull/812"),
        LinkKind::GithubPull {
            repo: "acme/web".into(),
            number: 812
        }
    );
    assert_eq!(
        classify("https://github.com/acme/web/issues/9#c"),
        LinkKind::GithubIssue {
            repo: "acme/web".into(),
            number: 9
        }
    );
    assert_eq!(
        classify("https://linear.app/acme/issue/eng-142/title"),
        LinkKind::Linear {
            key: Some("ENG-142".into())
        }
    );
    assert_eq!(
        classify("https://acme.slack.com/archives/C1/p2"),
        LinkKind::Slack
    );
    assert_eq!(classify("https://www.notion.so/x"), LinkKind::Notion);
    assert_eq!(
        classify("diri://session/abc-1"),
        LinkKind::DiriSession {
            session: "abc-1".into()
        }
    );
    assert_eq!(
        classify("diri://note/n1"),
        LinkKind::DiriNote { note: "n1".into() }
    );
    assert!(!is_allowed("diri://session/../x"));
    assert_eq!(
        classify("https://user:pw@www.example.com:8080/a"),
        LinkKind::Web {
            host: "example.com".into()
        }
    );
    assert_eq!(
        LinkKind::GithubPull {
            repo: "a/b".into(),
            number: 1
        }
        .default_label(),
        "a/b#1"
    );
}

#[test]
fn set_keeps_checklist_state() {
    let mut doc = today();
    doc.interact(&Interaction::Check {
        checklist: "today".into(),
        item: "review-812".into(),
        checked: true,
    })
    .unwrap();
    let p = patch(json!([
        {"op": "set", "id": "m-shipped", "fields": {"value": "8", "delta": "+4"}},
        {"op": "insert", "parent": "agents", "node": {"type": "text", "text": "added"}}
    ]));
    let (next, applied) = apply(&doc, &p).expect("applies");
    assert_eq!(applied.revision, doc.revision + 1);
    assert_eq!(next.state, doc.state, "unrelated patches keep every tick");
    let Some(Node {
        kind: NodeKind::Metric { value, .. },
        ..
    }) = node::find(&next.root, "m-shipped")
    else {
        panic!("metric");
    };
    assert_eq!(value, "8");
}

#[test]
fn replacing_a_checklist_keeps_ticks_of_items_it_keeps() {
    let mut doc = today();
    for item in ["review-812", "spec"] {
        doc.interact(&Interaction::Check {
            checklist: "today".into(),
            item: item.into(),
            checked: true,
        })
        .unwrap();
    }
    let p = patch(json!([{"op": "replace", "id": "today", "node": {
        "type": "checklist", "id": "today",
        "items": [{"id": "review-812", "text": "Review the checkout retry PR"}, {"id": "new", "text": "New"}]
    }}]));
    let (next, _) = apply(&doc, &p).unwrap();
    assert_eq!(next.state.checks.get("today/review-812"), Some(&true));
    assert!(
        !next.state.checks.contains_key("today/spec"),
        "removed items drop their state"
    );
}

#[test]
fn failed_patches_change_nothing() {
    let doc = today();
    let p = patch(json!([
        {"op": "set", "id": "m-shipped", "fields": {"value": "99"}},
        {"op": "remove", "id": "does-not-exist"}
    ]));
    let error = apply(&doc, &p).unwrap_err();
    assert_eq!(error.code, "not_found");
    assert!(error.message.starts_with("op 1:"));
    // A patch that breaks a bound is refused as a whole too.
    let p = patch(json!([
        {"op": "set", "id": "m-shipped", "fields": {"value": "99"}},
        {"op": "insert", "node": {"type": "link", "url": "javascript:x"}}
    ]));
    assert_eq!(apply(&doc, &p).unwrap_err().code, "invalid_url");
    assert_eq!(doc, today(), "apply never mutates its input");
}

#[test]
fn revision_guard() {
    let doc = today();
    let mut p = patch(json!([{"op": "remove", "id": "summary"}]));
    p.expected_revision = Some(doc.revision + 5);
    assert_eq!(apply(&doc, &p).unwrap_err().code, "revision_conflict");
    p.expected_revision = Some(doc.revision);
    apply(&doc, &p).expect("current revision applies");
}

#[test]
fn insert_into_tabs_and_type_is_immutable() {
    let doc = today();
    let p = patch(
        json!([{"op": "insert", "parent": "tab-week", "index": 0, "node": {"type": "heading", "text": "Week"}}]),
    );
    let (next, _) = apply(&doc, &p).unwrap();
    let Some(Node {
        kind: NodeKind::Tabs { tabs },
        ..
    }) = node::find(&next.root, "views")
    else {
        panic!("tabs");
    };
    assert!(matches!(tabs[1].children[0].kind, NodeKind::Heading { .. }));
    let p = patch(json!([{"op": "set", "id": "today", "fields": {"type": "text"}}]));
    assert_eq!(apply(&doc, &p).unwrap_err().code, "invalid_value");
    let p = patch(json!([{"op": "insert", "parent": "m-shipped", "node": {"type": "divider"}}]));
    assert_eq!(
        apply(&doc, &p).unwrap_err().code,
        "not_found",
        "leaves hold no children"
    );
}

#[test]
fn full_replace_keeps_surviving_state() {
    let mut doc = today();
    doc.interact(&Interaction::Toggle {
        id: "dnd".into(),
        on: true,
    })
    .unwrap();
    let mut root = doc.root.clone();
    root.remove(1);
    let (next, _) = replace_all(&doc, Some("Today"), root, None).unwrap();
    assert_eq!(next.title, "Today");
    assert_eq!(next.state.toggles.get("dnd"), Some(&true));
}

#[test]
fn interactions_validate_their_target() {
    let mut doc = today();
    assert!(
        doc.interact(&Interaction::Check {
            checklist: "today".into(),
            item: "spec".into(),
            checked: true
        })
        .unwrap(),
        "the template asks to hear about ticks"
    );
    assert_eq!(
        doc.interact(&Interaction::Check {
            checklist: "today".into(),
            item: "nope".into(),
            checked: true
        })
        .unwrap_err()
        .code,
        "not_found"
    );
    assert_eq!(
        doc.interact(&Interaction::Select {
            id: "focus".into(),
            value: "golf".into()
        })
        .unwrap_err()
        .code,
        "invalid_value"
    );
    assert_eq!(
        doc.interact(&Interaction::Toggle {
            id: "today".into(),
            on: true
        })
        .unwrap_err()
        .code,
        "not_found"
    );
}

#[test]
fn actions_need_a_gesture_and_an_allowed_target() {
    let agents = vec!["helper".to_owned()];
    let authority = Authority {
        owner: Some("owner"),
        agents: &agents,
    };
    let prompt = Action::SendPrompt {
        session: None,
        text: "hi".into(),
    };
    assert_eq!(
        authorize(&prompt, Gesture::Programmatic, authority),
        Err(Denied::NoGesture)
    );
    assert_eq!(
        authorize(&prompt, Gesture::User, authority),
        Ok(Effect::Prompt {
            session: "owner".into(),
            text: "hi".into()
        })
    );
    let elsewhere = Action::SendPrompt {
        session: Some("stranger".into()),
        text: "rm".into(),
    };
    assert_eq!(
        authorize(&elsewhere, Gesture::User, authority),
        Err(Denied::AgentNotAllowed("stranger".into()))
    );
    let helper = Action::SendPrompt {
        session: Some("helper".into()),
        text: "ok".into(),
    };
    assert!(authorize(&helper, Gesture::User, authority).is_ok());
    let orphan = Authority {
        owner: None,
        agents: &[],
    };
    assert_eq!(
        authorize(&Action::Refresh, Gesture::User, orphan),
        Err(Denied::NoAgent)
    );
    assert_eq!(
        authorize(
            &Action::OpenUrl {
                url: "javascript:x".into()
            },
            Gesture::User,
            orphan
        ),
        Err(Denied::UrlNotAllowed)
    );
    assert_eq!(
        authorize(
            &Action::OpenUrl {
                url: "diri://session/s9".into()
            },
            Gesture::User,
            orphan
        ),
        Ok(Effect::OpenSession("s9".into()))
    );
    let skill = Action::RunAppSkill {
        name: "x\" && rm".into(),
        session: None,
    };
    assert_eq!(
        authorize(&skill, Gesture::User, authority),
        Err(Denied::InvalidName)
    );
}

#[test]
fn promotion_copies_tree_and_state() {
    let mut doc = template::whats_for_today(
        "s1",
        Scope::Session {
            session: "sess".into(),
        },
        &[],
    );
    doc.pinned = false;
    doc.interact(&Interaction::Check {
        checklist: "today".into(),
        item: "spec".into(),
        checked: true,
    })
    .unwrap();
    let saved = doc.promoted(
        "saved-1",
        Scope::Project {
            project: "/p".into(),
        },
        42,
    );
    assert!(saved.scope.is_saved() && saved.pinned);
    assert_eq!(saved.root, doc.root);
    assert_eq!(saved.state, doc.state);
    saved.validate().unwrap();
}

#[test]
fn outline_lists_ticks_for_agents() {
    let outline = today().outline();
    assert!(
        outline.contains("- [x] Post standup notes (standup)"),
        "{outline}"
    );
    assert!(outline.contains("button #plan [Plan tomorrow] → send_prompt"));
}

/// Tiny deterministic generator: no external dependency, same sequence on
/// every machine.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next() as usize) % items.len()]
    }
}

fn ids(doc: &SurfaceDoc) -> Vec<String> {
    let mut out = Vec::new();
    node::walk(&doc.root, &mut |node, _| out.extend(node.id.clone()));
    out
}

/// Random patch sequences: every accepted result is within bounds, every
/// refusal leaves the surface unchanged, and state only ever names
/// components that exist.
#[test]
fn random_patches_preserve_invariants() {
    let mut rng = Lcg(0x5eed);
    let mut doc = today();
    let kinds = [
        json!({"type": "divider"}),
        json!({"type": "text", "text": "x"}),
        json!({"type": "stack", "children": [{"type": "divider"}]}),
        json!({"type": "link", "url": "file:///x"}),
        json!({"type": "checklist", "id": "dup", "items": [{"id": "a", "text": "a"}]}),
        json!({"type": "toggle", "label": "no id"}),
    ];
    let mut accepted = 0;
    for step in 0..2000 {
        let existing = ids(&doc);
        let target = rng.pick(&existing).clone();
        let mut node = rng.pick(&kinds).clone();
        if rng.next() % 2 == 0 && node.get("id").is_none() {
            node["id"] = json!(format!("n{step}"));
        }
        let op = match rng.next() % 4 {
            0 => json!({"op": "insert", "parent": target, "node": node}),
            1 => json!({"op": "remove", "id": target}),
            2 => json!({"op": "replace", "id": target, "node": node}),
            _ => json!({"op": "set", "id": target, "fields": {"label": "L"}}),
        };
        let p: Patch = serde_json::from_value(json!({"ops": [op]})).unwrap();
        let before = doc.clone();
        match apply(&doc, &p) {
            Ok((next, _)) => {
                next.validate().expect("accepted patches stay in bounds");
                let (checks, toggles, selects) = limits::state_keys(&next.root);
                assert!(next.state.checks.keys().all(|k| checks.contains(k)));
                assert!(next.state.toggles.keys().all(|k| toggles.contains(k)));
                assert!(next.state.selects.keys().all(|k| selects.contains(k)));
                doc = next;
                accepted += 1;
            }
            Err(_) => assert_eq!(doc, before),
        }
        if ids(&doc).is_empty() {
            doc = today();
        }
    }
    assert!(
        accepted > 200,
        "the generator exercises real changes: {accepted}"
    );
}

#[test]
fn event_prompt_is_one_line_then_json() {
    let doc = today();
    let event = SurfaceEvent {
        seq: 3,
        at_ms: 1,
        what: EventKind::Interaction(Interaction::Check {
            checklist: "today".into(),
            item: "spec".into(),
            checked: true,
        }),
        delivered_to: Some("owner".into()),
    };
    let prompt = event_prompt(&doc, &event);
    let (head, body) = prompt.split_once('\n').unwrap();
    assert!(head.starts_with("[Diri Live Surface event]"));
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(parsed["event"]["event"], "interaction");
    assert_eq!(parsed["event"]["kind"], "check");
}
