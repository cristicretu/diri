//! Backlinks under a note and the notes graph, driven through the note pane
//! with a fixture link index.

use std::sync::Arc;

use diri_notes::backlinks::LinkIndex;
use diri_notes::store::NoteStore;
use diri_proto::SessionId;

use super::*;
use crate::store::StoreRuntime;

struct Fixture {
    _dir: tempfile::TempDir,
    store: Arc<NoteStore>,
    plan: String,
    brief: String,
    retro: String,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(NoteStore::open(dir.path().join("notes")).expect("store"));
    let create = |md: &str| {
        let (_, doc) = diri_notes::markdown::parse(md);
        store.create(doc, None).expect("create").0
    };
    let plan = create("# Release plan\n\nShip on Friday.\n");
    let brief = create(&format!(
        "# Launch brief\n\nBuilds on [@Release plan](diri://note/{plan}) and the pricing page.\n"
    ));
    let retro = create("# Retro\n\nThe release plan slipped a day.\n");
    Fixture {
        _dir: dir,
        store,
        plan,
        brief,
        retro,
    }
}

/// A pane on `note`, with the shared model holding the store's links as
/// the off-thread pass would have built them.
fn open<'a>(
    cx: &'a mut gpui::TestAppContext,
    fixture: &Fixture,
    note: &str,
) -> (Entity<NotePane>, &'a mut gpui::VisualTestContext) {
    let runtime = Arc::new(StoreRuntime::inert());
    let index = LinkIndex::read(&fixture.store).expect("links");
    let model_runtime = Arc::clone(&runtime);
    cx.update(|cx| {
        let model = todos::TodosModel::global(&model_runtime, cx);
        model.update(cx, |model, cx| model.set_links_for_test(index, cx));
    });
    let store = Arc::clone(&fixture.store);
    let (pane, cx) =
        cx.add_window_view(move |_, cx| NotePane::with_store(runtime, Some(store), false, cx));
    let session = SessionId::new("s_note");
    let note = note.to_owned();
    pane.update_in(cx, |pane, window, cx| {
        pane.show(&session, &note, window, cx)
    });
    (pane, cx)
}

#[gpui::test]
fn backlinks_and_unlinked_mentions_show_under_the_note(cx: &mut gpui::TestAppContext) {
    let fixture = fixture();
    let (pane, cx) = open(cx, &fixture, &fixture.plan);
    let footer = pane
        .read_with(cx, |pane, _| pane.backlinks_for_test())
        .expect("a footer under the open note");
    let (linked, unlinked) =
        footer.read_with(cx, |view, _| (view.linked.clone(), view.unlinked.clone()));
    assert_eq!(linked.len(), 1);
    assert_eq!(linked[0].source, fixture.brief);
    assert_eq!(linked[0].source_title, "Launch brief");
    assert_eq!(
        &linked[0].context[linked[0].mention.clone()],
        "@Release plan"
    );
    assert_eq!(unlinked.len(), 1);
    assert_eq!(unlinked[0].source, fixture.retro);

    // Link turns the retro's words into a link, in the retro's file.
    pane.update(cx, |pane, cx| pane.link_unlinked(&unlinked[0], cx));
    let retro = fixture.store.load(&fixture.retro).unwrap().to_markdown();
    assert!(
        retro.contains(&format!(
            "The [@release plan](diri://note/{}) slipped a day.",
            fixture.plan
        )),
        "{retro}"
    );
    assert!(
        footer.read_with(cx, |view, _| view.unlinked.is_empty()),
        "the mention leaves the list at once"
    );
    // The editor shows the footer as its last child.
    let editor = pane
        .read_with(cx, |pane, _| pane.editor_for_test())
        .unwrap();
    assert!(editor.read_with(cx, |view, _| view.has_footer_for_test()));
}

#[gpui::test]
fn the_graph_shows_the_neighbourhood_or_every_note_and_settles(cx: &mut gpui::TestAppContext) {
    let fixture = fixture();
    let (pane, cx) = open(cx, &fixture, &fixture.plan);
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("note-footer").is_some(),
        "the note is drawn"
    );
    pane.update_in(cx, |pane, window, cx| {
        pane.show_graph(graph::Scope::Local, window, cx)
    });
    cx.run_until_parked();
    // Under glass the graph's fill is clear: the note must not be drawn
    // beneath it, or its text shows through the graph.
    assert!(
        cx.debug_bounds("note-footer").is_none(),
        "the graph replaces the note"
    );
    let graph = pane
        .read_with(cx, |pane, _| pane.graph_for_test())
        .expect("graph shown");
    graph.update(cx, |graph, cx| {
        let ids: Vec<&str> = graph.graph.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids.len(), 2, "the note and the note linking to it");
        assert_eq!(ids[0], fixture.plan, "centred on the open note");
        assert_eq!(graph.graph.edges, [(0, 1)]);
        graph.set_scope(graph::Scope::All, cx);
        assert_eq!(graph.graph.nodes.len(), 3, "every note");
        assert_eq!(graph.graph.edges.len(), 1);
        assert!(graph.animating(), "a fresh layout moves");
        let steps = graph.settle_for_test();
        assert!(steps > 0 && steps < 1_000, "{steps}");
        assert!(!graph.animating(), "a settled graph asks for no frames");
    });
    // The command toggles it away again.
    pane.update_in(cx, |pane, window, cx| {
        pane.toggle_graph(&crate::commands::NoteGraph, window, cx)
    });
    assert!(pane.read_with(cx, |pane, _| pane.graph_for_test().is_none()));
}

/// Renders a note's links offscreen (no window on screen):
/// `DIRI_VISUAL_OUTPUT=<png>`, `DIRI_VISUAL_NOTE_LINKS=backlinks|graph|all|menu|slash|find`
/// (`DIRI_VISUAL_NOTE_FIND=<query>` sets what `find` searches for),
/// `DIRI_VISUAL_DARK=1` for the dark palette.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "writes the note links screenshot artifact"]
fn render_note_links_screenshot() {
    use gpui::{AppContext as _, HeadlessAppContext, px, size};
    let output = std::env::var("DIRI_VISUAL_OUTPUT").expect("DIRI_VISUAL_OUTPUT");
    let scene = std::env::var("DIRI_VISUAL_NOTE_LINKS").unwrap_or_else(|_| "backlinks".into());
    let colors = if std::env::var_os("DIRI_VISUAL_DARK").is_some() {
        diri_ui::SemanticColors::dark()
    } else {
        diri_ui::SemanticColors::light()
    };
    let glass = std::env::var_os("DIRI_VISUAL_GLASS").is_some();
    let colors = if glass {
        colors.with_material(diri_ui::Material::Glass)
    } else {
        colors
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
        cx.bind_keys(key_bindings());
    });
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(NoteStore::open(dir.path().join("notes")).unwrap());
    let create = |md: &str| {
        let (_, doc) = diri_notes::markdown::parse(md);
        store.create(doc, None).unwrap().0
    };
    let plan = match std::env::var("DIRI_VISUAL_NOTE_FILE") {
        Ok(path) => create(&std::fs::read_to_string(path).unwrap()),
        Err(_) => {
            create("# Q4 launch plan\n\nShip the onboarding emails before the pricing test.\n")
        }
    };
    let link =
        |id: &str, title: &str| diri_notes::backlinks::mention_markdown(&format!("@{title}"), id);
    let pricing = create(&format!(
        "# Pricing test\n\nPart of {}: two prices, one week.\n\n- [ ] Draft the variants\n",
        link(&plan, "Q4 launch plan")
    ));
    let emails = create(&format!(
        "# Onboarding emails\n\nThree emails, written against {} and the {}.\n",
        link(&plan, "Q4 launch plan"),
        link(&pricing, "Pricing test")
    ));
    let sync = create(&format!(
        "# Weekly sync\n\nWe agreed the Q4 launch plan moves a week. See {} for the copy.\n\n- [ ] Follow up on {}\n",
        link(&emails, "Onboarding emails"),
        link(&plan, "Q4 launch plan")
    ));
    let retro = create(&format!(
        "# Q3 retro\n\nWhat went well. The Q4 launch plan should start earlier. {}\n",
        link(&sync, "Weekly sync")
    ));
    let _ = create(&format!(
        "# Hiring\n\nTwo roles. Ask {} for the budget.\n",
        link(&retro, "Q3 retro")
    ));
    let _ = create("# Groceries\n\n- [ ] Oat milk\n");
    let _ = create(&format!(
        "# Brand voice\n\nUsed by {}.\n",
        link(&emails, "Onboarding emails")
    ));
    let index = LinkIndex::read(&store).unwrap();
    let runtime = Arc::new(StoreRuntime::inert());
    cx.update(|cx| {
        let model =
            cx.new(|cx| todos::TodosModel::with_store(Arc::clone(&runtime), None, false, cx));
        model.update(cx, |model, cx| model.set_links_for_test(index, cx));
        todos::TodosModel::install(model, cx);
    });
    let pane_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
    let window = cx
        .open_window(size(px(1000.0), px(720.0)), {
            let pane_slot = pane_slot.clone();
            let store = Arc::clone(&store);
            move |_, cx| {
                let pane = cx.new(|cx| {
                    let mut pane = NotePane::with_store(runtime, Some(store), false, cx);
                    pane.colors_override = Some(colors);
                    pane
                });
                *pane_slot.borrow_mut() = Some(pane.clone());
                cx.new(|_| ScreenshotHost {
                    pane,
                    colors,
                    glass,
                })
            }
        })
        .unwrap();
    let pane = pane_slot.borrow().clone().unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        pane.update(cx, |pane, cx| {
            pane.show(&SessionId::new("s_plan"), &plan, window, cx);
            if glass {
                pane.set_trailing_inset(GLASS_CONTROLS_INSET, cx);
            }
            if let Some(footer) = pane.backlinks_for_test() {
                footer.update(cx, |view, cx| view.expand_unlinked_for_test(cx));
            }
            match scene.as_str() {
                "graph" => pane.show_graph(graph::Scope::Local, window, cx),
                "all" => pane.show_graph(graph::Scope::All, window, cx),
                "menu" | "slash" => {
                    if let Some(editor) = pane.editor_for_test() {
                        editor.update(cx, |view, cx| {
                            view.open_menu_for_screenshot(scene == "slash", window, cx)
                        });
                    }
                }
                "find" => {
                    if let Some(editor) = pane.editor_for_test() {
                        let query = std::env::var("DIRI_VISUAL_NOTE_FIND")
                            .unwrap_or_else(|_| "launch plan".into());
                        editor.update(cx, |view, cx| {
                            view.open_find_for_screenshot(&query, window, cx)
                        });
                    }
                }
                _ => {}
            }
            if let Some(graph) = pane.graph_for_test() {
                graph.update(cx, |graph, _| {
                    graph.settle_for_test();
                });
            }
        });
    })
    .unwrap();
    for _ in 0..3 {
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, _| window.refresh())
            .unwrap();
    }
    cx.run_until_parked();
    if std::env::var_os("DIRI_VISUAL_NOTE_BOTTOM").is_some() {
        let editor = cx.update(|cx| pane.read(cx).editor_for_test()).unwrap();
        for _ in 0..200 {
            cx.update(|cx| editor.update(cx, |view, cx| view.scroll_by_for_test(px(-400.0), cx)));
            cx.run_until_parked();
            cx.update_window(window.into(), |_, window, _| window.refresh())
                .unwrap();
            cx.run_until_parked();
        }
    }
    cx.capture_screenshot(window.into())
        .unwrap()
        .save(&output)
        .unwrap();
    cx.update_window(window.into(), |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
}

/// Room the workbench keeps for a lone pane's split and inspector controls.
#[cfg(target_os = "macos")]
const GLASS_CONTROLS_INSET: f32 = diri_ui::Metrics::TOOLBAR_CONTROL_SIZE * 2.0 + 4.0;

/// The note pane as a single workbench pane hosts it: under glass, over a
/// stand-in wallpaper on the terminal's fill, with the pane's top-right
/// controls drawn over it.
#[cfg(target_os = "macos")]
struct ScreenshotHost {
    pane: Entity<NotePane>,
    colors: diri_ui::SemanticColors,
    glass: bool,
}

#[cfg(target_os = "macos")]
impl Render for ScreenshotHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        use diri_ui::Metrics;
        use gpui::px;
        if !self.glass {
            return div().size_full().child(self.pane.clone());
        }
        let colors = self.colors;
        let blob = |left: f32, top: f32, size: f32, color: u32| {
            div()
                .absolute()
                .left(px(left))
                .top(px(top))
                .size(px(size))
                .rounded(px(size / 2.0))
                .bg(gpui::rgba(color))
        };
        let control = div()
            .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
            .flex()
            .items_center()
            .justify_center()
            .child(crate::icons::sf_symbol(
                "rectangle.split.2x1",
                14.0,
                colors.secondary,
            ));
        div()
            .relative()
            .size_full()
            .bg(gpui::rgba(0x2b3a4fff))
            .child(blob(-80.0, -60.0, 420.0, 0x5d7f6cff))
            .child(blob(620.0, 380.0, 460.0, 0x8a5a6cff))
            .child(blob(380.0, 120.0, 260.0, 0x3f5f8aff))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .bg(colors.terminal_surface())
                    .child(self.pane.clone()),
            )
            .child(
                div()
                    .absolute()
                    .top(px(
                        (Metrics::TITLE_BAR - Metrics::TOOLBAR_CONTROL_SIZE) / 2.0
                    ))
                    .right(px(Metrics::TOOLBAR_EDGE_INSET))
                    .flex()
                    .gap(px(4.0))
                    .child(control)
                    .child(crate::right_panel::dispatching_toggle(
                        "screenshot-toggle-inspector",
                        false,
                        colors,
                        0.0,
                    )),
            )
    }
}
