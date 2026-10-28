//! What the hover card says a session is doing.
//!
//! A row shows one mark for its whole tab: a split tab folds its panes into
//! the most urgent one. The card is where you look before switching, so it
//! says what that mark means: the question an Agent is asking, how far a
//! job has got, or how long ago a turn finished. In a split tab it gives
//! one line per pane, led by a glyph of the tab's layout with that pane
//! filled in its state's colour, so you can see which pane is asking.
//!
//! Everything here is built while the card is on screen, from records
//! the store already holds. Nothing is kept per session, and nothing
//! animates or wakes.

use std::sync::Arc;

use diri_proto::workspace::{LayoutAxis, LayoutNode, MAX_LAYOUT_DEPTH};
use diri_proto::{
    AgentKind, SessionId, SessionRecord, SessionStatus, TerminalProgressState as ProgressState,
};
use diri_ui::{Ink, SemanticColors, StatusState, Typo};
use gpui::{AnyElement, Rgba, div, prelude::*, px};

use crate::i18n::{t, tf};
use crate::session_presentation::{status_state, ui_agent_kind};
use crate::switcher::display_title;

/// Pane lines past this fold into "N more": the card is a glance.
pub(super) const MAX_PANE_LINES: usize = 6;
/// The layout glyph. The workbench is wider than it is tall, and so is this.
const GLYPH_WIDTH: f32 = 20.0;
const GLYPH_HEIGHT: f32 = 14.0;
/// Space between neighbouring tiles, the glyph's version of a divider.
const GLYPH_GAP: f32 = 1.5;

/// A tile of the layout glyph, as fractions of the whole tab.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Tile {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// The tab's panes in reading order (first child before second), each as
/// a fraction of the tab. A pane's place, not its size limits: unlike
/// `WorkspaceGeometry` this never clamps a pane to its minimum, so an
/// uneven split keeps its proportions at glyph size.
pub(super) fn layout_tiles(layout: &LayoutNode) -> Vec<(SessionId, Tile)> {
    fn walk(node: &LayoutNode, tile: Tile, depth: usize, out: &mut Vec<(SessionId, Tile)>) {
        if depth > MAX_LAYOUT_DEPTH {
            return;
        }
        match node {
            LayoutNode::Pane { session_id, .. } => out.push((session_id.clone(), tile)),
            LayoutNode::Split {
                axis,
                fraction,
                first,
                second,
                ..
            } => {
                let fraction = if fraction.is_finite() {
                    fraction.clamp(0.1, 0.9)
                } else {
                    0.5
                };
                let (a, b) = match axis {
                    LayoutAxis::Horizontal => (
                        Tile {
                            width: tile.width * fraction,
                            ..tile
                        },
                        Tile {
                            x: tile.x + tile.width * fraction,
                            width: tile.width * (1.0 - fraction),
                            ..tile
                        },
                    ),
                    LayoutAxis::Vertical => (
                        Tile {
                            height: tile.height * fraction,
                            ..tile
                        },
                        Tile {
                            y: tile.y + tile.height * fraction,
                            height: tile.height * (1.0 - fraction),
                            ..tile
                        },
                    ),
                };
                walk(first, a, depth + 1, out);
                walk(second, b, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(
        layout,
        Tile {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        },
        0,
        &mut out,
    );
    out
}

/// What a session is doing, in a few words, or `None` when there is
/// nothing worth saying (a shell at its prompt).
pub(super) fn state_text(session: &SessionRecord, state: StatusState, now_ms: f64) -> Option<String> {
    let agent = session.effective_kind() != &AgentKind::SHELL;
    // A job's own progress report (`OSC 9;4`) is the most specific thing
    // a running terminal says, and a shell has no other "working" signal.
    if !matches!(state, StatusState::NeedsInput { .. })
        && let Some(progress) = crate::progress_mark::shown(session)
        && progress.percent > 0
    {
        let percent: &dyn std::fmt::Display = &progress.percent;
        return Some(match progress.state {
            ProgressState::Error => tf("session.hover.progress_error", &[("percent", percent)]),
            ProgressState::Paused => tf("session.hover.progress_paused", &[("percent", percent)]),
            _ => tf("session.hover.working_percent", &[("percent", percent)]),
        });
    }
    match state {
        StatusState::NeedsInput { .. } => Some(
            match session
                .needs_input
                .as_ref()
                .map(|detail| detail.summary.trim())
                .filter(|summary| !summary.is_empty())
            {
                Some(summary) => tf("session.hover.needs_you_detail", &[("detail", &summary)]),
                None => t("session.hover.needs_you").to_owned(),
            },
        ),
        StatusState::Working => Some(t("session.hover.working").to_owned()),
        StatusState::DoneUnseen | StatusState::IdleSeen => {
            match session.last_turn_completed_at {
                Some(done) if agent => Some(done_ago(now_ms, done.0)),
                _ if agent => Some(t("session.hover.idle").to_owned()),
                _ => None,
            }
        }
        StatusState::Hibernated => Some(t("session.sleeping").to_owned()),
        StatusState::None => {
            matches!(session.status, SessionStatus::Exited(_))
                .then(|| t("session.hover.ended").to_owned())
        }
    }
}

fn done_ago(now_ms: f64, done_ms: f64) -> String {
    let minutes = ((now_ms - done_ms).max(0.0) / 60_000.0) as u64;
    match minutes {
        0 => t("session.hover.done_now").to_owned(),
        1..=59 => tf("session.hover.done_minutes", &[("count", &minutes)]),
        60..=1_439 => tf("session.hover.done_hours", &[("count", &(minutes / 60))]),
        _ => tf("session.hover.done_days", &[("count", &(minutes / 1_440))]),
    }
}

/// The colour a pane's tile wears in its glyph: the row's mark colours
/// for states that ask for a look, the ink at rest otherwise.
pub(super) fn state_ink(session: &SessionRecord, state: StatusState, colors: SemanticColors) -> Rgba {
    if state == StatusState::None
        && let Some(progress) = crate::progress_mark::shown(session)
    {
        return match progress.state {
            ProgressState::Error => Ink::on_surface(Ink::DANGER, colors),
            ProgressState::Paused => Ink::on_surface(Ink::ATTENTION, colors),
            _ => colors.primary.alpha(0.55),
        };
    }
    match state {
        StatusState::Working => Ink::working(ui_agent_kind(session.effective_kind()), colors),
        StatusState::NeedsInput { destructive: true } => Ink::on_surface(Ink::DANGER, colors),
        StatusState::NeedsInput { destructive: false } => Ink::on_surface(Ink::ATTENTION, colors),
        StatusState::DoneUnseen => Ink::on_surface(Ink::FRESH, colors),
        // Filled, so the pane's place still reads, but quieter than work.
        StatusState::IdleSeen | StatusState::None | StatusState::Hibernated => {
            colors.primary.alpha(0.34)
        }
    }
}

/// One pane of a split tab, as the card lists it.
pub(super) struct PaneLine {
    pub session: Arc<SessionRecord>,
    pub state: StatusState,
    /// Index into the tab's tiles.
    pub tile: usize,
    /// The hovered session itself, whose title already heads the card.
    pub owner: bool,
}

/// The card's status section for one hovered row.
pub(super) enum HoverStatus {
    /// A single pane: one line under the title.
    Single {
        state: StatusState,
        text: String,
    },
    /// A split tab: a line per pane.
    Panes {
        tiles: Vec<Tile>,
        lines: Vec<PaneLine>,
        hidden: usize,
    },
}

impl HoverStatus {
    /// `tab` is the hovered session's tab layout, when it is split.
    pub(super) fn of(
        session: &SessionRecord,
        tab: Option<&LayoutNode>,
        lookup: impl Fn(&SessionId) -> Option<Arc<SessionRecord>>,
        migrating: bool,
        now_ms: f64,
    ) -> Option<Self> {
        if let Some(layout) = tab.filter(|layout| matches!(layout, LayoutNode::Split { .. })) {
            let placed = layout_tiles(layout);
            let mut lines: Vec<PaneLine> = placed
                .iter()
                .enumerate()
                .filter_map(|(tile, (id, _))| {
                    let pane = lookup(id)?;
                    let state = status_state(&pane, migrating && pane.id == session.id);
                    Some(PaneLine {
                        owner: pane.id == session.id,
                        session: pane,
                        state,
                        tile,
                    })
                })
                .collect();
            if lines.len() >= 2 {
                let hidden = lines.len().saturating_sub(MAX_PANE_LINES);
                lines.truncate(MAX_PANE_LINES);
                return Some(Self::Panes {
                    tiles: placed.into_iter().map(|(_, tile)| tile).collect(),
                    lines,
                    hidden,
                });
            }
        }
        let state = status_state(session, migrating);
        // Sleeping already has its own line among the details.
        if state == StatusState::Hibernated {
            return None;
        }
        let text = state_text(session, state, now_ms)?;
        Some(Self::Single { state, text })
    }
}

/// The tab's layout at glyph size with tile `filled` in `ink`.
pub(super) fn layout_glyph(tiles: &[Tile], filled: usize, ink: Rgba, colors: SemanticColors) -> AnyElement {
    let mut glyph = div()
        .flex_none()
        .relative()
        .w(px(GLYPH_WIDTH))
        .h(px(GLYPH_HEIGHT));
    for (index, tile) in tiles.iter().enumerate() {
        let half = GLYPH_GAP / 2.0;
        // Inset only the edges a tile shares with a neighbour, so the
        // glyph's outline stays one clean rectangle.
        let left = if tile.x > 0.001 { half } else { 0.0 };
        let top = if tile.y > 0.001 { half } else { 0.0 };
        let right = if tile.x + tile.width < 0.999 { half } else { 0.0 };
        let bottom = if tile.y + tile.height < 0.999 { half } else { 0.0 };
        let fill = if index == filled {
            ink
        } else {
            colors.primary.alpha(0.14)
        };
        glyph = glyph.child(
            div()
                .absolute()
                .left(px(tile.x * GLYPH_WIDTH + left))
                .top(px(tile.y * GLYPH_HEIGHT + top))
                .w(px((tile.width * GLYPH_WIDTH - left - right).max(1.0)))
                .h(px((tile.height * GLYPH_HEIGHT - top - bottom).max(1.0)))
                .rounded(px(2.0))
                .bg(fill),
        );
    }
    glyph.into_any_element()
}

impl HoverStatus {
    pub(super) fn render(&self, now_ms: f64, colors: SemanticColors) -> AnyElement {
        match self {
            Self::Single { state, text } => div()
                .id("session-hover-state")
                .debug_selector(|| "session-hover-state".into())
                .flex()
                .items_start()
                .gap(px(7.0))
                .child(
                    div()
                        .w(px(13.0))
                        .h(px(16.0))
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .child(div().size(px(6.0)).rounded_full().bg(state_dot(*state, colors))),
                )
                .child(
                    div()
                        .min_w(px(0.0))
                        .flex_1()
                        .line_clamp(3)
                        .line_height(px(16.0))
                        .text_size(px(Typo::META.size))
                        .text_color(colors.primary.alpha(0.82))
                        .child(text.clone()),
                )
                .into_any_element(),
            Self::Panes {
                tiles,
                lines,
                hidden,
            } => {
                let mut list = div()
                    .id("session-hover-panes")
                    .debug_selector(|| "session-hover-panes".into())
                    .flex()
                    .flex_col()
                    .gap(px(6.0));
                for line in lines {
                    let ink = state_ink(&line.session, line.state, colors);
                    let text = state_text(&line.session, line.state, now_ms);
                    // The card's title already names the hovered session, so
                    // its own line leads with what it is doing.
                    let (name, text) = match text {
                        Some(text) if line.owner => (text, None),
                        text => (display_title(&line.session), text),
                    };
                    list = list.child(
                        div()
                            .flex()
                            .items_start()
                            .gap(px(8.0))
                            .child(div().pt(px(1.0)).child(layout_glyph(
                                tiles,
                                line.tile,
                                ink,
                                colors,
                            )))
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .flex_1()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .line_clamp(2)
                                            .line_height(px(16.0))
                                            .text_size(px(Typo::META.size))
                                            .text_color(colors.primary)
                                            .child(name),
                                    )
                                    .when_some(text, |column, text| {
                                        column.child(
                                            div()
                                                .line_clamp(2)
                                                .line_height(px(15.0))
                                                .text_size(px(Typo::META.size))
                                                .text_color(colors.secondary)
                                                .child(text),
                                        )
                                    }),
                            ),
                    );
                }
                if *hidden > 0 {
                    list = list.child(
                        div()
                            .pl(px(GLYPH_WIDTH + 8.0))
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child(tf("session.hover.more_panes", &[("count", hidden)])),
                    );
                }
                list.into_any_element()
            }
        }
    }
}

/// The single line's dot: the state's colour, quiet at rest.
fn state_dot(state: StatusState, colors: SemanticColors) -> Rgba {
    match state {
        StatusState::Working => colors.primary.alpha(0.55),
        StatusState::NeedsInput { destructive: true } => Ink::on_surface(Ink::DANGER, colors),
        StatusState::NeedsInput { destructive: false } => Ink::on_surface(Ink::ATTENTION, colors),
        StatusState::DoneUnseen => Ink::on_surface(Ink::FRESH, colors),
        StatusState::IdleSeen | StatusState::None | StatusState::Hibernated => colors.tertiary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::workspace::{PaneId, SplitId};

    fn pane(id: &str) -> LayoutNode {
        LayoutNode::Pane {
            id: PaneId(id.into()),
            session_id: SessionId::new(id),
        }
    }

    fn split(axis: LayoutAxis, fraction: f32, first: LayoutNode, second: LayoutNode) -> LayoutNode {
        LayoutNode::Split {
            id: SplitId(format!("{fraction}-{axis:?}")),
            axis,
            fraction,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn tiles_keep_an_uneven_split_in_proportion_and_reading_order() {
        // Agent on the left two thirds; two shells stacked on the right.
        let layout = split(
            LayoutAxis::Horizontal,
            0.66,
            pane("agent"),
            split(LayoutAxis::Vertical, 0.5, pane("top"), pane("bottom")),
        );
        let tiles = layout_tiles(&layout);
        let ids: Vec<_> = tiles.iter().map(|(id, _)| id.0.clone()).collect();
        assert_eq!(ids, ["agent", "top", "bottom"]);
        let close = |a: f32, b: f32| (a - b).abs() < 1e-5;
        let agent = tiles[0].1;
        assert!(close(agent.width, 0.66) && close(agent.height, 1.0));
        let bottom = tiles[2].1;
        assert!(close(bottom.x, 0.66) && close(bottom.y, 0.5));
        assert!(close(bottom.width, 0.34) && close(bottom.height, 0.5));
        let area: f32 = tiles.iter().map(|(_, t)| t.width * t.height).sum();
        assert!(close(area, 1.0), "tiles cover the tab exactly: {area}");
    }

    #[test]
    fn a_corrupt_fraction_still_draws_a_glyph() {
        let layout = split(LayoutAxis::Horizontal, f32::NAN, pane("a"), pane("b"));
        let tiles = layout_tiles(&layout);
        assert_eq!(tiles.len(), 2);
        assert!((tiles[0].1.width - 0.5).abs() < 1e-6);
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

    fn shell_pane(id: &str, title: &str) -> Arc<SessionRecord> {
        let mut shell = (*record("preview-shell")).clone();
        shell.id = SessionId::new(id);
        shell.title = title.into();
        shell.parent = Some(SessionId::new("preview-claude"));
        Arc::new(shell)
    }

    fn text(status: &HoverStatus) -> String {
        match status {
            HoverStatus::Single { text, .. } => text.clone(),
            HoverStatus::Panes { .. } => panic!("expected one line"),
        }
    }

    #[test]
    fn a_blocked_agent_says_what_it_is_asking() {
        let claude = record("preview-claude");
        let status = HoverStatus::of(&claude, None, |_| None, false, 0.0).unwrap();
        assert_eq!(text(&status), "Needs you · Wants to publish the release tag");
    }

    #[test]
    fn a_job_reports_its_own_progress_even_in_a_plain_shell() {
        let mut shell = (*record("preview-shell")).clone();
        shell.status = SessionStatus::Working;
        shell.terminal_progress = Some(diri_proto::TerminalProgress {
            state: ProgressState::Normal,
            percent: 40,
        });
        let status = HoverStatus::of(&shell, None, |_| None, false, 0.0).unwrap();
        assert_eq!(text(&status), "Working · 40%");
        shell.terminal_progress = Some(diri_proto::TerminalProgress {
            state: ProgressState::Error,
            percent: 72,
        });
        let status = HoverStatus::of(&shell, None, |_| None, false, 0.0).unwrap();
        assert_eq!(text(&status), "Failed at 72%");
    }

    #[test]
    fn a_shell_at_its_prompt_adds_no_line() {
        let shell = record("preview-shell");
        assert!(HoverStatus::of(&shell, None, |_| None, false, 0.0).is_none());
    }

    #[test]
    fn a_finished_turn_says_how_long_ago() {
        let mut codex = (*record("preview-codex")).clone();
        codex.status = SessionStatus::Idle;
        codex.last_turn_completed_at = Some(diri_proto::DateMillis(1_000_000.0));
        codex.last_seen_at = Some(diri_proto::DateMillis(1_000_001.0));
        let now = 1_000_000.0 + 12.0 * 60_000.0;
        let status = HoverStatus::of(&codex, None, |_| None, false, now).unwrap();
        assert_eq!(text(&status), "Done 12m ago");
    }

    #[test]
    fn a_split_tab_names_each_pane_and_where_it_sits() {
        let claude = record("preview-claude");
        let mut tests = (*shell_pane("pane-tests", "cargo test")).clone();
        tests.status = SessionStatus::Working;
        tests.terminal_progress = Some(diri_proto::TerminalProgress {
            state: ProgressState::Normal,
            percent: 40,
        });
        let tests = Arc::new(tests);
        let server = shell_pane("pane-server", "pnpm dev");
        let layout = split(
            LayoutAxis::Horizontal,
            0.58,
            LayoutNode::Pane {
                id: PaneId("agent".into()),
                session_id: claude.id.clone(),
            },
            split(LayoutAxis::Vertical, 0.55, pane("pane-tests"), pane("pane-server")),
        );
        let panes = [claude.clone(), tests, server];
        let lookup = |id: &SessionId| panes.iter().find(|pane| &pane.id == id).cloned();
        let Some(HoverStatus::Panes {
            tiles,
            lines,
            hidden,
        }) = HoverStatus::of(&claude, Some(&layout), lookup, false, 0.0)
        else {
            panic!("a split tab lists its panes");
        };
        assert_eq!(tiles.len(), 3);
        assert_eq!(hidden, 0);
        let read: Vec<_> = lines
            .iter()
            .map(|line| {
                (
                    line.session.id.0.as_str(),
                    line.tile,
                    state_text(&line.session, line.state, 0.0),
                )
            })
            .collect();
        assert_eq!(
            read,
            [
                (
                    "preview-claude",
                    0,
                    Some("Needs you · Wants to publish the release tag".to_owned())
                ),
                ("pane-tests", 1, Some("Working · 40%".to_owned())),
                ("pane-server", 2, None),
            ]
        );
    }

    #[test]
    fn panes_past_the_cap_fold_into_a_count() {
        let claude = record("preview-claude");
        let ids: Vec<String> = (0..8).map(|index| format!("pane-{index}")).collect();
        let mut layout = pane(&ids[7]);
        for id in ids[..7].iter().rev() {
            layout = split(LayoutAxis::Vertical, 0.5, pane(id), layout);
        }
        let lookup = |id: &SessionId| Some(shell_pane(&id.0, &id.0));
        let Some(HoverStatus::Panes { lines, hidden, .. }) =
            HoverStatus::of(&claude, Some(&layout), lookup, false, 0.0)
        else {
            panic!("a split tab lists its panes");
        };
        assert_eq!((lines.len(), hidden), (MAX_PANE_LINES, 2));
    }

    #[test]
    fn done_age_reads_in_the_largest_whole_unit() {
        let now = 1_000_000_000_000.0;
        assert_eq!(done_ago(now, now + 9_000.0), "Done just now");
        assert_eq!(done_ago(now, now - 7.0 * 60_000.0), "Done 7m ago");
        assert_eq!(done_ago(now, now - 3.0 * 3_600_000.0), "Done 3h ago");
        assert_eq!(done_ago(now, now - 49.0 * 3_600_000.0), "Done 2d ago");
    }
}
