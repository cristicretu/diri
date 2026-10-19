//! Records the What's New clips from the real window, frame by frame, because
//! an agent session cannot record the screen and a hand-made recording goes
//! stale with the next redesign.
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/clips \
//!   cargo test -p diri-app --bin diri -- --ignored render_whats_new_clips
//! scripts/whats-new-clips.sh /tmp/clips   # frames -> assets/whats-new/*.webp
//! ```
//!
//! Writes `<clip>-<theme>/frame_NNNN.png` plus `frames.txt`, one
//! `frame_NNNN.png <hold ms>` line per frame. `DIRI_CLIP=notes|todos|search`
//! records one clip; `DIRI_CLIP_THEME=dark|light` one theme.

use std::path::{Path, PathBuf};
use std::time::Duration;

use diri_notes::edit::Pos;
use diri_proto::{DateMillis, NeedsInputDetail, NeedsInputKind, SessionStatus};
use gpui::{AnyWindowHandle, EntityInputHandler as _, HeadlessAppContext, size};

use super::tests::test_services;
use super::*;
use crate::SidebarPreviewFixture;
use crate::notes::NotePane;
use crate::notes::editor_view::NoteEditorView;

/// The window every clip is recorded in, 16:10 like the sheet's clip frame.
const WINDOW: (f32, f32) = (1040.0, 650.0);

pub(super) struct Recorder<'a> {
    pub(super) cx: &'a mut HeadlessAppContext,
    pub(super) window: AnyWindowHandle,
    dir: PathBuf,
    frames: Vec<(String, u32)>,
    step: Option<u32>,
}

impl<'a> Recorder<'a> {
    pub(super) fn new(cx: &'a mut HeadlessAppContext, window: AnyWindowHandle, dir: PathBuf) -> Self {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            cx,
            window,
            dir,
            frames: Vec::new(),
            step: None,
        }
    }

    /// Splits every hold into frames `step_ms` apart and moves the app's
    /// clock between them, so looping marks (the working spinner) turn
    /// instead of freezing for the length of the hold.
    pub(super) fn animated(mut self, step_ms: u32) -> Self {
        self.step = Some(step_ms);
        self
    }

    pub(super) fn settle(&mut self) {
        for _ in 0..3 {
            self.cx
                .update_window(self.window, |view, window, cx| {
                    if let Ok(root) = view.downcast::<RootView>() {
                        root.update(cx, |root, cx| {
                            root.sidebar.update(cx, |_, cx| cx.notify());
                            cx.notify();
                        });
                    }
                    window.refresh();
                    window.draw(cx).clear();
                })
                .unwrap();
            self.cx.run_until_parked();
        }
    }

    /// Captures the window as it stands and holds it for `hold_ms`.
    pub(super) fn shot(&mut self, hold_ms: u32) {
        let Some(step) = self.step else {
            self.capture(hold_ms);
            return;
        };
        let mut left = hold_ms;
        while left > 0 {
            // The last frame takes the remainder rather than a sliver.
            let hold = if left >= 2 * step { step } else { left };
            self.capture(hold);
            self.cx.advance_clock(Duration::from_millis(u64::from(hold)));
            left -= hold;
        }
    }

    fn capture(&mut self, hold_ms: u32) {
        self.settle();
        let name = format!("frame_{:04}.png", self.frames.len());
        self.cx
            .capture_screenshot(self.window)
            .unwrap()
            .save(self.dir.join(&name))
            .unwrap();
        self.frames.push((name, hold_ms));
    }

    pub(super) fn finish(self) {
        let list: String = self
            .frames
            .iter()
            .map(|(name, ms)| format!("{name} {ms}\n"))
            .collect();
        std::fs::write(self.dir.join("frames.txt"), list).unwrap();
        self.cx
            .update_window(self.window, |_, window, _| window.remove_window())
            .unwrap();
        self.cx.run_until_parked();
    }

    fn edit(
        &mut self,
        editor: &Entity<NoteEditorView>,
        f: impl FnOnce(&mut NoteEditorView, &mut Window, &mut Context<NoteEditorView>),
    ) {
        self.cx
            .update_window(self.window, |_, window, cx| {
                editor.update(cx, |view, cx| f(view, window, cx));
            })
            .unwrap();
        self.cx.run_until_parked();
        // The app paints between keystrokes; menus anchor to laid-out text.
        self.settle();
    }

    /// Types `text` the way a person does: a frame every `per` characters.
    fn type_text(&mut self, editor: &Entity<NoteEditorView>, text: &str, per: usize, ms: u32) {
        let chars: Vec<char> = text.chars().collect();
        for chunk in chars.chunks(per.max(1)) {
            let piece: String = chunk.iter().collect();
            self.edit(editor, |view, window, cx| {
                view.replace_text_in_range(None, &piece, window, cx);
            });
            self.shot(ms);
        }
    }

    fn enter(&mut self, editor: &Entity<NoteEditorView>) {
        self.edit(editor, |view, window, cx| {
            view.newline(&crate::notes::editor_view::Newline, window, cx);
        });
    }
}

fn theme(light: bool) -> &'static str {
    if light {
        "dirijor-light"
    } else {
        "dirijor-dark"
    }
}

fn note_record(template: &SessionRecord, id: &str, title: &str, note_id: String) -> SessionRecord {
    let mut note = template.clone();
    note.id = SessionId::new(id);
    note.kind = AgentKind::NOTE;
    note.title = title.into();
    note.title_source = diri_proto::TitleSource::DirijorAssigned;
    note.status = SessionStatus::Idle;
    note.needs_input = None;
    note.resumability = diri_proto::Resumability::NotResumable;
    note.note_id = Some(note_id);
    note.parent = None;
    note.pinned = false;
    note.archived_at = None;
    note.agent_session_id = None;
    note.transcript_path = None;
    note.git_branch = None;
    note.foreground_agent = None;
    note.pull_requests = None;
    note.listening_ports = None;
    note.artifacts = None;
    note.worktree_path = None;
    note
}

struct NoteWindow {
    window: AnyWindowHandle,
    runtime: Arc<crate::store::StoreRuntime>,
    pane: Entity<NotePane>,
    editor: Entity<NoteEditorView>,
}

/// The real window with `markdown` open as the selected note `note_session`.
/// `sessions` adds agents to the sidebar after the fixture's own.
fn open_note(
    cx: &mut HeadlessAppContext,
    notes_dir: &Path,
    markdown: &str,
    note_session: (&str, &str),
    sessions: impl FnOnce(&SessionRecord, &SessionRecord) -> Vec<SessionRecord>,
    light: bool,
) -> NoteWindow {
    open_notes(
        cx,
        notes_dir,
        &[(note_session.0, note_session.1, markdown)],
        sessions,
        light,
    )
}

/// [`open_note`] with more notes beside the selected first one, each a
/// sidebar row `(session id, title, markdown)`.
fn open_notes(
    cx: &mut HeadlessAppContext,
    notes_dir: &Path,
    notes: &[(&str, &str, &str)],
    sessions: impl FnOnce(&SessionRecord, &SessionRecord) -> Vec<SessionRecord>,
    light: bool,
) -> NoteWindow {
    let note_store = Arc::new(diri_notes::store::NoteStore::open(notes_dir.join("notes")).unwrap());
    let services = test_services();
    let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    let template = fixture.list.sessions[0].clone();
    let mut records: Vec<SessionRecord> = notes
        .iter()
        .map(|(id, title, markdown)| {
            let (_, doc) = diri_notes::markdown::parse(markdown);
            let (note_id, _) = note_store.create(doc, None).unwrap();
            note_record(&template, id, title, note_id)
        })
        .collect();
    let note = records.remove(0);
    let note_session = (notes[0].0, notes[0].1);
    let extra = sessions(&template, &note);
    for other in records.into_iter().rev() {
        fixture.list.sessions.insert(0, other);
    }
    fixture.list.sessions.insert(0, note);
    for session in extra.into_iter().rev() {
        fixture.list.sessions.insert(1, session);
    }
    {
        let mut store = services.store.store.write().unwrap();
        store.hydrate(fixture.list);
        store.mark_connected_for_test();
        store.set_agent_catalog(crate::agent_setup::bundled_catalog(&[
            "claude-code",
            "codex",
            "gemini",
        ]));
        store.select(SessionId::new(note_session.0));
        store
            .update_preferences(|prefs| {
                prefs.sidebar_visible = true;
                prefs.terminal_theme = theme(light).into();
                // Headless rendering has no backdrop to blur, so glass
                // would show sharp text through every panel.
                prefs.window_material = crate::store::WindowMaterial::Opaque;
            })
            .unwrap();
    }
    let runtime = Arc::clone(&services.store);
    cx.update(|cx| {
        let model = cx.new(|cx| {
            crate::notes::todos::TodosModel::with_store(
                Arc::clone(&runtime),
                Some(Arc::clone(&note_store)),
                false,
                cx,
            )
        });
        crate::notes::todos::TodosModel::install(model, cx);
    });
    let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
    let window = cx
        .open_window(size(px(WINDOW.0), px(WINDOW.1)), {
            let slot = slot.clone();
            let runtime = Arc::clone(&runtime);
            move |window, cx| {
                cx.new(|cx| {
                    let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
                    let pane =
                        cx.new(|cx| NotePane::with_store(runtime, Some(note_store), false, cx));
                    *slot.borrow_mut() = Some(pane.clone());
                    if let Some(terminal) = &root.terminal {
                        terminal.update(cx, |terminal, _| terminal.set_note_pane_for_test(pane));
                    }
                    root
                })
            }
        })
        .unwrap();
    cx.run_until_parked();
    // Entrance motion runs on the wall clock; let it finish.
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(120));
        cx.update_window(window.into(), |_, window, _| window.refresh())
            .unwrap();
        cx.run_until_parked();
    }
    let pane = slot.borrow().clone().expect("note pane");
    let editor = cx
        .update(|cx| pane.read(cx).editor_for_test())
        .expect("open note editor");
    cx.update_window(window.into(), |_, window, cx| {
        editor.update(cx, |view, cx| {
            window.focus(&gpui::Focusable::focus_handle(view, cx), cx);
        });
    })
    .unwrap();
    NoteWindow {
        window: window.into(),
        runtime,
        pane,
        editor,
    }
}

fn find_block(cx: &mut HeadlessAppContext, editor: &Entity<NoteEditorView>, prefix: &str) -> usize {
    cx.update(|cx| {
        editor
            .read(cx)
            .editor
            .blocks()
            .iter()
            .position(|b| b.text.starts_with(prefix))
            .unwrap_or_else(|| panic!("no block starting {prefix:?}"))
    })
}

/// A blank note becomes a plan: title, a line, `/to-do`, an `@` mention.
fn record_notes(cx: &mut HeadlessAppContext, dir: PathBuf, light: bool) {
    let notes_dir = tempfile::tempdir().unwrap();
    let w = open_note(
        cx,
        notes_dir.path(),
        "",
        ("s_clip_note", "New note"),
        |template, _| {
            // The agent the note's `@` mention links to.
            let mut claude = template.clone();
            claude.id = SessionId::new("s_claude");
            claude.kind = AgentKind::CLAUDE_CODE;
            claude.title = "Draft launch email".into();
            claude.status = SessionStatus::Working;
            claude.parent = None;
            claude.archived_at = None;
            claude.pinned = false;
            claude.needs_input = None;
            claude.pull_requests = None;
            vec![claude]
        },
        light,
    );
    let editor = w.editor.clone();
    let pane = w.pane.clone();
    let runtime = w.runtime.clone();
    let mut rec = Recorder::new(cx, w.window, dir);
    rec.edit(&editor, |view, _, cx| {
        view.set_mentions(
            crate::notes::editor_view::MentionDirectory {
                entries: crate::notes::tests::fixture_mentions(),
            },
            cx,
        );
        view.editor.set_caret(Pos::new(0, 0));
    });
    rec.shot(700);
    rec.type_text(&editor, "Launch plan", 1, 55);
    {
        let mut store = runtime.store.write().unwrap();
        let mut note = (**store
            .sessions()
            .get(&SessionId::new("s_clip_note"))
            .unwrap())
        .clone();
        note.title = "Launch plan".into();
        store.upsert_session(note);
    }
    rec.shot(250);
    rec.enter(&editor);
    rec.type_text(
        &editor,
        "Ship on Thursday. Everything below is due Wednesday.",
        3,
        45,
    );
    rec.shot(350);
    rec.enter(&editor);
    rec.type_text(&editor, "/", 1, 350);
    rec.type_text(&editor, "to", 1, 160);
    rec.shot(450);
    rec.enter(&editor);
    rec.shot(200);
    rec.type_text(&editor, "Draft the announcement post", 2, 45);
    rec.shot(250);
    rec.enter(&editor);
    rec.type_text(&editor, "Record a ten second demo — ask ", 2, 45);
    rec.type_text(&editor, "@", 1, 350);
    rec.type_text(&editor, "cl", 1, 180);
    rec.shot(500);
    rec.enter(&editor);
    // The linked agent's live status, as the pane polls it.
    rec.cx
        .update(|cx| pane.update(cx, |pane, cx| pane.push_work(cx)));
    rec.shot(2200);
    rec.finish();
}

/// A to-do becomes an agent's work, then reports back live.
fn record_todos(cx: &mut HeadlessAppContext, dir: PathBuf, light: bool) {
    let notes_dir = tempfile::tempdir().unwrap();
    let w = open_note(
        cx,
        notes_dir.path(),
        crate::notes::work_item_tests::TRACKING,
        ("s_note_launch", "Launch plan"),
        |template, note| {
            let child = |id: &str, kind: AgentKind, title: &str, status: SessionStatus| {
                let mut s = template.clone();
                s.id = SessionId::new(id);
                s.kind = kind;
                s.title = title.into();
                s.parent = Some(note.id.clone());
                s.archived_at = None;
                s.pinned = false;
                s.needs_input = None;
                s.pull_requests = None;
                s.foreground_agent = None;
                s.last_turn_completed_at = None;
                s.status = status;
                s
            };
            let mut faq = child(
                "s_faq",
                AgentKind::CLAUDE_CODE,
                "Write the launch FAQ",
                SessionStatus::Idle,
            );
            faq.last_turn_completed_at = Some(DateMillis(2.0));
            let mut redirect = child(
                "s_redirect",
                AgentKind::CODEX,
                "Fix the signup redirect loop",
                SessionStatus::Idle,
            );
            redirect.last_turn_completed_at = Some(DateMillis(2.0));
            vec![
                child(
                    "s_posts",
                    AgentKind::CLAUDE_CODE,
                    "Draft 3 LinkedIn posts",
                    SessionStatus::Working,
                ),
                child(
                    "s_pricing",
                    AgentKind::CODEX,
                    "Pick the pricing headline",
                    SessionStatus::Working,
                ),
                faq,
                redirect,
            ]
        },
        light,
    );
    let NoteWindow {
        window,
        runtime,
        pane,
        editor,
    } = w;
    cx.update(|cx| pane.update(cx, |pane, cx| pane.push_work(cx)));
    let venue = find_block(cx, &editor, "Book the venue");
    let block = cx.update(|cx| editor.read(cx).editor.block(venue).id);
    let mut rec = Recorder::new(cx, window, dir);
    rec.edit(&editor, |view, _, _| {
        let end = view.editor.block(venue).text.len();
        view.editor.set_caret(Pos::new(venue, end));
    });
    rec.shot(1100);
    // ⌘↩ on the to-do: the Start panel, with what the agent will get.
    rec.cx.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.on_work(&crate::notes::work_item::WorkRequest::Prepare { block }, cx)
        })
    });
    rec.shot(2000);
    rec.enter(&editor);
    rec.shot(450);
    let ticket = rec
        .cx
        .update(|cx| editor.read(cx).work_tickets_for_test())
        .first()
        .map(|(_, ticket)| *ticket)
        .expect("a start in flight");
    let set = |rec: &mut Recorder, f: &dyn Fn(&mut SessionRecord)| {
        {
            let mut store = runtime.store.write().unwrap();
            let mut session = (**store.sessions().get(&SessionId::new("s_venue")).unwrap()).clone();
            f(&mut session);
            store.upsert_session(session);
        }
        rec.cx
            .update(|cx| pane.update(cx, |pane, cx| pane.push_work(cx)));
    };
    {
        let mut store = runtime.store.write().unwrap();
        let mut venue = (**store.sessions().get(&SessionId::new("s_posts")).unwrap()).clone();
        venue.id = SessionId::new("s_venue");
        venue.title = "Book the venue for the meetup".into();
        venue.status = SessionStatus::Working;
        store.upsert_session(venue);
        store.finish_work_item(ticket, Ok(SessionId::new("s_venue")));
    }
    rec.cx
        .update(|cx| pane.update(cx, |pane, cx| pane.push_work(cx)));
    rec.shot(1700);
    set(&mut rec, &|s| {
        s.status = SessionStatus::NeedsInput(NeedsInputKind::Question);
        s.needs_input = Some(NeedsInputDetail {
            kind: NeedsInputKind::Question,
            source: diri_proto::NeedsInputSource::ClaudeNotificationHook,
            tool_name: None,
            summary: "Two venues fit. Book the one with a projector?".into(),
            prompt_excerpt: None,
            options: None,
            risk_hint: diri_proto::RiskHint::Neutral,
            occurred_at: DateMillis(1.0),
            secret: false,
        });
    });
    rec.shot(1700);
    set(&mut rec, &|s| {
        s.status = SessionStatus::Idle;
        s.needs_input = None;
        s.last_turn_completed_at = Some(DateMillis(3.0));
    });
    rec.shot(2200);
    rec.finish();
}

/// ⇧⌘F finds a note by what it says, not only its title.
fn record_search(cx: &mut HeadlessAppContext, dir: PathBuf, light: bool) {
    let notes_dir = tempfile::tempdir().unwrap();
    let notes = [
        (
            "s_note_q4",
            "Q4 launch plan",
            "# Q4 launch plan\n\nShip the onboarding email sequence before the pricing test.\n\n- [ ] Draft the launch email\n- [ ] Brief the pricing page\n",
        ),
        (
            "s_note_sync",
            "Weekly growth sync",
            "# Weekly growth sync\n\nWe will A/B test the pricing page against the annual plan.\n",
        ),
        (
            "s_note_interviews",
            "Customer interviews",
            "# Customer interviews\n\nFive teams asked for a cheaper starter plan; two mentioned pricing confusion on the checkout page.\n",
        ),
        (
            "s_note_hiring",
            "Hiring loop",
            "# Hiring loop\n\nInterview plan for the product designer role.\n",
        ),
        (
            "s_note_offsite",
            "Offsite agenda",
            "# Offsite agenda\n\nDay one: roadmap. Day two: packaging workshop.\n",
        ),
    ];
    let w = open_notes(cx, notes_dir.path(), &notes, |_, _| Vec::new(), light);
    let window = w.window;
    let mut rec = Recorder::new(cx, window, dir);
    rec.shot(700);
    let navigation = |rec: &mut Recorder,
                      f: &dyn Fn(
        &mut crate::navigation::NavigationOverlay,
        &mut Window,
        &mut Context<crate::navigation::NavigationOverlay>,
    )| {
        rec.cx
            .update_window(rec.window, |view, window, cx| {
                let root = view.downcast::<RootView>().unwrap();
                let navigation = root
                    .read(cx)
                    .navigation
                    .clone()
                    .expect("navigation overlay");
                navigation.update(cx, |navigation, cx| f(navigation, window, cx));
            })
            .unwrap();
        rec.cx.run_until_parked();
    };
    rec.cx
        .update_window(window, |view, window, cx| {
            let root = view.downcast::<RootView>().unwrap();
            root.update(cx, |root, cx| {
                root.run_command(CommandId::SearchNotes, window, cx)
            });
        })
        .unwrap();
    rec.cx.run_until_parked();
    // The index builds off the main thread.
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(60));
        rec.cx.run_until_parked();
    }
    rec.shot(900);
    for ch in "pricing".chars() {
        navigation(&mut rec, &|n, _, cx| n.type_for_test(&ch.to_string(), cx));
        rec.shot(110);
    }
    rec.shot(1100);
    navigation(&mut rec, &|n, _, cx| n.arrow_for_test(1, cx));
    rec.shot(700);
    navigation(&mut rec, &|n, _, cx| n.arrow_for_test(1, cx));
    rec.shot(1800);
    rec.finish();
}

#[test]
#[ignore = "writes What's New clip frames to the DIRI_VISUAL_OUTPUT directory"]
fn render_whats_new_clips() {
    let output = PathBuf::from(std::env::var("DIRI_VISUAL_OUTPUT").unwrap());
    let only = std::env::var("DIRI_CLIP").ok();
    let only_theme = std::env::var("DIRI_CLIP_THEME").ok();
    // The account row shows $USER; never the recording machine's.
    // SAFETY: set before the app context starts any thread.
    unsafe { std::env::set_var("USER", "alex") };
    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(diri_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| {
        crate::fonts::init(cx);
        cx.set_reduce_motion(true);
        cx.bind_keys(crate::notes::key_bindings());
    });
    diri_ui::set_mark_rasterizer(crate::macos::brand_raster::raster_mark);
    type Record = fn(&mut HeadlessAppContext, PathBuf, bool);
    let clips: [(&str, Record); 3] = [
        ("notes", record_notes),
        ("todos", record_todos),
        ("search", record_search),
    ];
    for (name, record) in clips {
        if only.as_deref().is_some_and(|only| only != name) {
            continue;
        }
        for (theme, light) in [("dark", false), ("light", true)] {
            if only_theme.as_deref().is_some_and(|only| only != theme) {
                continue;
            }
            record(&mut cx, output.join(format!("{name}-{theme}")), light);
        }
    }
}

/// The footer line and the sheet it opens, in the real window.
/// `DIRI_VISUAL_OUTPUT=<dir>` gets `{pill,pill-hover,sheet,sheet-2}-{dark,light}.png`.
#[test]
#[ignore = "writes What's New screenshots to the DIRI_VISUAL_OUTPUT directory"]
fn render_whats_new_screenshots() {
    let output = PathBuf::from(std::env::var("DIRI_VISUAL_OUTPUT").unwrap());
    std::fs::create_dir_all(&output).unwrap();
    // SAFETY: set before the app context starts any thread.
    unsafe { std::env::set_var("USER", "alex") };
    crate::whats_new::set_current_version_for_test(Some(crate::whats_new::RELEASES[0].version));
    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(diri_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| crate::fonts::init(cx));
    diri_ui::set_mark_rasterizer(crate::macos::brand_raster::raster_mark);
    for (name, light) in [("dark", false), ("light", true)] {
        let services = test_services();
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(SidebarPreviewFixture::make(PreviewScenario::Typical).list);
            store.mark_connected_for_test();
            store
                .update_preferences(|prefs| {
                    prefs.sidebar_visible = true;
                    prefs.terminal_theme = theme(light).into();
                    prefs.window_material = crate::store::WindowMaterial::Opaque;
                    prefs.whats_new_seen_version = "0.0.1".into();
                })
                .unwrap();
        }
        let window: AnyWindowHandle = cx
            .open_window(size(px(1240.0), px(800.0)), |window, cx| {
                cx.new(|cx| RootView::new(services, false, PreviewScenario::Empty, window, cx))
            })
            .unwrap()
            .into();
        cx.run_until_parked();
        let mut rec = Recorder::new(&mut cx, window, output.join(format!("frames-{name}")));
        rec.settle();
        let shot = |rec: &mut Recorder, file: &str| {
            rec.settle();
            rec.cx
                .capture_screenshot(rec.window)
                .unwrap()
                .save(output.join(format!("{file}-{name}.png")))
                .unwrap();
        };
        shot(&mut rec, "pill");
        // Rest the pointer on the line: the dismiss control shows.
        rec.cx
            .update_window(window, |_, window, cx| {
                // The line sits just above the account row.
                window.simulate_mouse_move(gpui::point(px(120.0), px(748.0)), cx);
            })
            .unwrap();
        rec.cx.run_until_parked();
        shot(&mut rec, "pill-hover");
        rec.cx
            .update_window(window, |view, window, cx| {
                let root = view.downcast::<RootView>().unwrap();
                root.update(cx, |root, cx| root.open_whats_new(window, cx));
            })
            .unwrap();
        // The clip decodes on its own thread; wait for frames to arrive.
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(50));
            rec.cx.run_until_parked();
        }
        shot(&mut rec, "sheet");
        rec.cx
            .update_window(window, |view, window, cx| {
                let root = view.downcast::<RootView>().unwrap();
                let sheet = root.read(cx).whats_new.clone().unwrap();
                sheet.update(cx, |sheet, cx| sheet.go(1, window, cx));
            })
            .unwrap();
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(50));
            rec.cx.run_until_parked();
        }
        shot(&mut rec, "sheet-2");
        rec.finish();
    }
    crate::whats_new::set_current_version_for_test(None);
}
