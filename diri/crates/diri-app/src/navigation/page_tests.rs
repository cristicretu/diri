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
            .finish_scan(Vec::new(), Instant::now());
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
            overlay
                .directory_index
                .finish_scan(Vec::new(), Instant::now());
            overlay.focus_handle.focus(window, cx);
            overlay
        });
        Harness {
            overlay,
            previous_focus,
        }
    });
    let overlay = view.read_with(cx, |view, _| view.overlay.clone());
    cx.simulate_keystrokes("cmd-p");
    assert_eq!(
        overlay.read_with(cx, |overlay, _| overlay.overlay),
        Some(Overlay::QuickOpen)
    );
    cx.simulate_keystrokes("cmd-k");
    assert_eq!(
        overlay.read_with(cx, |overlay, _| overlay.overlay),
        Some(Overlay::CommandPalette)
    );
    cx.simulate_keystrokes("cmd-shift-h");
    assert_eq!(
        overlay.read_with(cx, |overlay, _| overlay.overlay),
        Some(Overlay::History)
    );
    cx.simulate_keystrokes("cmd-shift-h");
    assert!(!overlay.read_with(cx, |overlay, _| overlay.is_open()));
}

#[gpui::test]
fn pending_project_search_cannot_change_a_new_page(cx: &mut TestAppContext) {
    let (overlay, cx) = cx.add_window_view(|_, cx| {
        let mut overlay = NavigationOverlay::opened_for_test(Arc::new(StoreRuntime::inert()), cx);
        seed_history(&mut overlay);
        overlay
            .directory_index
            .finish_scan(Vec::new(), Instant::now());
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
            OpenWorktrees,
            ToggleSidebar,
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
        all.retain(|row| matches!(row.command, PaletteCommand::Action(id) if !matches!(id, CommandId::ToggleHistory | CommandId::ToggleQuickOpen | CommandId::OpenSettings)));
        all
    };
    assert_eq!(actions.len(), 7, "new static actions need a dispatch probe");
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
            overlay
                .directory_index
                .finish_scan(Vec::new(), Instant::now());
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
        let StoreEffect::Spawn(params) = effects.try_recv().unwrap() else {
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
        effects.try_recv().unwrap(),
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
    let StoreEffect::Spawn(terminal) = effects.try_recv().unwrap() else {
        panic!("terminal spawn")
    };
    assert_eq!(terminal.kind, AgentKind::SHELL);
    assert_eq!(terminal.cwd, "/work/project");
    overlay.update_in(cx, |overlay, window, cx| {
        overlay.overlay = Some(Overlay::QuickOpen);
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
            StoreEffect::Spawn(params) => Some(params),
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
