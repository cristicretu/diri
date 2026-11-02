//! ⌘F in a note: a find bar pinned to the editor's top-right corner.
//!
//! The query field owns the keyboard while it is being typed in (the editor
//! switches to [`FIND_EDITOR_CONTEXT`] so none of its bindings match), and
//! typed text arrives through the editor's input handler, as in the ⌘K link
//! panel. Return and ⌘G step forward, ⇧Return and ⇧⌘G back, Escape closes.
//! Clicking into the note hands the keyboard back while the bar stays open
//! with its matches painted; ⌘F returns to the field.
//!
//! The current match is the editor's selection, so it is painted, scrolled
//! to and copied like any selection, and closing the bar leaves it selected.

use super::super::find::{self, Match};
use super::*;

/// The editor's context while the find field owns the keyboard.
pub(super) const FIND_EDITOR_CONTEXT: &str = "DiriNoteFind";
const FIND_BAR_WIDTH: f32 = 340.0;
const FIND_FIELD_HEIGHT: f32 = 30.0;
const FIND_BUTTON: f32 = 24.0;

pub(super) struct FindBar {
    pub(super) query: crate::query_editor::QueryEditor,
    /// Whether the field owns the keyboard.
    pub(super) typing: bool,
    pub(super) matches: Vec<Match>,
    pub(super) current: Option<usize>,
    /// Where the search starts from: the caret when the bar opened.
    origin: Pos,
    /// The editor revision `matches` were computed for.
    revision: u64,
}

impl NoteEditorView {
    /// ⌘F: opens the find bar, or returns the keyboard to it. A selection
    /// within one block becomes the query.
    pub(super) fn find(&mut self, _: &Find, _: &mut Window, cx: &mut Context<Self>) {
        let selection = self.editor.selection;
        let selected = (!selection.is_collapsed()
            && selection.start().block == selection.end().block)
            .then(|| {
                let block = self.editor.block(selection.start().block);
                block.text[selection.start().offset..selection.end().offset].to_owned()
            })
            .filter(|text| !text.trim().is_empty() && !text.contains('\n'));
        self.slash = None;
        self.mention = None;
        self.link_editor = None;
        self.table_menu = None;
        self.block_menu = None;
        let bar = self.find_bar.get_or_insert_with(|| FindBar {
            query: crate::query_editor::QueryEditor::default(),
            typing: true,
            matches: Vec::new(),
            current: None,
            origin: selection.start(),
            revision: u64::MAX,
        });
        bar.typing = true;
        if let Some(text) = selected {
            bar.query.clear();
            bar.query.insert(&text);
            bar.origin = selection.start();
            bar.revision = u64::MAX;
        }
        bar.query.select_all();
        self.refresh_find(cx);
        cx.notify();
    }

    pub(super) fn find_next(&mut self, _: &FindNext, _: &mut Window, cx: &mut Context<Self>) {
        self.find_step(true, cx);
    }

    pub(super) fn find_previous(
        &mut self,
        _: &FindPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.find_step(false, cx);
    }

    /// Closes the bar, leaving the current match selected.
    pub(super) fn close_find(&mut self, cx: &mut Context<Self>) {
        if self.find_bar.take().is_some() {
            cx.notify();
        }
    }

    /// Recomputes the matches after the query or the note changed. A new
    /// query jumps to its first match from where the search started.
    fn refresh_find(&mut self, cx: &mut Context<Self>) {
        let Some(bar) = self.find_bar.as_mut() else {
            return;
        };
        if bar.revision == self.editor.revision {
            return;
        }
        bar.revision = self.editor.revision;
        bar.matches = find::find_all(self.editor.blocks(), bar.query.text());
        bar.current = find::first_from(&bar.matches, bar.origin.block, bar.origin.offset);
        if bar.current.is_some() {
            self.select_match(cx);
        }
    }

    /// Keeps the matches in step with edits made while the bar is open,
    /// without moving the caret the edit left.
    pub(super) fn sync_find(&mut self) {
        let Some(bar) = self.find_bar.as_mut() else {
            return;
        };
        if bar.revision == self.editor.revision {
            return;
        }
        bar.revision = self.editor.revision;
        bar.matches = find::find_all(self.editor.blocks(), bar.query.text());
        let head = self.editor.selection.start();
        bar.current = bar
            .matches
            .iter()
            .position(|m| m.block == head.block && m.range.start == head.offset);
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(bar) = self.find_bar.as_mut() {
            bar.revision = u64::MAX;
        }
        self.refresh_find(cx);
        cx.notify();
    }

    fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.sync_find();
        let Some(bar) = self.find_bar.as_mut() else {
            return;
        };
        let next = match bar.current {
            Some(_) => find::step(bar.current, bar.matches.len(), forward),
            // After an edit the caret is off every match: go to the nearest
            // one past it (or before it, going back).
            None => {
                let head = self.editor.selection.head;
                let after = find::first_from(&bar.matches, head.block, head.offset);
                if forward {
                    after
                } else {
                    after.and_then(|at| find::step(Some(at), bar.matches.len(), false))
                }
            }
        };
        bar.current = next;
        if next.is_some() {
            self.select_match(cx);
        }
        cx.notify();
    }

    /// Selects the current match, unfolding any list item that hides it.
    fn select_match(&mut self, cx: &mut Context<Self>) {
        let Some(found) = self
            .find_bar
            .as_ref()
            .and_then(|bar| bar.matches.get(bar.current?))
            .cloned()
        else {
            return;
        };
        let hidden = self.editor.hidden();
        if hidden.get(found.block).copied().unwrap_or(false) {
            for index in (0..found.block).rev() {
                if self.editor.is_collapsed(index) && self.editor.children(index).contains(&found.block)
                {
                    self.editor.set_collapsed(index, false);
                }
            }
        }
        self.editor.set_selection(Selection {
            anchor: Pos::new(found.block, found.range.start),
            head: Pos::new(found.block, found.range.end),
        });
        self.goal_x = None;
        self.marked = None;
        self.slash = None;
        self.mention = None;
        self.touched(cx);
    }

    /// Keys while the find field owns the keyboard. Typed text arrives
    /// through the input handler.
    pub(super) fn find_key_down(
        &mut self,
        event: &KeyDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::query_editor::{self, ClipboardEdit, Edit, LocalEdit};
        if self.link_editor.is_some() || !self.find_bar.as_ref().is_some_and(|bar| bar.typing) {
            return;
        }
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        match keystroke.key.as_str() {
            "escape" => self.close_find(cx),
            "enter" => self.find_step(!modifiers.shift, cx),
            "g" if modifiers.platform => self.find_step(!modifiers.shift, cx),
            "f" if modifiers.platform => {
                if let Some(bar) = self.find_bar.as_mut() {
                    bar.query.select_all();
                }
            }
            _ => {
                let bar = self.find_bar.as_mut().expect("checked above");
                let changed = match query_editor::edit_for(keystroke) {
                    // Text goes through the input handler, like every field.
                    None | Some(Edit::Local(LocalEdit::Insert(_))) => return,
                    Some(Edit::Local(local)) => bar.query.apply(local),
                    Some(Edit::Clipboard(ClipboardEdit::Copy)) => {
                        query_editor::copy_selection(&bar.query, cx);
                        false
                    }
                    Some(Edit::Clipboard(ClipboardEdit::Cut)) => {
                        query_editor::cut_selection(&mut bar.query, cx)
                    }
                    Some(Edit::Clipboard(ClipboardEdit::Paste)) => cx
                        .read_from_clipboard()
                        .and_then(|item| item.text())
                        .is_some_and(|text| {
                            bar.query
                                .insert(text.trim().lines().next().unwrap_or_default())
                        }),
                };
                if changed {
                    self.query_changed(cx);
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// Text typed while the find field owns the keyboard. Returns whether
    /// the field took it.
    pub(super) fn find_insert(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        let Some(bar) = self.find_bar.as_mut().filter(|bar| bar.typing) else {
            return false;
        };
        if bar.query.insert(text) {
            self.query_changed(cx);
        }
        true
    }

    pub(super) fn find_typing(&self) -> bool {
        self.find_bar.as_ref().is_some_and(|bar| bar.typing)
    }

    /// A click in the note hands the keyboard back; the bar stays open.
    pub(super) fn release_find_keyboard(&mut self) {
        if let Some(bar) = self.find_bar.as_mut() {
            bar.typing = false;
        }
    }

    /// Every match a painted block holds, as (block, display range).
    pub(super) fn find_highlights(&self) -> Vec<(usize, Range<usize>)> {
        let Some(bar) = &self.find_bar else {
            return Vec::new();
        };
        bar.matches
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != bar.current)
            .map(|(_, m)| (m.block, m.range.clone()))
            .collect()
    }

    /// Opens the bar on `query` with the field owning the keyboard, as ⌘F
    /// and typing would.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn open_find_for_screenshot(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        self.find(&Find, window, cx);
        self.find_insert(query, cx);
    }

    #[cfg(test)]
    pub(crate) fn find_state_for_test(&self) -> Option<(String, usize, Option<usize>, bool)> {
        self.find_bar.as_ref().map(|bar| {
            (
                bar.query.text().to_owned(),
                bar.matches.len(),
                bar.current,
                bar.typing,
            )
        })
    }

    /// The bar: a field, "2 of 5", previous, next and close.
    pub(super) fn find_bar_view(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.sync_find();
        let bar = self.find_bar.as_ref()?;
        let colors = self.colors;
        let typing = bar.typing;
        let found = !bar.matches.is_empty();
        let (count, missing) = match bar.current {
            _ if bar.query.is_empty() => (String::new(), false),
            _ if !found => (crate::i18n::t("notes.find.no_results").to_owned(), true),
            Some(current) => (
                crate::i18n::tf(
                    "notes.find.count",
                    &[
                        ("current", &(current + 1).to_string()),
                        ("total", &total_label(bar.matches.len())),
                    ],
                ),
                false,
            ),
            None => (
                crate::i18n::tf(
                    "notes.find.total",
                    &[("total", &total_label(bar.matches.len()))],
                ),
                false,
            ),
        };
        let field_text = if bar.query.is_empty() {
            div()
                .text_color(colors.tertiary)
                .child(if typing {
                    format!(
                        "{}{}",
                        crate::navigation::CARET,
                        crate::i18n::t("notes.find.placeholder")
                    )
                } else {
                    crate::i18n::t("notes.find.placeholder").to_owned()
                })
                .into_any_element()
        } else if typing {
            crate::navigation::query_label(&bar.query)
        } else {
            div().child(bar.query.text().to_owned()).into_any_element()
        };
        let field = div()
            .id("note-find-field")
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(px(7.0))
            .h(px(FIND_FIELD_HEIGHT))
            .px(px(9.0))
            .rounded(px(floating::MENU_ROW_RADIUS))
            .bg(colors.primary.alpha(0.06))
            .border_1()
            .border_color(if typing {
                accent().alpha(0.55)
            } else {
                colors.primary.alpha(0.0)
            })
            .cursor_text()
            .child(crate::icons::sf_symbol(
                "magnifyingglass",
                MENU_ICON,
                colors.tertiary,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(field_text),
            )
            .child(
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .text_size(px(Typo::META.size))
                    .text_color(if missing {
                        Ink::on_surface(Ink::DANGER, colors)
                    } else {
                        colors.tertiary
                    })
                    .child(count),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.focus(&this.focus, cx);
                    if let Some(bar) = this.find_bar.as_mut() {
                        bar.typing = true;
                    }
                    cx.notify();
                }),
            );
        let button = |id: &'static str,
                      symbol: &'static str,
                      enabled: bool,
                      cx: &mut Context<Self>,
                      action: fn(&mut NoteEditorView, &mut Context<NoteEditorView>)| {
            div()
                .id(id)
                .flex_none()
                .size(px(FIND_BUTTON))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(8.0))
                .when(enabled, |button| {
                    button
                        .cursor_pointer()
                        .hover(move |button| button.bg(colors.primary.alpha(0.08)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                action(this, cx);
                            }),
                        )
                })
                .child(crate::icons::sf_symbol(
                    symbol,
                    11.0,
                    if enabled {
                        colors.secondary
                    } else {
                        colors.tertiary
                    },
                ))
        };
        let row = div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .p(px(5.0))
            .child(field)
            .child(button("note-find-previous", "chevron.up", found, cx, |this, cx| {
                this.find_step(false, cx)
            }))
            .child(button("note-find-next", "chevron.down", found, cx, |this, cx| {
                this.find_step(true, cx)
            }))
            .child(button("note-find-close", "xmark", true, cx, |this, cx| {
                this.close_find(cx)
            }));
        Some(
            div()
                .absolute()
                .top(px(10.0))
                .right(px(12.0))
                .occlude()
                .debug_selector(|| "note-find-bar".into())
                .child(floating::surface(
                    colors,
                    floating::MENU_RADIUS,
                    FIND_BAR_WIDTH,
                    row,
                ))
                .into_any_element(),
        )
    }
}

fn total_label(total: usize) -> String {
    if total >= find::MATCH_LIMIT {
        format!("{total}+")
    } else {
        total.to_string()
    }
}
