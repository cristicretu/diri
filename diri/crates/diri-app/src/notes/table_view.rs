//! Tables in the note editor: the grid, its menu and its keys.
//!
//! A table is a run of cell blocks (see `diri_notes::edit::table`), so each
//! cell lays out its own text like any block and the caret, selection, IME
//! and marks need nothing new. This module draws the run as a grid: columns
//! sized to their content up to a cap, wrapping inside it, the header row
//! emphasised, theme hairlines between cells, and a table wider than the
//! column scrolling sideways inside its block. Rows are the unit of the
//! editor's virtualized layout, so a long table lays out only the rows near
//! the viewport.

use super::*;
use diri_notes::doc::{Align, table_range};

/// Space inside a cell around its text.
const CELL_PAD_X: f32 = 10.0;
const CELL_PAD_Y: f32 = 6.0;
/// A column never narrows below this or grows past it; longer text wraps.
const COL_MIN: f32 = 72.0;
const COL_MAX: f32 = 320.0;
/// Space above and below a table block.
const TABLE_PAD: f32 = 8.0;
const CELL_TEXT: f32 = 14.0;
const CELL_LINE: f32 = 20.0;
const TABLE_MENU_WIDTH: f32 = 248.0;

/// A line's width at the cell text size, by character class: digits and
/// capitals run wider than lower case, spaces and punctuation narrower.
/// Close enough to size a column without laying the text out.
fn text_width(text: &str) -> f32 {
    text.chars()
        .map(|c| {
            let em = if c.is_ascii_digit() || c == '$' || c == '%' {
                0.62
            } else if c.is_uppercase() {
                0.68
            } else if c == ' ' || c == ',' || c == '.' || c == ':' || c == ';' || c == '\'' {
                0.3
            } else if c.is_ascii_lowercase() {
                0.53
            } else {
                0.9
            };
            em * CELL_TEXT
        })
        .sum()
}

/// How far the edge fade reaches into a wide table, and the scroll distance
/// over which it comes to full strength (the sidebar's numbers, sideways).
const EDGE_FADE: f32 = 28.0;
const EDGE_RAMP: f32 = 14.0;

/// The edge where a wide table has more to see dissolves into the surface,
/// the way the sidebar's list does at its ends, so a cut column reads as
/// "scroll for more". The mask lands on exactly the color the work surface
/// settles to over the window, so it is invisible at rest and never a dark
/// band on glass; cut columns also fade themselves (`column_alpha`) because a
/// translucent mask alone cannot hide them there.
fn scroll_fade(scroll: ScrollHandle, colors: SemanticColors) -> impl IntoElement {
    let fill: gpui::Hsla = diri_ui::composite(colors.work_surface(), colors.window_fill()).into();
    let opaque = colors.material() == diri_ui::Material::Opaque;
    canvas(
        |_, _, _| {},
        move |_, _, window, _| {
            // Under glass the surface's settled color depends on whatever is
            // behind the window, so a mask would paint a band; the cut
            // columns fade themselves instead, as sidebar rows do.
            if !opaque {
                return;
            }
            let viewport = scroll.bounds();
            let max = scroll.max_offset().x;
            if max <= px(0.0) || viewport.size.width <= px(0.0) {
                return;
            }
            let scrolled = f32::from(-scroll.offset().x).max(0.0);
            let remaining = (f32::from(max) - scrolled).max(0.0);
            let mut paint = |left: Pixels, toward_right: bool, strength: f32| {
                if strength <= 0.01 {
                    return;
                }
                let solid = fill.opacity(strength);
                let clear = fill.opacity(0.0);
                let (from, to) = if toward_right {
                    (clear, solid)
                } else {
                    (solid, clear)
                };
                window.paint_quad(fill_quad(
                    Bounds::new(
                        point(left, viewport.top() + px(1.0)),
                        size(px(EDGE_FADE), viewport.size.height - px(2.0)),
                    ),
                    gpui::linear_gradient(
                        90.0,
                        gpui::linear_color_stop(from, 0.0),
                        gpui::linear_color_stop(to, 1.0),
                    ),
                ));
            };
            paint(
                viewport.right() - px(EDGE_FADE),
                true,
                (remaining / EDGE_RAMP).min(1.0),
            );
            paint(viewport.left(), false, (scrolled / EDGE_RAMP).min(1.0));
        },
    )
    .absolute()
    .inset_0()
}

fn fill_quad(bounds: Bounds<Pixels>, background: gpui::Background) -> gpui::PaintQuad {
    fill(bounds, background)
}

/// Opacity for a column of a sideways-scrolled table: a column cut by an
/// edge fades by how much of it is out of view, the way sidebar rows
/// dissolve near the list's ends. Uses last frame's scroll geometry.
fn column_alpha(scroll: &ScrollHandle, left: f32, width: f32) -> f32 {
    let viewport = f32::from(scroll.bounds().size.width);
    let max = f32::from(scroll.max_offset().x);
    if max <= 0.0 || viewport <= 0.0 || width <= 0.0 {
        return 1.0;
    }
    let scrolled = f32::from(-scroll.offset().x).max(0.0);
    let remaining = (max - scrolled).max(0.0);
    let visible_left = scrolled;
    let visible_right = scrolled + viewport;
    let right_cut = ((left + width) - visible_right).max(0.0).min(width) / width;
    let left_cut = (visible_left - left).max(0.0).min(width) / width;
    let right = 1.0 - (remaining / EDGE_RAMP).min(1.0) * right_cut;
    let left_side = 1.0 - (scrolled / EDGE_RAMP).min(1.0) * left_cut;
    right.min(left_side).max(0.0)
}

/// What a frame drew for one table.
pub(super) struct TableFrame {
    /// The block, or `None` when no row of it is near the viewport.
    pub element: Option<AnyElement>,
    pub layouts: Vec<Option<TextLayout>>,
    pub shown: Vec<Shown>,
    /// Height of the whole table when it is off screen, for the spacer.
    pub offscreen: f32,
}

/// The glass table menu's state.
pub(super) struct TableMenu {
    pub selected: usize,
    /// Where the menu opens: under the pointer that opened it. From the
    /// keyboard, the caret's cell is scrolled into view and anchors it.
    pub anchor: Option<Bounds<Pixels>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum TableAction {
    RowAbove,
    RowBelow,
    ColLeft,
    ColRight,
    Align(Align),
    DeleteRow,
    DeleteCol,
    DeleteTable,
}

impl TableAction {
    fn row(self, current: Align) -> (&'static str, &'static str, Option<&'static str>) {
        match self {
            Self::RowAbove => (
                "arrow.up",
                crate::i18n::t("notes.table.row_above"),
                Some(KEY_TABLE_ROW_ABOVE),
            ),
            Self::RowBelow => (
                "arrow.down",
                crate::i18n::t("notes.table.row_below"),
                Some(KEY_TABLE_ROW_BELOW),
            ),
            Self::ColLeft => (
                "arrow.left.solid",
                crate::i18n::t("notes.table.col_left"),
                Some(KEY_TABLE_COL_LEFT),
            ),
            Self::ColRight => (
                "arrow.right",
                crate::i18n::t("notes.table.col_right"),
                Some(KEY_TABLE_COL_RIGHT),
            ),
            Self::Align(align) => {
                let icon = if align == current { "checkmark" } else { "" };
                let label = match align {
                    Align::Center => crate::i18n::t("notes.table.align_center"),
                    Align::Right => crate::i18n::t("notes.table.align_right"),
                    _ => crate::i18n::t("notes.table.align_left"),
                };
                (icon, label, None)
            }
            Self::DeleteRow => ("trash", crate::i18n::t("notes.table.delete_row"), None),
            Self::DeleteCol => ("trash", crate::i18n::t("notes.table.delete_col"), None),
            Self::DeleteTable => ("trash", crate::i18n::t("notes.table.delete_table"), None),
        }
    }
}

/// The menu's rows in order, with the separators between groups.
const TABLE_ACTIONS: &[(TableAction, u8)] = &[
    (TableAction::RowAbove, 0),
    (TableAction::RowBelow, 0),
    (TableAction::ColLeft, 0),
    (TableAction::ColRight, 0),
    (TableAction::Align(Align::Left), 1),
    (TableAction::Align(Align::Center), 1),
    (TableAction::Align(Align::Right), 1),
    (TableAction::DeleteRow, 2),
    (TableAction::DeleteCol, 2),
    (TableAction::DeleteTable, 2),
];

pub(super) const TABLE_MENU: floating::Target<NoteEditorView> = floating::Target {
    key: "note-table-menu",
    radius: floating::MENU_RADIUS,
    content: NoteEditorView::table_menu_content,
    dismiss: |this, _, cx| {
        this.table_menu = None;
        cx.notify();
    },
};

/// A pointer's position as a menu anchor: the menu opens just under it.
fn pointer_anchor(at: Point<Pixels>) -> Bounds<Pixels> {
    Bounds::new(at, size(px(2.0), px(0.0)))
}

impl NoteEditorView {
    /// Column widths for the table at `range`: each column's widest cell,
    /// estimated from its text, between [`COL_MIN`] and [`COL_MAX`]. Every
    /// row counts, laid out or not, so columns hold still while scrolling.
    fn table_widths(&self, range: &Range<usize>, cols: usize) -> Vec<f32> {
        let mut widths = vec![COL_MIN; cols];
        for (i, block) in self.editor.blocks()[range.clone()].iter().enumerate() {
            let header = i < cols;
            let weight = if header { 1.06 } else { 1.0 };
            let chips = 22.0 * mention::in_block(block).len() as f32;
            let width = text_width(&block.text) * weight + chips + 2.0 * CELL_PAD_X + 4.0;
            let col = i % cols;
            widths[col] = widths[col].max(width.min(COL_MAX));
        }
        widths
    }

    /// The height of the table row starting at `first`: measured when it
    /// was last laid out, else estimated from its longest cell.
    pub(super) fn table_row_height(&self, first: usize) -> f32 {
        let block = self.editor.block(first);
        if let Some(height) = self.row_heights.borrow().get(&block.id) {
            return *height;
        }
        let cell = block.kind.cell().expect("a table row starts with a cell");
        let cols = usize::from(cell.cols.max(1));
        let lines = self.editor.blocks()[first..(first + cols).min(self.editor.blocks().len())]
            .iter()
            .map(|b| {
                let per_line = (COL_MAX - 2.0 * CELL_PAD_X) / (CELL_TEXT * 0.53);
                (b.text.chars().count() as f32 / per_line).ceil().max(1.0)
            })
            .fold(1.0f32, f32::max);
        let chrome = if cell.header {
            2.0 * TABLE_PAD + 1.0
        } else {
            0.0
        };
        lines * CELL_LINE + 2.0 * CELL_PAD_Y + 1.0 + chrome
    }

    /// Draws the table starting at block `start`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_table(
        &self,
        start: usize,
        rendered: &[bool],
        focused: bool,
        colors: SemanticColors,
        autoscroll: bool,
        cx: &mut Context<Self>,
    ) -> TableFrame {
        let range = table_range(self.editor.blocks(), start).unwrap_or(start..start + 1);
        let cols = self
            .editor
            .block(start)
            .kind
            .cell()
            .map_or(1, |c| usize::from(c.cols.max(1)));
        let rows = range.len().div_ceil(cols);
        let mut layouts = Vec::with_capacity(range.len());
        let mut shown_all = Vec::with_capacity(range.len());
        let any_rendered = (0..rows).any(|row| rendered[range.start + row * cols]);
        if !any_rendered {
            let offscreen = (0..rows)
                .map(|row| self.table_row_height(range.start + row * cols))
                .sum();
            layouts.resize(range.len(), None);
            shown_all.resize(range.len(), Shown::default());
            return TableFrame {
                element: None,
                layouts,
                shown: shown_all,
                offscreen,
            };
        }
        let widths = self.table_widths(&range, cols);
        let total: f32 = widths.iter().sum();
        let hairline = colors.primary.alpha(0.12);
        let header_fill = colors.primary.alpha(0.04);
        let ui = crate::fonts::ui_family();
        let mono = crate::fonts::mono_family();
        let selection = self.editor.selection;
        let first_id = self.editor.block(range.start).id;
        let scroll = self
            .table_scrolls
            .borrow_mut()
            .entry(first_id)
            .or_default()
            .clone();
        // Keep the caret's column in view when the caret moved.
        if autoscroll
            && let Some(table) = self.editor.caret_table()
            && table.range.start == range.start
        {
            let left: f32 = widths[..table.col].iter().sum();
            let right = left + widths[table.col];
            let viewport = f32::from(scroll.bounds().size.width);
            let offset = -f32::from(scroll.offset().x);
            if viewport > 0.0 {
                let target = if left < offset {
                    Some(left)
                } else if right > offset + viewport {
                    Some(right - viewport)
                } else {
                    None
                };
                if let Some(x) = target {
                    scroll.set_offset(point(px(-x), scroll.offset().y));
                }
            }
        }

        let heights = Rc::clone(&self.row_heights);
        let mut grid = div()
            .w(px(total))
            .flex_none()
            .flex()
            .flex_col()
            .rounded(px(8.0))
            .border_1()
            .border_color(hairline)
            .overflow_hidden();
        let mut spacer = 0.0;
        for row in 0..rows {
            let first = range.start + row * cols;
            if !rendered[first] {
                spacer += self.table_row_height(first);
                for _ in 0..cols {
                    layouts.push(None);
                    shown_all.push(Shown::default());
                }
                continue;
            }
            if spacer > 0.0 {
                grid = grid.child(div().flex_none().h(px(spacer)));
                spacer = 0.0;
            }
            let header = row == 0;
            let mut cells = div().flex().flex_row().w_full();
            if row + 1 < rows {
                cells = cells.border_b_1().border_color(hairline);
            }
            let mut column_left = 0.0;
            for (col, &width) in widths.iter().enumerate() {
                let alpha = column_alpha(&scroll, column_left, width);
                column_left += width;
                let index = first + col;
                let block = self.editor.block(index);
                let shown = Shown::of(block);
                let text: SharedString = if block.text.is_empty() {
                    "\u{200B}".into()
                } else {
                    shown.text.clone().into()
                };
                let mut styled = StyledText::new(text).with_highlights(if block.text.is_empty() {
                    Vec::new()
                } else {
                    highlights(block, &shown, colors, false)
                });
                let code: Vec<(Range<usize>, SharedString)> = block
                    .marks
                    .iter()
                    .filter(|m| m.style == Style::Code)
                    .map(|m| (shown.range(&m.range), SharedString::from(mono)))
                    .collect();
                if !code.is_empty() {
                    styled = styled.with_font_family_overrides(code);
                }
                layouts.push(Some(styled.layout().clone()));
                shown_all.push(shown);
                let align = block.kind.cell().map_or(Align::None, |c| c.align);
                let caret_here =
                    focused && selection.is_collapsed() && selection.head.block == index;
                let content = div()
                    .min_w_0()
                    .max_w_full()
                    .text_size(px(CELL_TEXT))
                    .line_height(px(CELL_LINE))
                    .font_weight(if header {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(colors.primary)
                    .font_family(ui)
                    .child(styled);
                // A right click is the ⋯ menu for this cell: caret where it
                // landed (or at the cell's end), menu under the pointer.
                let on_context = cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.focus(&this.focus, cx);
                    let at = this
                        .hit(event.position)
                        .filter(|pos| pos.block == index)
                        .unwrap_or_else(|| Pos::new(index, this.editor.block(index).text.len()));
                    this.editor.set_caret(at);
                    this.open_table_menu(Some(pointer_anchor(event.position)), cx);
                });
                let cell = div()
                    .relative()
                    .on_mouse_down(MouseButton::Right, on_context)
                    .opacity(alpha)
                    .w(px(width))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .px(px(CELL_PAD_X))
                    .py(px(CELL_PAD_Y))
                    .when(header, |cell| cell.bg(header_fill))
                    .when(col + 1 < cols, |cell| {
                        cell.border_r_1().border_color(hairline)
                    })
                    .map(|cell| match align {
                        Align::Center => cell.justify_center(),
                        Align::Right => cell.justify_end(),
                        _ => cell,
                    })
                    .child(content)
                    // The cell holding the caret wears a quiet accent ring.
                    .when(caret_here, |cell| {
                        cell.child(
                            div()
                                .absolute()
                                .inset_0()
                                .border_2()
                                .border_color(accent().alpha(0.45)),
                        )
                    });
                cells = cells.child(cell);
            }
            let id = self.editor.block(first).id;
            let chrome = if header { 2.0 * TABLE_PAD } else { 0.0 };
            let heights = Rc::clone(&heights);
            cells = cells.relative().child(
                canvas(
                    move |bounds, _, _| {
                        heights
                            .borrow_mut()
                            .insert(id, f32::from(bounds.size.height) + chrome);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            );
            grid = grid.child(cells);
        }
        if spacer > 0.0 {
            grid = grid.child(div().flex_none().h(px(spacer)));
        }
        let in_table = self
            .editor
            .caret_table()
            .is_some_and(|t| t.range.start == range.start);
        let group: SharedString = format!("note-table-{first_id}").into();
        let handle = div()
            .id(("table-handle", first_id))
            .absolute()
            .top(px(TABLE_PAD - 9.0))
            .right(px(-9.0))
            .size(px(20.0))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(colors.floating_fill())
            .border_1()
            .border_color(hairline)
            .cursor_pointer()
            .opacity(if in_table { 1.0 } else { 0.0 })
            .group_hover(group.clone(), |handle| handle.opacity(1.0))
            .child(crate::icons::sf_symbol("ellipsis", 11.0, colors.secondary))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    if this
                        .editor
                        .caret_table()
                        .is_none_or(|t| t.range.start != start)
                    {
                        let len = this.editor.block(start).text.len();
                        this.editor.set_caret(Pos::new(start, len));
                    }
                    this.open_table_menu(Some(pointer_anchor(event.position)), cx);
                }),
            );
        // The block hugs the grid (capped at the column), so the handle sits
        // on the table's corner and a wide table scrolls inside it.
        let element = div()
            .id(("block", first_id))
            .group(group)
            .relative()
            .w(px(total + 2.0))
            .max_w_full()
            .py(px(TABLE_PAD))
            .child(
                // Scrolled here rather than by `overflow_x_scroll`, which
                // also turns vertical wheel motion sideways and cannot tell
                // a swipe's axis: only a gesture locked sideways moves the
                // table, and it then stops short of the page.
                div()
                    .id(("table-scroll", first_id))
                    .w_full()
                    .overflow_x_hidden()
                    .track_scroll(&scroll)
                    .on_scroll_wheel({
                        let scroll = scroll.clone();
                        cx.listener(move |this, event: &ScrollWheelEvent, window, cx| {
                            if this.wheel_axis(event) != Some(gpui::Axis::Horizontal) {
                                return;
                            }
                            cx.stop_propagation();
                            let dx = event.delta.pixel_delta(window.line_height()).x;
                            let offset = scroll.offset();
                            let x = (offset.x + dx).clamp(-scroll.max_offset().x, px(0.0));
                            if x != offset.x {
                                scroll.set_offset(point(x, offset.y));
                                cx.notify();
                            }
                        })
                    })
                    .child(grid),
            )
            .child(handle)
            .child(scroll_fade(scroll.clone(), colors))
            .into_any_element();
        TableFrame {
            element: Some(element),
            layouts,
            shown: shown_all,
            offscreen: 0.0,
        }
    }

    /// Among the cells in the row at block `index`, the one under `x`: the
    /// last whose text starts left of the pointer. Rows lay cells side by
    /// side, so the block found by height alone is only the row's first.
    pub(super) fn cell_at_x(&self, index: usize, x: Pixels) -> usize {
        let Some(table) = self.editor.table_pos(index) else {
            return index;
        };
        let mut chosen = table.index(table.row, 0);
        for col in 0..table.cols {
            let candidate = table.index(table.row, col);
            let Some(layout) = self.layout(candidate) else {
                continue;
            };
            if layout.bounds().left() - px(CELL_PAD_X) <= x {
                chosen = candidate;
            }
        }
        chosen
    }

    /// ↑ / ↓ inside a table: the cell in the same column one row away, the
    /// caret at the same x; past the first or last row, the block before or
    /// after the table.
    pub(super) fn table_vertical(&mut self, down: bool, x: Pixels) -> Option<Option<Pos>> {
        let head = self.editor.selection.head;
        let table = self.editor.table_pos(head.block)?;
        let row = if down {
            (table.row + 1 < table.rows).then_some(table.row + 1)
        } else {
            table.row.checked_sub(1)
        };
        let Some(row) = row else {
            // Leave the table.
            let blocks = self.editor.blocks().len();
            let target = if down {
                (table.range.end < blocks).then_some(table.range.end)
            } else {
                table.range.start.checked_sub(1)
            };
            return Some(target.map(|index| {
                let len = self.text_len(index);
                let offset = self
                    .layout(index)
                    .and_then(|layout| {
                        let bounds = layout.bounds();
                        let y = if down {
                            bounds.top() + layout.line_height() * 0.5
                        } else {
                            bounds.bottom() - layout.line_height() * 0.5
                        };
                        self.hit(point(x, y)).map(|pos| pos.offset)
                    })
                    .unwrap_or(if down { 0 } else { len });
                Pos::new(index, offset.min(len))
            }));
        };
        let index = table.index(row, table.col);
        let len = self.text_len(index);
        let offset = self
            .layout(index)
            .and_then(|layout| {
                let bounds = layout.bounds();
                let y = if down {
                    bounds.top() + layout.line_height() * 0.5
                } else {
                    bounds.bottom() - layout.line_height() * 0.5
                };
                let clamped = point(x.clamp(bounds.left(), bounds.right()), y);
                layout
                    .index_for_position(clamped)
                    .map_or_else(Some, Some)
                    .map(|i| self.shown.get(index).map_or(i, |s| s.to_model(i)))
            })
            .unwrap_or(len);
        Some(Some(Pos::new(index, offset.min(len))))
    }

    // -----------------------------------------------------------------------
    // Menu

    pub(crate) fn open_table_menu(
        &mut self,
        anchor: Option<Bounds<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if self.editor.caret_table().is_none() {
            return;
        }
        self.slash = None;
        self.mention = None;
        self.block_menu = None;
        if anchor.is_none() {
            // The caret anchors the menu: bring its cell into view first.
            self.autoscroll = true;
        }
        self.table_menu = Some(TableMenu {
            selected: 0,
            anchor,
        });
        cx.notify();
    }

    /// The menu's anchor when a pointer opened it.
    pub(super) fn table_menu_anchor(&self) -> Option<Bounds<Pixels>> {
        self.table_menu.as_ref()?.anchor
    }

    /// The viewport a table cell's text is clipped to, while block `index`
    /// is a cell of a table that has been drawn.
    pub(super) fn cell_clip(&self, index: usize) -> Option<ScrollHandle> {
        if !self.editor.block(index).kind.is_cell() {
            return None;
        }
        let range = table_range(self.editor.blocks(), index)?;
        let first = self.editor.block(range.start).id;
        self.table_scrolls.borrow().get(&first).cloned()
    }

    fn table_menu_content(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rows = self.table_menu_rows(cx)?;
        Some(
            floating::surface(self.colors, floating::MENU_RADIUS, TABLE_MENU_WIDTH, rows)
                .into_any_element(),
        )
    }

    pub(super) fn table_menu_rows(&mut self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let selected = self.table_menu.as_ref()?.selected;
        let table = self.editor.caret_table()?;
        let colors = self.colors;
        let current = self
            .editor
            .block(table.index(0, table.col))
            .kind
            .cell()
            .map_or(Align::None, |c| c.align);
        let current = if current == Align::None {
            Align::Left
        } else {
            current
        };
        let mut list = div().flex().flex_col().py(px(floating::MENU_PADDING_Y));
        let mut group = None;
        for (i, (action, in_group)) in TABLE_ACTIONS.iter().enumerate() {
            if group.is_some_and(|g| g != *in_group) {
                list = list.child(floating::menu_separator(colors));
            }
            group = Some(*in_group);
            let (icon, label, keys) = action.row(current);
            let glyph = if icon.is_empty() {
                div().into_any_element()
            } else {
                crate::icons::sf_symbol(icon, MENU_ICON, colors.secondary)
            };
            let row = floating::menu_row(("note-table-row", i), glyph, colors, i == selected)
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    if *hovered
                        && let Some(menu) = &mut this.table_menu
                        && menu.selected != i
                    {
                        menu.selected = i;
                        cx.notify();
                    }
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.apply_table_row(i, cx);
                    }),
                )
                .child(menu_label(label, colors))
                .when_some(keys, |row, keys| {
                    row.child(floating::menu_shortcut(
                        crate::commands::keystroke_label(keys),
                        colors,
                    ))
                });
            list = list.child(row);
        }
        Some(list)
    }

    pub(super) fn table_menu_height(&self) -> f32 {
        menu_height(TABLE_ACTIONS.len(), 2)
    }

    pub(super) fn table_menu_len(&self) -> usize {
        TABLE_ACTIONS.len()
    }

    pub(super) fn apply_table_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some((action, _)) = TABLE_ACTIONS.get(index).copied() else {
            return;
        };
        self.table_menu = None;
        self.table_action(action, cx);
    }

    /// Runs one table operation as an undoable edit, counting rows and
    /// columns added (counts only).
    pub(super) fn table_action(&mut self, action: TableAction, cx: &mut Context<Self>) -> bool {
        let Some(before) = self.editor.caret_table() else {
            return false;
        };
        let now = now_ms();
        let done = match action {
            TableAction::RowAbove => self.editor.table_add_row(false, now),
            TableAction::RowBelow => self.editor.table_add_row(true, now),
            TableAction::ColLeft => self.editor.table_add_col(false, now),
            TableAction::ColRight => self.editor.table_add_col(true, now),
            TableAction::Align(align) => self.editor.table_set_align(align, now),
            TableAction::DeleteRow => self.editor.table_delete_row(now),
            TableAction::DeleteCol => self.editor.table_delete_col(now),
            TableAction::DeleteTable => self.editor.table_delete(now),
        };
        self.count_table_growth(&before);
        if done {
            self.edited(cx);
        } else {
            cx.notify();
        }
        done
    }

    /// `notes.table.row_added` / `col_added` when the caret's table grew.
    pub(super) fn count_table_growth(&self, before: &diri_notes::edit::TablePos) {
        if let Some(after) = self.editor.caret_table() {
            if after.rows > before.rows {
                crate::telemetry::notes_event("notes.table.row_added", "");
            }
            if after.cols > before.cols {
                crate::telemetry::notes_event("notes.table.col_added", "");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Keys

    pub(super) fn table_row_above(
        &mut self,
        _: &TableRowAbove,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.table_action(TableAction::RowAbove, cx) {
            cx.propagate();
        }
    }

    pub(super) fn table_row_below(
        &mut self,
        _: &TableRowBelow,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.table_action(TableAction::RowBelow, cx) {
            cx.propagate();
        }
    }

    pub(super) fn table_col_left(
        &mut self,
        _: &TableColLeft,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.table_action(TableAction::ColLeft, cx) {
            cx.propagate();
        }
    }

    pub(super) fn table_col_right(
        &mut self,
        _: &TableColRight,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.table_action(TableAction::ColRight, cx) {
            cx.propagate();
        }
    }

    pub(super) fn table_menu_key(
        &mut self,
        _: &TableMenuAction,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editor.caret_table().is_some() {
            self.open_table_menu(None, cx);
        } else {
            cx.propagate();
        }
    }

    /// Tab / ⇧Tab in a table, counting a row Tab added.
    pub(super) fn table_tab(&mut self, forward: bool, cx: &mut Context<Self>) -> bool {
        let Some(before) = self.editor.caret_table() else {
            return false;
        };
        let revision = self.editor.revision;
        self.editor.table_tab(forward, now_ms());
        self.count_table_growth(&before);
        if self.editor.revision != revision {
            self.edited(cx);
        } else {
            self.moved(cx);
        }
        true
    }
}
