//! Row motion against the real palette: what Return runs while rows are still
//! in flight, when frames are asked for, and when nothing moves at all.

use std::time::Duration;

use diri_proto::{
    AgentDescriptor, AgentPathSource, AgentReadinessItem, AgentReadinessResult, HostEntry,
};
use gpui::{Entity, TestAppContext, VisualTestContext, point, size};

use super::*;
use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};

fn agent(id: &str, name: &str) -> AgentReadinessItem {
    AgentReadinessItem {
        kind: AgentKind::new(id),
        binary: id.into(),
        path: Some(format!("/usr/bin/{id}")),
        detected_path: Some(format!("/usr/bin/{id}")),
        path_source: Some(AgentPathSource::SystemPath),
        show_in_quick_create: true,
        descriptor: Some(AgentDescriptor {
            id: id.into(),
            display_name: name.into(),
            first_class: true,
            ..AgentDescriptor::default()
        }),
        ..AgentReadinessItem::default()
    }
}

/// A palette as full as a working one: thirteen sessions across three
/// projects, a remote host, and four agents installed on both targets, which
/// comes to sixty-odd rows.
pub(super) fn working_runtime() -> Arc<StoreRuntime> {
    let runtime = Arc::new(StoreRuntime::inert());
    let fixture = SidebarPreviewFixture::make(PreviewScenario::Stress);
    {
        let mut store = runtime.store.write().expect("session store lock poisoned");
        store.hydrate(fixture.list);
        if let Some(selected) = fixture.selected_session_id {
            store.select(selected);
        }
        store.set_hosts(vec![HostEntry {
            id: "forge".into(),
            name: Some("Forge".into()),
            ssh: "cristi@forge".into(),
            default_cwd: None,
            node: None,
        }]);
        for host in [None, Some("forge".to_owned())] {
            store.set_agent_catalog(AgentReadinessResult {
                host,
                agents: vec![
                    agent("claude-code", "Claude Code"),
                    agent("codex", "Codex"),
                    agent("cursor", "Cursor"),
                    agent("gemini", "Gemini"),
                ],
                ..AgentReadinessResult::default()
            });
        }
    }
    runtime
}

struct Harness {
    overlay: Entity<NavigationOverlay>,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(crate::root::cached_window_overlay(self.overlay.clone()))
    }
}

/// An open palette on a pinned clock, focused so keystrokes reach it.
fn open_palette(
    runtime: Arc<StoreRuntime>,
    cx: &mut TestAppContext,
) -> (Entity<NavigationOverlay>, Instant, &mut VisualTestContext) {
    let start = Instant::now();
    let (view, cx) = cx.add_window_view(move |_, cx| Harness {
        overlay: cx.new(|cx| {
            let mut overlay = NavigationOverlay::opened_for_test(runtime, cx);
            overlay.clear_overlay(cx);
            overlay.motion_clock = Some(start);
            overlay
        }),
    });
    cx.simulate_resize(size(px(900.0), px(700.0)));
    let overlay = view.read_with(cx, |view, _| view.overlay.clone());
    overlay.update_in(cx, |overlay, window, cx| {
        overlay.open_overlay(Overlay::CommandPalette, window, cx);
    });
    cx.run_until_parked();
    (overlay, start, cx)
}

fn frames_requested(overlay: &Entity<NavigationOverlay>, cx: &mut VisualTestContext) -> usize {
    let requested = overlay.update_in(cx, |_, window, cx| window.simulate_next_frame(cx));
    cx.run_until_parked();
    requested
}

/// The palette's own chrome (its entry fade, the scroller's flash) animates on
/// the wall clock, which a test cannot pin. Pump frames until only `expected`
/// requests remain, so what is left is the row motion's alone.
fn frames_once_chrome_settles(
    overlay: &Entity<NavigationOverlay>,
    cx: &mut VisualTestContext,
    expected: usize,
) -> usize {
    let mut requested = frames_requested(overlay, cx);
    for _ in 0..60 {
        if requested == expected {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
        requested = frames_requested(overlay, cx);
    }
    requested
}

fn titles(overlay: &NavigationOverlay) -> Vec<String> {
    overlay
        .ranked_sessions
        .iter()
        .map(|row| row.item.title.clone())
        .chain(
            overlay
                .ranked_actions
                .iter()
                .map(|row| row.item.title.clone()),
        )
        .collect()
}

#[gpui::test]
fn return_runs_the_final_first_row_while_rows_are_still_sliding(cx: &mut TestAppContext) {
    let runtime = working_runtime();
    let (overlay, start, cx) = open_palette(Arc::clone(&runtime), cx);

    // The last letter drops two chats from under the first row, and the pane
    // commands below them close the gap.
    cx.simulate_keystrokes("f o c u s");
    let first = overlay.read_with(cx, |overlay, _| {
        assert!(
            overlay.row_motion.is_moving(start),
            "the clock has not moved"
        );
        let keys = overlay.leading_row_keys();
        assert_eq!(overlay.row_motion.offset(keys[0], start), 0.0);
        assert!(overlay.row_motion.offset(keys[1], start) > 0.0);
        assert_eq!(overlay.highlight, 0);
        assert_eq!(titles(overlay)[0], "Fix project switching focus");
        overlay.ranked_sessions[0].item.id.clone()
    });

    cx.simulate_keystrokes("enter");

    assert!(!overlay.read_with(cx, |overlay, _| overlay.is_open()));
    // Selection belongs to the window, not to the shared store.
    let selected = overlay.read_with(cx, |overlay, _| {
        overlay.store.read().unwrap().selected_session_id().cloned()
    });
    assert_eq!(selected, Some(first));
}

#[gpui::test]
fn rows_in_flight_ask_for_frames_and_a_settled_list_asks_for_none(cx: &mut TestAppContext) {
    let (overlay, start, cx) = open_palette(working_runtime(), cx);
    assert_eq!(frames_once_chrome_settles(&overlay, cx, 0), 0);

    // More rows match than fit, so freed slots are backfilled: a plain cut.
    // Twenty-seven matches down to ten: still more than fit, so the slots
    // rows leave are refilled from below the fold and each key is a plain cut.
    cx.simulate_keystrokes("f o c u");
    assert!(!overlay.read_with(cx, |overlay, _| overlay.row_motion.is_moving(start)));
    assert_eq!(frames_once_chrome_settles(&overlay, cx, 0), 0);

    // Seven matches: the list closes up by two slots under its first row.
    cx.simulate_keystrokes("s");
    let sliding = overlay.read_with(cx, |overlay, _| overlay.leading_row_keys()[1]);
    let from = overlay.read_with(cx, |overlay, _| overlay.row_motion.offset(sliding, start));
    assert_eq!(from, 2.0 * ROW_HEIGHT);
    // The clock is pinned mid-slide, so the one request left never goes away.
    assert_eq!(frames_once_chrome_settles(&overlay, cx, 1), 1);
    for _ in 0..3 {
        assert_eq!(frames_requested(&overlay, cx), 1);
    }

    let midway = start + Duration::from_millis(70);
    overlay.update(cx, |overlay, _| overlay.motion_clock = Some(midway));
    assert_eq!(frames_requested(&overlay, cx), 1);
    let offset = overlay.read_with(cx, |overlay, _| overlay.row_motion.offset(sliding, midway));
    assert!(offset > 0.0 && offset < from);

    // The frame after the rows land paints them at rest and schedules nothing.
    let landed = start + Duration::from_millis(140);
    overlay.update(cx, |overlay, _| overlay.motion_clock = Some(landed));
    assert_eq!(frames_requested(&overlay, cx), 1);
    assert_eq!(frames_requested(&overlay, cx), 0);
    assert_eq!(frames_requested(&overlay, cx), 0);
}

#[gpui::test]
fn reduce_motion_paints_every_row_in_place(cx: &mut TestAppContext) {
    let (overlay, start, cx) = open_palette(working_runtime(), cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));

    cx.simulate_keystrokes("f o c u s");

    overlay.read_with(cx, |overlay, _| {
        assert!(!overlay.row_motion.is_moving(start));
        for key in overlay.leading_row_keys() {
            assert_eq!(overlay.row_motion.offset(key, start), 0.0);
        }
    });
    assert_eq!(frames_once_chrome_settles(&overlay, cx, 0), 0);
}

#[gpui::test]
fn a_scrolled_list_and_a_page_change_both_snap(cx: &mut TestAppContext) {
    let (overlay, start, cx) = open_palette(working_runtime(), cx);

    // Scrolled: the keystroke returns the list to its top, so slots say
    // nothing about where rows were painted.
    cx.simulate_keystrokes("f o c u");
    overlay.update(cx, |overlay, _| {
        overlay
            .list_scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.0), px(-2.0 * ROW_HEIGHT)));
    });
    cx.simulate_keystrokes("s");
    assert!(!overlay.read_with(cx, |overlay, _| overlay.row_motion.is_moving(start)));

    cx.simulate_keystrokes("backspace s");
    assert!(overlay.read_with(cx, |overlay, _| overlay.row_motion.is_moving(start)));
    overlay.update_in(cx, |overlay, window, cx| {
        overlay.push_page(Overlay::Themes, window, cx);
    });
    assert!(!overlay.read_with(cx, |overlay, _| overlay.row_motion.is_moving(start)));
}
