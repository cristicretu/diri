//! The to-do → agent → note loop, end to end, with fake sessions in the
//! store instead of real agents.

use std::sync::Arc;

use diri_notes::markdown;
use diri_notes::store::NoteStore;
use diri_notes::work::{self, WorkState};
use diri_proto::{
    AgentKind, DateMillis, NeedsInputDetail, NeedsInputKind, NeedsInputSource, ProjectId,
    Resumability, RiskHint, SessionId, SessionRecord, SessionStatus, TitleSource,
};

use super::editor_view::EditorEvent;
use super::work_item::WorkRequest;
use super::*;
use crate::store::StoreRuntime;

pub(crate) const LAUNCH: &str = "# Launch plan

We ship the new onboarding on Thursday.

## Marketing

- [ ] Draft 3 LinkedIn posts for the launch
  - Tone: plain, confident, no emojis
  - [Launch brief](https://example.com/launch-brief)
  - One post per audience: founders, marketers, ops
- [ ] Book the venue for the meetup
";

/// A launch plan mid-flight, for screenshots: work in every state.
#[cfg(target_os = "macos")]
pub(crate) const TRACKING: &str = "# Launch plan

We ship the new onboarding on Thursday. Everything below is due Wednesday.

## Marketing

- [ ] Draft 3 LinkedIn posts for the launch [@Claude](diri://session/s_posts)
  - Tone: plain, confident, no emojis
  - [Launch brief](https://example.com/launch-brief)
  - One post per audience: founders, marketers, ops
  - [@Claude](diri://session/s_posts) 14:02 Drafted the founders post; marketers next
- [ ] Pick the pricing page headline [@Codex](diri://session/s_pricing)
- [ ] Write the launch FAQ [@Claude](diri://session/s_faq)
  - The five questions sales hears most
- [ ] Book the venue for the meetup
  - Budget under $2k, near Union Square
  - 60 people, Thursday evening

## Engineering

- [ ] Fix the signup redirect loop [@Codex](diri://session/s_redirect)
- [x] Ship the onboarding checklist
";

const NOTE_SESSION: &str = "s_note_launch";
const AGENT: &str = "s_posts";

pub(crate) fn record(id: &str, kind: AgentKind) -> SessionRecord {
    SessionRecord { command_run: None,
        attention_state: None,
        id: SessionId::new(id),
        kind,
        cwd: "/Users/me/launch".into(),
        project_id: ProjectId::new("p_launch"),
        worktree_path: None,
        git_branch: None,
        title: id.into(),
        title_source: TitleSource::Placeholder,
        account_profile: None,
        originating_prompt: None,
        agent_session_id: None,
        transcript_path: None,
        status: SessionStatus::Idle,
        status_evidence: None,
        needs_input: None,
        resumability: Resumability::Live,
        capabilities: None,
        parent: None,
        created_at: DateMillis(0.0),
        updated_at: DateMillis(0.0),
        last_turn_completed_at: None,
        last_seen_at: None,
        pinned: false,
        archived_at: None,
        host: None,
        remote_persistence: None,
        remote_connection: None,
        hibernation: None,
        memory_bytes: None,
        artifacts: None,
        pull_requests: None,
        listening_ports: None,
        foreground_agent: None,
        terminal_cwd: None,
        agent_workspace: None,
        note_id: None,
        foreground_ports: None,
        terminal_progress: None,
        scheduled_run: None,
    }
}

pub(crate) struct Loop {
    pub _dir: tempfile::TempDir,
    pub store: Arc<NoteStore>,
    pub runtime: Arc<StoreRuntime>,
    pub id: String,
}

pub(crate) fn launch_loop(source: &str) -> Loop {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(NoteStore::open(dir.path().join("notes")).expect("store"));
    let (_, doc) = markdown::parse(source);
    let (id, _) = store.create(doc, Some("/Users/me/launch")).expect("create");
    let runtime = Arc::new(StoreRuntime::inert());
    {
        let mut sessions = runtime.store.write().unwrap();
        sessions.set_agent_catalog(crate::agent_setup::bundled_catalog(&[
            "claude-code",
            "codex",
        ]));
        let mut note = record(NOTE_SESSION, AgentKind::NOTE);
        note.note_id = Some(id.clone());
        note.title = "Launch plan".into();
        sessions.upsert_session(note);
    }
    Loop {
        _dir: dir,
        store,
        runtime,
        id,
    }
}

fn open<'a>(
    cx: &'a mut gpui::TestAppContext,
    fixture: &Loop,
) -> (
    Entity<NotePane>,
    Entity<NoteEditorView>,
    &'a mut gpui::VisualTestContext,
) {
    let runtime = fixture.runtime.clone();
    let store = fixture.store.clone();
    let (pane, cx) =
        cx.add_window_view(move |_, cx| NotePane::with_store(runtime, Some(store), false, cx));
    let id = fixture.id.clone();
    pane.update_in(cx, |pane, window, cx| {
        pane.show(&SessionId::new(NOTE_SESSION), &id, window, cx)
    });
    let editor = pane.read_with(cx, |pane, _| pane.editor_for_test().expect("open"));
    (pane, editor, cx)
}

fn todo_index(
    editor: &Entity<NoteEditorView>,
    cx: &mut gpui::VisualTestContext,
    text: &str,
) -> usize {
    editor.read_with(cx, |view, _| {
        view.editor
            .blocks()
            .iter()
            .position(|b| b.text.starts_with(text))
            .expect(text)
    })
}

fn state(
    editor: &Entity<NoteEditorView>,
    cx: &mut gpui::VisualTestContext,
    index: usize,
) -> WorkState {
    editor.read_with(cx, |view, _| view.work.state(view.editor.block(index)))
}

fn set_session(fixture: &Loop, update: impl FnOnce(&mut SessionRecord)) {
    let mut store = fixture.runtime.store.write().unwrap();
    let mut session = store
        .sessions()
        .get(&SessionId::new(AGENT))
        .map(|s| (**s).clone())
        .unwrap_or_else(|| {
            let mut s = record(AGENT, AgentKind::CLAUDE_CODE);
            s.parent = Some(SessionId::new(NOTE_SESSION));
            s.status = SessionStatus::Starting;
            s
        });
    update(&mut session);
    store.upsert_session(session);
}

#[gpui::test]
fn a_todo_becomes_tracked_work_inside_its_note(cx: &mut gpui::TestAppContext) {
    let fixture = launch_loop(LAUNCH);
    let (pane, editor, cx) = open(cx, &fixture);
    let posts = todo_index(&editor, cx, "Draft 3 LinkedIn");
    assert_eq!(state(&editor, cx, posts), WorkState::Ready);

    // ⌃⌘↩ with the caret in the to-do opens the Start panel with the brief.
    editor.update(cx, |view, cx| {
        view.editor.set_caret(diri_notes::edit::Pos::new(posts, 3));
        view.start_work_at_caret(cx);
    });
    cx.run_until_parked();
    let brief = editor.read_with(cx, |view, _| {
        view.work
            .start_panel_brief()
            .cloned()
            .expect("Start panel open")
    });
    assert!(
        brief
            .prompt
            .contains("Draft 3 LinkedIn posts for the launch")
            && brief.prompt.contains("Tone: plain, confident, no emojis")
            && brief
                .prompt
                .contains("[Launch brief](https://example.com/launch-brief)")
            && brief.prompt.contains("report_to_parent"),
        "{}",
        brief.prompt
    );
    assert_eq!(brief.context_lines, 3);
    assert_eq!(brief.links, 1);

    // Return starts the default agent (Claude Code) and stays in the note.
    editor.update_in(cx, |view, window, cx| {
        view.newline(&super::editor_view::Newline, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(state(&editor, cx, posts), WorkState::Starting);
    assert!(
        editor.read_with(cx, |view, _| view.editor.is_collapsed(posts)),
        "context folds once work starts"
    );
    let ticket = editor.read_with(cx, |view, _| view.work.pending_tickets()[0].1);

    // The Engine names the session; the to-do links it.
    set_session(&fixture, |_| {});
    fixture
        .runtime
        .store
        .write()
        .unwrap()
        .finish_work_item(ticket, Ok(SessionId::new(AGENT)));
    pane.update(cx, |pane, cx| pane.push_work(cx));
    pane.update(cx, |pane, cx| pane.save(cx));
    let saved = std::fs::read_to_string(fixture.store.path_for(&fixture.id).unwrap()).unwrap();
    assert!(
        saved.contains(
            "- [ ] Draft 3 LinkedIn posts for the launch [@Claude Code](diri://session/s_posts)"
        ),
        "{saved}"
    );
    assert_eq!(state(&editor, cx, posts), WorkState::Starting);

    // Working, then a report lands folded under the to-do.
    set_session(&fixture, |s| s.status = SessionStatus::Working);
    pane.update(cx, |pane, cx| pane.push_work(cx));
    assert_eq!(state(&editor, cx, posts), WorkState::Working);
    fixture
        .store
        .update(&fixture.id, &diri_notes::history::Author::Cli, |note| {
            diri_notes::handoff::append_update(
                note,
                "2026-09-30T14:02:00Z",
                "@Claude Code",
                AGENT,
                "Drafted the founders post",
            );
            Ok(())
        })
        .unwrap();
    pane.update(cx, |pane, cx| {
        pane.reconcile(cx);
        pane.push_work(cx);
    });
    let (posts, children, update_hidden) = editor.read_with(cx, |view, _| {
        let blocks = view.editor.blocks();
        let posts = blocks
            .iter()
            .position(|b| b.text.starts_with("Draft 3 LinkedIn"))
            .unwrap();
        let children = view.editor.children(posts);
        let last = children.end - 1;
        (
            posts,
            children.clone(),
            view.editor.is_hidden(last) && blocks[last].text.ends_with("Drafted the founders post"),
        )
    });
    assert_eq!(children.len(), 4, "three context lines and one update");
    assert!(update_hidden, "the report sits folded under its to-do");
    assert_eq!(state(&editor, cx, posts), WorkState::Working);

    // Needs you: the question shows, and "Answer in session" opens it.
    set_session(&fixture, |s| {
        s.status = SessionStatus::NeedsInput(NeedsInputKind::Question);
        s.needs_input = Some(NeedsInputDetail {
            kind: NeedsInputKind::Question,
            source: NeedsInputSource::ClaudeNotificationHook,
            tool_name: None,
            summary: "Which launch date should the posts mention?".into(),
            prompt_excerpt: None,
            options: None,
            risk_hint: RiskHint::Unknown,
            occurred_at: DateMillis(1.0),
            secret: false,
        });
    });
    pane.update(cx, |pane, cx| pane.push_work(cx));
    assert!(matches!(
        state(&editor, cx, posts),
        WorkState::NeedsYou(q) if q.summary.contains("launch date")
    ));
    let revealed = Arc::new(std::sync::Mutex::new(None));
    let seen = revealed.clone();
    let _sub = cx.update(|_, cx| {
        cx.subscribe(&pane, move |_, event, _| {
            if let NotePaneEvent::Reveal(id) = event {
                *seen.lock().unwrap() = Some(id.clone());
            }
        })
    });
    editor.update(cx, |_, cx| {
        cx.emit(EditorEvent::Work(WorkRequest::Open {
            session: AGENT.into(),
        }))
    });
    cx.run_until_parked();
    assert_eq!(*revealed.lock().unwrap(), Some(SessionId::new(AGENT)));

    // Ticking while the agent runs asks first; Escape keeps it open.
    editor.update(cx, |view, cx| {
        view.editor.set_caret(diri_notes::edit::Pos::new(posts, 0));
        assert!(view.guard_tick(posts, cx));
        assert!(view.work.tick_panel_open());
        assert!(view.work_panel_key(super::work_item::PanelKey::Escape, cx));
    });
    assert!(matches!(state(&editor, cx, posts), WorkState::NeedsYou(_)));
    assert!(!editor.read_with(cx, |view, _| view.work.tick_panel_open()));
    assert!(
        editor.read_with(cx, |view, _| view.editor.block(posts).kind
            == diri_notes::doc::BlockKind::Todo { checked: false }),
        "escape leaves the to-do unticked"
    );

    // Done: ready to review, and ticking is a plain tick.
    set_session(&fixture, |s| {
        s.status = SessionStatus::Idle;
        s.needs_input = None;
        s.last_turn_completed_at = Some(DateMillis(2.0));
    });
    pane.update(cx, |pane, cx| pane.push_work(cx));
    assert_eq!(state(&editor, cx, posts), WorkState::Review(None));
    let intercepted = editor.update(cx, |view, cx| view.guard_tick(posts, cx));
    assert!(!intercepted, "a finished agent does not need asking");
}

#[gpui::test]
fn a_failed_start_says_why_and_links_nothing(cx: &mut gpui::TestAppContext) {
    let fixture = launch_loop(LAUNCH);
    let (pane, editor, cx) = open(cx, &fixture);
    let venue = todo_index(&editor, cx, "Book the venue");
    let block = editor.read_with(cx, |view, _| view.editor.block(venue).id);
    editor.update(cx, |_, cx| {
        cx.emit(EditorEvent::Work(WorkRequest::Start {
            block,
            kind: AgentKind::CODEX,
            agent_name: "Codex".into(),
            session: None,
        }))
    });
    cx.run_until_parked();
    let ticket = editor.read_with(cx, |view, _| view.work.pending_tickets()[0].1);
    fixture
        .runtime
        .store
        .write()
        .unwrap()
        .finish_work_item(ticket, Err("codex: command not found".into()));
    pane.update(cx, |pane, cx| pane.push_work(cx));
    editor.read_with(cx, |view, _| {
        let block = view.editor.block(venue);
        assert!(work::sessions(block).is_empty());
        assert!(view.work.pending_tickets().is_empty());
    });
}

#[gpui::test]
fn a_session_opens_its_note_at_the_todo_it_works_on(cx: &mut gpui::TestAppContext) {
    let linked = LAUNCH.replace(
        "- [ ] Book the venue for the meetup",
        "- [ ] Book the venue for the meetup [@Codex](diri://session/s_posts)",
    );
    let fixture = launch_loop(&linked);
    set_session(&fixture, |s| s.status = SessionStatus::Working);
    fixture
        .runtime
        .store
        .write()
        .unwrap()
        .reveal_in_note(SessionId::new(NOTE_SESSION), SessionId::new(AGENT));
    let (_pane, editor, cx) = open(cx, &fixture);
    let venue = todo_index(&editor, cx, "Book the venue");
    let caret = editor.read_with(cx, |view, _| view.editor.selection.head.block);
    assert_eq!(caret, venue);
}

/// Mentioning an existing session in a to-do and pressing Start asks that
/// session to take it on instead of spawning a new agent; the note records
/// the ask, and the to-do then tracks the session like any started work.
#[gpui::test]
fn start_on_a_todo_that_mentions_a_session_prompts_that_session(cx: &mut gpui::TestAppContext) {
    let mentioned = LAUNCH.replace(
        "- [ ] Book the venue for the meetup",
        "- [ ] Book the venue for the meetup [@Meeting Notes Review](diri://session/s_review)",
    );
    let fixture = launch_loop(&mentioned);
    {
        let mut review = record("s_review", AgentKind::new("cursor"));
        review.title = "Meeting Notes Review".into();
        review.title_source = TitleSource::DirijorAssigned;
        review.last_turn_completed_at = Some(DateMillis(1.0));
        fixture
            .runtime
            .store
            .write()
            .unwrap()
            .upsert_session(review);
    }
    let (pane, editor, cx) = open(cx, &fixture);
    pane.update(cx, |pane, cx| pane.push_work(cx));
    let venue = todo_index(&editor, cx, "Book the venue");
    assert_eq!(
        state(&editor, cx, venue),
        WorkState::Ready,
        "Start is offered"
    );

    editor.update(cx, |view, cx| {
        let end = view.editor.block(venue).text.len();
        view.editor
            .set_caret(diri_notes::edit::Pos::new(venue, end));
        view.start_work_at_caret(cx);
    });
    cx.run_until_parked();
    let agents = editor.read_with(cx, |view, _| view.work.start_panel_agents());
    assert_eq!(agents[0].session.as_deref(), Some("s_review"));
    assert_eq!(agents[0].name, "Meeting Notes Review");
    assert!(agents[0].is_default && agents.iter().filter(|a| a.is_default).count() == 1);

    editor.update(cx, |view, cx| {
        assert!(view.work_panel_key(super::work_item::PanelKey::Enter, cx));
    });
    cx.run_until_parked();
    let sent = pane.read_with(cx, |pane, _| pane.sent_for_test.clone());
    assert_eq!(sent.len(), 1, "one prompt, to the mentioned session");
    assert_eq!(sent[0].session_id, SessionId::new("s_review"));
    assert!(sent[0].submit);
    assert!(sent[0].text.contains("Book the venue for the meetup"));
    let spawned = fixture
        .runtime
        .store
        .read()
        .unwrap()
        .sessions()
        .values()
        .filter(|s| !s.is_note())
        .count();
    assert_eq!(spawned, 1, "no new agent");

    editor.read_with(cx, |view, _| {
        let update = view.editor.block(venue + 1);
        assert!(
            update.text.starts_with("@Meeting Notes Review"),
            "{}",
            update.text
        );
        assert!(update.text.ends_with("Asked to take this on"));
    });
    assert_ne!(
        state(&editor, cx, venue),
        WorkState::Ready,
        "the to-do now tracks the session it asked"
    );
}
