//! Insert Path (⌘E): a file picker anchored at the terminal cursor.
//!
//! The picker is terminal chrome, so it works identically whatever runs in
//! the session — fish, zsh, bash, nvim, an agent. It roots itself at the
//! session child's live working directory (`session.process_info`), falling
//! back to the directory the session was started in, and delivers the chosen
//! path as one paste. Remote sessions are refused up front: their files are
//! not on this Mac.
use super::*;
use crate::path_picker::{self, PathPicker, PickerIndex};
use diri_ui::GlassMenuRow;
use gpui::{AvailableSpace, Bounds, Pixels, Point, Size, canvas, point, size};
use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

const WIDTH: f32 = 380.0;
const ROW_HEIGHT: f32 = 26.0;
const VISIBLE_ROWS: usize = 10;
/// Characters of path a row shows before eliding its middle.
const ROW_CHARS: usize = 46;
const EDGE: f32 = 6.0;
/// The live cwd is a nicety; a slow Engine must not hold the picker open empty.
const CWD_TIMEOUT: Duration = Duration::from_millis(750);

pub(super) struct PathPickerState {
    session: SessionId,
    picker: PathPicker,
    /// Whether the last frame opened the picker above the cursor. It is then
    /// drawn upside down — field nearest the cursor, best match just above
    /// it — so ↑ walks away from the cursor in both orientations.
    above: Rc<Cell<bool>>,
}

impl TerminalPane {
    pub(super) fn open_path_picker(
        &mut self,
        _: &crate::commands::InsertPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.selected_id() else {
            return;
        };
        if self.qol.copy_mode.is_some() {
            self.show_terminal_feedback("Exit copy mode before inserting a path", window, cx);
            cx.stop_propagation();
            return;
        }
        let Some(session) = self.session_record(&id) else {
            return;
        };
        if session.host.is_some() {
            self.show_terminal_feedback(
                format!(
                    "Insert Path browses {}, so it's unavailable in remote sessions",
                    crate::platform::local_machine_label_lowercase()
                ),
                window,
                cx,
            );
            cx.stop_propagation();
            return;
        }
        if self
            .path_picker
            .as_ref()
            .is_some_and(|state| state.session == id)
        {
            // ⌘E again closes, like a toggle.
            self.close_path_picker();
            cx.stop_propagation();
            cx.notify();
            return;
        }
        self.close_path_picker();
        if self.close_find_for_selected() {
            find_input::discard_native(window, cx);
        }
        let Some(resident) = self.residents.get(&id) else {
            return;
        };
        resident.element.set_text_input_enabled(false);

        self.path_picker_generation += 1;
        let generation = self.path_picker_generation;
        self.path_picker = Some(PathPickerState {
            session: id.clone(),
            picker: PathPicker::new(generation),
            above: Rc::default(),
        });

        let client = Arc::clone(self.runtime.client());
        let fallback = PathBuf::from(&session.cwd);
        let job = self.tokio.spawn(async move {
            let live = tokio::time::timeout(CWD_TIMEOUT, client.process_info(&id))
                .await
                .ok()
                .and_then(Result::ok)
                .and_then(|info| match info.process.working_directory {
                    diri_proto::process_facts::ProcessValue::Available { value } => {
                        Some(PathBuf::from(value))
                    }
                    diri_proto::process_facts::ProcessValue::Unavailable { .. } => None,
                });
            let root = live.filter(|path| path.is_dir()).unwrap_or(fallback);
            tokio::task::spawn_blocking(move || {
                if root.is_dir() {
                    Ok(path_picker::scan(&root))
                } else {
                    Err(format!(
                        "{} is not a folder on {}",
                        root.display(),
                        crate::platform::local_machine_label_lowercase()
                    ))
                }
            })
            .await
            .unwrap_or_else(|error| Err(format!("scan failed: {error}")))
        });
        cx.spawn(async move |this, cx| {
            let result = job
                .await
                .unwrap_or_else(|error| Err(format!("scan failed: {error}")));
            let _ = this.update(cx, |this, cx| {
                let Some(state) = this.path_picker.as_mut() else {
                    return;
                };
                if state.picker.generation != generation {
                    return;
                }
                match result {
                    Ok(index) => state.picker.adopt_index(index),
                    Err(message) => state.picker.fail(message),
                }
                cx.notify();
            });
        })
        .detach();

        window.focus(&self.focus, cx);
        cx.stop_propagation();
        cx.notify();
    }

    #[cfg(test)]
    pub(super) fn path_picker_adopt_for_test(&mut self, index: path_picker::PathIndex) {
        if let Some(state) = self.path_picker.as_mut() {
            state.picker.adopt_index(index);
        }
    }

    // Only the macOS screenshot fixture drives the query.
    #[cfg(all(test, target_os = "macos"))]
    pub(super) fn path_picker_query_for_test(&mut self, query: &str) {
        if let Some(state) = self.path_picker.as_mut() {
            state.picker.query.insert(query);
            state.picker.refresh();
        }
    }

    fn session_record(&self, id: &SessionId) -> Option<SessionRecord> {
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        store.sessions().get(id).map(|session| (**session).clone())
    }

    /// Close the picker, if any, and hand keystrokes back to its terminal.
    /// Returns whether one was open.
    pub(super) fn close_path_picker(&mut self) -> bool {
        let Some(state) = self.path_picker.take() else {
            return false;
        };
        if let Some(resident) = self.residents.get(&state.session)
            && resident.find.is_none()
        {
            resident.element.set_text_input_enabled(true);
        }
        true
    }

    /// Route one key to an open picker. Returns false when no picker owns the
    /// selected session's keyboard, so the caller carries on as usual.
    pub(super) fn path_picker_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let selected = self.selected_id();
        let Some(state) = self.path_picker.as_mut() else {
            return false;
        };
        if selected.as_ref() != Some(&state.session) {
            // The user moved to another session; the picker belongs to the old one.
            self.close_path_picker();
            cx.notify();
            return false;
        }
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let away = if state.above.get() { 1 } else { -1 };
        let picker = &mut state.picker;
        match keystroke.key.as_str() {
            "escape" => {
                self.close_path_picker();
            }
            "enter" => self.insert_picked_path(window, cx),
            "up" => picker.move_selection(away),
            "down" => picker.move_selection(-away),
            "p" if modifiers.control => picker.move_selection(-1),
            "n" if modifiers.control => picker.move_selection(1),
            "tab" if !modifiers.shift => {
                picker.descend();
            }
            _ => {
                let Some(edit) = query_editor::edit_for(keystroke) else {
                    cx.propagate();
                    return true;
                };
                match edit {
                    Edit::Local(local) => {
                        if picker.query.apply(local) {
                            picker.refresh();
                        }
                    }
                    Edit::Clipboard(ClipboardEdit::Copy) => {
                        query_editor::copy_selection(&picker.query, cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Cut) => {
                        if query_editor::cut_selection(&mut picker.query, cx) {
                            picker.refresh();
                        }
                    }
                    Edit::Clipboard(ClipboardEdit::Paste) => {
                        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
                            && picker.query.insert(text.lines().next().unwrap_or_default())
                        {
                            picker.refresh();
                        }
                    }
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
        true
    }

    fn insert_picked_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.path_picker.as_ref() else {
            return;
        };
        let Some(row) = state.picker.selected_row() else {
            return;
        };
        let text = path_picker::insertion_text(row);
        let session = state.session.clone();
        self.close_path_picker();
        self.paste_into_session(&session, &text);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    pub(super) fn render_path_picker(
        &self,
        session: &SessionRecord,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self
            .path_picker
            .as_ref()
            .filter(|state| state.session == session.id)?;
        let resident = self.residents.get(&session.id)?;
        let picker = &state.picker;

        let placeholder = picker.root().map_or_else(
            || "Finding files…".to_owned(),
            |root| {
                let home = diri_platform::home_dir()
                    .map(|p| p.into_os_string())
                    .map(PathBuf::from)
                    .unwrap_or_default();
                crate::quick_open::collapse_home(root, &home)
            },
        );
        let first = picker
            .selected
            .saturating_sub(VISIBLE_ROWS - 1)
            .min(picker.rows.len().saturating_sub(VISIBLE_ROWS));
        let mut build = |upside_down: bool| {
            let field = div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(7.0))
                .h(px(30.0))
                .px(px(9.0))
                .m(px(4.0))
                .when(upside_down, |field| field.mt(px(0.0)))
                .when(!upside_down, |field| field.mb(px(0.0)))
                .rounded(px(Radius::inner(Radius::PANEL, 4.0)))
                .bg(colors.primary.alpha(0.06))
                .text_size(px(Typo::ROW.size))
                .text_color(colors.primary)
                .child(sf_symbol("magnifyingglass", 12.0, colors.tertiary))
                .child(div().flex_1().min_w(px(0.0)).overflow_hidden().child(
                    if picker.query.is_empty() {
                        div()
                            .text_color(colors.tertiary)
                            .child(format!("{}{placeholder}", crate::navigation::CARET))
                            .into_any_element()
                    } else {
                        crate::navigation::query_label(&picker.query)
                    },
                ));
            let body: AnyElement = match &picker.index {
                PickerIndex::Loading => status_line("Finding files…", colors),
                PickerIndex::Failed(message) => status_line(message, colors),
                PickerIndex::Ready(_) if picker.rows.is_empty() => {
                    status_line("No matching files", colors)
                }
                PickerIndex::Ready(_) => {
                    let mut rows: Vec<AnyElement> = picker
                        .rows
                        .iter()
                        .enumerate()
                        .skip(first)
                        .take(VISIBLE_ROWS)
                        .map(|(index, row)| {
                            picker_row(row, index, index == picker.selected, colors, cx)
                        })
                        .collect();
                    if upside_down {
                        rows.reverse();
                    }
                    div()
                        .flex()
                        .flex_col()
                        .p(px(4.0))
                        .children(rows)
                        .into_any_element()
                }
            };
            let column = if upside_down {
                div().flex().flex_col().child(body).child(field)
            } else {
                div().flex().flex_col().child(field).child(body)
            };
            div()
                .id("path-picker")
                .debug_selector(|| "path-picker".into())
                .w_full()
                .child(FloatingSurface::new(colors, column))
                .into_any_element()
        };
        let below = build(false);
        let above = build(true);
        Some(render_at_cursor(
            resident.element.clone(),
            below,
            above,
            Rc::clone(&state.above),
        ))
    }
}

fn status_line(text: &str, colors: SemanticColors) -> AnyElement {
    div()
        .px(px(14.0))
        .py(px(9.0))
        .text_size(px(Typo::META.size))
        .text_color(colors.tertiary)
        .child(text.to_owned())
        .into_any_element()
}

fn picker_row(
    row: &path_picker::PickerRow,
    index: usize,
    selected: bool,
    colors: SemanticColors,
    cx: &mut Context<TerminalPane>,
) -> AnyElement {
    let (label, highlights) = path_picker::elide_middle(&row.relative, &row.highlights, ROW_CHARS);
    let label = if row.is_dir {
        format!("{label}/")
    } else {
        label
    };
    div()
        .id(("path-picker-row", index))
        .debug_selector(move || format!("path-picker-row-{index}"))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(7.0))
        .h(px(ROW_HEIGHT))
        .px(px(8.0))
        .rounded(px(Radius::inner(Radius::PANEL, 4.0)))
        .glass_menu_row(colors, selected)
        .cursor_pointer()
        .text_size(px(Typo::ROW.size))
        .text_color(colors.primary)
        .child(sf_symbol(
            if row.is_dir { "folder" } else { "doc" },
            12.0,
            if row.is_dir {
                colors.secondary
            } else {
                colors.tertiary
            },
        ))
        .child(
            div()
                .min_w(px(0.0))
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(crate::navigation::highlighted_label(label, &highlights)),
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            if let Some(state) = this.path_picker.as_mut() {
                state.picker.selected = index;
            }
            this.insert_picked_path(window, cx);
        }))
        .into_any_element()
}

/// Measure the popover, then place it at the cursor cell the terminal just
/// prepainted: below the input line when it fits, above it otherwise (an
/// agent's prompt sits at the bottom of the screen), clamped inside the pane.
/// Both orientations have the same size, so one measurement picks the side.
fn render_at_cursor(
    terminal: TerminalElement,
    below: AnyElement,
    above: AnyElement,
    opened_above: Rc<Cell<bool>>,
) -> AnyElement {
    canvas(
        move |bounds, window, cx| {
            let width = px(WIDTH).min((bounds.size.width - px(EDGE * 2.0)).max(px(0.0)));
            let available = size(AvailableSpace::Definite(width), AvailableSpace::MinContent);
            let mut popover = div().w(width).child(below).into_any_element();
            let measured = popover.layout_as_root(available, window, cx);
            let (origin, is_above) = placement(bounds, measured, terminal.input_cell_bounds());
            if is_above {
                popover = div().w(width).child(above).into_any_element();
                popover.layout_as_root(available, window, cx);
            }
            opened_above.set(is_above);
            popover.prepaint_at(origin, window, cx);
            popover
        },
        |_, mut popover, window, cx| popover.paint(window, cx),
    )
    .absolute()
    .inset_0()
    .into_any_element()
}

fn placement(
    viewport: Bounds<Pixels>,
    popover: Size<Pixels>,
    cell: Option<Bounds<Pixels>>,
) -> (Point<Pixels>, bool) {
    let min_x = viewport.left() + px(EDGE);
    let max_x = (viewport.right() - px(EDGE) - popover.width).max(min_x);
    let min_y = viewport.top() + px(EDGE);
    let max_y = (viewport.bottom() - px(EDGE) - popover.height).max(min_y);
    let Some(cell) = cell else {
        return (point(min_x, min_y), false);
    };
    let x = cell.left().clamp(min_x, max_x);
    let below = cell.bottom() + px(2.0);
    let above = cell.top() - px(2.0) - popover.height;
    let opens_above = if below + popover.height <= viewport.bottom() - px(EDGE) {
        false
    } else if above >= min_y {
        true
    } else {
        // Neither side fits whole: take the roomier one and clamp.
        viewport.bottom() - cell.bottom() < cell.top() - viewport.top()
    };
    let y = if opens_above { above } else { below };
    (point(x, y.clamp(min_y, max_y)), opens_above)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
    }

    #[test]
    fn opens_below_the_cursor_when_there_is_room() {
        let viewport = rect(0.0, 0.0, 900.0, 600.0);
        let placed = placement(
            viewport,
            size(px(380.0), px(300.0)),
            Some(rect(120.0, 40.0, 8.0, 17.0)),
        );
        assert_eq!(placed, (point(px(120.0), px(59.0)), false));
    }

    #[test]
    fn opens_above_an_input_line_at_the_bottom_of_the_screen() {
        let viewport = rect(0.0, 0.0, 900.0, 600.0);
        let popover = size(px(380.0), px(300.0));
        let cell = rect(40.0, 560.0, 8.0, 17.0);
        let (origin, above) = placement(viewport, popover, Some(cell));
        assert!(above);
        assert_eq!(origin.y, px(560.0 - 2.0 - 300.0));
        assert!(Bounds::new(origin, popover).bottom() <= cell.top());
    }

    #[test]
    fn stays_inside_the_pane_near_the_right_edge_and_without_a_cursor() {
        let viewport = rect(10.0, 20.0, 500.0, 400.0);
        let popover = size(px(380.0), px(200.0));
        let (origin, _) = placement(viewport, popover, Some(rect(480.0, 30.0, 8.0, 17.0)));
        assert!(Bounds::new(origin, popover).right() <= viewport.right());
        assert_eq!(
            placement(viewport, popover, None),
            (point(px(16.0), px(26.0)), false)
        );
    }
}
