//! Live Surfaces in the app: native dashboards agents build and keep live.
//!
//! [`SurfacesModel`] is the app-wide view of the surfaces directory: it
//! re-reads it off the main thread when the directory reports a write (no
//! polling, so an idle Diri spends nothing on surfaces), applies the person's
//! ticks optimistically, and turns clicks into effects through the one gate
//! in `diri_proto::surface::action`. [`view::SurfaceView`] draws one surface.
//!
//! Saved surfaces are sidebar rows that open as a page over the terminal;
//! a session's surfaces show in that session's right panel.

pub(crate) mod render;
#[cfg(test)]
mod tests;
pub(crate) mod view;

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use diri_proto::SessionId;
use diri_proto::surface::{
    Action, Effect, EventKind, Gesture, Interaction, Scope, SurfaceDoc, authorize, event_prompt,
};
use diri_surfaces::{Author, Loaded, SurfaceMeta, SurfaceStore};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Task};

use crate::notifications::SendTextCommand;
use crate::store::StoreRuntime;

const WATCH_DEBOUNCE: Duration = Duration::from_millis(120);

/// Saved surfaces' ids and titles, newest first, for code that has no GPUI
/// context (⌘K ranks its rows synchronously). Written by the model.
static SAVED_TITLES: std::sync::RwLock<Vec<(String, String)>> = std::sync::RwLock::new(Vec::new());

pub(crate) fn saved_titles() -> Vec<(String, String)> {
    SAVED_TITLES.read().map(|titles| titles.clone()).unwrap_or_default()
}

/// One surface as the app holds it.
#[derive(Clone, Debug)]
pub(crate) struct Surface {
    pub doc: Arc<SurfaceDoc>,
    pub read_only: bool,
}

impl From<Loaded> for Surface {
    fn from(loaded: Loaded) -> Self {
        Self {
            doc: Arc::new(loaded.doc),
            read_only: loaded.read_only,
        }
    }
}

/// What the model tells the window about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SurfacesEvent {
    /// An agent made a new surface for one of its sessions: the window may
    /// show it in that session's right panel.
    SessionSurfaceCreated { session: SessionId, surface: String },
    /// A click asked to show something in Diri.
    Navigate(Navigate),
    /// A short confirmation or refusal for the person.
    Feedback { message: String, error: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Navigate {
    Session(SessionId),
    Note(String),
    Surface(String),
}

pub(crate) struct SurfacesModel {
    runtime: Arc<StoreRuntime>,
    store: Option<Arc<SurfaceStore>>,
    surfaces: BTreeMap<String, Surface>,
    /// Ids seen since launch, to tell a new surface from a re-read.
    seen: HashSet<String>,
    ready: bool,
    dirty: bool,
    refresh: Task<()>,
    _watch: Task<()>,
    _watcher: Option<notify::RecommendedWatcher>,
}

impl EventEmitter<SurfacesEvent> for SurfacesModel {}

struct SurfacesGlobal(Entity<SurfacesModel>);

impl Global for SurfacesGlobal {}

impl SurfacesModel {
    /// The app-wide model, created on first use.
    pub(crate) fn global(runtime: &Arc<StoreRuntime>, cx: &mut App) -> Entity<Self> {
        if let Some(model) = cx.try_global::<SurfacesGlobal>() {
            return model.0.clone();
        }
        let runtime = Arc::clone(runtime);
        let model = cx.new(|cx| {
            // Tests never read the real directory or start a watcher; they
            // install their own model.
            if cfg!(test) {
                return Self::with_store(runtime, None, false, cx);
            }
            let store = SurfaceStore::resolve_dir()
                .and_then(|dir| SurfaceStore::open(dir).ok())
                .map(Arc::new);
            Self::with_store(runtime, store, true, cx)
        });
        cx.set_global(SurfacesGlobal(model.clone()));
        model
    }

    #[cfg(test)]
    pub(crate) fn install(model: Entity<Self>, cx: &mut App) {
        cx.set_global(SurfacesGlobal(model));
    }

    pub(crate) fn with_store(
        runtime: Arc<StoreRuntime>,
        store: Option<Arc<SurfaceStore>>,
        watch: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let (watcher, task) = match (&store, watch) {
            (Some(store), true) => watch_dir(store, cx),
            _ => (None, Task::ready(())),
        };
        Self {
            runtime,
            store,
            surfaces: BTreeMap::new(),
            seen: HashSet::new(),
            ready: false,
            dirty: true,
            refresh: Task::ready(()),
            _watch: task,
            _watcher: watcher,
        }
    }

    pub(crate) fn store(&self) -> Option<&Arc<SurfaceStore>> {
        self.store.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn is_ready(&self) -> bool {
        self.ready || self.store.is_none()
    }

    pub(crate) fn get(&self, id: &str) -> Option<&Surface> {
        self.surfaces.get(id)
    }

    /// Saved surfaces shown in the sidebar, most recently updated first.
    pub(crate) fn pinned(&self) -> Vec<Arc<SurfaceDoc>> {
        let mut docs: Vec<_> = self
            .surfaces
            .values()
            .filter(|s| s.doc.scope.is_saved() && s.doc.pinned)
            .map(|s| Arc::clone(&s.doc))
            .collect();
        docs.sort_by_key(|doc| doc.title.to_lowercase());
        docs
    }

    /// Every saved surface, for the palette.
    pub(crate) fn saved(&self) -> Vec<Arc<SurfaceDoc>> {
        let mut docs: Vec<_> = self
            .surfaces
            .values()
            .filter(|s| s.doc.scope.is_saved())
            .map(|s| Arc::clone(&s.doc))
            .collect();
        docs.sort_by_key(|doc| std::cmp::Reverse(doc.updated_ms));
        docs
    }

    /// A session's surfaces, newest first.
    pub(crate) fn for_session(&self, session: &str) -> Vec<Arc<SurfaceDoc>> {
        let mut docs: Vec<_> = self
            .surfaces
            .values()
            .filter(|s| s.doc.scope.session() == Some(session))
            .map(|s| Arc::clone(&s.doc))
            .collect();
        docs.sort_by_key(|doc| std::cmp::Reverse(doc.created_ms));
        docs
    }

    /// Re-reads the directory if it reported a write. Called from renders;
    /// the read happens off the main thread.
    pub(crate) fn sync(&mut self, cx: &mut Context<Self>) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let Some(store) = self.store.clone() else {
            return;
        };
        self.refresh = cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { read_all(&store) })
                .await;
            let _ = this.update(cx, |this, cx| this.replace_all(loaded, cx));
        });
    }

    pub(crate) fn mark_dirty(&mut self, cx: &mut Context<Self>) {
        self.dirty = true;
        self.sync(cx);
        cx.notify();
    }

    fn replace_all(&mut self, loaded: Vec<Surface>, cx: &mut Context<Self>) {
        let first = !self.ready;
        let mut next = BTreeMap::new();
        for surface in loaded {
            let id = surface.doc.id.clone();
            if !first
                && !self.seen.contains(&id)
                && let Some(session) = surface.doc.scope.session()
            {
                cx.emit(SurfacesEvent::SessionSurfaceCreated {
                    session: SessionId::new(session),
                    surface: id.clone(),
                });
            }
            self.seen.insert(id.clone());
            next.insert(id, surface);
        }
        self.surfaces = next;
        self.ready = true;
        if let Ok(mut titles) = SAVED_TITLES.write() {
            *titles = self
                .saved()
                .iter()
                .map(|doc| (doc.id.clone(), doc.title.clone()))
                .collect();
        }
        cx.notify();
    }

    /// The person ticked, toggled or chose: shown at once, then written
    /// through the locked store path. If the component asked to notify, the
    /// owning agent receives the event.
    pub(crate) fn interact(&mut self, id: &str, interaction: Interaction, cx: &mut Context<Self>) {
        let Some(surface) = self.surfaces.get_mut(id) else {
            return;
        };
        if surface.read_only {
            cx.emit(SurfacesEvent::Feedback {
                message: crate::i18n::t("surfaces.read_only").into(),
                error: true,
            });
            return;
        }
        let mut doc = (*surface.doc).clone();
        if doc.interact(&interaction).is_err() {
            return;
        }
        surface.doc = Arc::new(doc);
        cx.notify();
        let Some(store) = self.store.clone() else {
            return;
        };
        let sender = self.runtime.notification_action_sender();
        let id = id.to_owned();
        let task = cx.background_executor().spawn(async move {
            match store.interact(&id, &interaction) {
                Ok((doc, event, notify)) => {
                    if notify && let Some(owner) = &doc.owner {
                        let _ = sender.send(SendTextCommand {
                            session_id: SessionId::new(owner),
                            text: event_prompt(&doc, &event),
                            submit: true,
                        });
                    }
                    Ok(())
                }
                Err(error) => Err(error.to_string()),
            }
        });
        cx.spawn(async move |this, cx| {
            if let Err(message) = task.await {
                let _ = this.update(cx, |this, cx| {
                    // The write was refused: show what is on disk again.
                    this.mark_dirty(cx);
                    cx.emit(SurfacesEvent::Feedback { message, error: true });
                });
            }
        })
        .detach();
    }

    /// The person pressed a button (or a link): through the gate, then done.
    pub(crate) fn act(&mut self, id: &str, node: &str, action: &Action, cx: &mut Context<Self>) {
        let Some(surface) = self.surfaces.get(id) else {
            return;
        };
        let doc = Arc::clone(&surface.doc);
        match authorize(action, Gesture::User, doc.authority()) {
            Ok(effect) => self.perform(&doc, node, action, effect, cx),
            Err(denied) => cx.emit(SurfacesEvent::Feedback {
                message: denied.to_string(),
                error: true,
            }),
        }
    }

    /// A rich link was clicked: links are actions too, so they pass the
    /// same gate as buttons.
    pub(crate) fn open_link(&mut self, id: &str, url: &str, cx: &mut Context<Self>) {
        self.act(id, "link", &Action::OpenUrl { url: url.to_owned() }, cx);
    }

    /// "Ask agent to refresh": the header's shortcut for the owner.
    pub(crate) fn ask_refresh(&mut self, id: &str, cx: &mut Context<Self>) {
        self.act(id, "header", &Action::Refresh, cx);
    }

    fn perform(
        &mut self,
        doc: &SurfaceDoc,
        node: &str,
        action: &Action,
        effect: Effect,
        cx: &mut Context<Self>,
    ) {
        let sender = self.runtime.notification_action_sender();
        match effect {
            Effect::OpenExternal(url) => cx.open_url(&url),
            Effect::OpenSession(session) => {
                cx.emit(SurfacesEvent::Navigate(Navigate::Session(SessionId::new(session))))
            }
            Effect::OpenNote(note) => cx.emit(SurfacesEvent::Navigate(Navigate::Note(note))),
            Effect::OpenSurface(surface) => {
                cx.emit(SurfacesEvent::Navigate(Navigate::Surface(surface)))
            }
            Effect::Prompt { session, text } => {
                let _ = sender.send(SendTextCommand {
                    session_id: SessionId::new(&session),
                    text,
                    submit: true,
                });
                self.log_action(doc, node, action, Some(session), cx);
                cx.emit(SurfacesEvent::Feedback {
                    message: crate::i18n::t("surfaces.sent").into(),
                    error: false,
                });
            }
            Effect::Deliver {
                session,
                name,
                payload,
            } => {
                let Some(store) = self.store.clone() else {
                    return;
                };
                let doc = doc.clone();
                let what = EventKind::Action {
                    node: node.to_owned(),
                    action: action.kind_name().to_owned(),
                    name: Some(name),
                    payload,
                };
                cx.background_executor()
                    .spawn(async move {
                        if let Ok(event) = store.record_action(&doc.id, what, Some(session.clone())) {
                            let _ = sender.send(SendTextCommand {
                                session_id: SessionId::new(&session),
                                text: event_prompt(&doc, &event),
                                submit: true,
                            });
                        }
                    })
                    .detach();
                cx.emit(SurfacesEvent::Feedback {
                    message: crate::i18n::t("surfaces.asked").into(),
                    error: false,
                });
            }
        }
    }

    fn log_action(
        &self,
        doc: &SurfaceDoc,
        node: &str,
        action: &Action,
        delivered_to: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let id = doc.id.clone();
        let what = EventKind::Action {
            node: node.to_owned(),
            action: action.kind_name().to_owned(),
            name: None,
            payload: None,
        };
        cx.background_executor()
            .spawn(async move {
                let _ = store.record_action(&id, what, delivered_to);
            })
            .detach();
    }

    /// "Save as Persistent Surface": a pinned saved copy, in the session's
    /// project when it has one.
    pub(crate) fn promote(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let project = self
            .surfaces
            .get(id)
            .and_then(|s| s.doc.scope.session().map(str::to_owned))
            .and_then(|session| {
                let store = self.runtime.store.read().ok()?;
                store
                    .sessions()
                    .get(&SessionId::new(&session))
                    .map(|record| record.project_id.0.clone())
            })
            .filter(|project| !project.is_empty());
        let scope = project.map_or(Scope::Global, |project| Scope::Project { project });
        let id = id.to_owned();
        let task = cx
            .background_executor()
            .spawn(async move { store.promote(&id, scope, &Author::User).map_err(|e| e.to_string()) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.mark_dirty(cx);
                match result {
                    Ok(doc) => {
                        cx.emit(SurfacesEvent::Feedback {
                            message: crate::i18n::tf("surfaces.saved", &[("title", &doc.title)]),
                            error: false,
                        });
                        cx.emit(SurfacesEvent::Navigate(Navigate::Surface(doc.id)));
                    }
                    Err(message) => cx.emit(SurfacesEvent::Feedback { message, error: true }),
                }
            });
        })
        .detach();
    }

    /// Pin or unpin a saved surface's sidebar row.
    pub(crate) fn set_pinned(&mut self, id: &str, pinned: bool, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let id = id.to_owned();
        let task = cx.background_executor().spawn(async move {
            store
                .update_meta(&id, &Author::User, |doc| doc.pinned = pinned)
                .map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.mark_dirty(cx);
                if let Err(message) = result {
                    cx.emit(SurfacesEvent::Feedback { message, error: true });
                }
            });
        })
        .detach();
    }
}

fn read_all(store: &SurfaceStore) -> Vec<Surface> {
    let metas: Vec<SurfaceMeta> = store.list().unwrap_or_default();
    metas
        .iter()
        .filter_map(|meta| store.load(&meta.id).ok())
        .map(Surface::from)
        .collect()
}

fn watch_dir(
    store: &Arc<SurfaceStore>,
    cx: &mut Context<SurfacesModel>,
) -> (Option<notify::RecommendedWatcher>, Task<()>) {
    use notify::{EventKind, RecursiveMode, Watcher};
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = &event {
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            // Temp files, the lock and history are not surfaces.
            if !event.paths.is_empty()
                && event
                    .paths
                    .iter()
                    .all(|path| diri_surfaces::surface_id(path).is_none())
            {
                return;
            }
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
            if this.update(cx, |this, cx| this.mark_dirty(cx)).is_err() {
                break;
            }
        }
    });
    (watcher, task)
}
