//! The editor driven like a person would: keystrokes, typed text, the
//! input method, and the find bar, in a headless window.

use std::collections::BTreeSet;

use gpui::{Entity, EntityInputHandler, TestAppContext, VisualTestContext, px, size};

use super::buffer::Buffer;
use super::cursor::Selection;
use super::history::History;
use super::{CodeEditor, Document, EditorPalette, key_bindings};
use crate::code_intelligence::SourceLanguage;

fn document(text: &str, language: SourceLanguage) -> Document {
    Document {
        id: super::editor::DocumentId::fresh(),
        relative_path: "src/sample.rs".into(),
        absolute_path: "/tmp/src/sample.rs".into(),
        language,
        buffer: Buffer::new(text),
        history: History::default(),
        selections: vec![Selection::caret(0)],
        folded: BTreeSet::new(),
        modified: None,
        read_only: false,
        scroll_row: 0,
    }
}

fn editor<'a>(
    text: &str,
    cx: &'a mut TestAppContext,
) -> (Entity<CodeEditor>, &'a mut VisualTestContext) {
    editor_for(text, SourceLanguage::Rust, cx)
}

fn editor_for<'a>(
    text: &str,
    language: SourceLanguage,
    cx: &'a mut TestAppContext,
) -> (Entity<CodeEditor>, &'a mut VisualTestContext) {
    cx.update(|cx| cx.bind_keys(key_bindings()));
    let text = text.to_owned();
    let (editor, cx) = cx.add_window_view(move |window, cx| {
        let colors = diri_ui::SemanticColors::dark();
        let palette = EditorPalette::from_theme(&diri_term::theme::TermTheme::DIRIJOR_DARK);
        let mut editor = CodeEditor::new(palette, colors, cx);
        editor.set_document(Some(document(&text, language)), cx);
        window.focus(&editor.focus, cx);
        editor
    });
    cx.simulate_resize(size(px(700.0), px(500.0)));
    cx.run_until_parked();
    (editor, cx)
}

fn text(editor: &Entity<CodeEditor>, cx: &mut VisualTestContext) -> String {
    editor.read_with(cx, |editor, _| editor.text().to_owned())
}

fn heads(editor: &Entity<CodeEditor>, cx: &mut VisualTestContext) -> Vec<usize> {
    editor.read_with(cx, |editor, _| {
        let mut heads: Vec<usize> = editor
            .selections()
            .iter()
            .map(|selection| selection.head)
            .collect();
        heads.sort_unstable();
        heads
    })
}

fn select(editor: &Entity<CodeEditor>, selections: Vec<Selection>, cx: &mut VisualTestContext) {
    editor.update(cx, |editor, cx| editor.set_selections(selections, cx));
}

#[gpui::test]
fn typing_pairs_brackets_steps_over_closers_and_undoes_as_one_burst(cx: &mut TestAppContext) {
    let (editor, cx) = editor("", cx);
    cx.simulate_input("f(x");
    assert_eq!(text(&editor, cx), "f(x)");
    cx.simulate_input(")");
    assert_eq!(
        text(&editor, cx),
        "f(x)",
        "the closer already there is stepped over"
    );
    assert_eq!(heads(&editor, cx), vec![4]);
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "", "one typing burst is one undo step");
    cx.simulate_keystrokes("cmd-shift-z");
    assert_eq!(text(&editor, cx), "f(x)");
}

#[gpui::test]
fn enter_indents_and_splits_brackets(cx: &mut TestAppContext) {
    let (editor, cx) = editor("fn a() {}", cx);
    select(&editor, vec![Selection::caret(8)], cx);
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "fn a() {\n    \n}");
    assert_eq!(
        heads(&editor, cx),
        vec![13],
        "the caret sits on the indented middle line"
    );
    cx.simulate_input("x");
    cx.simulate_keystrokes("enter");
    assert_eq!(
        text(&editor, cx),
        "fn a() {\n    x\n    \n}",
        "a new line keeps the indent"
    );
}

#[gpui::test]
fn many_cursors_type_delete_and_select_next_occurrences(cx: &mut TestAppContext) {
    let (editor, cx) = editor("let a = 1;\nlet a = 2;\nlet b = a;", cx);
    select(&editor, vec![Selection::caret(4)], cx);
    cx.simulate_keystrokes("cmd-d");
    assert_eq!(
        editor.read_with(cx, |editor, _| editor.primary().range()),
        4..5,
        "the first press selects the word"
    );
    cx.simulate_keystrokes("cmd-d cmd-d");
    assert_eq!(
        editor.read_with(cx, |editor, _| editor.selections().len()),
        3
    );
    cx.simulate_input("value");
    assert_eq!(
        text(&editor, cx),
        "let value = 1;\nlet value = 2;\nlet b = value;"
    );
    cx.simulate_keystrokes("backspace");
    assert_eq!(
        text(&editor, cx),
        "let valu = 1;\nlet valu = 2;\nlet b = valu;"
    );
    cx.simulate_keystrokes("escape");
    assert_eq!(
        editor.read_with(cx, |editor, _| editor.selections().len()),
        1
    );
}

#[gpui::test]
fn cursors_added_above_and_below_keep_their_column(cx: &mut TestAppContext) {
    let (editor, cx) = editor("abc\nabc\nabc", cx);
    select(&editor, vec![Selection::caret(5)], cx);
    cx.simulate_keystrokes("cmd-alt-down cmd-alt-up");
    assert_eq!(heads(&editor, cx), vec![1, 5, 9]);
    cx.simulate_input("-");
    assert_eq!(text(&editor, cx), "a-bc\na-bc\na-bc");
}

#[gpui::test]
fn select_all_occurrences_and_paste_one_line_per_cursor(cx: &mut TestAppContext) {
    let (editor, cx) = editor("x y x z x", cx);
    select(&editor, vec![Selection::caret(0)], cx);
    cx.simulate_keystrokes("cmd-shift-l");
    assert_eq!(
        editor.read_with(cx, |editor, _| editor.selections().len()),
        3
    );
    cx.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("1\n2\n3".into())));
    cx.simulate_keystrokes("cmd-v");
    assert_eq!(text(&editor, cx), "1 y 2 z 3");
}

#[gpui::test]
fn comments_toggle_with_the_language_prefix(cx: &mut TestAppContext) {
    let (editor, cx) = editor_for("a = 1\n  b = 2", SourceLanguage::Python, cx);
    select(&editor, vec![Selection::span(0..13)], cx);
    cx.simulate_keystrokes("cmd-/");
    assert_eq!(
        text(&editor, cx),
        "# a = 1\n#   b = 2",
        "the prefix lines up at the shallowest indent"
    );
    cx.simulate_keystrokes("cmd-/");
    assert_eq!(text(&editor, cx), "a = 1\n  b = 2");
}

#[gpui::test]
fn lines_move_duplicate_and_delete(cx: &mut TestAppContext) {
    let (editor, cx) = editor("one\ntwo\nthree", cx);
    select(&editor, vec![Selection::caret(5)], cx);
    cx.simulate_keystrokes("alt-up");
    assert_eq!(text(&editor, cx), "two\none\nthree");
    assert_eq!(
        heads(&editor, cx),
        vec![1],
        "the caret travels with its line"
    );
    cx.simulate_keystrokes("alt-shift-down");
    assert_eq!(text(&editor, cx), "two\ntwo\none\nthree");
    cx.simulate_keystrokes("cmd-shift-k");
    assert_eq!(text(&editor, cx), "two\none\nthree");
}

#[gpui::test]
fn tab_indents_and_outdents_selected_lines_in_the_files_unit(cx: &mut TestAppContext) {
    let (editor, cx) = editor("a:\n  b: 1\n  c: 2\nd: 3", cx);
    select(&editor, vec![Selection::span(0..17)], cx);
    cx.simulate_keystrokes("tab");
    assert_eq!(
        text(&editor, cx),
        "  a:\n    b: 1\n    c: 2\nd: 3",
        "two-space files indent by two"
    );
    cx.simulate_keystrokes("shift-tab shift-tab");
    assert_eq!(text(&editor, cx), "a:\nb: 1\nc: 2\nd: 3");
}

#[gpui::test]
fn folding_hides_rows_and_cursor_motion_skips_them(cx: &mut TestAppContext) {
    let source = "fn a() {\n    one();\n    two();\n}\nfn b() {}";
    let (editor, cx) = editor(source, cx);
    let rows = |editor: &Entity<CodeEditor>, cx: &mut VisualTestContext| {
        editor.update(cx, |editor, _| {
            editor.refresh_layout();
            editor
                .layout
                .rows
                .iter()
                .filter(|row| matches!(row, super::editor::Row::Line(_)))
                .count()
        })
    };
    assert_eq!(rows(&editor, cx), 5);
    editor.update(cx, |editor, cx| editor.toggle_fold(0, cx));
    assert_eq!(rows(&editor, cx), 3, "the body hides, the closer stays");
    select(&editor, vec![Selection::caret(3)], cx);
    cx.simulate_keystrokes("down");
    assert_eq!(
        editor.read_with(cx, |editor, _| editor.position().0),
        4,
        "down steps over the folded lines"
    );
    cx.simulate_keystrokes("cmd-alt-shift-]");
    assert_eq!(rows(&editor, cx), 5, "unfold all");
    cx.simulate_keystrokes("cmd-alt-shift-[");
    assert_eq!(rows(&editor, cx), 3, "fold all folds the outermost blocks");
    // A cursor moved into a fold opens it.
    select(&editor, vec![Selection::caret(12)], cx);
    assert_eq!(rows(&editor, cx), 5);
}

#[gpui::test]
fn find_and_replace_through_the_find_bar(cx: &mut TestAppContext) {
    let (editor, cx) = editor("let total = total_cost + Total;", cx);
    cx.simulate_keystrokes("cmd-f");
    assert!(editor.read_with(cx, |editor, _| editor.find.open));
    cx.simulate_input("total");
    assert_eq!(
        editor.read_with(cx, |editor, _| editor.find.matches.len()),
        3
    );
    cx.simulate_keystrokes("alt-c");
    assert_eq!(
        editor.read_with(cx, |editor, _| editor.find.matches.len()),
        2,
        "match case"
    );
    cx.simulate_keystrokes("alt-w");
    assert_eq!(
        editor.read_with(cx, |editor, _| editor.find.matches.clone()),
        [4..9].to_vec(),
        "whole word"
    );
    cx.simulate_keystrokes("alt-w alt-c alt-r");
    editor.update(cx, |editor, cx| {
        editor.find.query.clear();
        editor.find.query.insert(r"(\w+)_cost");
        editor.find.replacement.insert("${1}_sum");
        editor.find_changed(cx);
    });
    assert_eq!(
        editor.read_with(cx, |editor, _| editor.find.matches.clone()),
        [12..22].to_vec()
    );
    editor.update(cx, |editor, cx| editor.replace_all(cx));
    assert_eq!(text(&editor, cx), "let total = total_sum + Total;");
    cx.simulate_keystrokes("escape");
    assert!(!editor.read_with(cx, |editor, _| editor.find.open));
}

#[gpui::test]
fn find_steps_wrap_and_replace_one_moves_on(cx: &mut TestAppContext) {
    let (editor, cx) = editor("a b a b a", cx);
    editor.update(cx, |editor, cx| {
        editor.find.open = true;
        editor.find.query.insert("a");
        editor.find.replacement.insert("X");
        editor.find_changed(cx);
    });
    assert_eq!(
        editor.read_with(cx, |editor, _| editor.find.current),
        Some(0)
    );
    editor.update(cx, |editor, cx| editor.find_step(false, cx));
    assert_eq!(
        editor.read_with(cx, |editor, _| editor.find.current),
        Some(2),
        "back wraps to the end"
    );
    editor.update(cx, |editor, cx| editor.find_step(true, cx));
    assert_eq!(
        editor.read_with(cx, |editor, _| editor.find.current),
        Some(0)
    );
    editor.update(cx, |editor, cx| editor.replace_current(cx));
    assert_eq!(text(&editor, cx), "X b a b a");
    assert_eq!(
        editor.read_with(cx, |editor, _| editor.primary().range()),
        4..5,
        "the next match is selected"
    );
}

#[gpui::test]
fn an_input_method_composition_is_one_undo_step(cx: &mut TestAppContext) {
    let (editor, cx) = editor("x", cx);
    select(&editor, vec![Selection::caret(1)], cx);
    cx.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            editor.replace_and_mark_text_in_range(None, "n", None, window, cx);
            editor.replace_and_mark_text_in_range(None, "ni", None, window, cx);
            assert_eq!(editor.marked_text_range(window, cx), Some(1..3));
            editor.replace_text_in_range(None, "に", window, cx);
        })
    });
    assert_eq!(text(&editor, cx), "xに");
    assert_eq!(
        editor.read_with(cx, |editor, _| editor.marked.clone()),
        None
    );
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "x");
}

#[gpui::test]
fn read_only_documents_refuse_edits(cx: &mut TestAppContext) {
    let (editor, cx) = editor("fixed", cx);
    editor.update(cx, |editor, _| {
        editor.doc.as_mut().unwrap().read_only = true
    });
    cx.simulate_input("abc");
    cx.simulate_keystrokes("backspace cmd-z");
    assert_eq!(text(&editor, cx), "fixed");
    assert!(!editor.read_with(cx, |editor, _| editor.is_dirty()));
}

#[gpui::test]
fn edits_mark_the_document_dirty_until_undone(cx: &mut TestAppContext) {
    let (editor, cx) = editor("clean", cx);
    let dirty_events = std::rc::Rc::new(std::cell::Cell::new(0));
    let counter = dirty_events.clone();
    cx.update(|_, cx| {
        cx.subscribe(&editor, move |_, event: &super::EditorEvent, _| {
            if *event == super::EditorEvent::DirtyChanged {
                counter.set(counter.get() + 1);
            }
        })
        .detach();
    });
    cx.simulate_input("!");
    assert!(editor.read_with(cx, |editor, _| editor.is_dirty()));
    cx.simulate_keystrokes("cmd-z");
    assert!(!editor.read_with(cx, |editor, _| editor.is_dirty()));
    assert_eq!(dirty_events.get(), 2);
}

#[gpui::test]
fn the_editor_holds_a_large_file_and_jumps_into_it(cx: &mut TestAppContext) {
    let source = "fn f() {\n    let x = vec![1, 2, 3];\n}\n".repeat(30_000);
    let (editor, cx) = editor(&source, cx);
    editor.update(cx, |editor, cx| {
        editor.go_to(60_000, 1, cx);
    });
    cx.run_until_parked();
    let (lines, rows) = editor.read_with(cx, |editor, _| {
        (
            editor.document().unwrap().buffer.lines(),
            editor.layout.rows.len(),
        )
    });
    assert_eq!(lines, 90_001);
    assert!(rows >= lines, "every line has a row (lenses add more)");
    assert_eq!(
        editor.read_with(cx, |editor, _| editor.position().0),
        60_000
    );
}
