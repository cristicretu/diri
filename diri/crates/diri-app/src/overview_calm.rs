//! Design exploration (phase 1): calm, Safari-like Session Overview variants.
//! Cards are true miniatures of each session's terminal pane: the resident
//! grid painted at a scaled font size inside a box with the pane's own aspect
//! ratio. Only the prototype layouts live here; the current overview stays the
//! default until one variant is chosen.
use super::*;
use crate::overview_zoom::{CardPose, FONT_STEP, ZoomRect};
use diri_term::metrics::CellMetrics;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum OverviewVariant {
    #[default]
    Current,
    /// A: Safari-literal. Two large cards, a title strip attached above.
    Safari,
    /// B: three columns, the title sits below the miniature.
    Gallery,
    /// C: miniature windows. The title bar is part of the pane's own surface,
    /// grouped by project with a quiet label.
    Windows,
}

impl OverviewVariant {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn from_env(value: &str) -> Self {
        match value {
            "a" | "safari" => Self::Safari,
            "b" | "gallery" => Self::Gallery,
            "c" | "windows" => Self::Windows,
            _ => Self::Current,
        }
    }
}

/// Font size and padding of the live pane the miniature is scaled from.
const PANE_FONT: f32 = 13.0;
const PANE_PAD: f32 = 12.0;
/// The overview's own top inset (title bar row with the search field).
const TOP_INSET: f32 = 42.0;
/// Scroll content padding above the first row, per variant.
const WINDOWS_TOP_PAD: f32 = 22.0;
const BOTTOM_PAD: f32 = 56.0;
/// Project label above each group of mini windows, and its gap to the cards.
const LABEL_HEIGHT: f32 = 16.0;
const LABEL_GAP: f32 = 14.0;

fn mix(a: gpui::Rgba, b: gpui::Rgba, t: f32) -> gpui::Rgba {
    gpui::Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.0,
    }
}

/// Width-fitted miniature: the largest font on the ladder whose columns fit,
/// and the box height that grid needs (line heights round to whole pixels at
/// small sizes, so the height follows the metrics rather than pane aspect).
fn miniature_geometry(
    window: &Window,
    font: &gpui::Font,
    pane: CellMetrics,
    card_width: f32,
    cols: u16,
    rows: u16,
) -> (f32, f32) {
    let scale = card_width / (f32::from(pane.cell_width) * f32::from(cols) + 2.0 * PANE_PAD);
    let pad = PANE_PAD * scale;
    let inner_w = card_width - 2.0 * pad;
    let mut size = ((PANE_FONT * scale) / FONT_STEP).ceil() * FONT_STEP;
    loop {
        let m = CellMetrics::measure(window.text_system(), font, px(size));
        if f32::from(m.cell_width) * f32::from(cols) <= inner_w + 0.5 || size <= FONT_STEP {
            // Half a pixel of slack: rows are floored from the painted height.
            return (
                size,
                (f32::from(m.line_height) * f32::from(rows) + 2.0 * pad + 0.5).ceil(),
            );
        }
        size -= FONT_STEP;
    }
}

/// The miniature's inset around the grid: the pane's padding at card scale.
fn miniature_pad(pane: CellMetrics, card_width: f32, cols: u16) -> f32 {
    PANE_PAD * card_width / (f32::from(pane.cell_width) * f32::from(cols) + 2.0 * PANE_PAD)
}

/// Consecutive sessions of one project share a labelled group, in order.
fn group_by_project(
    sessions: impl Iterator<Item = SessionRecord>,
    projects: &HashMap<diri_proto::ProjectId, String>,
) -> Vec<(String, Vec<SessionRecord>)> {
    let mut groups: Vec<(String, Vec<SessionRecord>)> = Vec::new();
    for session in sessions {
        let name = projects
            .get(&session.project_id)
            .cloned()
            .unwrap_or_else(|| "Sessions".to_owned());
        match groups.last_mut() {
            Some((last, items)) if *last == name => items.push(session),
            _ => groups.push((name, vec![session])),
        }
    }
    groups
}

/// The miniature's grid box: `element` at `size`, `left`/`bottom` inside a
/// `width` x `height` box on the terminal background. The grid and the zoom
/// build cards through this one function so a landing is the card itself.
#[allow(clippy::too_many_arguments)]
pub(super) fn miniature_box(
    element: Option<&TerminalElement>,
    width: f32,
    height: f32,
    left: f32,
    bottom: f32,
    size: f32,
    font: &gpui::Font,
    theme: diri_term::theme::TermTheme,
    hibernating: bool,
    window: &Window,
) -> gpui::Div {
    let mut mini = div()
        .relative()
        .flex_none()
        .w(px(width))
        .h(px(height))
        .overflow_hidden()
        .bg(theme.background);
    if let Some(element) = element {
        let metrics = CellMetrics::measure(window.text_system(), font, px(size));
        let own_cols = element.grid_cols().max(1);
        let own_rows = element.grid_rows().max(1);
        mini = mini.child(
            div()
                .absolute()
                .left(px(left))
                .bottom(px(bottom))
                .w(metrics.cell_width * f32::from(own_cols))
                .h(metrics.line_height * f32::from(own_rows))
                .child(
                    element
                        .clone()
                        .font(font.clone())
                        .font_size(px(size))
                        .theme(theme),
                ),
        );
    }
    if hibernating {
        mini = mini.opacity(0.55);
    }
    mini
}

struct Layout {
    columns: usize,
    margin: f32,
    gutter: f32,
    row_gap: f32,
    radius: f32,
    strip: f32,
}

impl Layout {
    fn for_variant(variant: OverviewVariant, width: f32) -> Self {
        match variant {
            OverviewVariant::Safari => Layout {
                columns: if width >= 1800.0 { 3 } else { 2 },
                margin: 88.0,
                gutter: 64.0,
                row_gap: 52.0,
                radius: 10.0,
                strip: 28.0,
            },
            OverviewVariant::Gallery => Layout {
                columns: if width >= 1200.0 {
                    3
                } else if width >= 760.0 {
                    2
                } else {
                    1
                },
                margin: 56.0,
                gutter: 36.0,
                row_gap: 30.0,
                radius: 9.0,
                strip: 34.0,
            },
            OverviewVariant::Windows | OverviewVariant::Current => Layout {
                columns: ((width - 112.0) / 520.0).round().clamp(1.0, 4.0) as usize,
                margin: 56.0,
                gutter: 40.0,
                row_gap: 40.0,
                radius: 10.0,
                strip: 26.0,
            },
        }
    }

    fn card_width(&self, width: f32) -> f32 {
        (width - 2.0 * self.margin - self.gutter * (self.columns as f32 - 1.0))
            / self.columns as f32
    }
}

/// Where every mini-window card sits inside the scroll content, from the same
/// numbers the layout uses. The painted bounds win once the grid has drawn;
/// this is the first frame of a zoom into a grid that has not drawn yet.
struct CardPlan {
    /// Content-space top-left of each session's card, in grid order.
    cards: Vec<(SessionId, f32, f32)>,
    card_width: f32,
    card_height: f32,
    content_height: f32,
}

fn plan_cards(
    layout: &Layout,
    card_width: f32,
    card_height: f32,
    groups: &[Vec<SessionId>],
    show_new: bool,
) -> CardPlan {
    let mut cards = Vec::new();
    let mut y = WINDOWS_TOP_PAD;
    for (index, ids) in groups.iter().enumerate() {
        if index > 0 {
            y += layout.row_gap;
        }
        y += LABEL_HEIGHT + LABEL_GAP;
        let cells = ids.len() + usize::from(show_new && index + 1 == groups.len());
        for (cell, id) in ids.iter().enumerate() {
            let (row, column) = (cell / layout.columns, cell % layout.columns);
            cards.push((
                id.clone(),
                layout.margin + column as f32 * (card_width + layout.gutter),
                y + row as f32 * (card_height + layout.row_gap),
            ));
        }
        let rows = cells.div_ceil(layout.columns);
        if rows > 0 {
            y += rows as f32 * card_height + (rows - 1) as f32 * layout.row_gap;
        }
    }
    CardPlan {
        cards,
        card_width,
        card_height,
        content_height: y + BOTTOM_PAD,
    }
}

/// How far the grid must scroll (as a positive distance from the top) so the
/// card spanning `top..bottom` of the content is on screen with its margin.
fn reveal_scroll(
    current: f32,
    top: f32,
    bottom: f32,
    viewport: f32,
    content: f32,
    margin: f32,
) -> f32 {
    let mut scroll = current;
    if bottom + margin > scroll + viewport {
        scroll = bottom + margin - viewport;
    }
    if top - margin < scroll {
        scroll = top - margin;
    }
    scroll.clamp(0.0, (content - viewport).max(0.0))
}

/// The mini-window card's geometry shared by the grid and the zoom.
pub(super) struct CalmCardGeometry {
    /// The card's inside: strip, font ladder size, grid inset.
    pub(super) pose: CardPose,
    pub(super) font: gpui::Font,
}

impl SessionSurfaces {
    /// The overview's backdrop, which is also what the workbench turns into
    /// behind a page shrinking into its card.
    pub(super) fn calm_desk(&self) -> gpui::Rgba {
        let (theme, dark) = {
            let store = self.store.read().expect("session store lock poisoned");
            (
                crate::app_theme::terminal_theme_in(&store),
                crate::app_theme::colors_in(&store).appearance == diri_ui::Appearance::Dark,
            )
        };
        let black = gpui::rgba(0x000000ff);
        if dark {
            mix(theme.background, black, 0.42)
        } else {
            mix(theme.background, black, 0.075)
        }
    }

    /// The mini-window pose every card paints, for the zoom to land on.
    pub(super) fn calm_card_geometry(&self, window: &Window) -> CalmCardGeometry {
        let width = f32::from(window.viewport_size().width);
        let layout = Layout::for_variant(OverviewVariant::Windows, width);
        let card_width = layout.card_width(width);
        let font = self.terminal_font();
        let pane = CellMetrics::measure(window.text_system(), &font, px(PANE_FONT));
        let (cols, rows) = self.fleet_grid();
        let (size, _) = miniature_geometry(window, &font, pane, card_width, cols, rows);
        let pad = miniature_pad(pane, card_width, cols);
        CalmCardGeometry {
            pose: CardPose {
                strip: layout.strip,
                font: size,
                grid_left: pad,
                grid_bottom: pad,
                radius: layout.radius,
            },
            font,
        }
    }

    fn calm_plan(&self, window: &Window) -> CardPlan {
        let width = f32::from(window.viewport_size().width);
        let layout = Layout::for_variant(OverviewVariant::Windows, width);
        let card_width = layout.card_width(width);
        let font = self.terminal_font();
        let pane = CellMetrics::measure(window.text_system(), &font, px(PANE_FONT));
        let (cols, rows) = self.fleet_grid();
        let (_, mini_height) = miniature_geometry(window, &font, pane, card_width, cols, rows);
        let (groups, show_new) = {
            let mut store = self.store.write().expect("session store lock poisoned");
            let sessions = store.ordered_sessions();
            let state = store.overview_state().clone();
            let projects: HashMap<_, _> = store
                .projects()
                .values()
                .map(|project| (project.id.clone(), project.name.clone()))
                .collect();
            let groups = group_by_project(state.visible_sessions(&sessions).cloned(), &projects)
                .into_iter()
                .map(|(_, items)| items.into_iter().map(|session| session.id).collect())
                .collect::<Vec<Vec<_>>>();
            (groups, state.query().is_empty())
        };
        plan_cards(
            &layout,
            card_width,
            mini_height + layout.strip + 2.0,
            &groups,
            show_new,
        )
    }

    /// Where `id`'s card will paint this frame, in window coordinates, before
    /// the grid has measured it: the plan at the grid's current scroll.
    pub(super) fn calm_card_estimate(&self, id: &SessionId, window: &Window) -> Option<ZoomRect> {
        if self.overview_variant != OverviewVariant::Windows {
            return None;
        }
        let plan = self.calm_plan(window);
        let scroll = f32::from(self.overview_grid_scroll.offset().y);
        plan.cards
            .iter()
            .find(|(card, _, _)| card == id)
            .map(|(_, x, y)| ZoomRect {
                x: *x,
                y: TOP_INSET + y + scroll,
                width: plan.card_width,
                height: plan.card_height,
            })
    }

    /// Scroll the grid so `id`'s card is on screen. `from_top` starts from an
    /// unscrolled grid, which is how every fresh open begins. Measured card
    /// bounds move with the scroll so the next zoom frame stays exact.
    pub(super) fn reveal_calm_card(&self, id: &SessionId, window: &Window, from_top: bool) {
        if self.overview_variant == OverviewVariant::Current {
            return;
        }
        let plan = self.calm_plan(window);
        let Some((_, _, top)) = plan.cards.iter().find(|(card, _, _)| card == id) else {
            if from_top {
                self.overview_grid_scroll
                    .set_offset(point(px(0.0), px(0.0)));
            }
            return;
        };
        let viewport = f32::from(window.viewport_size().height) - TOP_INSET;
        let current = if from_top {
            0.0
        } else {
            -f32::from(self.overview_grid_scroll.offset().y)
        };
        let before = -f32::from(self.overview_grid_scroll.offset().y);
        let scroll = reveal_scroll(
            current,
            *top,
            top + plan.card_height,
            viewport,
            plan.content_height,
            WINDOWS_TOP_PAD,
        );
        self.overview_grid_scroll
            .set_offset(point(px(0.0), px(-scroll)));
        self.zoom.shift_cards(before - scroll);
    }

    pub(super) fn calm_fleet_rows(&self) -> u16 {
        self.fleet_grid().1
    }

    fn fleet_grid(&self) -> (u16, u16) {
        self.resident_previews
            .values()
            .map(|element| (element.grid_cols(), element.grid_rows()))
            .find(|(cols, rows)| *cols > 0 && *rows > 0)
            .unwrap_or((120, 36))
    }

    pub(super) fn render_calm_overview(
        &mut self,
        variant: OverviewVariant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (sessions, state, theme, projects) = {
            let mut store = self.store.write().expect("session store lock poisoned");
            let projects: HashMap<_, _> = store
                .projects()
                .values()
                .map(|project| (project.id.clone(), project.name.clone()))
                .collect();
            (
                store.ordered_sessions(),
                store.overview_state().clone(),
                crate::app_theme::terminal_theme_in(&store),
                projects,
            )
        };
        let colors = self.colors();
        let dark = colors.appearance == diri_ui::Appearance::Dark;
        let black = gpui::rgba(0x000000ff);
        let desk = if dark {
            mix(theme.background, black, 0.42)
        } else {
            mix(theme.background, black, 0.075)
        };
        let viewport = window.viewport_size();
        let width = f32::from(viewport.width);
        let layout = Layout::for_variant(variant, width);
        self.store
            .write()
            .unwrap()
            .set_overview_columns(layout.columns);
        let card_width = layout.card_width(width);
        // Cards re-measure themselves as they paint below.
        self.zoom.forget_cards();

        let font = self.terminal_font();
        let pane = CellMetrics::measure(window.text_system(), &font, px(PANE_FONT));

        let visible = state.visible_sessions(&sessions).cloned();
        let groups: Vec<(Option<String>, Vec<SessionRecord>)> =
            if variant == OverviewVariant::Windows {
                group_by_project(visible, &projects)
                    .into_iter()
                    .map(|(name, items)| (Some(name), items))
                    .collect()
            } else {
                vec![(None, visible.collect())]
            };
        let show_new = state.query().is_empty()
            && matches!(variant, OverviewVariant::Safari | OverviewVariant::Windows);

        let mut body = div()
            .id("calm-overview-scroll")
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .px(px(layout.margin))
            .pt(px(if variant == OverviewVariant::Safari {
                30.0
            } else {
                WINDOWS_TOP_PAD
            }))
            .pb(px(BOTTOM_PAD))
            .gap(px(layout.row_gap))
            .track_scroll(&self.overview_grid_scroll)
            .overflow_y_scroll();
        let group_count = groups.len();
        for (group_index, (label, items)) in groups.into_iter().enumerate() {
            let mut cells: Vec<AnyElement> = items
                .iter()
                .map(|session| {
                    self.calm_card(
                        variant, session, &state, &layout, card_width, pane, &font, theme, colors,
                        desk, window, cx,
                    )
                })
                .collect();
            if show_new && group_index + 1 == group_count {
                cells.push(self.calm_new_card(variant, &layout, card_width, pane, colors, window));
            }
            let mut section = div().flex().flex_none().flex_col().gap(px(layout.row_gap));
            if let Some(label) = label {
                section = section.gap(px(LABEL_GAP)).child(
                    div()
                        .flex_none()
                        .h(px(LABEL_HEIGHT))
                        .line_height(px(LABEL_HEIGHT))
                        .text_size(px(12.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.secondary)
                        .child(label),
                );
            }
            let mut rows = div().flex().flex_none().flex_col().gap(px(layout.row_gap));
            let mut cells = cells.into_iter().peekable();
            while cells.peek().is_some() {
                let mut row = div().flex().flex_none().gap(px(layout.gutter));
                let mut count = 0;
                for cell in cells.by_ref().take(layout.columns) {
                    row = row.child(div().w(px(card_width)).flex_none().child(cell));
                    count += 1;
                }
                for _ in count..layout.columns {
                    row = row.child(div().w(px(card_width)).flex_none());
                }
                rows = rows.child(row);
            }
            body = body.child(section.child(rows));
        }

        let search = self.calm_search(&state, colors, cx);

        let content = div()
            .id("overview-content")
            .debug_selector(|| "OVERVIEW_CONTENT".into())
            .absolute()
            .inset_0()
            .size_full()
            .flex()
            .flex_col()
            .pt(px(TOP_INSET))
            .bg(desk)
            .overflow_hidden()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(body)
            .child(search);

        div()
            .id("overview-scrim")
            .absolute()
            .inset_0()
            .size_full()
            .bg(desk)
            .occlude()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(content)
            .into_any_element()
    }

    fn calm_search(
        &self,
        state: &crate::switcher::SessionOverviewState,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let empty = state.query().is_empty();
        div()
            .id("overview-search")
            .absolute()
            .top(px(9.0))
            .right(px(14.0))
            .w(px(if empty { 168.0 } else { 220.0 }))
            .h(px(26.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .rounded(px(Radius::ROW))
            .bg(colors.primary.alpha(0.07))
            .border_1()
            .border_color(colors.primary.alpha(0.06))
            .on_click(cx.listener(|this, _, window, cx| window.focus(&this.focus_handle, cx)))
            .child(sf_symbol("magnifyingglass", 12.0, colors.tertiary))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(12.5))
                    .text_color(if empty {
                        colors.tertiary
                    } else {
                        colors.primary
                    })
                    .child(if empty {
                        "Search".to_owned()
                    } else {
                        state.query().to_owned()
                    }),
            )
            .into_any_element()
    }

    /// The miniature: the pane's grid at a scaled font inside a box that has
    /// the pane's aspect ratio, so the live pane can zoom into it.
    #[allow(clippy::too_many_arguments)]
    fn calm_miniature(
        &self,
        session: &SessionRecord,
        card_width: f32,
        pane: CellMetrics,
        font: &gpui::Font,
        theme: diri_term::theme::TermTheme,
        window: &Window,
        cx: &Context<Self>,
    ) -> (AnyElement, f32) {
        let live = self.card_terminal(&session.id);
        // Every card has the shape of the pane you work in and one text size,
        // whatever size each session's own terminal happens to be. A session
        // wider or taller than that is cropped, keeping its bottom rows, where
        // the prompt and the latest output are.
        let (cols, rows) = self.fleet_grid();
        let (size, height) = miniature_geometry(window, font, pane, card_width, cols, rows);
        let pad = miniature_pad(pane, card_width, cols);
        let mut mini = miniature_box(
            live,
            card_width,
            height,
            pad,
            pad,
            size,
            font,
            theme,
            session.hibernation.is_some(),
            window,
        );
        if live.is_none()
            && !self.screens.contains_key(&session.id)
            && !self.screen_requests.contains_key(&session.id)
        {
            // Only cards that are actually on screen ask for their screen;
            // a completion repaints and lets the next ones in.
            let weak = cx.entity().downgrade();
            let id = session.id.clone();
            mini = mini.child(
                gpui::canvas(
                    move |bounds, window, cx| {
                        if bounds.intersects(&window.content_mask().bounds) {
                            let id = id.clone();
                            let weak = weak.clone();
                            cx.defer(move |cx| {
                                let _ = weak.update(cx, |this, cx| this.request_screen(id, cx));
                            });
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );
        }
        (mini.into_any_element(), height)
    }

    /// What a card paints: the mounted terminal, else its colored snapshot.
    pub(super) fn card_terminal(&self, id: &SessionId) -> Option<&TerminalElement> {
        self.resident_previews
            .get(id)
            .or_else(|| self.screen_grids.get(id))
    }

    /// The mini-window card (variant C) around `mini`: the integrated title
    /// strip, hairline, shadow and focus ring. `chrome` fades the strip's
    /// contents, hairlines, shadow and ring in; at 1 it is the resting card.
    /// The grid and the zoom both build cards here, so a zoom that lands at 1
    /// paints exactly what the grid paints.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn calm_window_card(
        &mut self,
        session: &SessionRecord,
        strip: f32,
        radius: f32,
        chrome: f32,
        focused: bool,
        mini: AnyElement,
        backdrop: Option<AnyElement>,
        theme: diri_term::theme::TermTheme,
        colors: SemanticColors,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> gpui::Div {
        let dark = colors.appearance == diri_ui::Appearance::Dark;
        let chrome = chrome.clamp(0.0, 1.0);
        let veiled = backdrop.is_some();
        let glyph = Self::status_glyph(session, 12.0, colors);
        let title = div()
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .child(display_title(session));
        let shadow = vec![BoxShadow {
            color: gpui::hsla(0.0, 0.0, 0.0, if dark { 0.45 } else { 0.12 } * chrome),
            offset: point(px(0.0), px(if dark { 10.0 } else { 6.0 } * chrome)),
            blur_radius: px(if dark { 30.0 } else { 18.0 } * chrome),
            spread_radius: px(0.0),
            inset: false,
        }];
        let hairline = colors
            .primary
            .alpha(if dark { 0.10 } else { 0.09 } * chrome);
        let label = div()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(glyph)
            .child(title);
        let mut card = div()
            .relative()
            .flex()
            .flex_col()
            .rounded(px(radius))
            .overflow_hidden()
            .border_1()
            .border_color(hairline)
            .shadow(shadow)
            .bg(theme.background);
        if let Some(backdrop) = backdrop {
            // A flying snapshot paints under the strip and the grid area;
            // the strip's own fill fades in over the page's title bar.
            card = card.child(backdrop);
        }
        let mut title_strip = div().flex_none();
        if veiled {
            title_strip = title_strip.bg(gpui::Rgba {
                a: theme.background.a * chrome,
                ..theme.background
            });
        }
        let card = card
            .child(
                title_strip
                    .h(px(strip))
                    .flex()
                    .items_center()
                    .px(px(10.0))
                    .overflow_hidden()
                    .border_b_1()
                    .border_color(colors.primary.alpha(0.07 * chrome))
                    .text_size(px(11.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(colors.secondary)
                    .child(if chrome < 1.0 {
                        label.opacity(chrome)
                    } else {
                        label
                    }),
            )
            .child(mini);
        let ring = if dark {
            gpui::rgba(0xffffffd9)
        } else {
            gpui::rgba(0x0a84ffff)
        };
        // One clean ring, drawn outside the card so it never covers content.
        let inset = 4.0;
        div().relative().child(card).child(
            div()
                .absolute()
                .top(px(-inset))
                .left(px(-inset))
                .right(px(-inset))
                .bottom(px(-inset))
                .rounded(px(radius + inset))
                .border_2()
                .border_color(ring.alpha(if focused { chrome } else { 0.0 })),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn calm_card(
        &mut self,
        variant: OverviewVariant,
        session: &SessionRecord,
        state: &crate::switcher::SessionOverviewState,
        layout: &Layout,
        card_width: f32,
        pane: CellMetrics,
        font: &gpui::Font,
        theme: diri_term::theme::TermTheme,
        colors: SemanticColors,
        desk: gpui::Rgba,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let dark = colors.appearance == diri_ui::Appearance::Dark;
        let focused = state.focused() == Some(&session.id);
        let id = session.id.clone();
        let glyph = Self::status_glyph(session, 12.0, colors);
        let (mini, mini_height) =
            self.calm_miniature(session, card_width, pane, font, theme, window, cx);
        let title = div()
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .child(display_title(session));
        let black = gpui::rgba(0x000000ff);
        let ring = if dark {
            gpui::rgba(0xffffffd9)
        } else {
            gpui::rgba(0x0a84ffff)
        };
        let shadow = vec![BoxShadow {
            color: gpui::hsla(0.0, 0.0, 0.0, if dark { 0.45 } else { 0.12 }),
            offset: point(px(0.0), px(if dark { 10.0 } else { 6.0 })),
            blur_radius: px(if dark { 30.0 } else { 18.0 }),
            spread_radius: px(0.0),
            inset: false,
        }];
        let hairline = colors.primary.alpha(if dark { 0.10 } else { 0.09 });

        let card = match variant {
            OverviewVariant::Safari => {
                let strip_bg = if dark {
                    mix(theme.background, black, 0.45)
                } else {
                    mix(theme.background, black, 0.055)
                };
                div()
                    .flex()
                    .flex_col()
                    .rounded(px(layout.radius))
                    .overflow_hidden()
                    .border_1()
                    .border_color(hairline)
                    .shadow(shadow)
                    .child(
                        div()
                            .flex_none()
                            .h(px(layout.strip))
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap(px(6.0))
                            .px(px(12.0))
                            .bg(strip_bg)
                            .text_size(px(12.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.primary.alpha(0.88))
                            .child(glyph)
                            .child(title),
                    )
                    .child(mini)
            }
            OverviewVariant::Gallery => div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .rounded(px(layout.radius))
                        .overflow_hidden()
                        .border_1()
                        .border_color(hairline)
                        .shadow(shadow)
                        .child(mini),
                )
                .child(
                    div()
                        .flex_none()
                        .h(px(layout.strip))
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .px(px(2.0))
                        .text_size(px(12.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(colors.primary.alpha(0.82))
                        .child(glyph)
                        .child(title),
                ),
            _ => {
                // Variant C builds through the one card the zoom lands as.
                let ringed = if self.zoom_card_in_flight(&session.id) {
                    // The card is in the air above the grid; keep its place.
                    div().relative().h(px(mini_height + layout.strip + 2.0))
                } else {
                    self.calm_window_card(
                        session,
                        layout.strip,
                        layout.radius,
                        1.0,
                        focused,
                        mini,
                        None,
                        theme,
                        colors,
                        window,
                        cx,
                    )
                };
                return self.calm_card_shell(
                    id,
                    ringed.child(self.card_probe(session.id.clone())),
                    cx,
                );
            }
        };

        // One clean ring, drawn outside the card so it never covers content.
        let inset = 4.0;
        let ring_radius = layout.radius + inset;
        let ringed = div().relative().child(card).child(
            div()
                .absolute()
                .top(px(-inset))
                .left(px(-inset))
                .right(px(-inset))
                .bottom(px(if variant == OverviewVariant::Gallery {
                    layout.strip - inset
                } else {
                    -inset
                }))
                .rounded(px(ring_radius))
                .border_2()
                .border_color(if focused { ring } else { ring.alpha(0.0) }),
        );
        let _ = desk;
        self.calm_card_shell(id, ringed, cx)
    }

    fn calm_card_shell(
        &self,
        id: SessionId,
        ringed: gpui::Div,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(SharedString::from(format!("overview-card-{}", id.0)))
            .debug_selector(|| format!("OVERVIEW_CARD_{}", id.0))
            .cursor_pointer()
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                let mut store = this.store.write().expect("session store lock poisoned");
                if event.modifiers().platform {
                    store.toggle_overview_selection(id.clone());
                } else {
                    store.activate_overview_session(id.clone());
                }
                drop(store);
                cx.notify();
            }))
            .child(ringed)
            .into_any_element()
    }

    fn calm_new_card(
        &self,
        variant: OverviewVariant,
        layout: &Layout,
        card_width: f32,
        pane: CellMetrics,
        colors: SemanticColors,
        window: &Window,
    ) -> AnyElement {
        let (cols, rows) = self.fleet_grid();
        let font = self.terminal_font();
        let (_, mini_height) = miniature_geometry(window, &font, pane, card_width, cols, rows);
        let height = mini_height
            + if variant == OverviewVariant::Gallery {
                0.0
            } else {
                layout.strip + 2.0
            };
        div()
            .id("overview-new-session")
            .w(px(card_width))
            .h(px(height))
            .rounded(px(layout.radius))
            .bg(colors.primary.alpha(0.05))
            .border_1()
            .border_color(colors.primary.alpha(0.06))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|style| style.bg(colors.primary.alpha(0.08)))
            .child(sf_symbol_weighted(
                "plus",
                if variant == OverviewVariant::Safari {
                    44.0
                } else {
                    30.0
                },
                SymbolWeight::Regular,
                colors.primary.alpha(0.45),
            ))
            .into_any_element()
    }
}
