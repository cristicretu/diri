use super::*;
use diri_proto::{DateMillis, HistoryEntry};
use gpui::{Entity, TestAppContext};

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
