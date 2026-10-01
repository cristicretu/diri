use super::*;
use diri_proto::{DateMillis, HistoryEntry};
use gpui::{Entity, TestAppContext, point, size};

struct Harness {
    overlay: Entity<NavigationOverlay>,
    previous_focus: FocusHandle,
}
impl Render for Harness {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let overlay = self.overlay.read(cx);
        let colors = overlay.colors();
        div()
            .key_context(crate::commands::APP_CONTEXT)
            .size_full()
            .bg(colors.background)
            .child(div().id("previous").track_focus(&self.previous_focus))
            .child(crate::root::cached_window_overlay(self.overlay.clone()))
    }
}

pub(super) fn seed_history(overlay: &mut NavigationOverlay) {
    overlay.overlay = Some(Overlay::History);
    overlay.history_scanner = None; // Fixtures never inspect the developer's chats.
    let titles = [
        "Make conversation search fast and useful",
        "Polish sidebar navigation and rounded popovers",
        "Keep remote sessions alive after reconnecting",
        "Fix terminal rendering when switching projects",
        "Add keyboard shortcuts for quick navigation",
        "Explore a simpler onboarding flow",
        "Investigate search results across multiple projects",
    ];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        * 1000.0;
    overlay.history = (0..640)
        .map(|index| HistoryEntry {
            id: format!("conversation-{index}"),
            kind: if index % 2 == 0 {
                AgentKind::CODEX
            } else {
                AgentKind::CLAUDE_CODE
            },
            cwd: "/work/diri".into(),
            title: Some(titles[index % titles.len()].into()),
            transcript_path: String::new(),
            last_active_at: DateMillis(now - index as f64 * 3_600_000.0),
            created_at: None,
            cwd_exists: index != 4,
        })
        .collect();
    overlay.history_search.rebuild(&overlay.history);
    overlay.filter_history();
}

#[gpui::test]
fn history_virtualizes_and_aligns_shared_header(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let previous_focus = cx.focus_handle();
        let overlay = cx.new(|cx| {
            let mut overlay =
                NavigationOverlay::opened_for_test(Arc::new(StoreRuntime::inert()), cx);
            seed_history(&mut overlay);
            overlay.focus_handle.focus(window, cx);
            overlay
        });
        Harness {
            overlay,
            previous_focus,
        }
    });
    cx.simulate_resize(size(px(800.0), px(700.0)));
    cx.run_until_parked();
    let escape = cx.debug_bounds("palette-escape").unwrap();
    let enter = cx.debug_bounds("history-return-0").unwrap();
    assert_eq!(enter.size, escape.size);
    assert_eq!(enter.left(), escape.left());
    assert!(cx.debug_bounds("history-row-639").is_none());
    let overlay = view.read_with(cx, |view, _| view.overlay.clone());
    cx.simulate_keystrokes("up");
    cx.run_until_parked();
    let row = cx
        .debug_bounds("history-row-639")
        .expect("wrap scrolls into view");
    let panel = cx.debug_bounds("command-palette").unwrap();
    assert!(row.top() >= panel.top() && row.bottom() <= panel.bottom());
    cx.simulate_keystrokes("s e a r c h");
    overlay.read_with(cx, |overlay, _| {
        assert_eq!(overlay.query.text(), "search");
        assert_eq!(overlay.highlight, 0);
        assert!(overlay.history_matches.len() < 640);
    });
    overlay.update_in(cx, |overlay, window, cx| {
        overlay.history_resuming = Some("already-opening".into());
        overlay.resume_history(overlay.history[0].clone(), window, cx);
        assert_eq!(overlay.history_resuming.as_deref(), Some("already-opening"));
    });
}

#[gpui::test]
fn closing_releases_page_indexes_but_keeps_scan_bookkeeping(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|_, cx| {
        let previous_focus = cx.focus_handle();
        let overlay = cx.new(|cx| {
            let mut overlay =
                NavigationOverlay::opened_for_test(Arc::new(StoreRuntime::inert()), cx);
            seed_history(&mut overlay);
            overlay
        });
        Harness {
            overlay,
            previous_focus,
        }
    });
    let overlay = view.read_with(cx, |view, _| view.overlay.clone());
    overlay.update(cx, |overlay, cx| {
        let entries = vec![quick_open::DirectoryEntry {
            path: "/work/diri".into(),
            name: "diri".into(),
            is_git_repo: true,
            depth: 1,
        }];
        overlay.quick_snapshot = quick_open::build_snapshot(&entries, &[], &[]);
        let scanned = Instant::now();
        overlay
            .directory_index
            .finish_scan(entries, scanned, String::new(), Vec::new());
        assert!(!overlay.history_search.rank("").is_empty());

        overlay.clear_overlay(cx);

        assert!(overlay.history.is_empty());
        assert!(overlay.history_matches.is_empty());
        assert!(overlay.history_search.rank("").is_empty());
        assert!(overlay.quick_snapshot.pool.is_empty());
        assert!(overlay.directory_index.entries().is_empty());
        // Releasing memory must not turn every reopen into a disk walk.
        assert!(!overlay.directory_index.needs_scan(scanned, "", &[]));
    });
}

#[gpui::test]
fn pages_restore_query_selection_and_focus_and_theme_cancel(cx: &mut TestAppContext) {
    let runtime = Arc::new(StoreRuntime::inert());
    let saved = runtime.store.read().unwrap().theme_id().to_owned();
    let for_view = runtime.clone();
    let (view, cx) = cx.add_window_view(|window, cx| {
        let previous_focus = cx.focus_handle();
        previous_focus.focus(window, cx);
        let overlay = cx.new(|cx| {
            let mut overlay = NavigationOverlay::opened_for_test(for_view, cx);
            overlay.clear_overlay(cx);
            overlay.open_overlay(Overlay::CommandPalette, window, cx);
            overlay
        });
        Harness {
            overlay,
            previous_focus,
        }
    });
    let overlay = view.read_with(cx, |view, _| view.overlay.clone());
    cx.simulate_keystrokes("t h e m e");
    overlay.update_in(cx, |overlay, window, cx| {
        overlay.run_highlighted(false, window, cx);
        assert_eq!(overlay.overlay, Some(Overlay::Themes));
        assert_eq!(overlay.back_stack.len(), 1);
        overlay.move_highlight(1, cx);
        assert_ne!(overlay.store.read().unwrap().theme_id(), saved);
        assert_eq!(
            overlay.store.read().unwrap().preferences().terminal_theme,
            saved
        );
        overlay.back(window, cx);
        assert_eq!(overlay.overlay, Some(Overlay::CommandPalette));
        assert_eq!(overlay.query.text(), "theme");
        assert_eq!(overlay.store.read().unwrap().theme_id(), saved);
        overlay.push_page(Overlay::Settings, window, cx);
        overlay.run_highlighted(false, window, cx);
        assert_eq!(overlay.overlay, Some(Overlay::Themes));
        overlay.move_highlight(1, cx);
    });
    cx.simulate_keystrokes("escape");
    assert_eq!(runtime.store.read().unwrap().theme_id(), saved);
    view.update_in(cx, |view, window, _| {
        assert!(view.previous_focus.is_focused(window))
    });
    overlay.update_in(cx, |overlay, window, cx| {
        overlay.open_overlay(Overlay::Themes, window, cx);
        overlay.move_highlight(1, cx);
        let chosen = overlay.store.read().unwrap().theme_id().to_owned();
        overlay.commit_theme(window, cx);
        assert!(!overlay.is_open());
        assert_eq!(
            overlay.store.read().unwrap().preferences().terminal_theme,
            chosen
        );
        assert!(overlay.store.read().unwrap().preview_theme_id().is_none());
    });
}

#[gpui::test]
fn shortcuts_switch_pages_without_stacking_overlays(cx: &mut TestAppContext) {
    let (overlay, cx) = cx.add_window_view(|_, cx| {
        NavigationOverlay::opened_for_test(Arc::new(StoreRuntime::inert()), cx)
    });
    overlay.update_in(cx, |overlay, window, cx| {
        // A seeded scanner makes this a pure navigation test.
        seed_history(overlay);
        overlay
            .directory_index
            .finish_scan(Vec::new(), Instant::now(), String::new(), Vec::new());
        overlay.overlay = None;
        overlay.toggle_history(&ToggleHistory, window, cx);
        assert_eq!(overlay.overlay, Some(Overlay::History));
        overlay.toggle_quick_open(&ToggleQuickOpen, window, cx);
        assert_eq!(overlay.overlay, Some(Overlay::QuickOpen));
        overlay.toggle_command_palette(&ToggleCommandPalette, window, cx);
        assert_eq!(overlay.overlay, Some(Overlay::CommandPalette));
        assert!(overlay.back_stack.is_empty());
        overlay.toggle_command_palette(&ToggleCommandPalette, window, cx);
        assert!(!overlay.is_open());
    });
}

#[test]
fn theme_preview_never_persists_even_if_other_preferences_are_saved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prefs.json");
    let (mut store, _) = SessionStore::load(path.clone()).unwrap();
    let saved = store.preferences().terminal_theme.clone();
    store.preview_theme(Some("vesper".into()));
    store
        .update_preferences(|prefs| prefs.status_sounds = !prefs.status_sounds)
        .unwrap();
    let (reloaded, _) = SessionStore::load(path).unwrap();
    assert_eq!(reloaded.preferences().terminal_theme, saved);
    assert_eq!(store.theme_id(), "vesper");
    store.preview_theme(None);
    assert_eq!(store.theme_id(), saved);
}

#[gpui::test]
fn large_project_page_only_builds_visible_rows(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        let previous_focus = cx.focus_handle();
        let overlay = cx.new(|cx| {
            let mut overlay =
                NavigationOverlay::opened_for_test(Arc::new(StoreRuntime::inert()), cx);
            overlay.overlay = Some(Overlay::QuickOpen);
            overlay.quick_snapshot.folders = (0..20_000)
                .map(|index| QuickOpenItem {
                    name: format!("project-{index}"),
                    path: PathBuf::from(format!("/work/project-{index}")),
                    is_git_repo: true,
                })
                .collect();
            overlay.focus_handle.focus(window, cx);
            overlay
        });
        Harness {
            overlay,
            previous_focus,
        }
    });
    cx.simulate_resize(size(px(800.0), px(700.0)));
    cx.run_until_parked();
    assert!(cx.debug_bounds("palette-row-0").is_some());
    assert!(cx.debug_bounds("palette-row-10000").is_none());
    assert!(cx.debug_bounds("palette-row-19999").is_none());
    let started = Instant::now();
    cx.simulate_keystrokes("up");
    cx.run_until_parked();
    eprintln!(
        "20,000 projects: keyboard wrap and virtual layout {:?}",
        started.elapsed()
    );
    assert!(cx.debug_bounds("palette-row-19999").is_some());
    assert!(cx.debug_bounds("palette-row-10000").is_none());
    let overlay = view.read_with(cx, |view, _| view.overlay.clone());
    overlay.read_with(cx, |overlay, _| assert_eq!(overlay.highlight, 19_999));
}

#[gpui::test]
fn registered_shortcuts_route_to_the_focused_palette(cx: &mut TestAppContext) {
    cx.update(|cx| crate::commands::bind_keys(cx, &Default::default()));
    let (view, cx) = cx.add_window_view(|window, cx| {
        let previous_focus = cx.focus_handle();
        let overlay = cx.new(|cx| {
            let mut overlay =
                NavigationOverlay::opened_for_test(Arc::new(StoreRuntime::inert()), cx);
            seed_history(&mut overlay);
            overlay.directory_index.finish_scan(
                Vec::new(),
                Instant::now(),
                String::new(),
                Vec::new(),
            );
            overlay.focus_handle.focus(window, cx);
            overlay
        });
        Harness {
            overlay,
            previous_focus,
        }
    });
    let overlay = view.read_with(cx, |view, _| view.overlay.clone());
    cx.simulate_keystrokes(&crate::commands::test_chords("cmd-p"));
    assert_eq!(
        overlay.read_with(cx, |overlay, _| overlay.overlay),
        Some(Overlay::QuickOpen)
    );
    cx.simulate_keystrokes(&crate::commands::test_chords("cmd-k"));
    assert_eq!(
        overlay.read_with(cx, |overlay, _| overlay.overlay),
        Some(Overlay::CommandPalette)
    );
    cx.simulate_keystrokes(&crate::commands::test_chords("cmd-shift-h"));
    assert_eq!(
        overlay.read_with(cx, |overlay, _| overlay.overlay),
        Some(Overlay::History)
    );
    cx.simulate_keystrokes(&crate::commands::test_chords("cmd-shift-h"));
    assert!(!overlay.read_with(cx, |overlay, _| overlay.is_open()));
}

#[gpui::test]
fn pending_project_search_cannot_change_a_new_page(cx: &mut TestAppContext) {
    let (overlay, cx) = cx.add_window_view(|_, cx| {
        let mut overlay = NavigationOverlay::opened_for_test(Arc::new(StoreRuntime::inert()), cx);
        seed_history(&mut overlay);
        overlay
            .directory_index
            .finish_scan(Vec::new(), Instant::now(), String::new(), Vec::new());
        overlay
    });
    overlay.update_in(cx, |overlay, window, cx| {
        overlay.open_overlay(Overlay::QuickOpen, window, cx);
        overlay.query.insert("project");
        overlay.query_changed(cx);
    });
    cx.run_until_parked();
    overlay.update_in(cx, |overlay, window, cx| {
        overlay.open_overlay(Overlay::History, window, cx);
        overlay.query.insert("search");
        overlay.query_changed(cx);
        overlay.move_highlight(2, cx);
    });
    cx.executor().advance_clock(Duration::from_millis(50));
    cx.run_until_parked();
    overlay.read_with(cx, |overlay, _| {
        assert_eq!(overlay.overlay, Some(Overlay::History));
        assert_eq!(overlay.query.text(), "search");
        assert_eq!(overlay.highlight, 2);
        assert!(overlay.rank_task.is_none());
    });
}

#[gpui::test]
fn clicking_back_keeps_the_palette_open(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_reduce_motion(true));
    let (view, cx) = cx.add_window_view(|window, cx| {
        let previous_focus = cx.focus_handle();
        let overlay = cx.new(|cx| {
            let mut overlay =
                NavigationOverlay::opened_for_test(Arc::new(StoreRuntime::inert()), cx);
            seed_history(&mut overlay);
            overlay.focus_handle.focus(window, cx);
            overlay
        });
        Harness {
            overlay,
            previous_focus,
        }
    });
    cx.simulate_resize(size(px(800.0), px(700.0)));
    cx.run_until_parked();
    let back = cx.debug_bounds("palette-back").expect("back button");
    cx.simulate_click(back.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    let overlay = view.read_with(cx, |view, _| view.overlay.clone());
    overlay.read_with(cx, |overlay, _| {
        assert_eq!(
            overlay.overlay,
            Some(Overlay::CommandPalette),
            "clicking Back should return to commands, not dismiss the palette"
        );
    });
    overlay.update_in(cx, |overlay, window, cx| {
        overlay.query.insert("settings");
        overlay.query_changed(cx);
        overlay.push_page(Overlay::Settings, window, cx);
        overlay.push_page(Overlay::Themes, window, cx);
        overlay.move_highlight(1, cx);
    });
    cx.run_until_parked();
    for expected in [Overlay::Settings, Overlay::CommandPalette] {
        let back = cx.debug_bounds("palette-back").unwrap();
        cx.simulate_click(back.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        overlay.read_with(cx, |overlay, _| {
            assert_eq!(overlay.overlay, Some(expected));
            assert!(overlay.store.read().unwrap().preview_theme_id().is_none());
        });
    }
    overlay.update_in(cx, |overlay, window, _| {
        assert_eq!(overlay.query.text(), "settings");
        assert!(overlay.focus_handle.is_focused(window));
    });
    cx.simulate_click(point(px(5.0), px(600.0)), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(!overlay.read_with(cx, |overlay, _| overlay.is_open()));
}

#[gpui::test]
fn clicking_settings_opens_its_palette_page(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_reduce_motion(true));
    let (view, cx) = cx.add_window_view(|window, cx| {
        let previous_focus = cx.focus_handle();
        let overlay = cx.new(|cx| {
            let mut overlay =
                NavigationOverlay::opened_for_test(Arc::new(StoreRuntime::inert()), cx);
            overlay.query.insert("settings");
            overlay.query_changed(cx);
            overlay.focus_handle.focus(window, cx);
            overlay
        });
        Harness {
            overlay,
            previous_focus,
        }
    });
    cx.simulate_resize(size(px(800.0), px(700.0)));
    cx.run_until_parked();
    let settings = cx.debug_bounds("palette-row-0").expect("Settings result");
    cx.simulate_click(settings.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    let overlay = view.read_with(cx, |view, _| view.overlay.clone());
    overlay.read_with(cx, |overlay, _| {
        assert_eq!(overlay.overlay, Some(Overlay::Settings))
    });
}

#[gpui::test]
fn palette_landing_is_compact_and_notifications_are_searchable(cx: &mut TestAppContext) {
    let (overlay, cx) = cx.add_window_view(|_, cx| {
        let mut overlay = NavigationOverlay::opened_for_test(Arc::new(StoreRuntime::inert()), cx);
        overlay.refresh_command_items();
        overlay
    });
    overlay.update(cx, |overlay, cx| {
        assert_eq!(
            overlay.ranked_actions.len(),
            4,
            "only everyday actions on the landing page"
        );
        overlay.query.insert("notifications");
        overlay.query_changed(cx);
        assert!(
            overlay
                .ranked_actions
                .iter()
                .any(|row| row.item.command
                    == PaletteCommand::Action(CommandId::ToggleNotifications)),
            "the inbox must be reachable through command search"
        );
    });
}

struct ActionHarness {
    overlay: Entity<NavigationOverlay>,
    previous_focus: FocusHandle,
    dispatched: Arc<std::sync::Mutex<Vec<CommandId>>>,
}

impl Render for ActionHarness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            .key_context(crate::commands::APP_CONTEXT)
            .size_full()
            .track_focus(&self.previous_focus)
            .child(crate::root::cached_window_overlay(self.overlay.clone()));
        macro_rules! capture {
            ($($action:ident),+ $(,)?) => {$(
                let dispatched = self.dispatched.clone();
                root = root.on_action(move |_: &crate::commands::$action, _, _| {
                    dispatched.lock().unwrap().push(CommandId::$action);
                });
            )+};
        }
        capture!(
            NewDefaultSession,
            NewTerminal,
            ToggleOverview,
            ToggleTabPeek,
            ReviewLaunches,
            NewWindow,
            CloseWindow,
            FocusPaneLeft,
            FocusPaneRight,
            FocusPaneUp,
            FocusPaneDown,
            SplitPaneRight,
            SplitPaneBelow,
            TogglePaneZoom,
            RemoveFocusedPane,
            PaneGrowWidth,
            PaneShrinkWidth,
            PaneGrowHeight,
            PaneShrinkHeight,
            SwapPaneLeft,
            SwapPaneRight,
            SwapPaneUp,
            SwapPaneDown,
            MovePaneLeft,
            MovePaneRight,
            MovePaneUp,
            MovePaneDown,
            OpenWorktrees,
            NewNote,
            ShowTodos,
            ToggleSidebar,
            HorizontalTabs,
            VerticalTabs,
            OpenSettings,
            ToggleNotifications,
            CheckForUpdates
        );
        root
    }
}

#[gpui::test]
fn every_static_palette_action_dispatches_once_by_mouse_and_keyboard(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_reduce_motion(true));
    let dispatched = Arc::new(std::sync::Mutex::new(Vec::new()));
    let received = dispatched.clone();
    let (view, cx) = cx.add_window_view(move |window, cx| {
        let previous_focus = cx.focus_handle();
        previous_focus.focus(window, cx);
        let overlay =
            cx.new(|cx| NavigationOverlay::opened_for_test(Arc::new(StoreRuntime::inert()), cx));
        ActionHarness {
            overlay,
            previous_focus,
            dispatched,
        }
    });
    cx.simulate_resize(size(px(800.0), px(700.0)));
    let overlay = view.read_with(cx, |view, _| view.overlay.clone());
    let actions = {
        let mut all = palette::actions_for_catalogs(
            AgentKind::CLAUDE_CODE,
            &[],
            &[],
            None,
            None,
            &Default::default(),
        );
        all.retain(|row| matches!(row.command, PaletteCommand::Action(id) if !matches!(id, CommandId::ToggleHistory | CommandId::SearchNotes | CommandId::ToggleQuickOpen | CommandId::OpenSettings)));
        all
    };
    assert_eq!(
        actions.len(),
        35,
        "new static actions need a dispatch probe"
    );
    for action in actions {
        let PaletteCommand::Action(expected) = action.command else {
            unreachable!()
        };
        for mouse in [true, false] {
            overlay.update_in(cx, |overlay, window, cx| {
                overlay.clear_overlay(cx);
                overlay.open_overlay(Overlay::CommandPalette, window, cx);
                overlay.query.insert(&action.title);
                overlay.query_changed(cx);
                let index = overlay
                    .ranked_actions
                    .iter()
                    .position(|row| row.item.command == action.command)
                    .unwrap();
                overlay.highlight = overlay.ranked_sessions.len() + index;
                overlay.scroll_to_highlight();
            });
            cx.run_until_parked();
            if mouse {
                let index = overlay.read_with(cx, |overlay, _| overlay.highlight);
                assert_eq!(index, 0, "exact action title is the first search result");
                let position = cx.debug_bounds("palette-row-0").unwrap().center();
                cx.simulate_click(position, gpui::Modifiers::default());
            } else {
                cx.simulate_keystrokes("enter");
            }
            cx.run_until_parked();
            assert_eq!(std::mem::take(&mut *received.lock().unwrap()), [expected]);
            assert!(!overlay.read_with(cx, |overlay, _| overlay.is_open()));
        }
    }
}

#[test]
fn landing_membership_does_not_depend_on_shortcut_labels() {
    let mut actions = palette::actions_for_catalogs(
        AgentKind::CLAUDE_CODE,
        &[],
        &[],
        None,
        None,
        &Default::default(),
    );
    for row in &mut actions {
        row.shortcut = None;
    }
    assert_eq!(
        actions
            .iter()
            .filter(|row| landing_action_order(row).is_some())
            .count(),
        4
    );
    let default = actions.iter_mut().find(|row| row.is_default).unwrap();
    default.shortcut = Some("custom shortcut".into());
    assert_eq!(landing_action_order(default), Some(0));
}

#[gpui::test]
fn searchable_pages_open_by_mouse_and_keyboard(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_reduce_motion(true));
    let (view, cx) = cx.add_window_view(|window, cx| {
        let previous_focus = cx.focus_handle();
        let overlay = cx.new(|cx| {
            let mut overlay =
                NavigationOverlay::opened_for_test(Arc::new(StoreRuntime::inert()), cx);
            seed_history(&mut overlay);
            overlay.directory_index.finish_scan(
                Vec::new(),
                Instant::now(),
                String::new(),
                Vec::new(),
            );
            overlay.focus_handle.focus(window, cx);
            overlay
        });
        Harness {
            overlay,
            previous_focus,
        }
    });
    cx.simulate_resize(size(px(800.0), px(700.0)));
    let overlay = view.read_with(cx, |view, _| view.overlay.clone());
    for (query, expected) in [
        ("Open project", Overlay::QuickOpen),
        ("Search chats", Overlay::History),
        ("Settings", Overlay::Settings),
        ("Color theme", Overlay::Themes),
    ] {
        for mouse in [true, false] {
            overlay.update_in(cx, |overlay, window, cx| {
                overlay.open_overlay(Overlay::CommandPalette, window, cx);
                overlay.query.insert(query);
                overlay.query_changed(cx);
            });
            cx.run_until_parked();
            if mouse {
                let position = cx.debug_bounds("palette-row-0").unwrap().center();
                cx.simulate_click(position, gpui::Modifiers::default());
            } else {
                cx.simulate_keystrokes("enter");
            }
            cx.run_until_parked();
            overlay.read_with(cx, |overlay, _| {
                assert_eq!(overlay.overlay, Some(expected), "{query}, mouse={mouse}")
            });
            let back = cx.debug_bounds("palette-back").unwrap().center();
            cx.simulate_click(back, gpui::Modifiers::default());
            cx.run_until_parked();
            overlay.read_with(cx, |overlay, _| {
                assert_eq!(overlay.overlay, Some(Overlay::CommandPalette));
                assert_eq!(overlay.query.text(), query);
            });
        }
    }
}

#[gpui::test]
fn dynamic_palette_commands_preserve_their_targets(cx: &mut TestAppContext) {
    use crate::store::StoreEffect;
    let runtime = Arc::new(StoreRuntime::inert());
    let (mut store, mut effects) = SessionStore::headless(Default::default());
    let mut session =
        crate::sidebar::SidebarPreviewFixture::make(crate::sidebar::PreviewScenario::Typical)
            .list
            .sessions
            .remove(0);
    session.kind = AgentKind::CLAUDE_CODE;
    session.host = None;
    let selected = session.id.clone();
    store.upsert_session(session);
    store.select(selected.clone());
    store.set_hosts(vec![diri_proto::HostEntry {
        transport: Default::default(),
        id: "forge".into(),
        name: Some("Forge".into()),
        ssh: "forge".into(),
        default_cwd: Some("/srv/work".into()),
        node: None,
    }]);
    *runtime.store.write().unwrap() = store;
    while effects.try_recv().is_ok() {}
    let (overlay, cx) = cx.add_window_view(|_, cx| NavigationOverlay::opened_for_test(runtime, cx));
    for (cwd, host) in [
        (Some(PathBuf::from("/work/project")), None),
        (None, Some("forge".to_owned())),
    ] {
        overlay.update_in(cx, |overlay, window, cx| {
            overlay.open_overlay(Overlay::CommandPalette, window, cx);
            overlay.run_palette_command(
                PaletteCommand::SpawnAgent {
                    agent: AgentKind::CODEX,
                    cwd: cwd.clone(),
                    host: host.clone(),
                },
                window,
                cx,
            );
        });
        let StoreEffect::WorkspaceSpawn {
            params: Some(params),
            ..
        } = std::iter::from_fn(|| effects.try_recv().ok())
            .find(|effect| matches!(effect, StoreEffect::WorkspaceSpawn { .. }))
            .unwrap()
        else {
            panic!("spawn effect")
        };
        assert_eq!(params.kind, AgentKind::CODEX);
        assert_eq!(params.host, host);
        assert_eq!(
            params.cwd,
            cwd.as_ref()
                .map_or("/srv/work".into(), |cwd| cwd.to_string_lossy().into_owned())
        );
        assert_eq!(params.same_repo_as, host.map(|_| selected.clone()));
        assert!(!overlay.read_with(cx, |overlay, _| overlay.is_open()));
    }
    overlay.update_in(cx, |overlay, window, cx| {
        overlay.run_palette_command(
            PaletteCommand::MigrateSelected {
                target_host: Some("forge".into()),
            },
            window,
            cx,
        );
        overlay.run_palette_command(
            PaletteCommand::SyncPrefs {
                host: "forge".into(),
            },
            window,
            cx,
        );
    });
    assert_eq!(
        std::iter::from_fn(|| effects.try_recv().ok())
            .find(|effect| matches!(effect, StoreEffect::Migrate { .. }))
            .unwrap(),
        StoreEffect::Migrate {
            id: selected,
            target_host: Some("forge".into())
        }
    );
    assert_eq!(
        effects.try_recv().unwrap(),
        StoreEffect::SyncPrefs {
            host: "forge".into(),
            host_name: "Forge".into()
        }
    );
    assert!(effects.try_recv().is_err());
}

#[gpui::test]
fn project_open_keeps_its_context_until_an_agent_can_launch(cx: &mut TestAppContext) {
    use crate::store::StoreEffect;
    cx.update(|cx| cx.set_reduce_motion(true));
    let runtime = Arc::new(StoreRuntime::inert());
    let (store, mut effects) = SessionStore::headless(Default::default());
    *runtime.store.write().unwrap() = store;
    let (view, cx) = cx.add_window_view(|window, cx| {
        let previous_focus = cx.focus_handle();
        let overlay = cx.new(|cx| {
            let mut overlay = NavigationOverlay::opened_for_test(runtime, cx);
            overlay.overlay = Some(Overlay::QuickOpen);
            overlay.quick_snapshot.folders.push(QuickOpenItem {
                name: "project".into(),
                path: PathBuf::from("/work/project"),
                is_git_repo: false,
            });
            overlay.focus_handle.focus(window, cx);
            overlay
        });
        Harness {
            overlay,
            previous_focus,
        }
    });
    cx.simulate_resize(size(px(800.0), px(700.0)));
    cx.run_until_parked();
    let position = cx.debug_bounds("palette-row-0").unwrap().center();
    cx.simulate_click(position, gpui::Modifiers::default());
    cx.run_until_parked();
    let overlay = view.read_with(cx, |view, _| view.overlay.clone());
    assert_eq!(
        overlay.read_with(cx, |overlay, _| overlay.overlay),
        Some(Overlay::QuickOpen),
        "a declined launch must keep the project ready for retry"
    );
    assert!(matches!(
        effects.try_recv().unwrap(),
        StoreEffect::RefreshAgents { .. }
    ));
    assert!(effects.try_recv().is_err());
    overlay.update_in(cx, |overlay, window, _| {
        assert!(overlay.focus_handle.is_focused(window));
        assert!(
            overlay
                .page_error
                .as_deref()
                .is_some_and(|error| error.contains("Checking"))
        );
    });
    // Cmd+Enter remains an explicit Terminal escape hatch while readiness is pending.
    cx.simulate_keystrokes("cmd-enter");
    let StoreEffect::WorkspaceSpawn {
        params: Some(terminal),
        ..
    } = std::iter::from_fn(|| effects.try_recv().ok())
        .find(|effect| matches!(effect, StoreEffect::WorkspaceSpawn { .. }))
        .unwrap()
    else {
        panic!("terminal spawn")
    };
    assert_eq!(terminal.kind, AgentKind::SHELL);
    assert_eq!(terminal.cwd, "/work/project");
    overlay.update_in(cx, |overlay, window, cx| {
        overlay.overlay = Some(Overlay::QuickOpen);
        // The spawn closed the overlay, which released the index; a real
        // reopen reloads it from the disk cache.
        overlay.quick_snapshot.folders.push(QuickOpenItem {
            name: "project".into(),
            path: PathBuf::from("/work/project"),
            is_git_repo: false,
        });
        overlay.focus_handle.focus(window, cx);
        cx.notify();
    });
    overlay.update(cx, |overlay, _| {
        overlay
            .store
            .write()
            .unwrap()
            .set_agent_catalog(diri_proto::AgentReadinessResult::default());
    });
    // The refreshed empty catalog resolves to Terminal under the store's
    // existing policy. The original selected directory must survive retry.
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let spawned = std::iter::from_fn(|| effects.try_recv().ok())
        .find_map(|effect| match effect {
            StoreEffect::WorkspaceSpawn {
                params: Some(params),
                ..
            } => Some(params),
            _ => None,
        })
        .unwrap();
    assert_eq!(spawned.cwd, "/work/project");
    assert_eq!(spawned.kind, AgentKind::SHELL);
    assert!(!overlay.read_with(cx, |overlay, _| overlay.is_open()));
}

#[gpui::test]
fn project_picker_does_not_treat_remote_paths_as_local(cx: &mut TestAppContext) {
    let runtime = Arc::new(StoreRuntime::inert());
    let mut fixture =
        crate::sidebar::SidebarPreviewFixture::make(crate::sidebar::PreviewScenario::Typical).list;
    fixture.sessions.truncate(2);
    for (index, session) in fixture.sessions.iter_mut().enumerate() {
        session.cwd = format!("/work/project-{index}");
        session.project_id = diri_proto::ProjectId::new(format!("project-{index}"));
        session.host = (index == 1).then(|| "forge".into());
    }
    fixture.projects = fixture
        .sessions
        .iter()
        .map(|session| diri_proto::Project {
            id: session.project_id.clone(),
            root: session.cwd.clone(),
            name: session.cwd.clone(),
            pinned_order: None,
            host: session.host.clone(),
        })
        .collect();
    runtime.store.write().unwrap().hydrate(fixture);
    let (overlay, cx) = cx.add_window_view(|_, cx| NavigationOverlay::opened_for_test(runtime, cx));
    overlay.update(cx, |overlay, _| {
        let (projects, directories) = overlay.snapshot_inputs();
        assert_eq!(directories, [PathBuf::from("/work/project-0")]);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].0, PathBuf::from("/work/project-0"));
    });
}

/// Search notes opens every kind of note: a live note Session is selected
/// with the caret on the matching block, an archived one is restored, and a
/// file no Session holds is adopted through a note spawn carrying its id.
#[gpui::test]
fn search_notes_opens_live_archived_and_orphan_notes(cx: &mut TestAppContext) {
    use crate::notes::todos::TodosModel;
    use crate::notes::work_item_tests::record;
    use crate::store::StoreEffect;
    use std::cell::RefCell;
    use std::rc::Rc;

    let dir = tempfile::tempdir().unwrap();
    let notes = Arc::new(diri_notes::store::NoteStore::open(dir.path().join("notes")).unwrap());
    let create = |source: &str| {
        let (_, doc) = diri_notes::markdown::parse(source);
        notes.create(doc, Some("/work/launch")).unwrap().0
    };
    let live = create("# Launch plan\n\nIntro.\n\n- [ ] Book the venue\n");
    let archived = create("# Pricing study\n\nCompare the venue quotes.\n");
    let orphan = create("# Groceries\n\n- [ ] Oat milk\n");

    let runtime = Arc::new(StoreRuntime::inert());
    let (mut store, mut effects) = SessionStore::headless(Default::default());
    let mut live_session = record("s_live_note", AgentKind::NOTE);
    live_session.note_id = Some(live.clone());
    let mut archived_session = record("s_archived_note", AgentKind::NOTE);
    archived_session.note_id = Some(archived.clone());
    archived_session.archived_at = Some(DateMillis(1.0));
    store.upsert_session(live_session);
    store.upsert_session(archived_session);
    *runtime.store.write().unwrap() = store;
    while effects.try_recv().is_ok() {}

    let opened = Rc::new(RefCell::new(Vec::<NoteOpened>::new()));
    let (overlay, cx) = cx.add_window_view(|_, cx| {
        let model = cx.new(|cx| {
            TodosModel::with_store(Arc::clone(&runtime), Some(Arc::clone(&notes)), false, cx)
        });
        TodosModel::install(model, cx);
        let mut overlay = NavigationOverlay::opened_for_test(Arc::clone(&runtime), cx);
        overlay.overlay = None;
        overlay
    });
    let sink = Rc::clone(&opened);
    cx.update(|_, cx| {
        cx.subscribe(&overlay, move |_, event: &NoteOpened, _| {
            sink.borrow_mut().push(event.clone())
        })
        .detach()
    });

    let open = |cx: &mut gpui::VisualTestContext, query: &str, keep_caret: bool| {
        overlay.update_in(cx, |overlay, window, cx| {
            overlay.toggle_search_notes(&SearchNotes, window, cx);
        });
        cx.run_until_parked();
        overlay.update_in(cx, |overlay, window, cx| {
            assert_eq!(overlay.overlay, Some(Overlay::Notes));
            assert_eq!(overlay.notes.hits.len(), 3, "every note is listed");
            overlay.query.insert(query);
            overlay.query_changed(cx);
            overlay.run_highlighted(keep_caret, window, cx);
            assert!(!overlay.is_open(), "opening a note closes the palette");
        });
        cx.run_until_parked();
    };

    // Live, by a to-do in its body: selected, caret on that block.
    open(cx, "venue book", false);
    assert_eq!(
        runtime.store.read().unwrap().selected_session_id(),
        Some(&SessionId::new("s_live_note"))
    );
    assert_eq!(
        opened.borrow().last(),
        Some(&NoteOpened {
            note_id: live.clone(),
            block: Some(1),
        })
    );

    // Archived, opened with ⌘Return: restored, caret left where it was.
    open(cx, "pricing", true);
    {
        let store = runtime.store.read().unwrap();
        let session = &store.sessions()[&SessionId::new("s_archived_note")];
        assert!(!session.is_archived(), "opening restores an archived note");
    }
    assert!(
        std::iter::from_fn(|| effects.try_recv().ok()).any(|effect| matches!(
            effect,
            StoreEffect::Unarchive(id) | StoreEffect::Resume { id, .. }
                if id == SessionId::new("s_archived_note")
        ))
    );
    assert_eq!(
        opened.borrow().last(),
        Some(&NoteOpened {
            note_id: archived.clone(),
            block: None,
        })
    );

    // Orphan: adopted by a note spawn that names the file.
    open(cx, "groceries", false);
    let spawn = std::iter::from_fn(|| effects.try_recv().ok())
        .find_map(|effect| match effect {
            // The window's own launch path, or a plain spawn.
            StoreEffect::WorkspaceSpawn {
                params: Some(params),
                ..
            }
            | StoreEffect::Spawn(params) => Some(params),
            _ => None,
        })
        .expect("an orphan note is adopted by a spawn");
    assert_eq!(spawn.kind, AgentKind::NOTE);
    assert_eq!(spawn.note_id.as_deref(), Some(orphan.as_str()));
}

/// While typing in ⌘K, notes the query finds join the results above the
/// commands, and a live note whose Session already matched is not repeated.
#[gpui::test]
fn command_palette_mixes_in_matching_notes(cx: &mut TestAppContext) {
    use crate::notes::todos::TodosModel;

    let dir = tempfile::tempdir().unwrap();
    let notes = Arc::new(diri_notes::store::NoteStore::open(dir.path().join("notes")).unwrap());
    let (_, doc) = diri_notes::markdown::parse("# Quarterly roadmap\n\nShip notes search.\n");
    let id = notes.create(doc, None).unwrap().0;
    let runtime = Arc::new(StoreRuntime::inert());
    let (overlay, cx) = cx.add_window_view(|_, cx| {
        let model = cx.new(|cx| {
            TodosModel::with_store(Arc::clone(&runtime), Some(Arc::clone(&notes)), false, cx)
        });
        TodosModel::install(model, cx);
        let mut overlay = NavigationOverlay::opened_for_test(Arc::clone(&runtime), cx);
        overlay.overlay = None;
        overlay
    });
    overlay.update_in(cx, |overlay, window, cx| {
        overlay.open_overlay(Overlay::CommandPalette, window, cx);
    });
    cx.run_until_parked();
    overlay.update(cx, |overlay, cx| {
        overlay.query.insert("roadmap");
        overlay.query_changed(cx);
        let first = &overlay.ranked_actions[0].item;
        assert_eq!(first.title, "Quarterly roadmap");
        assert_eq!(
            first.command,
            PaletteCommand::OpenNote {
                note_id: id.clone(),
                block: None,
            }
        );
        overlay.query.clear();
        overlay.query_changed(cx);
        assert!(
            overlay
                .ranked_actions
                .iter()
                .all(|row| !matches!(row.item.command, PaletteCommand::OpenNote { .. })),
            "the landing page stays focused"
        );
    });
}

/// A notes folder for the Search notes screenshot: live notes, one archived
/// and one no Session holds, with to-dos linking agents and edits spread
/// over the last weeks. The folder outlives the test process on purpose.
#[cfg(target_os = "macos")]
pub(super) fn seed_notes(overlay: &mut NavigationOverlay, cx: &mut Context<NavigationOverlay>) {
    use crate::notes::todos::TodosModel;
    use diri_notes::store::NoteStore;

    let dir = tempfile::tempdir().unwrap().keep();
    let notes = Arc::new(NoteStore::open(dir.join("notes")).unwrap());
    let agents: Vec<SessionId> = {
        let mut store = overlay.store.write().unwrap();
        store
            .ordered_sessions()
            .into_iter()
            .filter(|session| !session.is_note())
            .map(|session| session.id.clone())
            .take(2)
            .collect()
    };
    let link = |index: usize, label: &str| {
        agents.get(index).map_or_else(String::new, |id| {
            format!(" [@{label}](diri://session/{})", id.0)
        })
    };
    let fixtures = [
        (
            "Q4 launch plan",
            format!(
                "Ship the onboarding email sequence before the pricing test.\n\n## Channels\n\n- [ ] Draft the launch email{}\n- [ ] Brief the pricing page{}\n- [x] Book the venue\n",
                link(0, "Claude Code · Launch copy"),
                link(1, "Codex · Pricing page"),
            ),
            "live",
            0.2,
        ),
        (
            "Weekly growth sync",
            "## Pricing\n\nWe will A/B test the pricing page against the annual plan.\n\n| Channel | CPA |\n|---|---|\n| Search | $14 |\n| Social | $22 |\n".to_owned(),
            "live",
            3.0,
        ),
        (
            "Customer interviews",
            "Five teams asked for a cheaper starter plan; two mentioned pricing confusion on the checkout page.\n\n- [ ] Share the notes with design\n".to_owned(),
            "live",
            26.0,
        ),
        (
            "Pricing study",
            "Compare annual pricing with the three closest competitors.\n".to_owned(),
            "archived",
            24.0 * 9.0,
        ),
        (
            "Brand refresh brief",
            "Moodboard, type and the new pricing illustrations.\n".to_owned(),
            "orphan",
            24.0 * 15.0,
        ),
        (
            "Hiring loop",
            "Interview plan for the product designer role.\n\n- [ ] Write the take-home\n".to_owned(),
            "live",
            48.0,
        ),
        (
            "Offsite agenda",
            "Day one: roadmap. Day two: pricing and packaging workshop.\n".to_owned(),
            "live",
            24.0 * 20.0,
        ),
    ];
    let now = std::time::SystemTime::now();
    for (index, (title, body, home, hours_ago)) in fixtures.into_iter().enumerate() {
        let (_, doc) = diri_notes::markdown::parse(&format!("# {title}\n\n{body}"));
        let (id, _) = notes.create(doc, Some("/Users/demo/fun/growth")).unwrap();
        let modified = now - Duration::from_secs_f64(hours_ago * 3600.0);
        std::fs::File::options()
            .write(true)
            .open(notes.dir().join(format!("{id}.md")))
            .unwrap()
            .set_modified(modified)
            .unwrap();
        if home == "orphan" {
            continue;
        }
        let mut record =
            crate::notes::work_item_tests::record(&format!("s_note_{index}"), AgentKind::NOTE);
        record.note_id = Some(id);
        record.title = title.into();
        if home == "archived" {
            record.archived_at = Some(diri_proto::DateMillis(1.0));
        }
        overlay.store.write().unwrap().upsert_session(record);
    }
    let runtime = Arc::clone(&overlay._runtime);
    let model = cx.new(|cx| TodosModel::with_store(runtime, Some(notes), false, cx));
    TodosModel::install(model, cx);
}
