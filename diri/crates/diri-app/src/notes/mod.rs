//! Diri Notes inside the main window.
//!
//! A note is a Session whose kind is `note`: it sits in the sidebar, sorts,
//! pins, archives, and nests exactly like an agent or a terminal, and a
//! session started from it is its child. When one is selected, RootView shows
//! this pane where the terminal would be. The pane owns the open note's file
//! (via `diri_notes::store`), saves continuously, keeps the Session's title in
//! step with the note's, and reloads writes made by the CLI or agents.

pub(crate) mod backlinks;
pub(crate) mod chip;
pub(crate) mod editor_view;
pub(crate) mod find;
pub(crate) mod graph;
#[cfg(test)]
mod links_tests;
pub(crate) mod search;
#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod todos;
mod versions;
pub(crate) mod work_item;
#[cfg(test)]
pub(crate) mod work_item_tests;

use std::sync::Arc;
use std::time::Duration;

use diri_notes::edit::Editor;
use diri_notes::markdown::FrontMatter;
use diri_notes::mention::{self as mentions, Candidate, MentionTarget};
use diri_notes::store::{self, Note, NoteStore};
use diri_proto::SessionId;
use diri_ui::SemanticColors;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, KeyBinding, Render, SharedString, Subscription,
    Task, Window, div, prelude::*, px,
};

use crate::store::StoreRuntime;
use editor_view::{EditorEvent, MentionDirectory, MentionEntry, NoteEditorView};

const SAVE_DEBOUNCE: Duration = Duration::from_millis(350);
const WATCH_DEBOUNCE: Duration = Duration::from_millis(120);

pub(crate) fn key_bindings() -> Vec<KeyBinding> {
    editor_view::key_bindings()
}

struct OpenNote {
    session: SessionId,
    id: String,
    front: FrontMatter,
    editor: Entity<NoteEditorView>,
    /// Markdown last written to (or read from) disk, to tell our own writes
    /// from outside edits when the watcher fires.
    saved: String,
    dirty: bool,
    /// The title last pushed to the Session, so renames go out once.
    synced_title: String,
    /// Markdown an autosave is writing off the main thread right now: the
    /// watcher and a flush take the file holding it as our own write.
    in_flight: Option<String>,
    /// Typing arrived while a write was in flight: save again when it lands.
    resave: bool,
    _subscription: Subscription,
}

enum PaneState {
    Empty,
    Open(OpenNote),
    Missing { session: SessionId, id: String },
}

pub(crate) struct NotePane {
    runtime: Arc<StoreRuntime>,
    store: Option<Arc<NoteStore>>,
    state: PaneState,
    focus: FocusHandle,
    focus_on_show: bool,
    save_task: Task<()>,
    _watch_task: Task<()>,
    _watcher: Option<notify::RecommendedWatcher>,
    /// Keeps mention chips' session status and the `@` menu live.
    _sessions_task: Task<()>,
    error: Option<SharedString>,
    /// The open note's Version History panel, while it is shown.
    versions: Option<versions::VersionPanel>,
    /// The open note's linked and unlinked mentions, shown under it.
    backlinks: Option<(Entity<backlinks::BacklinksView>, Subscription)>,
    /// The notes graph, while it is shown in place of the note.
    graph: Option<Entity<graph::NoteGraphView>>,
    /// Room the host keeps at the top-right for its pane controls.
    trailing_inset: f32,
    /// A note to put the caret in once it is shown: (note id, block), from a
    /// backlink or the graph.
    pending_reveal: Option<(String, usize)>,

    /// Fixture palette; live panes follow the store's theme.
    colors_override: Option<SemanticColors>,
    /// Prompts Start sent to mentioned sessions, for tests.
    #[cfg(test)]
    pub(crate) sent_for_test: Vec<crate::notifications::SendTextCommand>,
}

impl Focusable for NotePane {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if let Some(graph) = &self.graph {
            return graph.read(cx).focus_handle(cx);
        }
        match &self.state {
            PaneState::Open(open) => open.editor.read(cx).focus_handle(cx),
            _ => self.focus.clone(),
        }
    }
}

fn editor_view_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub(crate) enum NotePaneEvent {
    /// Escape with nothing left to dismiss in the editor.
    Dismiss,
    /// A mention chip was clicked: show that session (an agent, a
    /// terminal, or another note).
    Reveal(SessionId),
}

impl gpui::EventEmitter<NotePaneEvent> for NotePane {}

impl NotePane {
    pub(crate) fn new(runtime: Arc<StoreRuntime>, cx: &mut Context<Self>) -> Self {
        let store = NoteStore::resolve_dir()
            .and_then(|dir| NoteStore::open(dir).ok())
            .map(Arc::new);
        Self::with_store(runtime, store, true, cx)
    }

    /// `watch` is off only in deterministic tests, whose scheduler rejects
    /// wakeups from the file watcher's own thread.
    pub(crate) fn with_store(
        runtime: Arc<StoreRuntime>,
        store: Option<Arc<NoteStore>>,
        watch: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let (watcher, watch_task) = match (&store, watch) {
            (Some(store), true) => Self::watch(store, cx),
            _ => (None, Task::ready(())),
        };
        let sessions_task = if watch {
            Self::follow_sessions(&runtime, cx)
        } else {
            Task::ready(())
        };
        Self {
            runtime,
            store,
            state: PaneState::Empty,
            focus: cx.focus_handle(),
            focus_on_show: false,
            save_task: Task::ready(()),
            _watch_task: watch_task,
            _watcher: watcher,
            _sessions_task: sessions_task,
            error: None,
            versions: None,
            backlinks: None,
            graph: None,
            trailing_inset: 0.0,
            pending_reveal: None,

            colors_override: None,
            #[cfg(test)]
            sent_for_test: Vec::new(),
        }
    }

    fn colors(&self) -> SemanticColors {
        self.colors_override.unwrap_or_else(|| {
            crate::app_theme::colors_in(&self.runtime.store.read().expect("store"))
        })
    }

    #[cfg(test)]
    pub(crate) fn editor_for_test(&self) -> Option<Entity<NoteEditorView>> {
        match &self.state {
            PaneState::Open(open) => Some(open.editor.clone()),
            _ => None,
        }
    }

    /// Focus the editor the next time a note is shown (a new note, a click).
    pub(crate) fn request_focus(&mut self) {
        self.focus_on_show = true;
    }

    /// Shows `note_id` for `session`; switching away saves the previous one.
    pub(crate) fn show(
        &mut self,
        session: &SessionId,
        note_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let same = match &self.state {
            PaneState::Open(open) => open.session == *session && open.id == note_id,
            PaneState::Missing { session: s, id } => s == session && id == note_id,
            PaneState::Empty => false,
        };
        if !same {
            self.save(cx);
            self.load(session, note_id, window, cx);
        }
        if let Some(graph) = &self.graph {
            graph.update(cx, |graph, cx| graph.set_center(note_id, cx));
        }
        if self
            .pending_reveal
            .as_ref()
            .is_some_and(|(id, _)| id == note_id)
            && let Some((_, block)) = self.pending_reveal.take()
        {
            self.reveal_block(block, window, cx);
            return;
        }
        // Arrived from a session's header: show the to-do it works on.
        let reveal = self
            .runtime
            .store
            .write()
            .expect("store")
            .take_note_reveal(session);
        if let (Some(child), PaneState::Open(open)) = (reveal, &self.state) {
            let editor = open.editor.clone();
            if editor.update(cx, |view, cx| view.reveal_session(&child.0, cx)) {
                self.focus_on_show = true;
            }
        }
        if std::mem::take(&mut self.focus_on_show) {
            let handle = self.focus_handle(cx);
            window.focus(&handle, cx);
        }
    }

    /// Caret to the end of a note block (index in the file, title excluded)
    /// and focus the editor, as a jump from the To-dos page.
    pub(crate) fn reveal_block(
        &mut self,
        block: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let PaneState::Open(open) = &self.state else {
            return;
        };
        open.editor.update(cx, |view, cx| {
            let index = (block + 1).min(view.editor.blocks().len() - 1);
            let len = view.editor.block(index).text.len();
            view.editor
                .set_caret(diri_notes::edit::Pos::new(index, len));
            cx.notify();
        });
        let handle = self.focus_handle(cx);
        window.focus(&handle, cx);
    }

    fn load(
        &mut self,
        session: &SessionId,
        note_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.versions = None;
        let source = self
            .store
            .as_ref()
            .and_then(|store| store.path_for(note_id).ok())
            .and_then(|path| std::fs::read_to_string(path).ok());
        let Some(source) = source else {
            self.backlinks = None;
            self.state = PaneState::Missing {
                session: session.clone(),
                id: note_id.to_owned(),
            };
            cx.notify();
            return;
        };
        let note = store::parse_note(&source);
        let colors = self.colors();
        let assets = self.store.clone().map(|store| editor_view::AssetHome {
            store,
            note_id: note_id.to_owned(),
        });
        let editor = cx.new(|cx| {
            let mut view = NoteEditorView::new(Editor::new(&note.doc), colors, cx);
            if let Some(assets) = assets {
                view.set_asset_home(assets);
            }
            view.fold_started_work();
            view
        });
        let model = todos::TodosModel::global(&self.runtime, cx);
        // The links index is read off the main thread; make sure a first
        // read is under way (a no-op when it is current).
        model.update(cx, |model, cx| model.sync(cx));
        let footer =
            cx.new(|cx| backlinks::BacklinksView::new(model, note_id.to_owned(), colors, cx));
        let footer_subscription = cx.subscribe_in(&footer, window, |this, _, event, window, cx| {
            this.on_backlinks(event, window, cx)
        });
        editor.update(cx, |view, _| view.set_footer(Some(footer.clone().into())));
        let subscription =
            cx.subscribe_in(&editor, window, |this, _, event, window, cx| match event {
                EditorEvent::Changed => this.schedule_save(cx),
                EditorEvent::Dismiss => {
                    // Escape closes the graph, then the history panel, before it
                    // leaves the note.
                    if this.graph.is_some() {
                        this.close_graph(window, cx);
                        return;
                    }
                    if this.versions_open() {
                        this.close_versions(cx);
                        return;
                    }
                    this.save(cx);
                    cx.emit(NotePaneEvent::Dismiss);
                }
                EditorEvent::OpenMention(target) => this.open_mention(target, cx),
                EditorEvent::Work(request) => this.on_work(request, cx),
            });
        let synced_title = self
            .runtime
            .store
            .read()
            .expect("store")
            .sessions()
            .get(session)
            .map(|record| record.title.clone())
            .unwrap_or_default();
        self.state = PaneState::Open(OpenNote {
            session: session.clone(),
            id: note_id.to_owned(),
            front: note.front,
            editor,
            saved: source,
            dirty: false,
            synced_title,
            in_flight: None,
            resave: false,
            _subscription: subscription,
        });
        self.backlinks = Some((footer, footer_subscription));
        self.push_mentions(cx);
        self.push_work(cx);
        cx.notify();
    }

    fn follow_sessions(runtime: &Arc<StoreRuntime>, cx: &mut Context<Self>) -> Task<()> {
        let mut changes = runtime.changes();
        cx.spawn(async move |this, cx| {
            loop {
                match changes.recv().await {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if this
                            .update(cx, |this, cx| {
                                this.push_mentions(cx);
                                this.push_work(cx);
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        })
    }

    /// Everything the open note can mention, rebuilt from the session store.
    pub(crate) fn mention_directory(&self) -> MentionDirectory {
        let open = match &self.state {
            PaneState::Open(open) => Some(open.session.clone()),
            _ => None,
        };
        let store = self.runtime.store.read().expect("store");
        let entries = mention_entries(&store, open.as_ref());
        MentionDirectory { entries }
    }

    /// Hands the open editor a fresh directory; a no-op when nothing changed.
    pub(crate) fn push_mentions(&mut self, cx: &mut Context<Self>) {
        let PaneState::Open(open) = &self.state else {
            return;
        };
        let editor = open.editor.clone();
        let directory = self.mention_directory();
        editor.update(cx, |view, cx| view.set_mentions(directory, cx));
    }

    fn open_mention(&mut self, target: &MentionTarget, cx: &mut Context<Self>) {
        let session = {
            let store = self.runtime.store.read().expect("store");
            match target {
                MentionTarget::Session(id) => {
                    let id = SessionId::new(id.clone());
                    store.sessions().contains_key(&id).then_some(id)
                }
                MentionTarget::Note(note) => store
                    .sessions()
                    .values()
                    .find(|s| s.is_note() && s.note_id.as_deref() == Some(note))
                    .map(|s| s.id.clone()),
            }
        };
        if let Some(session) = session {
            self.save(cx);
            cx.emit(NotePaneEvent::Reveal(session));
        }
    }

    /// Opens a note by its file id, with the caret on `block` once shown.
    fn open_note_at(&mut self, note_id: &str, block: Option<usize>, cx: &mut Context<Self>) {
        if let PaneState::Open(open) = &self.state
            && open.id == note_id
        {
            return;
        }
        self.pending_reveal = block.map(|block| (note_id.to_owned(), block));
        self.open_mention(&MentionTarget::Note(note_id.to_owned()), cx);
    }

    fn on_backlinks(
        &mut self,
        event: &backlinks::BacklinksEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            backlinks::BacklinksEvent::Open { note_id, block } => {
                crate::telemetry::notes_event("notes.backlink.opened", "");
                self.open_note_at(note_id, Some(*block), cx);
            }
            backlinks::BacklinksEvent::Link(reference) => self.link_unlinked(reference, cx),
            backlinks::BacklinksEvent::ShowGraph => {
                self.show_graph(graph::Scope::Local, window, cx)
            }
        }
    }

    /// Links an unlinked mention of the open note, in the other note's file,
    /// through the locked store path (so an editor open on it merges it).
    pub(crate) fn link_unlinked(
        &mut self,
        reference: &diri_notes::backlinks::Reference,
        cx: &mut Context<Self>,
    ) {
        let (Some(store), PaneState::Open(open)) = (self.store.clone(), &self.state) else {
            return;
        };
        let target = open.id.clone();
        let footer = self.backlinks.as_ref().map(|(footer, _)| footer.clone());
        let expected = reference
            .context
            .get(reference.mention.clone())
            .unwrap_or_default()
            .to_owned();
        let linked = store.update(
            &reference.source,
            &diri_notes::history::Author::User,
            |note| {
                Ok(diri_notes::backlinks::link_mention(
                    &mut note.doc,
                    reference.block,
                    reference.range.clone(),
                    &expected,
                    &target,
                ))
            },
        );
        match linked {
            Ok((_, true)) => {
                crate::telemetry::notes_event("notes.backlink.linked", "");
                if let Some(footer) = footer {
                    footer.update(cx, |view, cx| view.forget_unlinked(reference, cx));
                }
                todos::TodosModel::global(&self.runtime, cx)
                    .update(cx, |model, cx| model.mark_dirty(cx));
            }
            Ok((_, false)) => {}
            Err(err) => {
                self.error = Some(crate::i18n::tf("notes.error.link", &[("error", &err)]).into());
                cx.notify();
            }
        }
    }

    /// The Graph view command: shows the open note's neighbourhood, or
    /// closes the graph when it is open.
    pub(crate) fn toggle_graph(
        &mut self,
        _: &crate::commands::NoteGraph,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.graph.is_some() {
            self.close_graph(window, cx);
        } else {
            self.show_graph(graph::Scope::Local, window, cx);
        }
    }

    pub(crate) fn show_graph(
        &mut self,
        scope: graph::Scope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let PaneState::Open(open) = &self.state else {
            return;
        };
        if let Some(graph) = &self.graph {
            graph.update(cx, |graph, cx| graph.set_scope(scope, cx));
            return;
        }
        let center = open.id.clone();
        // The graph reads the saved notes: pending typing goes first.
        self.save(cx);
        let model = todos::TodosModel::global(&self.runtime, cx);
        model.update(cx, |model, cx| model.sync(cx));
        let colors = self.colors();
        let view = cx.new(|cx| graph::NoteGraphView::new(model, center, scope, colors, cx));
        cx.subscribe_in(&view, window, |this, _, event, window, cx| match event {
            graph::GraphEvent::Open(note_id) => {
                crate::telemetry::notes_event("notes.graph.opened_note", "");
                this.close_graph(window, cx);
                this.open_note_at(note_id, None, cx);
            }
            graph::GraphEvent::Close => this.close_graph(window, cx),
        })
        .detach();
        crate::telemetry::notes_event("notes.graph.shown", "");
        let handle = view.read(cx).focus_handle(cx);
        self.graph = Some(view);
        window.focus(&handle, cx);
        cx.notify();
    }

    /// Keeps the graph header clear of controls the host draws over the
    /// pane's top-right corner.
    pub(crate) fn set_trailing_inset(&mut self, inset: f32, cx: &mut Context<Self>) {
        if (self.trailing_inset - inset).abs() >= f32::EPSILON {
            self.trailing_inset = inset;
            cx.notify();
        }
    }

    pub(crate) fn close_graph(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.graph.take().is_some() {
            let handle = self.focus_handle(cx);
            window.focus(&handle, cx);
            cx.notify();
        }
    }

    #[cfg(test)]
    pub(crate) fn graph_for_test(&self) -> Option<Entity<graph::NoteGraphView>> {
        self.graph.clone()
    }

    #[cfg(test)]
    pub(crate) fn backlinks_for_test(&self) -> Option<Entity<backlinks::BacklinksView>> {
        match &self.state {
            PaneState::Open(_) => self.backlinks.as_ref().map(|(footer, _)| footer.clone()),
            _ => None,
        }
    }

    fn watch(
        store: &Arc<NoteStore>,
        cx: &mut Context<Self>,
    ) -> (Option<notify::RecommendedWatcher>, Task<()>) {
        use notify::{EventKind, RecursiveMode, Watcher};
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if let Ok(event) = &event
                && matches!(event.kind, EventKind::Access(_))
            {
                return;
            }
            let _ = tx.send(());
        })
        .ok()
        .and_then(|mut watcher| {
            watcher
                .watch(store.dir(), RecursiveMode::NonRecursive)
                .ok()
                .map(|()| watcher)
        });
        let task = cx.spawn(async move |this, cx| {
            while rx.recv().await.is_some() {
                cx.background_executor().timer(WATCH_DEBOUNCE).await;
                while rx.try_recv().is_ok() {}
                if this.update(cx, |this, cx| this.reconcile(cx)).is_err() {
                    break;
                }
            }
        });
        (watcher, task)
    }

    /// An outside write to the open note (CLI append, an agent, another
    /// editor). A clean note reloads; with unsaved typing the write is merged
    /// into the editor as one undo step, so neither side is lost.
    pub(crate) fn reconcile(&mut self, cx: &mut Context<Self>) {
        let PaneState::Open(open) = &self.state else {
            return;
        };
        let Some(path) = self.store.as_ref().and_then(|s| s.path_for(&open.id).ok()) else {
            return;
        };
        let Ok(source) = std::fs::read_to_string(&path) else {
            return;
        };
        if source == open.saved || open.in_flight.as_deref() == Some(source.as_str()) {
            return;
        }
        self.absorb_outside(source, cx);
    }

    /// Takes the file's current `source` into the open note: a reload when
    /// there is no unsaved typing, else a three-way merge against what the
    /// editor last loaded or saved.
    fn absorb_outside(&mut self, source: String, cx: &mut Context<Self>) {
        let PaneState::Open(open) = &mut self.state else {
            return;
        };
        // A direct edit of the file (another editor, an agent's own file
        // tools) is kept in history and cannot break the note's identity:
        // the store repairs the front matter from the last known one.
        let source = self
            .store
            .as_ref()
            .and_then(|store| store.notice_outside_change(&open.id).ok())
            .unwrap_or(source);
        let mut theirs = store::parse_note(&source);
        store::repair_front(&mut theirs.front, &open.id, &open.front);
        if open.dirty {
            let base = store::parse_note(&open.saved).doc;
            open.editor.update(cx, |view, cx| {
                let merged = diri_notes::merge::merge3(&base, &view.editor.document(), &theirs.doc);
                view.editor.absorb(merged, diri_notes::history::now_ms());
                cx.notify();
            });
        } else {
            let editor = Editor::new(&theirs.doc);
            open.editor.update(cx, |view, cx| view.reload(editor, cx));
        }
        open.saved = source;
        open.front = theirs.front;
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        if let PaneState::Open(open) = &mut self.state {
            open.dirty = true;
        }
        self.save_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.save_with(true, cx));
        });
    }

    /// Writes the open note now, on this thread: before switching notes and
    /// on the way out, where the file must be current when this returns.
    pub(crate) fn save(&mut self, cx: &mut Context<Self>) {
        self.save_with(false, cx);
    }

    /// `background`: the autosave, whose store write runs off the main
    /// thread. Otherwise a flush, done here and now.
    fn save_with(&mut self, background: bool, cx: &mut Context<Self>) {
        if background {
            self.save_in_background(cx);
            return;
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        // Save only over the version this editor knows; an outside write in
        // between is merged in first, then saved over. Bounded: a file that
        // keeps changing under us is retried on the next save.
        let mut note = None;
        for _ in 0..3 {
            let PaneState::Open(open) = &mut self.state else {
                return;
            };
            let current = Note {
                front: open.front.clone(),
                doc: open.editor.read(cx).editor.document(),
            };
            let markdown = current.to_markdown();
            if markdown == open.saved {
                open.dirty = false;
                note = Some(current);
                break;
            }
            match store.save_if_unchanged(&open.id, &current, &open.saved) {
                Ok(store::SaveOutcome::Saved { source }) => {
                    open.saved = source;
                    open.dirty = false;
                    self.error = None;
                    note = Some(current);
                    break;
                }
                Ok(store::SaveOutcome::Conflict {
                    current: Some(outside),
                }) if open.in_flight.as_deref() == Some(outside.as_str()) => {
                    // Our own autosave landed first: it is the known version.
                    open.saved = outside;
                }
                Ok(store::SaveOutcome::Conflict {
                    current: Some(outside),
                }) => {
                    open.dirty = true;
                    self.absorb_outside(outside, cx);
                }
                Ok(store::SaveOutcome::Conflict { current: None }) => {
                    self.error = Some(crate::i18n::t("notes.error.moved_outside").into());
                    return;
                }
                Err(err) => {
                    self.error =
                        Some(crate::i18n::tf("notes.error.save", &[("error", &err)]).into());
                    return;
                }
            }
        }
        let Some(note) = note else {
            return;
        };
        self.sync_title(&note.doc.title, cx);
    }

    /// The sidebar row is the Session's title; keep it the note's title.
    fn sync_title(&mut self, title: &str, cx: &mut Context<Self>) {
        let PaneState::Open(open) = &mut self.state else {
            return;
        };
        let title = title.trim();
        let title = if title.is_empty() { "Untitled" } else { title };
        if title != open.synced_title {
            open.synced_title = title.to_owned();
            self.runtime
                .store
                .write()
                .expect("store")
                .rename(open.session.clone(), title);
        }
        cx.notify();
    }

    /// The autosave. The note is serialized here, but the store's
    /// merge-safe write (compare, atomic rename, fsync, history: several ms
    /// on a long note) runs off the main thread, so it never lands inside a
    /// keystroke's frame. One write is in flight at a time, so writes cannot
    /// land out of order; typing meanwhile saves again when it lands.
    fn save_in_background(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let PaneState::Open(open) = &mut self.state else {
            return;
        };
        if open.in_flight.is_some() {
            open.resave = true;
            return;
        }
        let current = Note {
            front: open.front.clone(),
            doc: open.editor.read(cx).editor.document(),
        };
        let markdown = current.to_markdown();
        if markdown == open.saved {
            open.dirty = false;
            let title = current.doc.title.clone();
            self.sync_title(&title, cx);
            return;
        }
        let expected = open.saved.clone();
        let id = open.id.clone();
        open.in_flight = Some(markdown);
        let title = current.doc.title.clone();
        let write_id = id.clone();
        let write =
            cx.background_spawn(
                async move { store.save_if_unchanged(&write_id, &current, &expected) },
            );
        cx.spawn(async move |this, cx| {
            let outcome = write.await;
            let _ = this.update(cx, |this, cx| {
                this.finish_background_save(&id, &title, outcome, cx)
            });
        })
        .detach();
    }

    fn finish_background_save(
        &mut self,
        id: &str,
        title: &str,
        outcome: std::io::Result<store::SaveOutcome>,
        cx: &mut Context<Self>,
    ) {
        let PaneState::Open(open) = &mut self.state else {
            return;
        };
        if open.id != id {
            return;
        }
        let written = open.in_flight.take();
        let resave = std::mem::take(&mut open.resave);
        match outcome {
            Ok(store::SaveOutcome::Saved { source }) => {
                open.saved = source;
                self.error = None;
                self.sync_title(title, cx);
            }
            Ok(store::SaveOutcome::Conflict {
                current: Some(outside),
            }) => {
                if written.as_deref() != Some(outside.as_str()) {
                    open.dirty = true;
                    self.absorb_outside(outside, cx);
                }
                self.schedule_save(cx);
                return;
            }
            Ok(store::SaveOutcome::Conflict { current: None }) => {
                self.error = Some(crate::i18n::t("notes.error.moved_outside").into());
            }
            Err(err) => {
                self.error = Some(crate::i18n::tf("notes.error.save", &[("error", &err)]).into());
            }
        }
        if resave {
            self.schedule_save(cx);
        }
        cx.notify();
    }
}

impl Render for NotePane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf_overlay::rendered("notes");
        let colors = self.colors();
        let versions = self.versions_element(_window, cx);
        let root = div()
            .relative()
            .size_full()
            .on_action(cx.listener(Self::open_versions))
            .on_action(cx.listener(Self::toggle_graph))
            .bg(colors.work_surface_nested())
            .font_family(crate::fonts::ui_family());
        let root = match &self.state {
            // The graph replaces the note rather than covering it: under glass
            // the nested surface is clear, so the note would show through.
            PaneState::Open(_) if self.graph.is_some() => root,
            PaneState::Open(open) => {
                open.editor.update(cx, |view, _| view.set_colors(colors));
                if let Some((footer, _)) = &self.backlinks {
                    footer.update(cx, |view, _| view.set_colors(colors));
                }
                root.child(open.editor.clone())
            }
            PaneState::Missing { .. } => root.track_focus(&self.focus).child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(px(14.0))
                            .text_color(colors.secondary)
                            .child(crate::i18n::t("notes.gone.title")),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(colors.tertiary)
                            .child(crate::i18n::t("notes.gone.detail")),
                    ),
            ),
            PaneState::Empty => root.track_focus(&self.focus),
        };
        let root = match &self.graph {
            Some(graph) => {
                let inset = self.trailing_inset;
                graph.update(cx, |graph, _| {
                    graph.set_colors(colors);
                    graph.set_trailing_inset(inset);
                });
                root.child(div().absolute().inset_0().child(graph.clone()))
            }
            None => root,
        };
        let root = root
            .when_some(versions, |el, panel| el.child(panel))
            .children(crate::perf_overlay::badge("notes"));
        root.when_some(self.error.clone(), |el, error| {
            el.child(
                div()
                    .absolute()
                    .bottom(px(14.0))
                    .right(px(14.0))
                    .px(px(12.0))
                    .py(px(7.0))
                    .rounded(px(8.0))
                    .bg(diri_ui::Ink::DANGER)
                    .text_color(gpui::white())
                    .text_size(px(12.0))
                    .child(error),
            )
        })
    }
}

/// A session mention's text. The row and chip already wear the agent's
/// logo, so its name only steals width from the title; a session with no
/// title yet still needs a word, so it says the agent.
pub(crate) fn mention_label(agent: &str, title: &str) -> String {
    if title.trim().is_empty() {
        mentions::session_label(agent, "")
    } else {
        mentions::session_label("", title)
    }
}

/// Live sessions first (most recently active), then notes, each as the
/// `@` menu offers it. Notes are Sessions too; they are mentioned by their
/// file id so the link survives the Session being archived and restored.
fn mention_entries(
    store: &crate::store::SessionStore,
    open: Option<&SessionId>,
) -> Vec<MentionEntry> {
    let mut records: Vec<_> = store
        .sessions()
        .values()
        .filter(|s| !s.is_archived() && Some(&s.id) != open)
        .collect();
    records.sort_by(|a, b| b.updated_at.0.total_cmp(&a.updated_at.0));
    let project_of = |record: &diri_proto::SessionRecord| {
        store
            .projects()
            .get(&record.project_id)
            .map(|p| p.name.clone())
            .unwrap_or_default()
    };
    let (notes, sessions): (Vec<_>, Vec<_>) = records.into_iter().partition(|s| s.is_note());
    let mut entries: Vec<MentionEntry> = sessions
        .into_iter()
        .map(|session| {
            let kind = session.effective_kind();
            let agent = crate::notifications::display_name(kind, store.agent_descriptor(kind));
            let title = crate::switcher::display_title_str(session);
            let project = project_of(session);
            let detail = match (&session.host, project.is_empty()) {
                (Some(host), false) => format!("{project} · {host}"),
                (Some(host), true) => host.clone(),
                (None, false) => project.clone(),
                (None, true) => {
                    crate::quick_open::home_relative(std::path::Path::new(&session.cwd))
                }
            };
            MentionEntry {
                candidate: Candidate {
                    target: MentionTarget::Session(session.id.0.clone()),
                    label: mention_label(agent, title),
                    keywords: format!(
                        "{agent} {project} {}",
                        session.git_branch.as_deref().unwrap_or("")
                    ),
                },
                agent: Some(crate::session_presentation::ui_agent_kind(kind)),
                status: Some(crate::session_presentation::status_state(session, false)),
                detail: detail.into(),
            }
        })
        .collect();
    entries.extend(notes.into_iter().filter_map(|note| {
        let id = note.note_id.clone()?;
        let project = project_of(note);
        Some(MentionEntry {
            candidate: Candidate {
                target: MentionTarget::Note(id),
                label: mentions::note_label(&note.title),
                keywords: format!("note {project}"),
            },
            agent: None,
            status: None,
            detail: project.into(),
        })
    }));
    entries
}
