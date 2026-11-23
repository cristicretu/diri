use std::sync::Arc;

use diri_proto::surface::{Interaction, Scope, SurfaceDoc, fixtures};
use diri_surfaces::{Author, SurfaceStore};
use gpui::{AppContext as _, Entity, TestAppContext};

use super::view::{Mode, SurfaceView, Target};
use super::{SurfacesEvent, SurfacesModel};
use crate::store::StoreRuntime;

fn seeded(cx: &mut TestAppContext) -> (tempfile::TempDir, Arc<SurfaceStore>, Entity<SurfacesModel>, String) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SurfaceStore::open(dir.path().join("surfaces")).unwrap());
    let doc = store
        .create(
            fixtures::whats_for_today("", Scope::Global, &[]),
            &Author::Agent("owner-1".into()),
        )
        .unwrap();
    let runtime = Arc::new(StoreRuntime::inert());
    let model = cx.update(|cx| {
        let model =
            cx.new(|cx| SurfacesModel::with_store(Arc::clone(&runtime), Some(Arc::clone(&store)), false, cx));
        SurfacesModel::install(model.clone(), cx);
        model
    });
    model.update(cx, |model, cx| model.sync(cx));
    cx.run_until_parked();
    (dir, store, model, doc.id)
}

#[gpui::test]
fn reads_the_directory_and_lists_pinned_surfaces(cx: &mut TestAppContext) {
    let (_dir, _store, model, id) = seeded(cx);
    model.read_with(cx, |model, _| {
        assert!(model.is_ready());
        let pinned = model.pinned();
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned[0].id, id);
        assert!(model.for_session("s1").is_empty());
    });
}

#[gpui::test]
fn ticks_show_at_once_and_reach_disk(cx: &mut TestAppContext) {
    let (_dir, store, model, id) = seeded(cx);
    model.update(cx, |model, cx| {
        model.interact(
            &id,
            Interaction::Check {
                checklist: "today".into(),
                item: "spec".into(),
                checked: true,
            },
            cx,
        );
        // Optimistic: the model shows it before the write lands.
        assert_eq!(model.get(&id).unwrap().doc.checklist_progress(), (2, 5));
    });
    cx.run_until_parked();
    let on_disk = store.load(&id).unwrap().doc;
    assert_eq!(on_disk.state.checks.get("today/spec"), Some(&true));
    let events = store.events(&id, 0, 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].delivered_to.as_deref(), Some("owner-1"));
}

#[gpui::test]
fn refused_clicks_explain_themselves(cx: &mut TestAppContext) {
    let (_dir, store, model, _) = seeded(cx);
    let orphan = store
        .create(SurfaceDoc::new("", "Orphan", Scope::Global), &Author::User)
        .unwrap();
    model.update(cx, |model, cx| model.mark_dirty(cx));
    cx.run_until_parked();
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    cx.update(|cx| {
        let events = Arc::clone(&events);
        cx.subscribe(&model, move |_, event: &SurfacesEvent, _| {
            events.lock().unwrap().push(event.clone());
        })
        .detach();
    });
    model.update(cx, |model, cx| model.ask_refresh(&orphan.id, cx));
    model.update(cx, |model, cx| model.open_link(&orphan.id, "javascript:alert(1)", cx));
    model.update(cx, |model, cx| model.open_link(&orphan.id, "diri://session/s-9", cx));
    let events = events.lock().unwrap().clone();
    assert!(matches!(&events[0], SurfacesEvent::Feedback { error: true, .. }), "{events:?}");
    assert!(matches!(&events[1], SurfacesEvent::Feedback { error: true, .. }));
    assert_eq!(
        events[2],
        SurfacesEvent::Navigate(super::Navigate::Session(diri_proto::SessionId::new("s-9")))
    );
}

#[gpui::test]
fn a_new_session_surface_is_announced_once(cx: &mut TestAppContext) {
    let (_dir, store, model, _) = seeded(cx);
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    cx.update(|cx| {
        let events = Arc::clone(&events);
        cx.subscribe(&model, move |_, event: &SurfacesEvent, _| {
            events.lock().unwrap().push(event.clone());
        })
        .detach();
    });
    let created = store
        .create(
            SurfaceDoc::new("", "Debug board", Scope::Session { session: "s-7".into() }),
            &Author::Agent("s-7".into()),
        )
        .unwrap();
    for _ in 0..2 {
        model.update(cx, |model, cx| model.mark_dirty(cx));
        cx.run_until_parked();
    }
    let events = events.lock().unwrap().clone();
    assert_eq!(
        events,
        vec![SurfacesEvent::SessionSurfaceCreated {
            session: diri_proto::SessionId::new("s-7"),
            surface: created.id.clone(),
        }]
    );
    model.read_with(cx, |model, _| assert_eq!(model.for_session("s-7").len(), 1));
}

#[gpui::test]
fn an_idle_model_does_no_work(cx: &mut TestAppContext) {
    let (_dir, _store, model, _) = seeded(cx);
    // Rendering asks to sync; with nothing reported by the directory it
    // spawns nothing and touches no file.
    model.update(cx, |model, cx| {
        model.sync(cx);
        assert!(!model.dirty);
    });
    cx.run_until_parked();
    model.read_with(cx, |model, _| assert!(!model.dirty));
}

#[gpui::test]
fn the_view_draws_page_and_panel(cx: &mut TestAppContext) {
    let (_dir, _store, model, id) = seeded(cx);
    let runtime = Arc::new(StoreRuntime::inert());
    for (mode, target) in [
        (Mode::Page, Target::Surface(id.clone())),
        (Mode::Panel, Target::Session(diri_proto::SessionId::new("nobody"))),
    ] {
        let (view, cx) = cx.add_window_view({
            let runtime = Arc::clone(&runtime);
            let model = model.clone();
            move |_, cx| SurfaceView::new(runtime, model, mode, target, cx)
        });
        cx.run_until_parked();
        let shown = view.read_with(cx, |view, cx| view.shown_surface(cx).map(|doc| doc.id.clone()));
        match mode {
            Mode::Page => assert_eq!(shown.as_deref(), Some(id.as_str())),
            Mode::Panel => assert_eq!(shown, None, "a session with no surface shows the empty state"),
        }
    }
}

/// The worst a surface may be (every level of nesting, every component the
/// bounds allow) still draws on a test thread's small stack and in bounded
/// time: a buggy agent cannot freeze or crash the window.
#[gpui::test]
fn the_largest_allowed_surfaces_draw(cx: &mut TestAppContext) {
    use diri_proto::surface::limits::{MAX_CHILDREN, MAX_DEPTH, MAX_NODES};
    use diri_proto::surface::{Node, NodeKind};
    let (_dir, store, model, _) = seeded(cx);
    // Deepest: cards nested to the limit, a checklist at the bottom.
    let mut deep = Node::with_id(
        "leaf",
        NodeKind::Checklist {
            title: None,
            items: vec![diri_proto::surface::node::ChecklistItem {
                id: "a".into(),
                text: "deep".into(),
                icon: None,
                link_label: None,
                checked: false,
                link: Some("https://github.com/a/b/pull/1".into()),
                detail: None,
            }],
            notify: false,
        },
    );
    for level in 1..MAX_DEPTH {
        deep = Node::new(NodeKind::Card {
            title: Some(format!("level {level}")),
            subtitle: None,
            tone: None,
            children: vec![deep],
        });
    }
    let mut deepest = SurfaceDoc::new("", "Deepest", Scope::Global);
    deepest.root = vec![deep];
    let deepest = store.create(deepest, &Author::User).unwrap();
    // Widest: as many components as allowed, in full containers.
    let per = MAX_CHILDREN - 1;
    let stacks = MAX_NODES / (per + 1);
    let mut widest = SurfaceDoc::new("", "Widest", Scope::Global);
    widest.root = (0..stacks)
        .map(|s| {
            Node::new(NodeKind::Stack {
                gap: None,
                children: (0..per)
                    .map(|n| {
                        Node::new(NodeKind::Badge {
                            label: format!("{s}-{n}"),
                            tone: None,
                            bind: None,
                        })
                    })
                    .collect(),
            })
        })
        .collect();
    let widest = store.create(widest, &Author::User).unwrap();
    model.update(cx, |model, cx| model.mark_dirty(cx));
    cx.run_until_parked();
    let runtime = Arc::new(StoreRuntime::inert());
    for id in [deepest.id, widest.id] {
        let started = std::time::Instant::now();
        let (view, cx) = cx.add_window_view({
            let runtime = Arc::clone(&runtime);
            let model = model.clone();
            move |_, cx| SurfaceView::new(runtime, model, Mode::Page, Target::Surface(id), cx)
        });
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, cx| view.shown_surface(cx).is_some()));
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    }
}

/// Renders "What's for Today" offscreen: `DIRI_VISUAL_OUTPUT=<png>`,
/// `DIRI_VISUAL_DARK=1` for the dark palette, `DIRI_VISUAL_SURFACE=panel`
/// for the right-panel layout, `DIRI_VISUAL_SURFACE_TAB=week` for the second
/// tab.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "writes the Live Surface screenshot artifact"]
fn render_live_surface_screenshot() {
    use gpui::{HeadlessAppContext, px, size};
    let output = std::env::var("DIRI_VISUAL_OUTPUT").expect("DIRI_VISUAL_OUTPUT");
    let dark = std::env::var_os("DIRI_VISUAL_DARK").is_some();
    let panel = std::env::var("DIRI_VISUAL_SURFACE").is_ok_and(|v| v == "panel");
    let colors = if dark {
        diri_ui::SemanticColors::dark()
    } else {
        diri_ui::SemanticColors::light()
    };
    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(diri_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| {
        crate::fonts::init(cx);
        cx.set_reduce_motion(true);
    });
    // Live sessions from the sidebar fixture, so session rows and the
    // bound metrics show real statuses.
    let fixture = crate::sidebar::SidebarPreviewFixture::make(
        crate::sidebar::PreviewScenario::Typical,
    );
    let mut sessions: Vec<String> = fixture
        .list
        .sessions
        .iter()
        .filter(|record| !record.is_note() && !record.is_archived())
        .map(|record| record.id.0.clone())
        .collect();
    sessions.sort();
    sessions.truncate(3);
    let runtime = Arc::new(StoreRuntime::inert());
    runtime.store.write().unwrap().hydrate(fixture.list);

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SurfaceStore::open(dir.path().join("surfaces")).unwrap());
    let scope = if panel {
        Scope::Session { session: sessions[0].clone() }
    } else {
        Scope::Global
    };
    let mut doc = fixtures::whats_for_today("", scope, &sessions);
    if panel {
        doc.pinned = false;
        doc.title = "Checkout retry: review".into();
        doc.subtitle = Some("What this agent is doing and what it needs from you".into());
    }
    let doc = store.create(doc, &Author::Agent(sessions[0].clone())).unwrap();
    store
        .interact(&doc.id, &Interaction::Check { checklist: "today".into(), item: "review-812".into(), checked: true })
        .unwrap();
    store
        .interact(&doc.id, &Interaction::Select { id: "focus".into(), value: "review".into() })
        .unwrap();
    // `DIRI_VISUAL_SURFACE_TICK=<item>` ticks one more item, as a click would.
    if let Ok(item) = std::env::var("DIRI_VISUAL_SURFACE_TICK") {
        store
            .interact(&doc.id, &Interaction::Check { checklist: "today".into(), item, checked: true })
            .unwrap();
    }
    let (width, height) = if panel { (420.0, 980.0) } else { (1240.0, 1120.0) };
    let window = cx
        .open_window(size(px(width), px(height)), {
            let runtime = Arc::clone(&runtime);
            let store = Arc::clone(&store);
            let id = doc.id.clone();
            let session = sessions[0].clone();
            move |_, cx| {
                let model = cx.new(|cx| SurfacesModel::with_store(Arc::clone(&runtime), Some(store), false, cx));
                SurfacesModel::install(model.clone(), cx);
                let todos = cx.new(|cx| crate::notes::todos::TodosModel::with_store(Arc::clone(&runtime), None, false, cx));
                crate::notes::todos::TodosModel::install(todos, cx);
                let (mode, target) = if panel {
                    (Mode::Panel, Target::Session(diri_proto::SessionId::new(session)))
                } else {
                    (Mode::Page, Target::Surface(id))
                };
                let view = cx.new(|cx| {
                    let mut view = SurfaceView::new(runtime, model, mode, target, cx);
                    view.colors_override = Some(colors);
                    view.now_override = Some(diri_surfaces::now_ms() + 120_000);
                    view
                });
                cx.new(|_| Host { view, colors, panel })
            }
        })
        .unwrap();
    if std::env::var("DIRI_VISUAL_SURFACE_TAB").is_ok_and(|v| v == "week") {
        cx.update_window(window.into(), |root, _, cx| {
            let host = root.downcast::<Host>().unwrap();
            let view = host.read(cx).view.clone();
            view.update(cx, |view, cx| view.select_tab_for_test("views", "tab-week", cx));
        })
        .unwrap();
    }
    for _ in 0..4 {
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, _| window.refresh()).unwrap();
    }
    cx.run_until_parked();
    cx.capture_screenshot(window.into()).unwrap().save(&output).unwrap();
    cx.update_window(window.into(), |_, window, _| window.remove_window()).unwrap();
    cx.run_until_parked();
}

#[cfg(target_os = "macos")]
struct Host {
    view: Entity<SurfaceView>,
    colors: diri_ui::SemanticColors,
    panel: bool,
}

#[cfg(target_os = "macos")]
impl gpui::Render for Host {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        use gpui::{ParentElement as _, Styled as _, div};
        let fill = if self.panel {
            crate::right_panel::panel_background(self.colors)
        } else {
            self.colors.background
        };
        div().size_full().bg(fill).child(self.view.clone())
    }
}

/// One of the example Lua apps, set up as an agent would leave it.
pub(crate) fn example_app(store: &SurfaceStore, name: &str, scope: Scope, approve: bool) -> String {
    use diri_proto::surface::{AppSpec, SyncSpec};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/surfaces/examples").join(name);
    let mut doc = SurfaceDoc::new("", name, scope);
    doc.pinned = doc.scope.is_saved();
    doc.app = Some(AppSpec {
        sync: Some(SyncSpec { every_s: Some(900), ..SyncSpec::default() }),
    });
    let doc = store.create(doc, &Author::Agent("s-agent".into())).unwrap();
    let source = std::fs::read_to_string(root.join("app.lua")).unwrap();
    store.write_file(&doc.id, "app.lua", source.as_bytes()).unwrap();
    for extra in ["repo.txt", "playlist.txt"] {
        if let Ok(text) = std::fs::read(root.join(extra)) {
            store.write_file(&doc.id, extra, &text).unwrap();
        }
    }
    let data: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join("data.json")).unwrap()).unwrap();
    store.write_data(&doc.id, &data).unwrap();
    if approve {
        store.approve(&doc.id, &diri_script::app::digest(&source)).unwrap();
    }
    doc.id
}

/// Renders an example Lua app offscreen: `DIRI_VISUAL_OUTPUT=<png>`,
/// `DIRI_VISUAL_APP=focus|queue`, `DIRI_VISUAL_DARK=1`,
/// `DIRI_VISUAL_SURFACE=panel` for the right panel, `DIRI_VISUAL_REVIEW=1`
/// for the approval review (an unapproved app whose sync asked to run git).
#[cfg(target_os = "macos")]
#[test]
#[ignore = "writes the Lua app screenshot artifact"]
fn render_lua_app_screenshot() {
    use gpui::{HeadlessAppContext, px, size};
    let output = std::env::var("DIRI_VISUAL_OUTPUT").expect("DIRI_VISUAL_OUTPUT");
    let dark = std::env::var_os("DIRI_VISUAL_DARK").is_some();
    let panel = std::env::var("DIRI_VISUAL_SURFACE").is_ok_and(|v| v == "panel");
    let review = std::env::var_os("DIRI_VISUAL_REVIEW").is_some();
    let name = match std::env::var("DIRI_VISUAL_APP").as_deref() {
        Ok("queue") => "study-queue",
        _ => "focus-ledger",
    };
    let colors = if dark { diri_ui::SemanticColors::dark() } else { diri_ui::SemanticColors::light() };
    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(diri_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| {
        crate::fonts::init(cx);
        cx.set_reduce_motion(true);
    });
    let runtime = Arc::new(StoreRuntime::inert());
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SurfaceStore::open(dir.path().join("surfaces")).unwrap());
    let scope = if panel { Scope::Session { session: "s-agent".into() } } else { Scope::Global };
    let id = example_app(&store, name, scope, !review);
    let model = cx.update(|cx| {
        let model = cx.new(|cx| SurfacesModel::with_store(Arc::clone(&runtime), Some(Arc::clone(&store)), false, cx));
        SurfacesModel::install(model.clone(), cx);
        let todos = cx.new(|cx| crate::notes::todos::TodosModel::with_store(Arc::clone(&runtime), None, false, cx));
        crate::notes::todos::TodosModel::install(todos, cx);
        model.update(cx, |model, cx| {
            model.set_dark(dark, cx);
            model.sync(cx);
        });
        model
    });
    cx.run_until_parked();
    for _ in 0..50 {
        if cx.update(|cx| model.read(cx).get(&id).is_some()) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        cx.run_until_parked();
    }
    let now = diri_surfaces::now_ms();
    cx.update(|cx| {
        model.update(cx, |model, cx| {
            if review {
                model.set_needs_approval_for_test(&id, cx);
            } else {
                model.set_synced_for_test(&id, now - 120_000, cx);
            }
        })
    });
    let (width, height) = if panel { (440.0, 1000.0) } else { (1240.0, if name == "study-queue" { 860.0 } else { 820.0 }) };
    let window = cx
        .open_window(size(px(width), px(height)), {
            let runtime = Arc::clone(&runtime);
            let model = model.clone();
            let id = id.clone();
            move |_, cx| {
                let (mode, target) = if panel {
                    (Mode::Panel, Target::Session(diri_proto::SessionId::new("s-agent")))
                } else {
                    (Mode::Page, Target::Surface(id))
                };
                let view = cx.new(|cx| {
                    let mut view = SurfaceView::new(runtime, model, mode, target, cx);
                    view.colors_override = Some(colors);
                    view.now_override = Some(now);
                    if review {
                        view.open_review_for_test(cx);
                    }
                    view
                });
                cx.new(|_| Host { view, colors, panel })
            }
        })
        .unwrap();
    // Thumbnails arrive from the shared web-image cache off the main thread.
    for _ in 0..40 {
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, _| window.refresh()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    cx.run_until_parked();
    cx.capture_screenshot(window.into()).unwrap().save(&output).unwrap();
    cx.update_window(window.into(), |_, window, _| window.remove_window()).unwrap();
    cx.run_until_parked();
}

fn lua_model(cx: &mut TestAppContext, name: &str, approve: bool) -> (tempfile::TempDir, Arc<SurfaceStore>, Entity<SurfacesModel>, String) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SurfaceStore::open(dir.path().join("surfaces")).unwrap());
    let id = example_app(&store, name, Scope::Global, approve);
    let runtime = Arc::new(StoreRuntime::inert());
    let model = cx.update(|cx| {
        let model = cx.new(|cx| SurfacesModel::with_store(runtime, Some(Arc::clone(&store)), false, cx));
        SurfacesModel::install(model.clone(), cx);
        model
    });
    model.update(cx, |model, cx| model.sync(cx));
    cx.run_until_parked();
    (dir, store, model, id)
}

#[gpui::test]
fn lua_apps_render_natively_with_their_own_identity(cx: &mut TestAppContext) {
    let (_dir, store, model, id) = lua_model(cx, "study-queue", true);
    model.read_with(cx, |model, _| {
        let surface = model.get(&id).unwrap();
        let run = surface.app.as_ref().unwrap();
        assert_eq!(run.error, None);
        assert!(run.approved);
        assert_eq!(surface.doc.title, "Study Queue", "the app's own title");
        assert_eq!(surface.doc.icon, Some(diri_proto::surface::Icon::Emoji("🎧".into())));
        assert_eq!(surface.doc.accent.as_deref(), Some("#f97316"));
        let mut nodes = 0;
        diri_proto::surface::node::walk(&surface.doc.root, &mut |_, _| nodes += 1);
        assert!(nodes > 20, "{nodes}");
    });
    // A click runs on_event; the state it returns is kept and redrawn.
    model.update(cx, |model, cx| {
        model.app_event(&id, serde_json::json!({"type": "action", "name": "next"}), true, cx)
    });
    cx.run_until_parked();
    assert_eq!(store.load(&id).unwrap().doc.state.app.unwrap()["playing"], 2);
    model.read_with(cx, |model, _| {
        assert_eq!(model.get(&id).unwrap().doc.state.app.as_ref().unwrap()["playing"], 2);
    });
}

#[gpui::test]
fn an_unapproved_sync_asks_and_changes_nothing(cx: &mut TestAppContext) {
    let (_dir, store, model, id) = lua_model(cx, "focus-ledger", false);
    let before = store.read_data(&id).unwrap();
    model.update(cx, |model, cx| model.sync_app(&id, cx));
    cx.run_until_parked();
    model.read_with(cx, |model, _| {
        let run = model.get(&id).unwrap().app.as_ref().unwrap();
        assert!(run.sync.needs_approval, "{:?}", run.sync);
        assert!(!run.approved);
    });
    assert_eq!(store.read_data(&id).unwrap(), before);
}

#[gpui::test]
fn editing_the_app_withdraws_its_approval(cx: &mut TestAppContext) {
    let (_dir, store, model, id) = lua_model(cx, "study-queue", false);
    let digest = model.read_with(cx, |model, _| model.get(&id).unwrap().app.as_ref().unwrap().digest.clone());
    store.approve(&id, &digest).unwrap();
    model.update(cx, |model, cx| model.mark_dirty(cx));
    cx.run_until_parked();
    assert!(model.read_with(cx, |model, _| model.get(&id).unwrap().app.as_ref().unwrap().approved));
    let source = store.app_source(&id).unwrap().unwrap();
    store.write_file(&id, "app.lua", format!("{source}\n-- edited").as_bytes()).unwrap();
    model.update(cx, |model, cx| model.mark_dirty(cx));
    cx.run_until_parked();
    assert!(!model.read_with(cx, |model, _| model.get(&id).unwrap().app.as_ref().unwrap().approved));
}

#[gpui::test]
fn a_lua_error_shows_instead_of_freezing(cx: &mut TestAppContext) {
    let (_dir, store, model, id) = lua_model(cx, "study-queue", true);
    store.write_file(&id, "app.lua", b"function render() while true do end end").unwrap();
    model.update(cx, |model, cx| model.mark_dirty(cx));
    cx.run_until_parked();
    model.read_with(cx, |model, _| {
        let surface = model.get(&id).unwrap();
        let error = surface.app.as_ref().unwrap().error.clone().unwrap();
        assert!(error.starts_with("budget"), "{error}");
        assert!(surface.doc.root.is_empty());
    });
}

#[gpui::test]
fn only_a_drawn_view_polls(cx: &mut TestAppContext) {
    let (_dir, _store, model, id) = lua_model(cx, "study-queue", true);
    let now = diri_surfaces::now_ms();
    model.update(cx, |model, cx| model.set_synced_for_test(&id, now, cx));
    // Nothing is scheduled by the model itself: polling belongs to a view.
    assert!(model.read_with(cx, |model, _| model.poll_due(&id, now)).unwrap() > std::time::Duration::from_secs(800));
    let runtime = Arc::new(StoreRuntime::inert());
    let (view, cx) = cx.add_window_view({
        let model = model.clone();
        let id = id.clone();
        move |_, cx| SurfaceView::new(runtime, model, Mode::Page, Target::Surface(id), cx)
    });
    cx.run_until_parked();
    let scheduled = view.read_with(cx, |view, _| view.scheduled_poll().map(str::to_owned));
    assert_eq!(scheduled.as_deref(), Some(id.as_str()), "a visible app waits for its next poll");
}
