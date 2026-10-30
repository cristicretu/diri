// Adapted from Ely GPUI Components (MIT OR Apache-2.0), https://github.com/ZacharyZhang-NY/Ely-GPUI-Components
//! The block handle: hovering a block shows a `+` and a ⋮⋮ grip in the
//! gutter to its left, as in Ely's `BlockEditor` and Notion. `+` adds a
//! block below and opens the `/` menu in it. Dragging the grip moves the
//! block (a list item with the items under it, a table whole) to where it
//! is dropped. Clicking the grip opens the block menu: turn it into another
//! kind, add a block below, duplicate, move, or delete it.
//!
//! The view only draws and routes the pointer; every change is a
//! `diri_notes::edit::Editor` operation, one undo step each.

use super::*;
use crate::palette_chrome::PaletteTooltip;
use crate::tooltip_warmth::WarmTooltip as _;

/// Width of the gutter the handle occupies, left of a block's text (and of
/// a list item's fold chevron).
pub(super) const HANDLE_WIDTH: f32 = 34.0;
const BUTTON: f32 = 16.0;
const BLOCK_MENU_WIDTH: f32 = 272.0;
/// Kind buttons in the menu's "Turn into" strip.
const TURN_BUTTON: f32 = 22.0;

/// The menu opened from a block's grip.
pub(super) struct BlockMenu {
    pub(super) block_id: u64,
    pub(super) selected: Option<usize>,
    /// The grip's place when the menu opened.
    anchor: Option<Bounds<Pixels>>,
}

/// What dragging a grip carries: the block, and a line of its text for the
/// ghost under the pointer.
#[derive(Clone)]
pub(super) struct BlockDrag {
    pub(super) block_id: u64,
    preview: SharedString,
    colors: SemanticColors,
}

impl Render for BlockDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors;
        div()
            .max_w(px(320.0))
            .px(px(10.0))
            .py(px(5.0))
            .rounded(px(7.0))
            .bg(colors.floating_fill())
            .border_1()
            .border_color(colors.floating_stroke())
            .shadow_md()
            .opacity(0.92)
            .text_size(px(Typo::ROW.size))
            .text_color(colors.primary)
            .whitespace_nowrap()
            .overflow_hidden()
            .text_ellipsis()
            .child(self.preview.clone())
    }
}

/// The "Turn into" strip: the kinds a text block can become, as the `/`
/// menu offers them.
const TURN_KINDS: &[(&str, &str, BlockKind)] = &[
    ("Text", "textformat", BlockKind::Paragraph),
    ("Heading 1", "textformat.h1", BlockKind::Heading(1)),
    ("Heading 2", "textformat.h2", BlockKind::Heading(2)),
    ("Heading 3", "textformat.h3", BlockKind::Heading(3)),
    (
        "To-do",
        "checkmark.square",
        BlockKind::Todo { checked: false },
    ),
    ("Bulleted list", "list.bullet", BlockKind::Bullet),
    ("Numbered list", "list.number", BlockKind::Numbered),
    ("Quote", "text.quote", BlockKind::Quote),
    (
        "Code",
        "chevron.left.forwardslash.chevron.right",
        BlockKind::Code,
    ),
    ("Callout", "info.circle", BlockKind::Callout(Tone::Note)),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BlockAction {
    AddBelow,
    Duplicate,
    MoveUp,
    MoveDown,
    Delete,
}

/// The menu's rows below the strip: (label, glyph, action, group).
const BLOCK_ACTIONS: &[(&str, &str, BlockAction, u8)] = &[
    ("Add block below", "plus", BlockAction::AddBelow, 0),
    ("Duplicate", "rectangle.stack", BlockAction::Duplicate, 0),
    ("Move up", "arrow.up", BlockAction::MoveUp, 1),
    ("Move down", "arrow.down", BlockAction::MoveDown, 1),
    ("Delete", "trash", BlockAction::Delete, 2),
];

pub(super) const BLOCK_MENU: floating::Target<NoteEditorView> = floating::Target {
    key: "note-block-menu",
    radius: floating::MENU_RADIUS,
    content: NoteEditorView::block_menu_content,
    dismiss: |this, _, cx| {
        this.block_menu = None;
        cx.notify();
    },
};

/// Six dots in two columns: the grip.
fn grip(color: gpui::Rgba) -> gpui::Div {
    let dot = || div().size(px(2.5)).rounded(px(1.25)).bg(color);
    let column = || {
        div()
            .flex()
            .flex_col()
            .gap(px(2.5))
            .child(dot())
            .child(dot())
            .child(dot())
    };
    div().flex().gap(px(2.5)).child(column()).child(column())
}

impl NoteEditorView {
    fn index_of_id(&self, id: u64) -> Option<usize> {
        self.editor.blocks().iter().position(|b| b.id == id)
    }

    /// The `+` and grip shown left of block `index` while its row is
    /// hovered (or its menu is open). `left` is the row-relative x of the
    /// gutter's right edge.
    pub(super) fn block_handle(
        &self,
        index: usize,
        left: f32,
        top: f32,
        line: f32,
        group: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let block = self.editor.block(index);
        let id = block.id;
        let colors = self.colors;
        let open = self.block_menu.as_ref().is_some_and(|m| m.block_id == id);
        let preview: SharedString = {
            let text = block.text.lines().next().unwrap_or_default().trim();
            let text = if text.is_empty() {
                match block.kind {
                    BlockKind::Divider => crate::i18n::t("notes.block.divider"),
                    BlockKind::Image => crate::i18n::t("notes.block.image"),
                    _ => crate::i18n::t("notes.block.empty"),
                }
            } else {
                text
            };
            text.chars().take(80).collect::<String>().into()
        };
        let button = |id: (&'static str, u64)| {
            div()
                .id(id)
                .size(px(BUTTON))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .cursor_pointer()
                .hover(|b| b.bg(colors.primary.alpha(0.07)))
        };
        let plus = button(("block-plus", id))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.focus(&this.focus, cx);
                    this.add_block_below(id, cx);
                }),
            )
            .child(crate::icons::sf_symbol("plus", 11.0, colors.tertiary));
        let drag = BlockDrag {
            block_id: id,
            preview,
            colors,
        };
        let entity = cx.entity().downgrade();
        let handle = button(("block-grip", id))
            .cursor_grab()
            .when(open, |b| b.bg(colors.primary.alpha(0.07)))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _: &gpui::ClickEvent, window, cx| {
                window.focus(&this.focus, cx);
                this.open_block_menu(id, cx);
            }))
            .on_drag(drag, move |drag, _, _, cx| {
                let _ = entity.update(cx, |this, cx| {
                    this.dragging = Some(drag.block_id);
                    this.block_menu = None;
                    cx.notify();
                });
                cx.new(|_| drag.clone())
            })
            .child(grip(colors.tertiary));
        div()
            .absolute()
            .left(px(left - HANDLE_WIDTH))
            .top(px(top))
            .w(px(HANDLE_WIDTH))
            .h(px(line))
            .flex()
            .items_center()
            .justify_end()
            .gap(px(1.0))
            .pr(px(2.0))
            .opacity(if open { 1.0 } else { 0.0 })
            .group_hover(group, |gutter| gutter.opacity(1.0))
            // The gutter sits outside its row: on the way to the grip the
            // pointer leaves the row, and the handle must stay.
            .hover(|gutter| gutter.opacity(1.0))
            .debug_selector(move || format!("note-handle-{id}"))
            .child(plus)
            .child(handle)
            .into_any_element()
    }

    /// Where the dragged block sits and the blocks it carries, looked up
    /// once a frame for every row's [`Self::drop_line`].
    pub(super) fn dragged(&self) -> Option<(usize, std::ops::Range<usize>)> {
        let from = self.index_of_id(self.dragging?)?;
        Some((from, self.editor.block_span(from)))
    }

    /// The line a drop would put the dragged block at: above this row when
    /// the block comes from below, under it when it comes from above.
    pub(super) fn drop_line(
        dragged: Option<&(usize, std::ops::Range<usize>)>,
        index: usize,
        group: SharedString,
    ) -> Option<AnyElement> {
        let (from, span) = dragged?;
        let from = *from;
        if from == index || span.contains(&index) {
            return None;
        }
        let line = div()
            .absolute()
            .left_0()
            .right_0()
            .h(px(2.0))
            .rounded(px(1.0))
            .bg(accent())
            .opacity(0.0)
            .group_drag_over::<BlockDrag>(group, |line| line.opacity(1.0));
        let line = if index < from {
            line.top(px(-1.0))
        } else {
            line.bottom(px(-1.0))
        };
        Some(line.into_any_element())
    }

    /// A grip dropped on block `target_id`.
    pub(super) fn drop_block(&mut self, from_id: u64, target_id: u64, cx: &mut Context<Self>) {
        self.dragging = None;
        let (Some(from), Some(to)) = (self.index_of_id(from_id), self.index_of_id(target_id))
        else {
            cx.notify();
            return;
        };
        if self.editor.move_block_to(from, to, now_ms()).is_some() {
            crate::telemetry::notes_event("notes.block.moved", "drag");
            self.edited(cx);
        } else {
            cx.notify();
        }
    }

    fn add_block_below(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(index) = self.index_of_id(id) else {
            return;
        };
        self.block_menu = None;
        if let Some(at) = self.editor.insert_block_below(index, now_ms()) {
            // Straight into the `/` menu, as Notion's `+` does.
            self.editor.insert_text("/", now_ms());
            let block = self.editor.block(at);
            self.slash = Some(SlashMenu {
                block_id: block.id,
                slash: 0,
                selected: 0,
            });
            self.edited(cx);
        }
    }

    fn open_block_menu(&mut self, id: u64, cx: &mut Context<Self>) {
        self.slash = None;
        self.mention = None;
        self.table_menu = None;
        self.block_menu = if self.block_menu.as_ref().is_some_and(|m| m.block_id == id) {
            None
        } else {
            Some(BlockMenu {
                block_id: id,
                selected: None,
                anchor: None,
            })
        };
        cx.notify();
    }

    /// Where the block menu hangs: under its grip, left of the block's first
    /// line. Read from the last frame's layout, before a render replaces it,
    /// and kept while the menu is open.
    pub(super) fn place_block_menu(&mut self) {
        let Some(menu) = &self.block_menu else {
            return;
        };
        let Some(index) = self.index_of_id(menu.block_id) else {
            return;
        };
        if let Some((point, line)) = self.caret_point(Pos::new(index, 0)) {
            let at = gpui::point(point.x - px(HANDLE_WIDTH - BUTTON - 2.0), point.y);
            if let Some(menu) = &mut self.block_menu {
                menu.anchor = Some(Bounds::new(at, size(px(2.0), line)));
            }
        }
    }

    pub(super) fn block_menu_anchor(&self) -> Option<Bounds<Pixels>> {
        self.block_menu.as_ref()?.anchor
    }

    fn block_menu_content(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rows = self.block_menu_rows(cx)?;
        Some(
            floating::surface(self.colors, floating::MENU_RADIUS, BLOCK_MENU_WIDTH, rows)
                .into_any_element(),
        )
    }

    pub(super) fn block_menu_height(&self) -> f32 {
        menu_height(BLOCK_ACTIONS.len(), 3) + TURN_BUTTON + 30.0
    }

    pub(super) fn block_menu_width() -> f32 {
        BLOCK_MENU_WIDTH
    }

    pub(super) fn block_menu_rows(&mut self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let menu = self.block_menu.as_ref()?;
        let index = self.index_of_id(menu.block_id)?;
        let selected = menu.selected;
        let colors = self.colors;
        let current = self.editor.block(index).kind;
        let turnable = current.has_text() && !current.is_cell();
        let mut strip = div()
            .mx(px(floating::MENU_ROW_MARGIN + 4.0))
            .flex()
            .gap(px(1.0));
        for (i, (label, icon, kind)) in TURN_KINDS.iter().enumerate() {
            let kind = *kind;
            let on = match (kind, current) {
                (BlockKind::Todo { .. }, BlockKind::Todo { .. })
                | (BlockKind::Callout(_), BlockKind::Callout(_)) => true,
                _ => kind == current,
            };
            strip = strip.child(
                div()
                    .id(("note-block-turn", i))
                    .size(px(TURN_BUTTON))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.0))
                    .when(on, |b| b.bg(accent().alpha(0.16)))
                    .when(turnable, |b| {
                        b.cursor_pointer()
                            .hover(|b| b.bg(colors.primary.alpha(0.07)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.turn_menu_block(kind, cx);
                                }),
                            )
                    })
                    .when(!turnable, |b| b.opacity(0.35))
                    .child(crate::icons::sf_symbol(
                        icon,
                        MENU_ICON,
                        if on { colors.primary } else { colors.secondary },
                    ))
                    .warm_tooltip(move |_, cx| {
                        cx.new(|_| PaletteTooltip(super::block_label(label).to_owned(), colors))
                            .into()
                    }),
            );
        }
        let mut list = div()
            .flex()
            .flex_col()
            .py(px(floating::MENU_PADDING_Y))
            .child(
                div()
                    .px(px(floating::MENU_ROW_MARGIN + floating::MENU_ROW_INSET))
                    .pt(px(4.0))
                    .pb(px(6.0))
                    .text_size(px(Typo::META.size))
                    .font_weight(Typo::META.weight)
                    .text_color(colors.tertiary)
                    .child(crate::i18n::t("notes.block.turn_into")),
            )
            .child(strip)
            .child(floating::menu_separator(colors));
        let mut group = None;
        for (i, (label, icon, action, in_group)) in BLOCK_ACTIONS.iter().enumerate() {
            if group.is_some_and(|g| g != *in_group) {
                list = list.child(floating::menu_separator(colors));
            }
            group = Some(*in_group);
            let danger = *action == BlockAction::Delete;
            let ink = if danger {
                Ink::on_surface(Ink::DANGER, colors)
            } else {
                colors.secondary
            };
            let action = *action;
            let row = floating::menu_row(
                ("note-block-row", i),
                crate::icons::sf_symbol(icon, MENU_ICON, ink),
                colors,
                selected == Some(i),
            )
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if let Some(menu) = &mut this.block_menu {
                    let next = hovered.then_some(i).or(menu.selected.filter(|s| *s != i));
                    if menu.selected != next {
                        menu.selected = next;
                        cx.notify();
                    }
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.apply_block_action(action, cx);
                }),
            )
            .child(if danger {
                menu_label(super::block_label(label), colors).text_color(ink)
            } else {
                menu_label(super::block_label(label), colors)
            });
            list = list.child(row);
        }
        Some(list.w(px(BLOCK_MENU_WIDTH)))
    }

    /// Opens the block menu on the note's first paragraph, or the `/` menu
    /// in a new block under it, for the offscreen screenshots.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn open_menu_for_screenshot(
        &mut self,
        slash: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.editor.blocks().get(1).map(|b| b.id) else {
            return;
        };
        window.focus(&self.focus, cx);
        if slash {
            self.add_block_below(id, cx);
        } else {
            self.open_block_menu(id, cx);
        }
    }

    /// The neighbour a block steps over when moved one place: a whole table,
    /// or a list item with the items nested under it, never into one.
    fn step_target(&self, index: usize, up: bool) -> Option<usize> {
        let span = self.editor.block_span(index);
        if !up {
            return Some(span.end).filter(|t| *t < self.editor.blocks().len());
        }
        let mut target = span.start.checked_sub(1).filter(|t| *t > 0)?;
        if let Some(table) = self.editor.table_pos(target) {
            return Some(table.range.start);
        }
        let block = self.editor.block(index);
        let depth = if block.kind.is_list() {
            block.indent
        } else {
            0
        };
        while target > 1 {
            let before = self.editor.block(target);
            if before.kind.is_list() && before.indent > depth {
                target -= 1;
            } else {
                break;
            }
        }
        Some(target)
    }

    fn turn_menu_block(&mut self, kind: BlockKind, cx: &mut Context<Self>) {
        let Some(menu) = self.block_menu.take() else {
            return;
        };
        let Some(index) = self.index_of_id(menu.block_id) else {
            return;
        };
        self.editor.turn_block(index, Turn::Kind(kind), now_ms());
        crate::telemetry::notes_event("notes.block.turned", "handle");
        self.edited(cx);
    }

    pub(super) fn apply_block_action(&mut self, action: BlockAction, cx: &mut Context<Self>) {
        let Some(menu) = self.block_menu.take() else {
            return;
        };
        let Some(index) = self.index_of_id(menu.block_id) else {
            cx.notify();
            return;
        };
        let now = now_ms();
        let changed = match action {
            BlockAction::AddBelow => {
                self.add_block_below(menu.block_id, cx);
                return;
            }
            BlockAction::Duplicate => self.editor.duplicate_block(index, now).is_some(),
            BlockAction::MoveUp | BlockAction::MoveDown => self
                .step_target(index, action == BlockAction::MoveUp)
                .and_then(|t| self.editor.move_block_to(index, t, now))
                .is_some(),
            BlockAction::Delete => self.editor.delete_block(index, now),
        };
        crate::telemetry::notes_event(
            "notes.block.menu",
            match action {
                BlockAction::AddBelow => "add",
                BlockAction::Duplicate => "duplicate",
                BlockAction::MoveUp => "up",
                BlockAction::MoveDown => "down",
                BlockAction::Delete => "delete",
            },
        );
        if changed {
            self.edited(cx);
        } else {
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use diri_notes::store::NoteStore;
    use diri_proto::SessionId;

    use super::*;
    use crate::notes::{NotePane, PaneState};
    use crate::store::StoreRuntime;

    const NOTE: &str = "# Plan\n\nIntro\n\n- [ ] first\n  - [ ] child\n- [ ] second\n\nOutro\n";

    fn open(
        cx: &mut gpui::TestAppContext,
    ) -> (
        tempfile::TempDir,
        Arc<NoteStore>,
        String,
        Entity<NoteEditorView>,
        Entity<NotePane>,
        &mut gpui::VisualTestContext,
    ) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Arc::new(NoteStore::open(dir.path().join("notes")).expect("store"));
        let (_, doc) = diri_notes::markdown::parse(NOTE);
        let (id, _) = store.create(doc, None).expect("create");
        let runtime = Arc::new(StoreRuntime::inert());
        let pane_store = Arc::clone(&store);
        let (pane, cx) = cx.add_window_view(move |_, cx| {
            NotePane::with_store(runtime, Some(pane_store), false, cx)
        });
        let session = SessionId::new("s_note");
        let note = id.clone();
        pane.update_in(cx, |pane, window, cx| {
            pane.show(&session, &note, window, cx)
        });
        let editor = pane.read_with(cx, |pane, _| match &pane.state {
            PaneState::Open(open) => open.editor.clone(),
            _ => panic!("note is not open"),
        });
        (dir, store, id, editor, pane, cx)
    }

    fn body(store: &NoteStore, id: &str) -> String {
        let text = std::fs::read_to_string(store.path_for(id).unwrap()).unwrap();
        let note = diri_notes::store::parse_note(&text);
        diri_notes::text_edit::body(&note)
    }

    fn id_of(view: &NoteEditorView, text: &str) -> u64 {
        view.editor
            .blocks()
            .iter()
            .find(|b| b.text == text)
            .unwrap_or_else(|| panic!("no block {text}"))
            .id
    }

    #[test]
    fn the_slash_menu_filters_blocks_and_links() {
        let labels = |query: &str| -> Vec<&str> {
            slash_filter(query)
                .into_iter()
                .map(|item| item.label)
                .collect()
        };
        assert_eq!(labels("").len(), SLASH_ITEMS.len());
        assert_eq!(labels("h2"), ["Heading 2"]);
        assert_eq!(labels("todo"), ["To-do"]);
        assert_eq!(labels("link"), ["Link to note"]);
        assert_eq!(labels("[["), ["Link to note"]);
        assert_eq!(labels("Men"), ["Mention"]);
        assert!(labels("note").contains(&"Link to note"));
        assert!(
            labels("note").contains(&"Callout"),
            "callout's keywords say note"
        );
        assert!(labels("zzz").is_empty());
    }

    #[gpui::test]
    fn dragging_a_block_moves_it_with_its_children_and_back(cx: &mut gpui::TestAppContext) {
        let (_dir, store, id, editor, pane, cx) = open(cx);
        let original = body(&store, &id);
        editor.update(cx, |view, cx| {
            let first = id_of(view, "first");
            let outro = id_of(view, "Outro");
            view.dragging = Some(first);
            view.drop_block(first, outro, cx);
            assert_eq!(view.dragging, None);
        });
        pane.update(cx, |pane, cx| pane.save(cx));
        assert_eq!(
            body(&store, &id).trim_end(),
            "# Plan\n\nIntro\n\n- [ ] second\n\nOutro\n\n- [ ] first\n  - [ ] child"
        );
        // Dropped back on the block it was above, it is where it started.
        editor.update(cx, |view, cx| {
            let first = id_of(view, "first");
            let second = id_of(view, "second");
            view.drop_block(first, second, cx);
            // Onto its own child: nothing moves.
            let child = id_of(view, "child");
            view.drop_block(first, child, cx);
        });
        pane.update(cx, |pane, cx| pane.save(cx));
        assert_eq!(body(&store, &id), original);
    }

    #[gpui::test]
    fn the_block_menu_duplicates_turns_moves_and_deletes(cx: &mut gpui::TestAppContext) {
        let (_dir, store, id, editor, pane, cx) = open(cx);
        editor.update(cx, |view, cx| {
            let intro = id_of(view, "Intro");
            view.open_block_menu(intro, cx);
            assert!(view.block_menu.is_some());
            view.apply_block_action(BlockAction::Duplicate, cx);
            assert!(view.block_menu.is_none(), "an action closes the menu");
            let outro = id_of(view, "Outro");
            view.open_block_menu(outro, cx);
            view.turn_menu_block(BlockKind::Heading(1), cx);
            view.open_block_menu(outro, cx);
            view.apply_block_action(BlockAction::MoveUp, cx);
            let copy = view.editor.blocks()[2].id;
            view.open_block_menu(copy, cx);
            view.apply_block_action(BlockAction::Delete, cx);
        });
        pane.update(cx, |pane, cx| pane.save(cx));
        assert_eq!(
            body(&store, &id).trim_end(),
            "# Plan\n\nIntro\n\n- [ ] first\n  - [ ] child\n\n## Outro\n\n- [ ] second"
        );
    }

    #[gpui::test]
    fn plus_adds_a_block_and_opens_the_slash_menu(cx: &mut gpui::TestAppContext) {
        let (_dir, _store, _id, editor, _pane, cx) = open(cx);
        editor.update_in(cx, |view, window, cx| {
            let intro = id_of(view, "Intro");
            view.add_block_below(intro, cx);
            assert!(view.slash.is_some());
            assert_eq!(view.editor.block(2).text, "/");
            for ch in "link".chars() {
                view.replace_text_in_range(None, &ch.to_string(), window, cx);
            }
            assert_eq!(view.slash_matches().len(), 1);
            view.apply_slash(0, cx);
            assert!(view.slash.is_none());
            let menu = view.mention.as_ref().expect("the note picker opens");
            assert!(menu.notes_only);
            assert_eq!(view.editor.block(2).text, "[[");
        });
    }

    #[gpui::test]
    fn double_brackets_link_a_note_and_offer_only_notes(cx: &mut gpui::TestAppContext) {
        let (_dir, store, id, editor, pane, cx) = open(cx);
        editor.update_in(cx, |view, window, cx| {
            view.set_mentions(
                MentionDirectory {
                    entries: crate::notes::tests::fixture_mentions(),
                },
                cx,
            );
            let outro = view
                .editor
                .blocks()
                .iter()
                .position(|b| b.text == "Outro")
                .unwrap();
            view.editor.set_caret(Pos::new(outro, 5));
            for ch in " see [[".chars() {
                view.replace_text_in_range(None, &ch.to_string(), window, cx);
            }
            assert!(view.mention.as_ref().is_some_and(|m| m.notes_only));
            assert!(
                view.mention_matches()
                    .iter()
                    .all(|e| matches!(e.candidate.target, MentionTarget::Note(_))),
                "[[ offers notes only"
            );
            for ch in "groc]]".chars() {
                view.replace_text_in_range(None, &ch.to_string(), window, cx);
            }
            assert_eq!(
                view.mention_matches().len(),
                1,
                "typed brackets still match"
            );
            view.newline(&Newline, window, cx);
            assert!(view.mention.is_none());
            // `a[[` is not a link.
            for ch in "a[[".chars() {
                view.replace_text_in_range(None, &ch.to_string(), window, cx);
            }
            assert!(view.mention.is_none());
        });
        pane.update(cx, |pane, cx| pane.save(cx));
        let text = body(&store, &id);
        assert!(
            text.contains("Outro see [@Groceries](diri://note/n-groceries) a\\[\\["),
            "{text}"
        );
    }
}
