use std::sync::Arc;

use diri_notes::markdown;
use diri_notes::store::NoteStore;
use diri_proto::SessionId;
use gpui::EntityInputHandler as _;

use super::*;
use crate::store::StoreRuntime;

pub(crate) const PLAN: &str = "# Notes launch plan

A **rich**, _calm_ place to think next to your agents. Everything is plain Markdown on disk — see [the spec](https://diri.sh/notes).

## This week

- [x] Block model and Markdown codec
- [x] Atomic file store
- [ ] Quick capture from anywhere
- [ ] Agents can `append` to a note

## Open questions

> Should to-dos roll up into a Today view, or stay inside their notes?

1. Keep files flat
2. Organise with front matter

```
dirijor note \"ship it\" --project .
```
";

fn store_with_plan() -> (tempfile::TempDir, Arc<NoteStore>, String) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = NoteStore::open(dir.path().join("notes")).expect("store");
    let (_, doc) = markdown::parse(PLAN);
    let (id, _) = store.create(doc, None).expect("create");
    (dir, Arc::new(store), id)
}

fn pane(
    cx: &mut gpui::TestAppContext,
    store: Arc<NoteStore>,
) -> (Entity<NotePane>, &mut gpui::VisualTestContext) {
    let runtime = Arc::new(StoreRuntime::inert());
    cx.add_window_view(move |_, cx| NotePane::with_store(runtime, Some(store), false, cx))
}

fn editor(pane: &Entity<NotePane>, cx: &mut gpui::VisualTestContext) -> Entity<NoteEditorView> {
    pane.read_with(cx, |pane, _| match &pane.state {
        PaneState::Open(open) => open.editor.clone(),
        _ => panic!("note is not open"),
    })
}

#[gpui::test]
fn typing_saves_markdown_and_keeps_formatting(cx: &mut gpui::TestAppContext) {
    let (_dir, store, id) = store_with_plan();
    let (pane, cx) = pane(cx, store.clone());
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    let editor = editor(&pane, cx);
    editor.update_in(cx, |view, window, cx| {
        // End of the note, then a new to-do typed with the Markdown shortcut.
        let last = view.editor.blocks().len() - 1;
        view.editor.set_caret(diri_notes::edit::Pos::new(last, 0));
        for ch in "[] ship **notes**".chars() {
            view.replace_text_in_range(None, &ch.to_string(), window, cx);
        }
    });
    pane.update(cx, |pane, cx| pane.save(cx));
    let text = std::fs::read_to_string(store.path_for(&id).unwrap()).unwrap();
    assert!(text.ends_with("- [ ] ship **notes**\n"), "{text}");
    assert!(text.contains("# Notes launch plan"));
}

#[gpui::test]
fn outside_writes_reload_a_clean_note(cx: &mut gpui::TestAppContext) {
    let (_dir, store, id) = store_with_plan();
    let (pane, cx) = pane(cx, store.clone());
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    // An agent appends through the CLI while the note is open and untouched.
    store
        .append(
            &id,
            "- [ ] added by an agent",
            &diri_notes::history::Author::Session("s_agent".into()),
        )
        .unwrap();
    pane.update(cx, |pane, cx| pane.reconcile(cx));
    let editor = editor(&pane, cx);
    let has = editor.read_with(cx, |view, _| {
        view.editor
            .blocks()
            .iter()
            .any(|b| b.text == "added by an agent")
    });
    assert!(has, "open note reloads outside edits");
}

#[gpui::test]
fn a_missing_file_shows_an_explanation_not_a_crash(cx: &mut gpui::TestAppContext) {
    let (_dir, store, _) = store_with_plan();
    let (pane, cx) = pane(cx, store);
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| {
        pane.show(&session, "20990101-000000-dead", window, cx)
    });
    pane.read_with(cx, |pane, _| {
        assert!(matches!(pane.state, PaneState::Missing { .. }));
    });
}

/// Sessions and a note a fixture editor can mention, one session per status
/// a chip draws.
pub(crate) fn fixture_mentions() -> Vec<editor_view::MentionEntry> {
    use diri_notes::mention::{Candidate, MentionTarget, note_label};
    use diri_ui::{AgentKind, StatusState};
    let mut entries: Vec<editor_view::MentionEntry> = [
        (
            "s_codex",
            AgentKind::Codex,
            "Codex",
            "fix resize flicker",
            StatusState::Working,
            "diri",
        ),
        (
            "s_claude",
            AgentKind::ClaudeCode,
            "Claude Code",
            "draft launch email",
            StatusState::NeedsInput { destructive: false },
            "Growth",
        ),
        (
            "s_gemini",
            AgentKind::Gemini,
            "Gemini",
            "release notes draft",
            StatusState::DoneUnseen,
            "diri-web",
        ),
        (
            "s_shell",
            AgentKind::Shell,
            "Terminal",
            "cargo test",
            StatusState::IdleSeen,
            "~/fun/diri",
        ),
    ]
    .into_iter()
    .map(
        |(id, agent, name, title, status, detail)| editor_view::MentionEntry {
            candidate: Candidate {
                target: MentionTarget::Session(id.into()),
                label: super::mention_label(name, title),
                keywords: name.to_lowercase(),
            },
            agent: Some(agent),
            status: Some(status),
            detail: detail.into(),
        },
    )
    .collect();
    for (id, title, project) in [
        ("n-groceries", "Groceries", ""),
        ("n-q4", "Q4 campaign brief", "Growth"),
    ] {
        entries.push(editor_view::MentionEntry {
            candidate: Candidate {
                target: MentionTarget::Note(id.into()),
                label: note_label(title),
                keywords: "note".into(),
            },
            agent: None,
            status: None,
            detail: project.into(),
        });
    }
    entries
}

#[gpui::test]
fn at_mentions_insert_session_and_note_links(cx: &mut gpui::TestAppContext) {
    let (_dir, store, id) = store_with_plan();
    let (pane, cx) = pane(cx, store.clone());
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    let editor = editor(&pane, cx);
    editor.update(cx, |view, cx| {
        view.set_mentions(
            editor_view::MentionDirectory {
                entries: fixture_mentions(),
            },
            cx,
        )
    });
    fn type_str(
        view: &mut NoteEditorView,
        text: &str,
        window: &mut Window,
        cx: &mut Context<NoteEditorView>,
    ) {
        for ch in text.chars() {
            view.replace_text_in_range(None, &ch.to_string(), window, cx);
        }
    }
    editor.update_in(cx, |view, window, cx| {
        let last = view.editor.blocks().len() - 1;
        view.editor.set_caret(diri_notes::edit::Pos::new(last, 0));
        type_str(view, "wait for @re", window, cx);
        // Two sessions match "re"; arrow to the second and take it with Tab.
        assert_eq!(
            view.mention_matches().len(),
            2,
            "gemini release + codex resize"
        );
        view.vertical(true, false, cx);
        view.indent(&editor_view::Indent, window, cx);
        type_str(view, "then @groc", window, cx);
        view.newline(&editor_view::Newline, window, cx);
        // An email address never opens the menu.
        type_str(view, "mail me@x", window, cx);
        assert!(!view.mention_open());
    });
    pane.update(cx, |pane, cx| pane.save(cx));
    let text = std::fs::read_to_string(store.path_for(&id).unwrap()).unwrap();
    assert!(
        text.contains(
            "wait for [@fix resize flicker](diri://session/s_codex) then [@Groceries](diri://note/n-groceries) mail me@x"
        ),
        "{text}"
    );
    let (_, doc) = markdown::parse(&text);
    assert_eq!(
        doc.mentions(),
        vec![
            diri_notes::mention::MentionTarget::Session("s_codex".into()),
            diri_notes::mention::MentionTarget::Note("n-groceries".into()),
        ]
    );
}

#[gpui::test]
fn hover_and_arrow_keys_share_one_menu_highlight(cx: &mut gpui::TestAppContext) {
    let (_dir, store, id) = store_with_plan();
    let (pane, cx) = pane(cx, store);
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    let editor = editor(&pane, cx);
    editor.update_in(cx, |view, window, cx| {
        let last = view.editor.blocks().len() - 1;
        view.editor.set_caret(diri_notes::edit::Pos::new(last, 0));
        view.replace_text_in_range(None, "/", window, cx);
        assert_eq!(view.menu_selected(), Some(0));
        view.vertical(true, false, cx);
        assert_eq!(view.menu_selected(), Some(1));
        // The pointer takes the same highlight the arrows moved...
        view.hover_menu_row(4, cx);
        assert_eq!(view.menu_selected(), Some(4));
        // ...and the arrows continue from where the pointer left it.
        view.vertical(true, false, cx);
        assert_eq!(view.menu_selected(), Some(5));
        // Enter applies the one highlighted row: Bulleted list.
        view.newline(&editor_view::Newline, window, cx);
        assert_eq!(
            view.editor.block(last).kind,
            diri_notes::doc::BlockKind::Bullet
        );
    });
}

#[gpui::test]
fn folding_is_view_state_that_survives_outside_writes(cx: &mut gpui::TestAppContext) {
    let (_dir, store, id) = store_with_plan();
    let (pane, cx) = pane(cx, store.clone());
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    let editor = editor(&pane, cx);
    let parent = editor.update_in(cx, |view, window, cx| {
        let parent = view
            .editor
            .blocks()
            .iter()
            .position(|b| b.text == "Quick capture from anywhere")
            .unwrap();
        let end = view.editor.block(parent).text.len();
        view.editor
            .set_caret(diri_notes::edit::Pos::new(parent, end));
        view.newline(&editor_view::Newline, window, cx);
        view.indent(&editor_view::Indent, window, cx);
        for ch in "a global hotkey".chars() {
            view.replace_text_in_range(None, &ch.to_string(), window, cx);
        }
        // ⌥⌘↩ inside the child folds the item that holds it.
        view.toggle_fold(&editor_view::ToggleFold, window, cx);
        assert!(view.editor.is_collapsed(parent));
        assert!(view.editor.is_hidden(parent + 1));
        assert_eq!(view.editor.selection.head.block, parent);
        parent
    });
    pane.update(cx, |pane, cx| pane.save(cx));
    let text = std::fs::read_to_string(store.path_for(&id).unwrap()).unwrap();
    assert!(
        text.contains("- [ ] Quick capture from anywhere\n  - [ ] a global hotkey\n"),
        "folding never changes the file:\n{text}"
    );
    // An agent appends while the item is folded; the fold survives the reload.
    store
        .append(
            &id,
            "- [ ] added by an agent",
            &diri_notes::history::Author::Session("s_agent".into()),
        )
        .unwrap();
    pane.update(cx, |pane, cx| pane.reconcile(cx));
    editor.read_with(cx, |view, _| {
        assert!(view.editor.is_collapsed(parent));
        assert!(
            view.editor
                .blocks()
                .iter()
                .any(|b| b.text == "added by an agent")
        );
    });
}

/// What the `write_note` MCP tool does to the file: an attributed, locked,
/// additive write through the store.
fn agent_appends(store: &NoteStore, id: &str, markdown: &str) {
    store
        .update(
            id,
            &diri_notes::history::Author::Session("s_agent".into()),
            |note| {
                diri_notes::store::append_markdown(note, markdown);
                Ok(())
            },
        )
        .unwrap();
}

fn type_text(editor: &Entity<NoteEditorView>, text: &str, cx: &mut gpui::VisualTestContext) {
    editor.update_in(cx, |view, window, cx| {
        for ch in text.chars() {
            view.replace_text_in_range(None, &ch.to_string(), window, cx);
        }
    });
}

#[gpui::test]
fn an_agent_append_while_typing_is_merged_not_lost(cx: &mut gpui::TestAppContext) {
    let (_dir, store, id) = store_with_plan();
    let (pane, cx) = pane(cx, store.clone());
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    let editor = editor(&pane, cx);
    // The person starts typing at the end of the first paragraph...
    editor.update(cx, |view, _| {
        let at = view
            .editor
            .blocks()
            .iter()
            .position(|b| b.text.starts_with("A rich"))
            .unwrap();
        let len = view.editor.block(at).text.len();
        view.editor.set_caret(diri_notes::edit::Pos::new(at, len));
    });
    type_text(&editor, " Also for PMs.", cx);
    // ...an agent adds its finding before the debounced save lands...
    agent_appends(&store, &id, "- Finding: the venue holds 300 people");
    // ...the file watcher reconciles, and the person keeps typing.
    pane.update(cx, |pane, cx| pane.reconcile(cx));
    type_text(&editor, " And ops.", cx);
    pane.update(cx, |pane, cx| pane.save(cx));

    let file = std::fs::read_to_string(store.path_for(&id).unwrap()).unwrap();
    assert!(file.contains("next to your agents. Everything is plain Markdown on disk — see [the spec](https://diri.sh/notes). Also for PMs. And ops."), "{file}");
    assert!(
        file.contains("- Finding: the venue holds 300 people"),
        "{file}"
    );
    // One undo takes back the typing after the merge; the next the agent's line.
    editor.update(cx, |view, _| {
        view.editor.undo();
        assert!(
            view.editor
                .blocks()
                .iter()
                .any(|b| b.text == "Finding: the venue holds 300 people")
        );
        view.editor.undo();
        assert!(
            !view
                .editor
                .blocks()
                .iter()
                .any(|b| b.text == "Finding: the venue holds 300 people")
        );
    });
}

#[gpui::test]
fn a_save_racing_an_agent_write_merges_before_writing(cx: &mut gpui::TestAppContext) {
    let (_dir, store, id) = store_with_plan();
    let (pane, cx) = pane(cx, store.clone());
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    let editor = editor(&pane, cx);
    editor.update(cx, |view, _| {
        let at = view
            .editor
            .blocks()
            .iter()
            .position(|b| b.text == "Quick capture from anywhere")
            .unwrap();
        view.editor
            .set_caret(diri_notes::edit::Pos::new(at, "Quick capture".len()));
    });
    type_text(&editor, " (hotkey)", cx);
    // The agent ticks that very to-do and links itself, then the save runs
    // before the watcher ever fires.
    store
        .update(
            &id,
            &diri_notes::history::Author::Session("s_agent".into()),
            |note| {
                let index = diri_notes::handoff::find_todo(
                    note,
                    &diri_notes::handoff::TodoSelector::Text("Quick capture".into()),
                )
                .unwrap();
                diri_notes::handoff::set_checked(note, index, true);
                diri_notes::handoff::link_session(note, index, "@Codex: capture", "s_agent");
                Ok(())
            },
        )
        .unwrap();
    pane.update(cx, |pane, cx| pane.save(cx));
    let file = std::fs::read_to_string(store.path_for(&id).unwrap()).unwrap();
    assert!(
        file.contains(
            "- [x] Quick capture (hotkey) from anywhere [@Codex: capture](diri://session/s_agent)"
        ),
        "{file}"
    );
    // History kept the agent's write as its own version.
    let versions = store.history().list(&id).unwrap();
    assert!(
        versions
            .iter()
            .any(|v| v.author == diri_notes::history::Author::Session("s_agent".into()))
    );
}

#[gpui::test]
fn version_history_lists_previews_and_restores(cx: &mut gpui::TestAppContext) {
    let (_dir, store, id) = store_with_plan();
    agent_appends(&store, &id, "- Finding: the venue holds 300 people");
    let (pane, cx) = pane(cx, store.clone());
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    pane.update_in(cx, |pane, window, cx| {
        pane.open_versions(&crate::commands::NoteVersionHistory, window, cx)
    });
    let (count, newest_by_agent, oldest) = pane.read_with(cx, |pane, _| {
        let panel = pane.versions.as_ref().expect("panel open");
        (
            panel.versions.len(),
            panel.versions[0].author == diri_notes::history::Author::Session("s_agent".into()),
            panel.versions.last().unwrap().id,
        )
    });
    assert!(count >= 2, "creation and the agent's write");
    assert!(newest_by_agent);

    // Selecting the oldest shows its text without the agent's line.
    pane.update(cx, |pane, cx| pane.select_version(count - 1, cx));
    pane.read_with(cx, |pane, _| {
        let preview = &pane.versions.as_ref().unwrap().preview;
        assert_eq!(preview.title, "Notes launch plan");
        assert!(!preview.plain_text().contains("300 people"));
    });

    // Restore (the confirmation sheet is the system's; this is its OK path).
    pane.update(cx, |pane, cx| pane.restore_version(oldest, cx));
    let file = std::fs::read_to_string(store.path_for(&id).unwrap()).unwrap();
    assert!(!file.contains("300 people"), "{file}");
    assert!(
        pane.read_with(cx, |pane, _| pane.versions.is_none()),
        "panel closes"
    );
    let editor = editor(&pane, cx);
    assert!(editor.read_with(cx, |view, _| {
        !view
            .editor
            .blocks()
            .iter()
            .any(|b| b.text.contains("300 people"))
    }));
    // What the restore replaced is one version away.
    let versions = store.history().list(&id).unwrap();
    assert!(versions.iter().any(|v| {
        store
            .history()
            .read(&id, v.id)
            .unwrap()
            .contains("300 people")
    }));

    // Escape closes the panel before it leaves the note.
    pane.update_in(cx, |pane, window, cx| {
        pane.open_versions(&crate::commands::NoteVersionHistory, window, cx)
    });
    editor.update(cx, |_, cx| cx.emit(EditorEvent::Dismiss));
    assert!(pane.read_with(cx, |pane, _| pane.versions.is_none()));
}

#[gpui::test]
fn the_session_chip_api_reads_live_status_and_inserts_links(cx: &mut gpui::TestAppContext) {
    use super::chip::ChipDot;
    use diri_notes::mention::MentionTarget;
    let (_dir, store, id) = store_with_plan();
    let (pane, cx) = pane(cx, store.clone());
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    let editor = editor(&pane, cx);
    editor.update(cx, |view, cx| {
        view.set_mentions(
            editor_view::MentionDirectory {
                entries: fixture_mentions(),
            },
            cx,
        );
        // A live session's dot carries its status ink; an unknown one is gone.
        let live = ChipDot::for_target(
            &MentionTarget::Session("s_codex".into()),
            view.mentions(),
            crate::app_theme::colors("dirijor-light"),
        );
        assert!(matches!(live, ChipDot::Status(_)));
        let gone = ChipDot::for_target(
            &MentionTarget::Session("s_missing".into()),
            view.mentions(),
            crate::app_theme::colors("dirijor-light"),
        );
        assert_eq!(gone, ChipDot::Gone);
        let _chip = view.session_chip("s_codex", "fix resize flicker");
        // Inserting programmatically writes the same link `@` would.
        let last = view.editor.blocks().len() - 1;
        view.insert_session_mention(diri_notes::edit::Pos::new(last, 0), "s_codex", cx);
    });
    pane.update(cx, |pane, cx| pane.save(cx));
    let text = std::fs::read_to_string(store.path_for(&id).unwrap()).unwrap();
    assert!(
        text.contains("[@fix resize flicker](diri://session/s_codex)"),
        "{text}"
    );
}

#[gpui::test]
fn the_link_panel_edits_links_through_its_own_field(cx: &mut gpui::TestAppContext) {
    use diri_notes::edit::{Pos, Selection};
    let (_dir, store, id) = store_with_plan();
    let (pane, cx) = pane(cx, store.clone());
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    let editor = editor(&pane, cx);
    editor.update_in(cx, |view, window, cx| {
        let intro = view
            .editor
            .blocks()
            .iter()
            .position(|b| b.text.starts_with("A rich"))
            .unwrap();
        let text = view.editor.block(intro).text.clone();
        // 1. On an existing link: the panel offers to open or remove it.
        let spec = text.find("the spec").unwrap();
        view.editor.set_caret(Pos::new(intro, spec + 2));
        view.link(&editor_view::Link, window, cx);
        assert_eq!(
            view.link_row_labels(),
            ["open https://diri.sh/notes", "remove"]
        );
        // Typing goes to the field, not the note.
        view.replace_text_in_range(None, "x.dev", window, cx);
        assert_eq!(view.editor.block(intro).text, text);
        assert_eq!(view.link_row_labels()[0], "apply https://x.dev");
        // Remove is the last row.
        view.apply_link_row(2, cx);
        assert!(view.editor.link_at(Pos::new(intro, spec + 2)).is_none());

        // 2. A selection linked by typing a bare domain.
        let calm = text.find("calm").unwrap();
        view.editor.set_selection(Selection {
            anchor: Pos::new(intro, calm),
            head: Pos::new(intro, calm + 4),
        });
        view.link(&editor_view::Link, window, cx);
        for ch in "notion.so/acme/Calm-1f2e3d4c5b6a79881f2e3d4c5b6a7988".chars() {
            view.replace_text_in_range(None, &ch.to_string(), window, cx);
        }
        view.apply_link_row(0, cx);

        // 3. At a bare caret, a tool URL inserts its titled chip.
        let last = view.editor.blocks().len() - 1;
        view.editor.set_caret(Pos::new(last, 0));
        view.link(&editor_view::Link, window, cx);
        for ch in "https://linear.app/acme/issue/GRO-9/pricing-page".chars() {
            view.replace_text_in_range(None, &ch.to_string(), window, cx);
        }
        view.apply_link_row(0, cx);
    });
    pane.update(cx, |pane, cx| pane.save(cx));
    let text = std::fs::read_to_string(store.path_for(&id).unwrap()).unwrap();
    assert!(text.contains("— see the spec."), "{text}");
    assert!(
        text.contains("[_calm_](https://notion.so/acme/Calm-1f2e3d4c5b6a79881f2e3d4c5b6a7988)"),
        "{text}"
    );
    assert!(
        text.contains("[GRO-9 Pricing page](https://linear.app/acme/issue/GRO-9/pricing-page)"),
        "{text}"
    );
}

/// A small bar chart, the kind of picture a PM pastes into a note.
pub(crate) fn chart_png(width: u32, height: u32) -> Vec<u8> {
    let bars = [0.35f32, 0.55, 0.48, 0.72, 0.9];
    let image = image::RgbaImage::from_fn(width, height, |x, y| {
        let slot = width / bars.len() as u32;
        let bar = (x / slot) as usize;
        let inset = slot / 5;
        let top = height as f32 * (1.0 - bars[bar.min(bars.len() - 1)] * 0.85);
        if x % slot > inset && x % slot < slot - inset && (y as f32) >= top {
            image::Rgba([217, 119, 87, 255])
        } else {
            image::Rgba([246, 244, 239, 255])
        }
    });
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .expect("encode png");
    bytes
}

#[gpui::test]
fn pasted_and_dropped_pictures_become_image_blocks(cx: &mut gpui::TestAppContext) {
    use diri_notes::doc::BlockKind;
    let (dir, store, id) = store_with_plan();
    let (pane, cx) = pane(cx, store.clone());
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    let editor = editor(&pane, cx);
    let png = chart_png(40, 20);
    cx.write_to_clipboard(gpui::ClipboardItem::new_image(&gpui::Image::from_bytes(
        gpui::ImageFormat::Png,
        png.clone(),
    )));
    let picture = dir.path().join("Q3 funnel.png");
    std::fs::write(&picture, &png).unwrap();
    let not_a_picture = dir.path().join("notes.txt");
    std::fs::write(&not_a_picture, "hi").unwrap();
    editor.update_in(cx, |view, window, cx| {
        let last = view.editor.blocks().len() - 1;
        view.editor.set_caret(diri_notes::edit::Pos::new(last, 0));
        view.paste(&editor_view::Paste, window, cx);
        // A Finder drop of the same bytes reuses the stored file.
        let inserted =
            view.insert_image_files(&[picture.clone(), not_a_picture.clone()], "drop", cx);
        assert_eq!(inserted, 1, "only pictures are taken in");
        let images: Vec<&diri_notes::doc::Block> = view
            .editor
            .blocks()
            .iter()
            .filter(|b| b.kind == BlockKind::Image)
            .collect();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].src, images[1].src, "content-addressed");
    });
    pane.update(cx, |pane, cx| pane.save(cx));
    let text = std::fs::read_to_string(store.path_for(&id).unwrap()).unwrap();
    let src = format!("assets/{id}/");
    assert_eq!(text.matches(&format!("![]({src}")).count(), 2, "{text}");
    let stored = std::fs::read_dir(store.dir().join("assets").join(&id))
        .unwrap()
        .count();
    assert_eq!(stored, 1);
}

#[gpui::test]
fn a_callout_glyph_cycles_its_tone(cx: &mut gpui::TestAppContext) {
    use diri_notes::doc::{BlockKind, Tone};
    let (_dir, store, id) = store_with_plan();
    let (pane, cx) = pane(cx, store.clone());
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    let editor = editor(&pane, cx);
    editor.update_in(cx, |view, window, cx| {
        let last = view.editor.blocks().len() - 1;
        view.editor.set_caret(diri_notes::edit::Pos::new(last, 0));
        view.replace_text_in_range(None, "/", window, cx);
        for ch in "callout".chars() {
            view.replace_text_in_range(None, &ch.to_string(), window, cx);
        }
        view.newline(&editor_view::Newline, window, cx);
        for ch in "Budget is capped".chars() {
            view.replace_text_in_range(None, &ch.to_string(), window, cx);
        }
        let block = view.editor.block(last).clone();
        assert_eq!(block.kind, BlockKind::Callout(Tone::Note));
        view.cycle_callout(block.id, cx);
        view.cycle_callout(block.id, cx);
        view.cycle_callout(block.id, cx);
        assert_eq!(
            view.editor.block(last).kind,
            BlockKind::Callout(Tone::Warning)
        );
    });
    pane.update(cx, |pane, cx| pane.save(cx));
    let text = std::fs::read_to_string(store.path_for(&id).unwrap()).unwrap();
    assert!(
        text.ends_with("> [!WARNING]\n> Budget is capped\n"),
        "{text}"
    );
}

/// A long, realistic note: sections of paragraphs with bold and links,
/// to-dos, bullets with children, quotes, a code block, mention and tool
/// chips, repeated to `blocks` blocks.
pub(crate) fn big_note_markdown(blocks: usize) -> String {
    let mut out = String::from("# Research log\n\n");
    let mut count = 0;
    let mut section = 0;
    while count < blocks {
        section += 1;
        out.push_str(&format!("## Week {section}\n\n"));
        out.push_str("Interviewed three **growth** leads about _activation_; notes in [the brief](https://www.notion.so/acme/Brief-1f2e3d4c5b6a79881f2e3d4c5b6a7988) and the [funnel sheet](https://docs.google.com/spreadsheets/d/1AbC/edit).\n\n");
        out.push_str("- [ ] Follow up with [@Codex: fix resize flicker](diri://session/s_codex) on the export\n");
        out.push_str("- [x] Ship the onboarding checklist `v2`\n");
        out.push_str("- Retention is flat week over week\n  - but D7 improved for teams\n  - see [GRO-42](https://linear.app/acme/issue/GRO-42/launch-email)\n");
        out.push_str("\n> Customers want the export to keep formatting.\n\n");
        out.push_str("1. Draft the email\n2. Review with legal\n3. Schedule for Tuesday\n\n");
        count += 12;
        if section % 5 == 0 {
            out.push_str("```\nSELECT week, count(*) FROM signups GROUP BY 1;\n```\n\n");
            count += 1;
        }
    }
    out
}

/// Measures the editor on a 2,000-block note: an idle re-render, one typed
/// character, and one scroll step, each through a full frame.
/// `DIRI_BENCH_BLOCKS` and `DIRI_BENCH_ITERATIONS` override the sizes.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "headless Metal render-cost bench for big notes; run explicitly"]
fn big_note_render_cost() {
    use gpui::{AppContext as _, HeadlessAppContext, px, size};
    use std::time::{Duration, Instant};
    let blocks: usize = std::env::var("DIRI_BENCH_BLOCKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    let iterations: usize = std::env::var("DIRI_BENCH_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(diri_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| crate::fonts::init(cx));
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(NoteStore::open(dir.path().join("notes")).unwrap());
    // `DIRI_BENCH_KIND=table`: one table of `DIRI_BENCH_ROWS` rows (200).
    let source = if std::env::var("DIRI_BENCH_KIND").as_deref() == Ok("table") {
        let rows: usize = std::env::var("DIRI_BENCH_ROWS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200);
        let mut table =
            String::from("# Funnel\n\n| Week | Channel | Signups | Notes |\n|--:|:--|--:|:--|\n");
        for row in 0..rows {
            table.push_str(&format!(
                "| {row} | Paid **search** | {} | Holding steady; see [sheet](https://docs.google.com/spreadsheets/d/1AbC) |\n",
                400 + row * 3
            ));
        }
        table
    } else {
        big_note_markdown(blocks)
    };
    let (_, doc) = markdown::parse(&source);
    let actual = doc.blocks.len();
    let (id, _) = store.create(doc, None).unwrap();
    let runtime = Arc::new(StoreRuntime::inert());
    let window = cx
        .open_window(size(px(1240.0), px(780.0)), move |_, cx| {
            cx.new(|cx| NotePane::with_store(runtime, Some(store), false, cx))
        })
        .unwrap();
    let session = SessionId::new("s_note");
    cx.update_window(window.into(), |root, window, cx| {
        let pane = root.downcast::<NotePane>().unwrap();
        pane.update(cx, |pane, cx| pane.show(&session, &id, window, cx));
    })
    .unwrap();
    cx.run_until_parked();
    let editor = cx
        .update(|cx| {
            window
                .update(cx, |pane, _, _| pane.editor_for_test())
                .ok()
                .flatten()
        })
        .expect("editor");
    let draw = |cx: &mut HeadlessAppContext| {
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear())
            .unwrap();
    };
    draw(&mut cx);
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    // The debounced autosave, which runs while the user is typing.
    let mut saves = Vec::new();
    for i in 0..20 {
        cx.update_window(window.into(), |root, window, cx| {
            use gpui::EntityInputHandler as _;
            editor.update(cx, |view, cx| {
                view.replace_text_in_range(None, "s", window, cx)
            });
            let pane = root.downcast::<NotePane>().unwrap();
            let start = Instant::now();
            pane.update(cx, |pane, cx| pane.save_with(true, cx));
            if i >= 2 {
                saves.push(start.elapsed());
            }
        })
        .unwrap();
        // Let the background write land before the next save.
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(30));
        cx.run_until_parked();
    }
    saves.sort();
    eprintln!(
        "notes-big autosave-main-thread: median_ms={:.2} max_ms={:.2}",
        ms(saves[saves.len() / 2]),
        ms(*saves.last().unwrap()),
    );
    // The same save as a flush, entirely on the main thread, for scale.
    let mut flushes = Vec::new();
    for i in 0..20 {
        cx.update_window(window.into(), |root, window, cx| {
            use gpui::EntityInputHandler as _;
            editor.update(cx, |view, cx| {
                view.replace_text_in_range(None, "f", window, cx)
            });
            let pane = root.downcast::<NotePane>().unwrap();
            let start = Instant::now();
            pane.update(cx, |pane, cx| pane.save(cx));
            if i >= 2 {
                flushes.push(start.elapsed());
            }
        })
        .unwrap();
    }
    flushes.sort();
    eprintln!(
        "notes-big flush-save-main-thread: median_ms={:.2} max_ms={:.2}",
        ms(flushes[flushes.len() / 2]),
        ms(*flushes.last().unwrap()),
    );
    for case in ["idle", "type", "scroll"] {
        let mut samples = Vec::with_capacity(iterations);
        for i in 0..iterations + 5 {
            let start = Instant::now();
            cx.update_window(window.into(), |_, window, cx| {
                editor.update(cx, |view, cx| match case {
                    "type" => {
                        use gpui::EntityInputHandler as _;
                        if i == 0 {
                            view.editor
                                .set_caret(diri_notes::edit::Pos::new(actual / 2, 0));
                        }
                        view.replace_text_in_range(None, "a", window, cx);
                    }
                    "scroll" => view.scroll_by_for_test(px(-40.0), cx),
                    _ => cx.notify(),
                });
            })
            .unwrap();
            draw(&mut cx);
            if i >= 5 {
                samples.push(start.elapsed());
            }
        }
        samples.sort();
        eprintln!(
            "notes-big {case}: blocks={actual} frames={iterations} median_ms={:.2} p90_ms={:.2} max_ms={:.2}",
            ms(samples[samples.len() / 2]),
            ms(samples[samples.len() * 9 / 10]),
            ms(*samples.last().unwrap()),
        );
    }
}

#[gpui::test]
fn autosave_writes_off_the_main_thread_and_keeps_the_file_current(cx: &mut gpui::TestAppContext) {
    let (_dir, store, id) = store_with_plan();
    let (pane, cx) = pane(cx, store.clone());
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    let editor = editor(&pane, cx);
    editor.update_in(cx, |view, window, cx| {
        let last = view.editor.blocks().len() - 1;
        view.editor.set_caret(diri_notes::edit::Pos::new(last, 0));
        for ch in "typed".chars() {
            view.replace_text_in_range(None, &ch.to_string(), window, cx);
        }
    });
    pane.update(cx, |pane, cx| pane.save_with(true, cx));
    cx.run_until_parked();
    let text = std::fs::read_to_string(store.path_for(&id).unwrap()).unwrap();
    assert!(text.trim_end().ends_with("typed"), "{text}");
    pane.read_with(cx, |pane, _| assert!(pane.error.is_none()));
}

#[gpui::test]
fn the_caret_stops_blinking_without_focus(cx: &mut gpui::TestAppContext) {
    let (_dir, store, id) = store_with_plan();
    let (pane, cx) = pane(cx, store);
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    let editor = editor(&pane, cx);
    // Focused: typing restarts the blink, and it keeps going.
    editor.update_in(cx, |view, window, cx| {
        window.focus(&view.focus_handle(cx), cx);
        view.replace_text_in_range(None, "a", window, cx);
    });
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(2));
    cx.run_until_parked();
    editor.read_with(cx, |view, _| assert!(view.is_blinking()));
    // Focus elsewhere: the loop ends instead of waking the window forever.
    pane.update_in(cx, |pane, window, cx| {
        let other = cx.focus_handle();
        window.focus(&other, cx);
        let _ = pane;
    });
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(2));
    cx.run_until_parked();
    editor.read_with(cx, |view, _| assert!(!view.is_blinking()));
}

#[gpui::test]
fn a_background_autosave_merges_an_outside_write_it_races(cx: &mut gpui::TestAppContext) {
    let (_dir, store, id) = store_with_plan();
    let (pane, cx) = pane(cx, store.clone());
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    let editor = editor(&pane, cx);
    editor.update_in(cx, |view, window, cx| {
        let last = view.editor.blocks().len() - 1;
        view.editor.set_caret(diri_notes::edit::Pos::new(last, 0));
        for ch in "mine".chars() {
            view.replace_text_in_range(None, &ch.to_string(), window, cx);
        }
    });
    // An agent appends before the watcher has told the pane.
    agent_appends(&store, &id, "- [ ] added by an agent");
    pane.update(cx, |pane, cx| pane.save_with(true, cx));
    cx.run_until_parked();
    // The conflict merged the agent's line in and queued another save.
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
    let text = std::fs::read_to_string(store.path_for(&id).unwrap()).unwrap();
    assert!(text.contains("added by an agent"), "{text}");
    assert!(text.contains("mine"), "{text}");
}

#[gpui::test]
fn tables_take_tab_navigation_and_paste_from_a_spreadsheet(cx: &mut gpui::TestAppContext) {
    let (_dir, store, id) = store_with_plan();
    let (pane, cx) = pane(cx, store.clone());
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    let editor = editor(&pane, cx);
    fn type_str(
        view: &mut NoteEditorView,
        text: &str,
        window: &mut Window,
        cx: &mut Context<NoteEditorView>,
    ) {
        for ch in text.chars() {
            view.replace_text_in_range(None, &ch.to_string(), window, cx);
        }
    }
    editor.update_in(cx, |view, window, cx| {
        let last = view.editor.blocks().len() - 1;
        view.editor.set_caret(diri_notes::edit::Pos::new(last, 0));
        // `/table` inserts a 3 × 3 table, caret in its first cell.
        type_str(view, "/table", window, cx);
        view.newline(&editor_view::Newline, window, cx);
        let table = view.editor.caret_table().expect("caret in the new table");
        assert_eq!((table.rows, table.cols, table.row, table.col), (3, 3, 0, 0));
        // Tab walks the cells; Tab in the last cell adds a row.
        for (i, word) in [
            "Channel", "Spend", "CPA", "Search", "$1.2k", "$14", "Social", "$800", "$21",
        ]
        .iter()
        .enumerate()
        {
            type_str(view, word, window, cx);
            view.indent(&editor_view::Indent, window, cx);
            let table = view.editor.caret_table().unwrap();
            if i < 8 {
                assert_eq!(table.rows, 3);
            } else {
                assert_eq!((table.rows, table.row, table.col), (4, 3, 0));
            }
        }
        view.outdent(&editor_view::Outdent, window, cx);
        assert_eq!(view.editor.caret_table().unwrap().row, 2);
    });
    // Rows copied from Google Sheets paste into a new table below.
    editor.update_in(cx, |view, _, _| {
        let last = view.editor.blocks().len() - 1;
        view.editor.set_caret(diri_notes::edit::Pos::new(last, 0));
    });
    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
        "Week\tSignups\tActivation\n1\t420\t31%\n2\t515\t\"34%, rising\"\n".into(),
    ));
    editor.update_in(cx, |view, window, cx| {
        view.paste(&editor_view::Paste, window, cx);
        let table = view.editor.caret_table().expect("pasted as a table");
        assert_eq!((table.rows, table.cols), (3, 3));
        // Copying cells inside the table puts TSV on the pasteboard.
        let start = table.index(1, 1);
        let end = table.index(2, 2);
        view.editor.set_selection(diri_notes::edit::Selection {
            anchor: diri_notes::edit::Pos::new(start, 0),
            head: diri_notes::edit::Pos::new(end, view.editor.block(end).text.len()),
        });
        view.copy(&editor_view::Copy, window, cx);
    });
    assert_eq!(
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .as_deref(),
        Some("420\t31%\n515\t34%, rising")
    );
    pane.update(cx, |pane, cx| pane.save(cx));
    let text = std::fs::read_to_string(store.path_for(&id).unwrap()).unwrap();
    assert!(text.contains("| Channel | Spend | CPA |"), "{text}");
    assert!(text.contains("| Week | Signups | Activation  |"), "{text}");
    assert!(text.contains("| 2    | 515     | 34%, rising |"), "{text}");
}

/// The table from the user's screenshot, written the way an agent writes
/// one: loose pipes, no padding, marks and chips in cells.
#[cfg(target_os = "macos")]
pub(crate) const AGENT_GAPS: &str = "# Launch readiness

Where the Q4 launch stands against the 5/5 bar.

| What's missing | Type | Requirement for 5/5 |
|---|---|---|
| Onboarding email sequence | Content | 3 emails, **A/B tested** on subject lines |
| Pricing page experiment | Experiment | Significant at 95%, see [Q4 brief](https://www.notion.so/acme/Q4-brief-1f2e3d4c5b6a79881f2e3d4c5b6a7988) |
| Attribution dashboard | Analytics | Weekly [funnel sheet](https://docs.google.com/spreadsheets/d/1AbC/edit) owned by growth |
| Sales handoff | Process | [HubSpot deal](https://app.hubspot.com/contacts/1/record/0-3/2) stages mapped |
| Launch QA | Engineering | [@Codex: fix resize flicker](diri://session/s_codex) merged |

Next: review with the team on Friday.
";

/// A table wider than the note's column: it scrolls sideways in its block.
#[cfg(target_os = "macos")]
pub(crate) const WIDE_TABLE: &str = "# Channel plan

| Channel | Owner | Budget | Target CPA | Q1 | Q2 | Q3 | Q4 | Notes |
|:--|:--|--:|--:|--:|--:|--:|--:|:--|
| Paid search | Ana | $48,000 | $14 | 11,200 | 12,400 | 13,100 | 15,800 | Brand terms capped at 20% |
| Paid social | Ravi | $36,000 | $21 | 7,900 | 8,300 | 9,800 | 12,100 | Creative refresh every 3 weeks |
| Lifecycle email | Mei | $6,000 | $3 | 4,100 | 4,600 | 5,200 | 6,900 | Win-back flow launches in Q2 |
| Partnerships | Tom | $12,000 | $9 | 1,200 | 2,600 | 3,100 | 4,400 | Two co-marketing launches |
";

#[gpui::test]
fn arrows_move_by_row_and_leave_the_table_at_its_edges(cx: &mut gpui::TestAppContext) {
    let (_dir, store, id) = store_with_plan();
    let (pane, cx) = pane(cx, store);
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    let editor = editor(&pane, cx);
    editor.update_in(cx, |view, _, cx| {
        let (_, doc) = markdown::parse(
            "# T\n\nabove\n\n| a | b |\n| - | - |\n| 1 | 2 |\n| 3 | 4 |\n\nbelow\n",
        );
        view.reload(diri_notes::edit::Editor::new(&doc), cx);
        // "b", the header's second cell.
        view.editor.set_caret(diri_notes::edit::Pos::new(3, 1));
    });
    cx.run_until_parked();
    let step = |cx: &mut gpui::VisualTestContext, down: bool| {
        editor.update_in(cx, |view, _, cx| view.vertical(down, false, cx));
        cx.run_until_parked();
        editor.read_with(cx, |view, _| view.editor.selection.head.block)
    };
    // Same column, one row at a time; past the last row, out below.
    assert_eq!(step(cx, true), 5, "2");
    assert_eq!(step(cx, true), 7, "4");
    assert_eq!(step(cx, true), 8, "below");
    // And back up into the last row, then out above the header.
    assert!(matches!(step(cx, false), 6 | 7));
    editor.update_in(cx, |view, _, _| {
        view.editor.set_caret(diri_notes::edit::Pos::new(2, 0))
    });
    cx.run_until_parked();
    assert_eq!(step(cx, false), 1, "above");
}

#[gpui::test]
fn edits_and_direct_file_changes_land_while_the_person_types(cx: &mut gpui::TestAppContext) {
    let (_dir, store, id) = store_with_plan();
    let (pane, cx) = pane(cx, store.clone());
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    let editor = editor(&pane, cx);
    editor.update(cx, |view, _| {
        let at = view
            .editor
            .blocks()
            .iter()
            .position(|b| b.text == "Keep files flat")
            .unwrap();
        view.editor
            .set_caret(diri_notes::edit::Pos::new(at, "Keep files flat".len()));
    });
    type_text(&editor, " (decided)", cx);

    // An agent's edit_note: change existing text in place.
    store
        .update(
            &id,
            &diri_notes::history::Author::Session("s_agent".into()),
            |note| {
                diri_notes::text_edit::edit(
                    note,
                    "Quick capture from anywhere",
                    "Quick capture with ⌥⌘N",
                    false,
                )
                .map(|_| ())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
            },
        )
        .unwrap();
    pane.update(cx, |pane, cx| pane.reconcile(cx));
    type_text(&editor, "!", cx);

    // Then an agent edits the .md with its own file tools, dropping the
    // front matter on the way.
    let path = store.path_for(&id).unwrap();
    let on_disk = std::fs::read_to_string(&path).unwrap();
    let body = on_disk
        .split_once("\n---\n")
        .map_or(on_disk.as_str(), |(_, b)| b);
    std::fs::write(
        &path,
        body.replace("Atomic file store", "Atomic, locked file store"),
    )
    .unwrap();
    pane.update(cx, |pane, cx| pane.reconcile(cx));
    type_text(&editor, "!", cx);
    pane.update(cx, |pane, cx| pane.save(cx));

    let file = std::fs::read_to_string(&path).unwrap();
    assert!(file.contains("Keep files flat (decided)!!"), "{file}");
    assert!(file.contains("Quick capture with ⌥⌘N"), "{file}");
    assert!(file.contains("Atomic, locked file store"), "{file}");
    assert!(
        file.starts_with(&format!("---\nid: {id}\n")),
        "identity repaired:\n{file}"
    );
    let versions = store.history().list(&id).unwrap();
    assert!(
        versions
            .iter()
            .any(|v| v.author == diri_notes::history::Author::File)
    );
    assert!(
        versions
            .iter()
            .any(|v| v.author == diri_notes::history::Author::Session("s_agent".into()))
    );
}

/// The backlinks start below the note's last row, however much its text
/// wraps. Regression: the page took its height from text measured wider than
/// it is drawn, so on a long note of wrapped list items the backlinks painted
/// over the last paragraphs and the note would not scroll to its end.
#[gpui::test]
fn backlinks_sit_below_the_last_row_of_wrapped_text(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(NoteStore::open(dir.path().join("notes")).expect("store"));
    let sentence = "Never overwrite the real app, test only in the temporary folder, \
        and stop for any password prompt rather than guessing. ";
    let mut md = String::from("# Handoff\n\n");
    for i in 0..12 {
        md.push_str(&format!("- Step {i}. {}\n", sentence.repeat(3)));
    }
    md.push_str(&format!("\nLast paragraph. {}\n", sentence.repeat(4)));
    let (_, doc) = markdown::parse(&md);
    let (id, _) = store.create(doc, None).expect("create");
    let (pane, cx) = pane(cx, store);
    cx.simulate_resize(gpui::size(px(900.0), px(600.0)));
    pane.update_in(cx, |pane, window, cx| {
        pane.show(&SessionId::new("s_note"), &id, window, cx)
    });
    let editor = editor(&pane, cx);
    for _ in 0..40 {
        editor.update(cx, |view, cx| view.scroll_by_for_test(px(-400.0), cx));
        cx.run_until_parked();
    }
    let last = editor.read_with(cx, |view, _| view.editor.blocks().last().unwrap().id);
    let selector: &'static str = format!("note-row-{last}").leak();
    let row = cx
        .debug_bounds(selector)
        .expect("the last row is on screen");
    let footer = cx.debug_bounds("note-footer").expect("backlinks shown");
    assert!(
        footer.top() >= row.bottom(),
        "backlinks at {:?} overlap the last row ending at {:?}",
        footer.top(),
        row.bottom()
    );
}

/// A trackpad gesture keeps to the axis it started on. Regression: a
/// sideways swipe scrolled the page instead of the table under it, since the
/// page turned horizontal motion vertical, and the table turned vertical
/// wheel motion sideways.
#[gpui::test]
fn wheel_gestures_keep_to_their_axis(cx: &mut gpui::TestAppContext) {
    use gpui::{ScrollDelta, ScrollWheelEvent, TouchPhase, point};
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(NoteStore::open(dir.path().join("notes")).expect("store"));
    let cell = "a cell with more text than any column has room for";
    let mut md = String::from("# Wide\n\n");
    md.push_str(&format!("|{}\n|{}\n", " Head |".repeat(8), "---|".repeat(8)));
    for _ in 0..3 {
        md.push_str(&format!("|{}\n", format!(" {cell} |").repeat(8)));
    }
    for i in 0..60 {
        md.push_str(&format!("\nParagraph {i}.\n"));
    }
    let (_, doc) = markdown::parse(&md);
    let (id, _) = store.create(doc, None).expect("create");
    let (pane, cx) = pane(cx, store);
    cx.simulate_resize(gpui::size(px(900.0), px(600.0)));
    pane.update_in(cx, |pane, window, cx| {
        pane.show(&SessionId::new("s_note"), &id, window, cx)
    });
    cx.run_until_parked();
    let editor = editor(&pane, cx);
    // The caret starts at the note's end; bring the table back into view.
    editor.update(cx, |view, cx| view.scroll_by_for_test(px(1e6), cx));
    cx.run_until_parked();
    let table = editor.read_with(cx, |view, _| {
        let scrolls = view.table_scrolls_for_test();
        assert_eq!(scrolls.len(), 1);
        scrolls[0].clone()
    });
    let page = editor.read_with(cx, |view, _| view.scroll_for_test());
    assert!(table.max_offset().x > px(0.0), "the table is wider than the page");
    assert!(page.max_offset().y > px(0.0), "the note scrolls");
    let gesture = |cx: &mut gpui::VisualTestContext, at, dx: f32, dy: f32| {
        for (touch_phase, delta) in [
            (TouchPhase::Started, point(px(0.0), px(0.0))),
            (TouchPhase::Moved, point(px(dx), px(dy))),
            // Off-axis drift later in the gesture stays locked out.
            (TouchPhase::Moved, point(px(dy), px(dx))),
            (TouchPhase::Ended, point(px(0.0), px(0.0))),
        ] {
            cx.simulate_event(ScrollWheelEvent {
                position: at,
                delta: ScrollDelta::Pixels(delta),
                touch_phase,
                ..ScrollWheelEvent::default()
            });
        }
    };

    gesture(cx, table.bounds().center(), -60.0, -4.0);
    assert_eq!(table.offset().x, px(-64.0));
    assert_eq!(page.offset().y, px(0.0), "a sideways swipe left the page");

    gesture(cx, table.bounds().center(), 3.0, -50.0);
    assert_eq!(page.offset().y, px(-47.0));
    assert_eq!(table.offset().x, px(-64.0), "page scrolling left the table");

    let below = point(table.bounds().center().x, table.bounds().bottom() + px(200.0));
    gesture(cx, below, -60.0, -4.0);
    assert_eq!(page.offset().y, px(-47.0), "a sideways swipe left the page");
    assert_eq!(table.offset().x, px(-64.0));

    // A mouse wheel over the table scrolls the page, as on the web.
    cx.simulate_event(ScrollWheelEvent {
        position: table.bounds().center(),
        delta: ScrollDelta::Lines(point(0.0, -1.0)),
        ..ScrollWheelEvent::default()
    });
    assert!(page.offset().y < px(-47.0));
    assert_eq!(table.offset().x, px(-64.0));
}

/// Text under the reader moves exactly as far as the scroll. Regression:
/// a long note opens at its end, and scrolling up swapped each block's
/// estimated height for its measured one as it entered the layout window,
/// so the text jumped by a line or two every few steps. The backlinks under
/// the note move with it, not one frame behind the correction.
#[gpui::test]
fn scrolling_up_a_long_note_never_jumps(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(NoteStore::open(dir.path().join("notes")).expect("store"));
    let (_, doc) = markdown::parse(&big_note_markdown(400));
    let (id, _) = store.create(doc, None).expect("create");
    let (pane, cx) = pane(cx, store);
    cx.simulate_resize(gpui::size(px(900.0), px(700.0)));
    pane.update_in(cx, |pane, window, cx| {
        pane.show(&SessionId::new("s_note"), &id, window, cx)
    });
    cx.run_until_parked();
    let editor = editor(&pane, cx);
    // Draw the frame the caret's autoscroll asked for.
    editor.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    let page = editor.read_with(cx, |view, _| view.scroll_for_test());
    assert!(page.offset().y < px(-2000.0), "the note opens at its end");
    let ids: Vec<u64> = editor.read_with(cx, |view, _| {
        view.editor.blocks().iter().map(|b| b.id).collect()
    });
    let mut steps = 0;
    let mut footer_steps = 0;
    while page.offset().y < px(-40.0) {
        let footer = cx
            .debug_bounds("note-footer")
            .map(|bounds| bounds.top())
            .filter(|top| *top < px(700.0));
        // A row in the middle of the viewport, before and after one step.
        let (selector, top) = ids
            .iter()
            .rev()
            .find_map(|id| {
                let selector: &'static str = format!("note-row-{id}").leak();
                let bounds = cx.debug_bounds(selector)?;
                (bounds.top() > px(200.0) && bounds.top() < px(500.0))
                    .then_some((selector, bounds.top()))
            })
            .expect("a row on screen");
        editor.update(cx, |view, cx| view.scroll_by_for_test(px(40.0), cx));
        cx.run_until_parked();
        let moved = cx.debug_bounds(selector).expect("still laid out").top() - top;
        assert!(
            (moved - px(40.0)).abs() < px(0.5),
            "step {steps}: the text moved {moved:?} for a 40px scroll"
        );
        if let Some(top) = footer
            && let Some(after) = cx.debug_bounds("note-footer")
        {
            let moved = after.top() - top;
            assert!(
                (moved - px(40.0)).abs() < px(0.5),
                "step {steps}: the backlinks moved {moved:?} for a 40px scroll"
            );
            footer_steps += 1;
        }
        steps += 1;
    }
    assert!(steps > 50);
    assert!(footer_steps > 0, "the backlinks were on screen at the end");
}

/// Only the row under the pointer builds its `+` and grip, and the handle
/// follows rows that scroll under a resting pointer.
#[gpui::test]
fn the_block_handle_follows_the_pointer(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(NoteStore::open(dir.path().join("notes")).expect("store"));
    let (_, doc) = markdown::parse(&big_note_markdown(120));
    let (id, _) = store.create(doc, None).expect("create");
    let (pane, cx) = pane(cx, store);
    cx.simulate_resize(gpui::size(px(900.0), px(700.0)));
    pane.update_in(cx, |pane, window, cx| {
        pane.show(&SessionId::new("s_note"), &id, window, cx)
    });
    let editor = editor(&pane, cx);
    editor.update(cx, |view, cx| view.scroll_by_for_test(px(1e6), cx));
    cx.run_until_parked();
    let ids: Vec<u64> = editor.read_with(cx, |view, _| {
        view.editor.blocks().iter().map(|b| b.id).collect()
    });
    let row = |cx: &mut gpui::VisualTestContext, id: u64| {
        let selector: &'static str = format!("note-row-{id}").leak();
        cx.debug_bounds(selector)
    };
    let handles = |cx: &mut gpui::VisualTestContext| {
        ids.iter()
            .copied()
            .filter(|id| {
                let selector: &'static str = format!("note-handle-{id}").leak();
                cx.debug_bounds(selector).is_some()
            })
            .collect::<Vec<_>>()
    };
    assert!(handles(cx).is_empty(), "no row is hovered yet");
    let at = row(cx, ids[3]).expect("row on screen").center();
    cx.simulate_mouse_move(at, None, gpui::Modifiers::default());
    cx.run_until_parked();
    assert_eq!(handles(cx), vec![ids[3]]);

    // Scroll so another row sits under the still pointer.
    editor.update(cx, |view, cx| view.scroll_by_for_test(px(-200.0), cx));
    cx.run_until_parked();
    // The frame that paint asked for, as the display link delivers it.
    cx.update(|window, cx| window.simulate_next_frame(cx));
    cx.run_until_parked();
    let under = ids
        .iter()
        .copied()
        .find(|&id| row(cx, id).is_some_and(|b| b.contains(&at)))
        .expect("a row under the pointer");
    assert_ne!(under, ids[3]);
    assert_eq!(handles(cx), vec![under]);
}

/// Switching notes writes the one left behind off the main thread, and an
/// outside write that landed meanwhile is merged in, not overwritten.
#[gpui::test]
fn switching_notes_saves_the_last_one_off_the_main_thread(cx: &mut gpui::TestAppContext) {
    let (_dir, store, id) = store_with_plan();
    let (_, doc) = markdown::parse("# Other\n\nSomething else.\n");
    let (other, _) = store.create(doc, None).expect("create");
    let (pane, cx) = pane(cx, store.clone());
    let session = SessionId::new("s_note");
    pane.update_in(cx, |pane, window, cx| pane.show(&session, &id, window, cx));
    let editor = editor(&pane, cx);
    editor.update_in(cx, |view, window, cx| {
        let last = view.editor.blocks().len() - 1;
        view.editor.set_caret(diri_notes::edit::Pos::new(last, 0));
        for ch in "typed before leaving".chars() {
            view.replace_text_in_range(None, &ch.to_string(), window, cx);
        }
    });
    store
        .append(
            &id,
            "- [ ] added by an agent",
            &diri_notes::history::Author::Session("s_agent".into()),
        )
        .unwrap();
    let path = store.path_for(&id).unwrap();
    let before = std::fs::read_to_string(&path).unwrap();
    pane.update_in(cx, |pane, window, cx| {
        pane.show(&session, &other, window, cx);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "the switch did not wait on the write"
        );
    });
    cx.run_until_parked();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("typed before leaving"), "{text}");
    assert!(text.contains("- [ ] added by an agent"), "{text}");
}

/// Heights measured at one column width do not size spacers at another.
#[gpui::test]
fn a_new_width_forgets_measured_heights(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(NoteStore::open(dir.path().join("notes")).expect("store"));
    let (_, doc) = markdown::parse(&big_note_markdown(120));
    let (id, _) = store.create(doc, None).expect("create");
    let (pane, cx) = pane(cx, store);
    cx.simulate_resize(gpui::size(px(900.0), px(700.0)));
    pane.update_in(cx, |pane, window, cx| {
        pane.show(&SessionId::new("s_note"), &id, window, cx)
    });
    cx.run_until_parked();
    let editor = editor(&pane, cx);
    editor.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    let measured = editor.read_with(cx, |view, _| view.measured_rows_for_test());
    assert!(measured > 0);
    // Narrower than the text column: every row re-wraps.
    cx.simulate_resize(gpui::size(px(500.0), px(700.0)));
    cx.run_until_parked();
    editor.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    let after = editor.read_with(cx, |view, _| view.measured_rows_for_test());
    assert!(
        after < measured,
        "rows measured at the old width were kept: {after} of {measured}"
    );
}

/// A right click on a table cell opens the table menu under the pointer
/// with the caret in that cell. Opened from the keyboard after the table was
/// scrolled away from the caret, the menu's anchor, the caret, is scrolled
/// back into view first. Regression: the menu opened at a caret scrolled
/// out of the table, out in the page margin.
#[gpui::test]
fn the_table_menu_opens_where_it_was_asked_for(cx: &mut gpui::TestAppContext) {
    use gpui::{Modifiers, MouseButton, MouseDownEvent, ScrollDelta, ScrollWheelEvent, point};
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(NoteStore::open(dir.path().join("notes")).expect("store"));
    let cell = "a cell with more text than any column has room for";
    let mut md = String::from("# Wide\n\n");
    md.push_str(&format!("|{}\n|{}\n", " Head |".repeat(8), "---|".repeat(8)));
    for _ in 0..3 {
        md.push_str(&format!("|{}\n", format!(" {cell} |").repeat(8)));
    }
    md.push_str("\nAfter the table.\n");
    let (_, doc) = markdown::parse(&md);
    let (id, _) = store.create(doc, None).expect("create");
    let (pane, cx) = pane(cx, store);
    cx.simulate_resize(gpui::size(px(900.0), px(600.0)));
    pane.update_in(cx, |pane, window, cx| {
        pane.show(&SessionId::new("s_note"), &id, window, cx)
    });
    cx.run_until_parked();
    let editor = editor(&pane, cx);
    editor.update(cx, |view, cx| view.scroll_by_for_test(px(1e6), cx));
    cx.run_until_parked();
    let table = editor.read_with(cx, |view, _| view.table_scrolls_for_test()[0].clone());

    let at = point(table.bounds().left() + px(40.0), table.bounds().center().y);
    cx.simulate_event(MouseDownEvent {
        button: MouseButton::Right,
        position: at,
        modifiers: Modifiers::default(),
        click_count: 1,
        first_mouse: false,
    });
    cx.run_until_parked();
    let (anchor, in_table) = editor.read_with(cx, |view, _| {
        (
            view.table_menu_for_test(),
            view.editor.caret_table().is_some(),
        )
    });
    assert_eq!(anchor.flatten().map(|b| b.origin), Some(at));
    assert!(in_table, "the caret went into the clicked cell");

    // Swipe the table well away from the caret's column, then open the
    // menu from the keyboard.
    editor.update(cx, |view, cx| view.close_table_menu_for_test(cx));
    cx.run_until_parked();
    for (touch_phase, dx) in [
        (gpui::TouchPhase::Started, 0.0),
        (gpui::TouchPhase::Moved, -1500.0),
        (gpui::TouchPhase::Ended, 0.0),
    ] {
        cx.simulate_event(ScrollWheelEvent {
            position: table.bounds().center(),
            delta: ScrollDelta::Pixels(point(px(dx), px(0.0))),
            touch_phase,
            ..ScrollWheelEvent::default()
        });
    }
    cx.run_until_parked();
    assert!(table.offset().x < px(-1000.0));
    editor.update(cx, |view, cx| view.open_table_menu_from_keys_for_test(cx));
    cx.run_until_parked();
    let caret = editor
        .read_with(cx, |view, _| view.caret_bounds_for_test())
        .expect("caret laid out");
    let view = table.bounds();
    assert!(
        caret.left() >= view.left() && caret.right() <= view.right(),
        "caret {caret:?} is outside the table's view {view:?}"
    );
}
