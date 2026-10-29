//! A split tab's mark: its layout at glyph size, each pane's tile tinted
//! by what that pane is doing.
//!
//! A split tab is one sidebar row and one strip tab, and that row shows
//! its most urgent pane. The mark says the rest at a glance: the tab's
//! shape (two columns, a stack beside a pane, a grid), which corner is
//! asking, which panes are working, which finished while you looked away.
//! It replaced a count ("▯▯ 3") that said only how many panes there were.
//!
//! Built from records the store already holds while the row or tab
//! renders its props. Nothing is kept per session; the mark has no
//! animation and no timer, and a resting tab's props compare equal, so
//! its cached view is reused.

use super::hover_status::{Tile, layout_tiles, state_ink};
use super::*;

/// The sidebar row's and the strip tab's glyph: the workbench is wider
/// than it is tall, and so is this.
pub(super) const WIDTH: f32 = 16.0;
pub(super) const HEIGHT: f32 = 11.0;
/// Space between neighbouring tiles, the glyph's version of a divider.
const GAP: f32 = 1.5;
const RADIUS: f32 = 1.5;
/// A resting pane's tile, as quiet as the hover card's unlit tiles.
const REST_ALPHA: f32 = 0.16;

fn rests(state: StatusState) -> bool {
    matches!(
        state,
        StatusState::IdleSeen | StatusState::None | StatusState::Hibernated
    )
}

/// One split tab's mark.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::sidebar) struct SplitMark {
    /// The panes in reading order, each with its tile's colour.
    tiles: Vec<(Tile, Rgba)>,
    /// What a screen reader hears: the pane count, then the states that
    /// ask for a look.
    label: String,
}

impl SplitMark {
    /// The mark for the split tab that shows `session`, or `None` when
    /// that tab has a single pane.
    pub(super) fn of(
        store: &crate::store::SessionStore,
        session: &SessionId,
        colors: SemanticColors,
    ) -> Option<Self> {
        let layout = store.split_layout_of(session)?;
        Self::from_panes(
            layout_tiles(layout).into_iter().map(|(id, tile)| {
                let pane = store.sessions().get(&id).cloned();
                let state = pane.as_deref().map_or(StatusState::None, |pane| {
                    sidebar_activity_state(
                        status_state(pane, store.migrating().contains(&id)),
                        store.notifications().session_unread(&id),
                    )
                });
                (tile, pane, state)
            }),
            colors,
        )
    }

    /// `panes` in reading order: each tile, its record (missing while a
    /// pane's session is still arriving) and its state.
    pub(super) fn from_panes(
        panes: impl IntoIterator<Item = (Tile, Option<Arc<SessionRecord>>, StatusState)>,
        colors: SemanticColors,
    ) -> Option<Self> {
        let mut states = Vec::new();
        let tiles: Vec<(Tile, Rgba)> = panes
            .into_iter()
            .map(|(tile, pane, state)| {
                states.push(state);
                // At rest a tile only holds the pane's place; anything it
                // reports stands out against that.
                let ink = match pane {
                    // A job's own progress report counts as news.
                    Some(pane)
                        if !rests(state) || crate::progress_mark::shown(&pane).is_some() =>
                    {
                        state_ink(&pane, state, colors)
                    }
                    _ => colors.primary.alpha(REST_ALPHA),
                };
                (tile, ink)
            })
            .collect();
        if tiles.len() < 2 {
            return None;
        }
        Some(Self {
            label: label(&states),
            tiles,
        })
    }

    #[cfg(test)]
    pub(super) fn inks(&self) -> Vec<Rgba> {
        self.tiles.iter().map(|(_, ink)| *ink).collect()
    }

    /// The glyph, `debug` naming it for tests.
    pub(super) fn render(&self, debug: String) -> AnyElement {
        draw(
            self.tiles.iter().copied(),
            WIDTH,
            HEIGHT,
            GAP,
            RADIUS,
        )
        .id(SharedString::from(debug.clone()))
        .debug_selector(move || debug.clone())
        .role(Role::Image)
        .aria_label(self.label.clone())
        .into_any_element()
    }
}

/// "3 panes: Needs input, Working": the count, then each state that asks
/// for a look, most urgent first, once each.
fn label(states: &[StatusState]) -> String {
    let mut label = tf("session.pane_count", &[("count", &states.len())]);
    let mut asking: Vec<StatusState> = Vec::new();
    for state in states {
        if super::activity::has_activity(*state) && !asking.contains(state) {
            asking.push(*state);
        }
    }
    asking.sort_by_key(|state| match state {
        StatusState::NeedsInput { destructive: true } => 0,
        StatusState::NeedsInput { .. } => 1,
        StatusState::Working => 2,
        _ => 3,
    });
    if !asking.is_empty() {
        label.push_str(": ");
        label.push_str(
            &asking
                .iter()
                .map(|state| state.label())
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    label
}

/// Tiles at `width` × `height`, `gap` apart. Only the edges a tile shares
/// with a neighbour are inset, so the glyph's outline stays one clean
/// rectangle.
pub(super) fn draw(
    tiles: impl IntoIterator<Item = (Tile, Rgba)>,
    width: f32,
    height: f32,
    gap: f32,
    radius: f32,
) -> gpui::Div {
    let half = gap / 2.0;
    let mut glyph = div().flex_none().relative().w(px(width)).h(px(height));
    for (tile, ink) in tiles {
        let left = if tile.x > 0.001 { half } else { 0.0 };
        let top = if tile.y > 0.001 { half } else { 0.0 };
        let right = if tile.x + tile.width < 0.999 { half } else { 0.0 };
        let bottom = if tile.y + tile.height < 0.999 { half } else { 0.0 };
        glyph = glyph.child(
            div()
                .absolute()
                .left(px(tile.x * width + left))
                .top(px(tile.y * height + top))
                .w(px((tile.width * width - left - right).max(1.0)))
                .h(px((tile.height * height - top - bottom).max(1.0)))
                .rounded(px(radius))
                .bg(ink),
        );
    }
    glyph
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::workspace::{LayoutAxis, LayoutNode, PaneId, SplitId};

    fn tile(x: f32, width: f32) -> Tile {
        Tile {
            x,
            y: 0.0,
            width,
            height: 1.0,
        }
    }

    fn record(id: &str) -> Arc<SessionRecord> {
        let fixture = crate::SidebarPreviewFixture::make(crate::PreviewScenario::Typical);
        Arc::new(
            fixture
                .list
                .sessions
                .into_iter()
                .find(|session| session.id.0 == id)
                .unwrap(),
        )
    }

    #[test]
    fn a_single_pane_has_no_mark() {
        let colors = SemanticColors::dark();
        assert!(SplitMark::from_panes([(tile(0.0, 1.0), None, StatusState::None)], colors).is_none());
    }

    #[test]
    fn each_tile_wears_its_own_panes_state() {
        let colors = SemanticColors::dark();
        let agent = record("preview-claude");
        let shell = record("preview-shell");
        let asking = StatusState::NeedsInput { destructive: false };
        let mark = SplitMark::from_panes(
            [
                (tile(0.0, 0.5), Some(agent.clone()), StatusState::IdleSeen),
                (tile(0.5, 0.5), Some(shell.clone()), asking),
            ],
            colors,
        )
        .unwrap();
        assert_eq!(
            mark.inks(),
            [
                colors.primary.alpha(REST_ALPHA),
                state_ink(&shell, asking, colors),
            ]
        );
        assert_ne!(mark.inks()[0], mark.inks()[1], "the asking pane stands out");
    }

    #[test]
    fn the_label_counts_panes_and_names_what_asks_most_urgent_first() {
        let working = StatusState::Working;
        let asking = StatusState::NeedsInput { destructive: false };
        assert_eq!(
            label(&[working, StatusState::IdleSeen, asking, working]),
            format!(
                "{}: {}, {}",
                tf("session.pane_count", &[("count", &4)]),
                asking.label(),
                working.label()
            )
        );
        assert_eq!(
            label(&[StatusState::IdleSeen, StatusState::None]),
            tf("session.pane_count", &[("count", &2)])
        );
    }

    #[test]
    fn a_tab_split_three_ways_marks_each_pane_where_it_sits() {
        let leaf = |id: &str| LayoutNode::Pane {
            id: PaneId::new(id),
            session_id: SessionId::new(id),
        };
        let layout = LayoutNode::Split {
            id: SplitId::new("columns"),
            axis: LayoutAxis::Horizontal,
            fraction: 0.6,
            first: Box::new(leaf("agent")),
            second: Box::new(LayoutNode::Split {
                id: SplitId::new("stack"),
                axis: LayoutAxis::Vertical,
                fraction: 0.5,
                first: Box::new(leaf("tests")),
                second: Box::new(leaf("deploy")),
            }),
        };
        let mark = SplitMark::from_panes(
            layout_tiles(&layout)
                .into_iter()
                .map(|(_, tile)| (tile, None, StatusState::IdleSeen)),
            SemanticColors::dark(),
        )
        .unwrap();
        let tiles: Vec<Tile> = mark.tiles.iter().map(|(tile, _)| *tile).collect();
        assert_eq!(tiles.len(), 3);
        assert!((tiles[0].width - 0.6).abs() < 1e-6, "the agent keeps its share");
        assert!(tiles[1].y < tiles[2].y, "the stack reads top to bottom");
    }
}
