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
//!
//! A surface with an `app.lua` is a native Lua app (`diri_script::app`).
//! Its `render`, `on_event` and `sync` run on background threads, each in a
//! fresh budgeted sandbox; the UI thread only draws the tree they return.
//! `sync` runs on open, on Refresh, and on the app's interval only while a
//! view of it is on screen ([`SurfacesModel::poll_due`]).

pub(crate) mod render;
#[cfg(test)]
pub(crate) mod tests;
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
    /// For a Lua app: `root` is what `render` returned, and the identity
    /// its `app` table set is applied.
    pub doc: Arc<SurfaceDoc>,
    pub read_only: bool,
    pub app: Option<AppRun>,
}

impl From<Loaded> for Surface {
    fn from(loaded: Loaded) -> Self {
        Self {
            doc: Arc::new(loaded.doc),
            read_only: loaded.read_only,
            app: None,
        }
    }
}

/// A Lua app's last render and sync.
#[derive(Clone, Debug, Default)]
pub(crate) struct AppRun {
    pub digest: String,
    /// The first lines of `app.lua`, for the approval review.
    pub source_head: String,
    pub data: serde_json::Value,
    /// Why the last render failed; the app shows it instead of a tree.
    pub error: Option<String>,
    pub render_ms: f64,
    /// The person approved this exact source for `ctx.exec`/`ctx.fetch`.
    pub approved: bool,
    pub sync: SyncStatus,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SyncStatus {
    pub running: bool,
    pub last_ms: Option<u64>,
    pub error: Option<String>,
    /// The last run asked for exec or fetch without approval.
    pub needs_approval: bool,
}

/// What a render or sync reads about Diri: the clock, the theme, and the
/// sessions (id, title, status, kind), as plain JSON.
pub(crate) fn app_context(runtime: &StoreRuntime, dark: bool) -> serde_json::Value {
    let sessions: Vec<serde_json::Value> = runtime
        .store
        .read()
        .map(|store| {
            store
                .sessions()
                .values()
                .filter(|record| !record.is_note() && !record.is_archived())
                .map(|record| {
                    serde_json::json!({
                        "id": record.id.0,
                        "title": record.title,
                        "status": render::session_status(record).text,
                        "kind": record.kind.id(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "now": diri_surfaces::now_ms(),
        "theme": if dark { "dark" } else { "light" },
        "sessions": sessions,
    })
}

/// Renders a Lua app into the surface the view draws. Runs off the main
/// thread; a Lua or shape error leaves an empty tree and the message.
pub(crate) fn run_app(store: &SurfaceStore, loaded: Loaded, ctx: &serde_json::Value) -> Surface {
    let mut surface = Surface::from(loaded);
    let Some(_) = surface.doc.app.as_ref() else {
        return surface;
    };
    let id = surface.doc.id.clone();
    let mut run = AppRun::default();
    let (Ok(Some(source)), Ok(folder)) = (store.app_source(&id), store.folder_for(&id)) else {
        run.error = Some("app.lua is missing".into());
        surface.app = Some(run);
        return surface;
    };
    run.digest = diri_script::app::digest(&source);
    // The review shows what asks for the rights: `sync` and what follows.
    let start = source
        .lines()
        .position(|line| line.trim_start().starts_with("function sync"))
        .unwrap_or(0);
    run.source_head = source.lines().skip(start).take(60).collect::<Vec<_>>().join("\n");
    run.approved = store.is_approved(&id, &run.digest);
    run.data = store.read_data(&id).unwrap_or(serde_json::Value::Null);
    let state = surface.doc.state.app.clone().unwrap_or(serde_json::Value::Null);
    // Where it draws, so an app can lay itself out for it: a session's app
    // lives in the narrow right panel, a saved one over the main pane.
    let mut ctx = ctx.clone();
    let panel = surface.doc.scope.session().is_some();
    ctx["layout"] = serde_json::json!(if panel { "panel" } else { "page" });
    ctx["width"] = serde_json::json!(if panel { 400 } else { 1000 });
    let ctx = &ctx;
    let started = std::time::Instant::now();
    let rendered = diri_script::app::render(
        diri_script::app::App {
            source: &source,
            dir: &folder,
        },
        &state,
        &run.data,
        ctx,
        diri_script::app::INTERACTIVE,
    );
    run.render_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut doc = (*surface.doc).clone();
    match rendered
        .map_err(|error| error.to_string())
        .and_then(|rendered| apply_render(&mut doc, rendered))
    {
        Ok(()) => surface.doc = Arc::new(doc),
        Err(error) => {
            let mut empty = (*surface.doc).clone();
            empty.root = Vec::new();
            surface.doc = Arc::new(empty);
            run.error = Some(error);
        }
    }
    surface.app = Some(run);
    surface
}

/// Puts a render's tree and `app` table into the doc, then checks it
/// against every bound exactly like an agent's write.
fn apply_render(doc: &mut SurfaceDoc, rendered: diri_script::app::Rendered) -> Result<(), String> {
    doc.root = serde_json::from_value(rendered.root).map_err(|error| format!("render output: {error}"))?;
    if let Some(meta) = rendered.meta {
        if let Some(title) = meta.get("title").and_then(serde_json::Value::as_str) {
            doc.title = title.to_owned();
        }
        if let Some(subtitle) = meta.get("subtitle").and_then(serde_json::Value::as_str) {
            doc.subtitle = Some(subtitle.to_owned());
        }
        if let Some(icon) = meta.get("icon") {
            doc.icon = match icon {
                serde_json::Value::String(text) => Some(render::icon_shorthand(text)),
                other => serde_json::from_value(other.clone()).ok(),
            };
        }
        if let Some(accent) = meta.get("accent").and_then(serde_json::Value::as_str) {
            doc.accent = Some(accent.to_owned());
        }
        if let Some(style) = meta.get("style") {
            doc.style = serde_json::from_value(style.clone()).map_err(|error| format!("app.style: {error}"))?;
        }
    }
    doc.validate().map(|_| ()).map_err(|error| error.to_string())
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
    /// The theme apps render for (`ctx.theme`).
    dark: bool,
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
            dark: true,
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
        let ctx = app_context(&self.runtime, self.dark);
        self.refresh = cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { read_all(&store, &ctx) })
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
        for mut surface in loaded {
            let id = surface.doc.id.clone();
            if let (Some(run), Some(previous)) = (
                surface.app.as_mut(),
                self.surfaces.get(&id).and_then(|s| s.app.as_ref()),
            ) {
                run.sync = previous.sync.clone();
                if !run.approved || run.digest != previous.digest {
                    run.sync.needs_approval = previous.sync.needs_approval && !run.approved;
                }
            }
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
                .map(|doc| {
                    // ⌘K rows have a built-in glyph; an emoji leads the title.
                    let title = match &doc.icon {
                        Some(diri_proto::surface::Icon::Emoji(emoji)) => format!("{emoji} {}", doc.title),
                        _ => doc.title.clone(),
                    };
                    (doc.id.clone(), title)
                })
                .collect();
        }
        cx.notify();
    }

    /// The person ticked, toggled or chose: shown at once, then written
    /// through the locked store path. If the component asked to notify, the
    /// owning agent receives the event.
    pub(crate) fn interact(&mut self, id: &str, interaction: Interaction, cx: &mut Context<Self>) {
        if self.surfaces.get(id).is_some_and(|s| s.app.is_some()) {
            let event = match &interaction {
                Interaction::Check { checklist, item, checked } => serde_json::json!({
                    "type": "check", "id": checklist, "item": item, "value": checked}),
                Interaction::Toggle { id, on } => {
                    let kind = self.surfaces.get(&id.clone()).and_then(|s| {
                        diri_proto::surface::node::find(&s.doc.root, id).map(|n| n.type_name())
                    });
                    serde_json::json!({"type": kind.unwrap_or("toggle"), "id": id, "value": on})
                }
                Interaction::Select { id, value } => serde_json::json!({"type": "select", "id": id, "value": value}),
                Interaction::Text { id, value } => serde_json::json!({"type": "input", "id": id, "value": value}),
            };
            self.app_event(id, event, false, cx);
            return;
        }
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
            Effect::AppEvent { name, payload } => {
                let event = serde_json::json!({
                    "type": "action", "id": node, "name": name, "payload": payload});
                self.app_event(&doc.id, event, true, cx);
            }
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

impl SurfacesModel {
    /// Apps render for the window's theme.
    pub(crate) fn set_dark(&mut self, dark: bool, cx: &mut Context<Self>) {
        if self.dark != dark {
            self.dark = dark;
            self.mark_dirty(cx);
        }
    }

    /// Runs a Lua app's `on_event` off the main thread, keeps the state it
    /// returns and redraws. `clicked`: a person's click, so actions the
    /// handler returns may pass the gate; a tick or a typed line may change
    /// state but never mint an action.
    pub(crate) fn app_event(
        &mut self,
        id: &str,
        event: serde_json::Value,
        clicked: bool,
        cx: &mut Context<Self>,
    ) {
        let (Some(store), Some(surface)) = (self.store.clone(), self.surfaces.get(id)) else {
            return;
        };
        if surface.app.is_none() {
            return;
        }
        let state = surface.doc.state.app.clone().unwrap_or(serde_json::Value::Null);
        let ctx = app_context(&self.runtime, self.dark);
        let id = id.to_owned();
        let task = cx.background_executor().spawn(async move {
            let source = store.app_source(&id).ok().flatten().ok_or("app.lua is missing")?;
            let folder = store.folder_for(&id).map_err(|e| e.to_string())?;
            let data = store.read_data(&id).unwrap_or(serde_json::Value::Null);
            let handled = diri_script::app::handle_event(
                diri_script::app::App { source: &source, dir: &folder },
                &state,
                &event,
                &data,
                &ctx,
                diri_script::app::INTERACTIVE,
            )
            .map_err(|e| e.to_string())?;
            store.set_app_state(&id, &handled.state).map_err(|e| e.to_string())?;
            let surface = run_app(&store, store.load(&id).map_err(|e| e.to_string())?, &ctx);
            Ok::<_, String>((surface, handled.actions))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok((surface, actions)) => {
                    let doc = Arc::clone(&surface.doc);
                    this.replace_one(surface, cx);
                    // Effects only for a click, and only through the gate.
                    let gesture = if clicked { Gesture::User } else { Gesture::Programmatic };
                    for action in actions.into_iter().take(4) {
                        let Ok(action) = serde_json::from_value::<Action>(action) else {
                            continue;
                        };
                        if matches!(action, Action::App { .. }) {
                            continue;
                        }
                        match authorize(&action, gesture, doc.authority()) {
                            Ok(effect) => this.perform(&doc, "app", &action, effect, cx),
                            Err(denied) => cx.emit(SurfacesEvent::Feedback {
                                message: denied.to_string(),
                                error: true,
                            }),
                        }
                    }
                }
                Err(message) => cx.emit(SurfacesEvent::Feedback { message, error: true }),
            });
        })
        .detach();
    }

    /// Runs a Lua app's `sync` off the main thread: it writes `data.json`
    /// and the app redraws. One run at a time per app.
    pub(crate) fn sync_app(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else { return };
        let Some(surface) = self.surfaces.get_mut(id) else { return };
        let Some(run) = surface.app.as_mut() else { return };
        if run.sync.running || surface.read_only {
            return;
        }
        run.sync.running = true;
        let timeout = surface
            .doc
            .app
            .as_ref()
            .and_then(|app| app.sync.as_ref())
            .map_or(30, |sync| sync.timeout_s);
        let mut ctx = app_context(&self.runtime, self.dark);
        let id = id.to_owned();
        let task_id = id.clone();
        cx.notify();
        let task = cx.background_executor().spawn(async move {
            let id = task_id;
            let source = store.app_source(&id).ok().flatten().ok_or("app.lua is missing")?;
            let folder = store.folder_for(&id).map_err(|e| e.to_string())?;
            ctx["data"] = store.read_data(&id).unwrap_or(serde_json::Value::Null);
            // Approval is read here, at the moment of the run, against the
            // source this run executes.
            let approved = store.is_approved(&id, &diri_script::app::digest(&source));
            let synced = diri_script::app::sync(
                diri_script::app::App { source: &source, dir: &folder },
                &ctx,
                diri_script::app::SyncCaps { approved },
                diri_script::app::sync_limits(Duration::from_secs(u64::from(timeout))),
            )
            .map_err(|e| e.to_string())?;
            if let Some(data) = &synced.data {
                store.write_data(&id, data).map_err(|e| e.to_string())?;
            }
            let surface = run_app(&store, store.load(&id).map_err(|e| e.to_string())?, &ctx);
            Ok::<_, String>((surface, synced.needs_approval))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let now = diri_surfaces::now_ms();
                match result {
                    Ok((mut surface, needs_approval)) => {
                        if let Some(run) = surface.app.as_mut() {
                            run.sync = SyncStatus {
                                running: false,
                                last_ms: Some(now),
                                error: None,
                                needs_approval,
                            };
                        }
                        this.replace_one(surface, cx);
                    }
                    Err(message) => {
                        if let Some(run) = this.surfaces.get_mut(&id).and_then(|s| s.app.as_mut()) {
                            run.sync = SyncStatus {
                                running: false,
                                last_ms: Some(now),
                                error: Some(message),
                                needs_approval: run.sync.needs_approval,
                            };
                        }
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// How long until a visible app's next poll, or `None` when it does not
    /// poll. Zero: due now. Only a view that is drawing asks, so a hidden
    /// app is never polled.
    pub(crate) fn poll_due(&self, id: &str, now_ms: u64) -> Option<Duration> {
        let surface = self.surfaces.get(id)?;
        let run = surface.app.as_ref()?;
        let every = surface.doc.app.as_ref()?.sync.as_ref()?.every_s?;
        if run.sync.running {
            return None;
        }
        let next = run.sync.last_ms.unwrap_or(0) + u64::from(every) * 1000;
        Some(Duration::from_millis(next.saturating_sub(now_ms)))
    }

    /// Whether an app wants a sync when it opens.
    pub(crate) fn syncs_on_open(&self, id: &str) -> bool {
        self.surfaces.get(id).is_some_and(|surface| {
            surface.app.as_ref().is_some_and(|run| run.sync.last_ms.is_none() && !run.sync.running)
                && surface
                    .doc
                    .app
                    .as_ref()
                    .and_then(|app| app.sync.as_ref())
                    .is_some_and(|sync| sync.on_open)
        })
    }

    /// The person allowed this exact `app.lua` to run programs and fetch:
    /// recorded by digest, then a sync with the new rights.
    pub(crate) fn approve_app(&mut self, id: &str, digest: &str, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else { return };
        if store.approve(id, digest).is_err() {
            return;
        }
        if let Some(run) = self.surfaces.get_mut(id).and_then(|s| s.app.as_mut())
            && run.digest == digest
        {
            run.approved = true;
            run.sync.needs_approval = false;
        }
        self.sync_app(id, cx);
    }

    /// Fixtures: an app that synced `ago_ms` ago.
    #[cfg(test)]
    pub(crate) fn set_synced_for_test(&mut self, id: &str, at_ms: u64, cx: &mut Context<Self>) {
        if let Some(run) = self.surfaces.get_mut(id).and_then(|s| s.app.as_mut()) {
            run.sync.last_ms = Some(at_ms);
            run.sync.needs_approval = false;
        }
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn set_needs_approval_for_test(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(run) = self.surfaces.get_mut(id).and_then(|s| s.app.as_mut()) {
            run.sync.last_ms = Some(diri_surfaces::now_ms());
            run.sync.needs_approval = true;
        }
        cx.notify();
    }

    fn replace_one(&mut self, surface: Surface, cx: &mut Context<Self>) {
        self.surfaces.insert(surface.doc.id.clone(), surface);
        cx.notify();
    }
}

fn read_all(store: &SurfaceStore, ctx: &serde_json::Value) -> Vec<Surface> {
    let metas: Vec<SurfaceMeta> = store.list().unwrap_or_default();
    metas
        .iter()
        .filter_map(|meta| store.load(&meta.id).ok())
        .map(|loaded| run_app(store, loaded, ctx))
        .collect()
}

fn watch_dir(
    store: &Arc<SurfaceStore>,
    cx: &mut Context<SurfacesModel>,
) -> (Option<notify::RecommendedWatcher>, Task<()>) {
    use notify::{EventKind, RecursiveMode, Watcher};
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    // FSEvents reports canonical paths (/private/var/...).
    let dir = store.dir().canonicalize().unwrap_or_else(|_| store.dir().to_path_buf());
    let watched = store.dir().to_path_buf();
    let raw = watched.clone();
    let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = &event {
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            // Temp files, the lock, history and approvals are not surfaces.
            if !event.paths.is_empty()
                && event
                    .paths
                    .iter()
                    .all(|path| {
                        diri_surfaces::surface_id(&dir, path).is_none()
                            && diri_surfaces::surface_id(&raw, path).is_none()
                    })
            {
                return;
            }
        }
        let _ = tx.send(());
    })
    .ok()
    .and_then(|mut watcher| {
        watcher
            .watch(&watched, RecursiveMode::Recursive)
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
