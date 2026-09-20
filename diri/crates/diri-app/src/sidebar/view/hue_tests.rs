//! Where the project hue is drawn, and where it is left out.

use super::*;
use gpui::{TestAppContext, VisualTestContext};

struct Harness {
    sidebar: Entity<Sidebar>,
    strip: bool,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.strip {
            let strip = self.sidebar.update(cx, |sidebar, cx| {
                sidebar.render_horizontal_tabs(900.0, None, cx)
            });
            div().size_full().child(strip)
        } else {
            div().size_full().child(self.sidebar.clone())
        }
    }
}

fn harness(
    cx: &mut TestAppContext,
    scenario: PreviewScenario,
    grouping: SidebarGrouping,
    strip: bool,
) -> (Entity<Sidebar>, &mut VisualTestContext) {
    let (view, cx) = cx.add_window_view(move |_, cx| {
        let sidebar = cx.new(|cx| {
            let mut sidebar = Sidebar::new(None, true, scenario, cx);
            sidebar.set_sidebar_grouping(grouping);
            if strip {
                sidebar
                    .set_tab_orientation(crate::store::TabOrientation::Horizontal, cx)
                    .unwrap();
            }
            sidebar
        });
        cx.observe(&sidebar, |_, _, cx| cx.notify()).detach();
        Harness { sidebar, strip }
    });
    (view.read_with(cx, |view, _| view.sidebar.clone()), cx)
}

#[gpui::test]
fn recency_rows_wear_a_tick_that_moves_nothing(cx: &mut TestAppContext) {
    let (_, cx) = harness(
        cx,
        PreviewScenario::Projects,
        SidebarGrouping::Recency,
        false,
    );
    let row = cx.debug_bounds("SESSION_hue-api-1").unwrap();
    let tick = cx.debug_bounds("PROJECT_HUE_hue-api-1").unwrap();
    assert_eq!(tick.size.width, px(3.0));
    assert!(tick.left() > row.left() && tick.right() < row.left() + px(8.0));
    assert_eq!(tick.center().y, row.center().y);
}

#[gpui::test]
fn rows_under_a_project_header_carry_no_tick(cx: &mut TestAppContext) {
    let (_, cx) = harness(
        cx,
        PreviewScenario::Projects,
        SidebarGrouping::Project,
        false,
    );
    assert!(cx.debug_bounds("SESSION_hue-api-1").is_some());
    assert!(cx.debug_bounds("PROJECT_HUE_hue-api-1").is_none());
}

#[gpui::test]
fn a_single_project_is_never_marked(cx: &mut TestAppContext) {
    let (sidebar, cx) = harness(cx, PreviewScenario::Fleet, SidebarGrouping::Recency, false);
    assert!(cx.debug_bounds("SESSION_preview-fleet-0").is_some());
    assert!(cx.debug_bounds("PROJECT_HUE_preview-fleet-0").is_none());
    sidebar.read_with(cx, |sidebar, _| {
        assert_eq!(sidebar.hues, crate::project_hue::ProjectHues::default());
    });
}

#[gpui::test]
fn a_filter_does_not_recolor_the_projects_it_leaves(cx: &mut TestAppContext) {
    let (sidebar, cx) = harness(
        cx,
        PreviewScenario::Projects,
        SidebarGrouping::Recency,
        false,
    );
    let before = sidebar.read_with(cx, |sidebar, _| sidebar.hues.clone());
    sidebar.update(cx, |sidebar, cx| {
        sidebar.filter_open = true;
        sidebar.filter_query.insert("invoices");
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("PROJECT_HUE_hue-web-1").is_none());
    assert!(cx.debug_bounds("PROJECT_HUE_hue-api-1").is_some());
    assert_eq!(
        sidebar.read_with(cx, |sidebar, _| sidebar.hues.clone()),
        before
    );
}

#[gpui::test]
fn the_strip_draws_nothing_extra(cx: &mut TestAppContext) {
    // One project per strip: its glyph takes the hue and no tab is marked.
    let (_, cx) = harness(
        cx,
        PreviewScenario::Projects,
        SidebarGrouping::Project,
        true,
    );
    assert!(cx.debug_bounds("horizontal-tab-hue-api-2").is_some());
    assert!(cx.debug_bounds("PROJECT_HUE_hue-api-2").is_none());
}
