use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use diri_proto::remote_pty::PersistenceCapability;
use diri_proto::{
    AgentKind as ProtoAgentKind, AttentionLevel as ProtoAttentionLevel, ProjectId, SessionId,
    SessionRecord,
};
use diri_ui::{
    AgentKind, AgentLogo, AlertChip, AttentionDot, AttentionLevel, Fill, FloatingSurface,
    HairlineDivider, HoverMarquee, Ink, LoadingIndicator, Metrics, Palette, Radius, RowFill,
    SemanticColors, Space, StateChip, StatusGlyph, StatusState, Typo,
};
use gpui::{
    Anchor, AnyElement, App, AppContext as _, Context, Entity, EventEmitter, ExternalPaths,
    FocusHandle, Focusable, FontWeight, Hsla, IntoElement, MouseButton, Pixels, Point, Render,
    Rgba, ScrollHandle, SharedString, Task, Window, anchored, deferred, div, linear_color_stop,
    linear_gradient, point, prelude::*, px,
};
use tokio::sync::mpsc;

use crate::commands::OpenSettings;
use crate::external_drop::{ExternalDropPlan, ExternalDropTarget, plan_external_drop};
use crate::macos::sf_symbols::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::navigation::query_label;
use crate::query_editor::{self, ClipboardEdit, Edit};
use crate::seam::toggle_has_settled;
use crate::store::{
    ClickModifiers, DirectoryListingState, SessionStore, SpawnOptions, StoreEffect, StoreRuntime,
};
use crate::updates::{UpdateCommand, UpdatePhase, UpdateState};
use crate::usage::{UsageFormat, UsageSnapshot};

use super::{
    DragItem, Popover, PreviewScenario, SidebarPreviewFixture, SidebarUiState, move_before,
    move_to_end,
};

const PREVIEW_USAGE: f64 = 4.82;

#[derive(Clone, Debug)]
pub(crate) enum SidebarEvent {
    VisibilityChanged,
    WidthChanged,
    /// The Agents page of Settings, for one target host. Plain Settings goes
    /// through the typed `OpenSettings` action; this event carries the host
    /// the picker row was about, which an action cannot.
    OpenAgentSettings(Option<String>),
    /// One-click path from the footer menu into the Remote host editor.
    AddRemoteHost,
    /// A plain click (or shortcut) selected a session: hand keyboard focus
    /// to its terminal surface so the user can type immediately.
    SessionActivated,
    /// The user acted on the update pill. The sidebar holds no updater of its
    /// own; RootView owns the handle and forwards these.
    Update(UpdateCommand),
    /// The close confirmation was raised, confirmed, or cancelled. RootView
    /// paints that dialog but only re-renders on our events -- without this it
    /// keeps showing a stale frame until some unrelated update wakes it, which
    /// reads as "the ✕ did nothing".
    ConfirmationChanged,
    /// Finder input has been fully validated and reduced to a UI-only staged
    /// action. RootView owns both composer destinations, so the sidebar never
    /// sends daemon input or spawns a session itself.
    ExternalDrop(ExternalDropPlan),
}

#[derive(Clone)]
struct DraggedSidebarItem(DragItem);

struct DragPreview {
    label: SharedString,
    colors: SemanticColors,
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.0))
            .h(px(28.0))
            .flex()
            .items_center()
            .rounded(px(Radius::ROW))
            .bg(self.colors.background.alpha(0.92))
            .border_1()
            .border_color(self.colors.primary.alpha(0.10))
            .text_size(px(Typo::META.size))
            .text_color(self.colors.primary)
            .child(self.label.clone())
    }
}

pub struct Sidebar {
    store: Arc<RwLock<SessionStore>>,
    // Preview stores have no daemon adapter, so retain their effect receiver.
    _preview_effects: Option<mpsc::UnboundedReceiver<StoreEffect>>,
    _store_changes: Option<Task<()>>,
    ui: SidebarUiState,
    /// Session list scroll position, read back each frame to size the top and
    /// bottom fades.
    list_scroll: ScrollHandle,
    directory_scroll: ScrollHandle,
    glyphs: HashMap<SessionId, Entity<StatusGlyph>>,
    /// Rebuilt once per projection render. Looking up ⌘1…⌘9 inside every row
    /// previously re-locked the store and scanned the full session list N times.
    shortcut_ranks: HashMap<SessionId, usize>,
    rename_focus: FocusHandle,
    hover_generation: u64,
    usage: Option<UsageSnapshot>,
    update: UpdateState,
    /// When visibility last flipped, so a held ⌘B cannot outrun the slide.
    last_toggle: Option<Instant>,
    preview: bool,
    /// The New Agent popover toggles between agent choices and a bounded
    /// one-level directory browser. The listing payload itself lives in the
    /// Store so the daemon adapter can complete it asynchronously.
    directory_picker_open: bool,
    /// Inline, contextual feedback for rejected or partially accepted Finder
    /// drops. It remains until dismissed or replaced by the next drop so an
    /// error can never disappear between mouse-up and the next frame.
    external_drop_feedback: Option<String>,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Focusable for Sidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.rename_focus.clone()
    }
}

impl Sidebar {
    pub fn new(
        runtime: Option<Arc<StoreRuntime>>,
        preview: bool,
        scenario: PreviewScenario,
        cx: &mut Context<Self>,
    ) -> Self {
        let (store, preview_effects) = if preview {
            let fixture = SidebarPreviewFixture::make(scenario);
            let (mut store, effects) = SessionStore::headless(fixture.prefs);
            store.hydrate(fixture.list);
            if let Some(id) = fixture.selected_session_id {
                store.select(id);
            }
            (Arc::new(RwLock::new(store)), Some(effects))
        } else {
            (
                Arc::clone(
                    &runtime
                        .as_ref()
                        .expect("live sidebar requires StoreRuntime")
                        .store,
                ),
                None,
            )
        };
        let (width, visible) = {
            let store = store.read().expect("session store lock poisoned");
            let prefs = store.preferences();
            (prefs.sidebar_width, prefs.sidebar_visible)
        };
        let store_changes = runtime.map(|runtime| {
            let mut changes = runtime.changes();
            cx.spawn(async move |this, cx| {
                loop {
                    match changes.recv().await {
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if this.update(cx, |_, cx| cx.notify()).is_err() {
                                return;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
            })
        });
        let mut ui = SidebarUiState::new(width);
        ui.visible = visible;
        let mut sidebar = Self {
            store,
            _preview_effects: preview_effects,
            _store_changes: store_changes,
            ui,
            list_scroll: ScrollHandle::new(),
            directory_scroll: ScrollHandle::new(),
            glyphs: HashMap::new(),
            shortcut_ranks: HashMap::new(),
            rename_focus: cx.focus_handle(),
            hover_generation: 0,
            usage: None,
            update: UpdateState::default(),
            last_toggle: None,
            preview,
            directory_picker_open: false,
            external_drop_feedback: None,
        };
        sidebar.ui.preview_account = preview;
        // Preview-only hook so headless screenshots can verify popover layout.
        if preview
            && std::env::var("DIRIJOR_SIDEBAR_POPOVER").is_ok_and(|value| value == "new-agent")
        {
            sidebar.ui.popover = Some(Popover::NewAgent {
                directory: None,
                host: None,
            });
        }
        sidebar
    }

    pub fn width(&self) -> f32 {
        self.ui.width
    }

    pub fn is_visible(&self) -> bool {
        self.ui.visible
    }

    pub fn selected_session(&self) -> Option<SessionRecord> {
        self.store
            .read()
            .expect("session store lock poisoned")
            .selected_session()
            .cloned()
    }

    pub fn session_count(&self) -> usize {
        self.store
            .read()
            .expect("session store lock poisoned")
            .sessions()
            .len()
    }

    pub fn set_update(&mut self, state: UpdateState, cx: &mut Context<Self>) {
        self.update = state;
        cx.notify();
    }

    pub fn set_usage(&mut self, snapshot: UsageSnapshot, cx: &mut Context<Self>) {
        self.usage = Some(snapshot);
        cx.notify();
    }

    pub fn pending_close_copy(&self) -> Option<(String, String)> {
        let store = self.store.read().expect("session store lock poisoned");
        let pending = store.pending_close()?;
        let title = if pending.ids.len() == 1 {
            store
                .sessions()
                .get(&pending.ids[0])
                .map(|session| format!("Close “{}”?", session.title))
                .unwrap_or_else(|| "Close session?".into())
        } else {
            format!("Close {} sessions?", pending.ids.len())
        };
        let running = pending
            .ids
            .iter()
            .filter(|id| {
                store.sessions().get(*id).is_some_and(|session| {
                    !matches!(session.status, diri_proto::SessionStatus::Exited(_))
                })
            })
            .count();
        Some((title, format!("{running} still running.")))
    }

    pub fn confirm_close(&mut self, cx: &mut Context<Self>) {
        let mut store = self.store.write().expect("session store lock poisoned");
        let ids = store
            .pending_close()
            .map(|pending| pending.ids.clone())
            .unwrap_or_default();
        store.confirm_pending_close();
        if self.preview {
            for id in ids {
                store.remove_session_record(&id);
            }
        }
        drop(store);
        cx.emit(SidebarEvent::ConfirmationChanged);
        cx.notify();
    }

    pub fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.store
            .write()
            .expect("session store lock poisoned")
            .cancel_pending_close();
        cx.emit(SidebarEvent::ConfirmationChanged);
        cx.notify();
    }

    /// Flips sidebar visibility, unless the last flip is still sliding. Every
    /// entry point -- ⌘B, the terminal chrome button, the menu bar, and the
    /// sidebar's own collapse button -- routes through here, so the gate is the
    /// single place the debounce has to hold.
    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        if !toggle_has_settled(self.last_toggle.map(|at| now.duration_since(at))) {
            return;
        }
        self.last_toggle = Some(now);
        self.ui.toggle();
        let visible = self.ui.visible;
        if let Err(error) = self
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| prefs.sidebar_visible = visible)
        {
            eprintln!("diri: could not remember sidebar visibility: {error}");
        }
        cx.emit(SidebarEvent::VisibilityChanged);
        cx.notify();
    }

    pub fn show_new_agent(&mut self, cx: &mut Context<Self>) {
        self.open_new_agent_popover(None, cx);
    }

    /// Opens the new-agent picker, refreshing the host catalog first so
    /// hosts.json edits show up without an app relaunch. The picker remembers
    /// the last local/remote spawn target. A remote target always starts from
    /// its own default cwd; repo resolution is only needed when switching from
    /// an active remote session back to this Mac.
    fn open_new_agent_popover(&mut self, directory: Option<String>, cx: &mut Context<Self>) {
        self.open_new_agent_popover_at(directory, None, cx);
    }

    fn open_new_agent_popover_at(
        &mut self,
        directory: Option<String>,
        location_host: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.directory_picker_open = false;
        let host = {
            let mut store = self.store.write().expect("session store lock poisoned");
            store.reload_hosts();
            let remembered_host = store
                .begin_repo_targeting()
                .filter(|id| store.host(id).is_some());
            if location_host.is_some() {
                location_host
            } else if directory.is_none() {
                let active_host = store
                    .selected_session()
                    .and_then(|session| session.host.as_deref());
                if should_resolve_active_repo(
                    directory.as_deref(),
                    remembered_host.as_deref(),
                    active_host,
                ) {
                    store.request_repo_target(remembered_host.clone());
                }
                remembered_host
            } else {
                None
            }
        };
        self.ui.popover = Some(Popover::NewAgent { directory, host });
        cx.notify();
    }

    /// Reopen the most recently closed session via the daemon's reopen stack.
    pub fn reopen_last(&mut self, cx: &mut Context<Self>) {
        self.store
            .read()
            .expect("session store lock poisoned")
            .reopen_last();
        cx.notify();
    }

    /// Live width during a resize drag. Deliberately does not touch the
    /// preferences store: `update_preferences` writes the prefs file and
    /// reconfigures the daemon governor, which is far too heavy to run on
    /// every mouse-move frame. `commit_width` persists once the drag ends.
    pub fn set_width(&mut self, width: f32, cx: &mut Context<Self>) {
        let previous = self.ui.width;
        self.ui.set_width(width);
        // Dragging past the clamp keeps producing the same width; don't
        // repaint the world for it.
        if (self.ui.width - previous).abs() < f32::EPSILON {
            return;
        }
        cx.emit(SidebarEvent::WidthChanged);
        cx.notify();
    }

    /// Persist whatever width the drag settled on.
    pub fn commit_width(&mut self, _cx: &mut Context<Self>) {
        let persisted_width = self.ui.width;
        let _ = self
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| prefs.sidebar_width = persisted_width);
    }

    pub fn reset_width(&mut self, cx: &mut Context<Self>) {
        self.ui.reset_width();
        let persisted_width = self.ui.width;
        let _ = self
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| prefs.sidebar_width = persisted_width);
        cx.emit(SidebarEvent::WidthChanged);
        cx.notify();
    }

    fn colors(&self) -> SemanticColors {
        let store = self.store.read().expect("session store lock poisoned");
        crate::app_theme::sidebar_colors(&store.preferences().terminal_theme)
    }

    fn begin_rename(
        &mut self,
        session: &SessionRecord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_rename();
        self.ui
            .begin_rename(session.id.clone(), session.title.clone());
        self.rename_focus.focus(window, cx);
        cx.notify();
    }

    fn commit_rename(&mut self) {
        if let Some((id, title)) = self.ui.take_rename() {
            self.store
                .write()
                .expect("session store lock poisoned")
                .rename(id, title);
        }
    }

    fn on_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ui.renaming.is_none() {
            if self.ui.popover.is_some() && event.keystroke.key.as_str() == "escape" {
                self.ui.popover = None;
                cx.notify();
            }
            return;
        }
        match event.keystroke.key.as_str() {
            "enter" => self.commit_rename(),
            "escape" => self.ui.cancel_rename(),
            _ => {
                let Some(edit) = query_editor::edit_for(&event.keystroke) else {
                    return;
                };
                match edit {
                    Edit::Local(local) => {
                        self.ui.rename_draft.apply(local);
                    }
                    Edit::Clipboard(ClipboardEdit::Copy) => {
                        query_editor::copy_selection(&self.ui.rename_draft, cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Cut) => {
                        query_editor::cut_selection(&mut self.ui.rename_draft, cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Paste) => {
                        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                            self.ui.rename_draft.insert(&text);
                        }
                    }
                }
            }
        }
        cx.notify();
    }

    fn schedule_hover_card(
        &mut self,
        id: SessionId,
        hovering: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.hover_generation = self.hover_generation.wrapping_add(1);
        let generation = self.hover_generation;
        if !hovering {
            if self
                .ui
                .hover_card
                .as_ref()
                .is_some_and(|(card_id, _)| card_id == &id)
            {
                self.ui.hover_card = None;
            }
            cx.notify();
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(700))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.hover_generation == generation
                    && this.ui.hovered_session.as_ref() == Some(&id)
                {
                    let pointer_y = f32::from(window.mouse_position().y);
                    this.ui.hover_card = Some((id, pointer_y));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn new_agent_row(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let hovering = self.ui.hovered_control == Some("new-agent");
        let (agent, location) = {
            let store = self.store.read().expect("session store lock poisoned");
            let agent = store.agent_catalog(None).map_or_else(
                || crate::agent_catalog::title_case_id(store.preferences().default_agent.id()),
                |catalog| {
                    crate::agent_catalog::display_name(&store.preferences().default_agent, catalog)
                },
            );
            let location = store
                .default_spawn_host()
                .map_or_else(|| "This Mac".to_owned(), |id| store.host_display_name(&id));
            (agent, location)
        };
        div()
            .id("new-agent")
            .mx(px(Space::INSET))
            .mb(px(4.0))
            .px(px(Space::ROW_H))
            .h(px(44.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(Radius::ROW))
            .bg(Fill::hover(colors, hovering))
            .cursor_pointer()
            .text_size(px(Typo::ROW.size))
            .text_color(colors.text(diri_ui::TextTone::Label))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.ui.hovered_control = hovered.then_some("new-agent");
                cx.notify();
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.commit_rename();
                this.open_new_agent_popover(None, cx);
            }))
            .child(
                div()
                    .w(px(16.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(sf_symbol("square.and.pencil", 13.0, colors.secondary)),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(
                        div()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child("New Agent"),
                    )
                    .child(
                        div()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child(format!("{agent} · {location}")),
                    ),
            )
            .child(
                div()
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child("⌘T"),
            )
            .into_any_element()
    }

    fn top_bar(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let settings_hover = self.ui.hovered_control == Some("settings");
        let toggle_hover = self.ui.hovered_control == Some("sidebar-toggle");
        div()
            .h(px(Metrics::TITLE_BAR))
            .flex_none()
            .flex()
            .items_center()
            .justify_end()
            .pr(px(Metrics::TOOLBAR_EDGE_INSET))
            .gap(px(Metrics::TOOLBAR_COMPACT_GAP))
            .child(icon_button(
                "settings",
                "gearshape",
                settings_hover,
                colors,
                cx.listener(|this, _, window, cx| {
                    this.ui.popover = None;
                    window.dispatch_action(Box::new(OpenSettings), cx);
                }),
                cx.listener(|this, hovered: &bool, _, cx| {
                    this.ui.hovered_control = hovered.then_some("settings");
                    cx.notify();
                }),
            ))
            .child(icon_button(
                "sidebar-toggle",
                "sidebar.left",
                toggle_hover,
                colors,
                cx.listener(|this, _, _, cx| this.toggle(cx)),
                cx.listener(|this, hovered: &bool, _, cx| {
                    this.ui.hovered_control = hovered.then_some("sidebar-toggle");
                    cx.notify();
                }),
            ))
            .into_any_element()
    }

    fn external_drop(
        &mut self,
        paths: &ExternalPaths,
        target: ExternalDropTarget,
        cx: &mut Context<Self>,
    ) {
        let plan = plan_external_drop(paths.paths(), target);
        self.external_drop_feedback = plan.feedback();
        if plan.action.is_some() {
            cx.emit(SidebarEvent::ExternalDrop(plan));
        }
        cx.notify();
    }

    fn can_accept_external_drop(paths: &ExternalPaths, target: ExternalDropTarget) -> bool {
        plan_external_drop(paths.paths(), target).accepts_drop()
    }

    fn external_drop_feedback(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let message = self.external_drop_feedback.clone()?;
        Some(
            div()
                .id("external-drop-feedback")
                .mx(px(Space::INSET))
                .mb(px(6.0))
                .p(px(8.0))
                .flex()
                .items_start()
                .gap(px(7.0))
                .rounded(px(Radius::ROW))
                .bg(Ink::ATTENTION.alpha(0.08))
                .border_1()
                .border_color(Ink::ATTENTION.alpha(0.22))
                .child(sf_symbol(
                    "exclamationmark.circle.fill",
                    11.0,
                    Ink::ATTENTION,
                ))
                .child(
                    div()
                        .min_w(px(0.0))
                        .flex_1()
                        .text_size(px(Typo::META.size))
                        .line_height(px(15.0))
                        .text_color(colors.secondary)
                        .child(message),
                )
                .child(
                    div()
                        .id("dismiss-external-drop-feedback")
                        .size(px(18.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(Radius::CHIP))
                        .cursor_pointer()
                        .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                        .child(sf_symbol("xmark", 8.0, colors.tertiary))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.external_drop_feedback = None;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    fn empty_space_drop_target(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("sidebar-empty-space-drop-target")
            .flex_1()
            .min_h(px(52.0))
            .rounded(px(Radius::ROW))
            .drag_over::<ExternalPaths>(move |element, paths, _, _| {
                if Self::can_accept_external_drop(paths, ExternalDropTarget::EmptySpace) {
                    element
                        .bg(Ink::FRESH.alpha(0.07))
                        .border_1()
                        .border_color(Ink::FRESH.alpha(0.32))
                } else {
                    element
                }
            })
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                cx.stop_propagation();
                this.external_drop(paths, ExternalDropTarget::EmptySpace, cx);
            }))
            .into_any_element()
    }

    fn empty_state(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("sidebar-empty-state")
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.0))
            .rounded(px(Radius::PANEL))
            .drag_over::<ExternalPaths>(move |element, paths, _, _| {
                if Self::can_accept_external_drop(paths, ExternalDropTarget::EmptySpace) {
                    element
                        .bg(Ink::FRESH.alpha(0.07))
                        .border_1()
                        .border_color(Ink::FRESH.alpha(0.32))
                } else {
                    element
                }
            })
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                cx.stop_propagation();
                this.external_drop(paths, ExternalDropTarget::EmptySpace, cx);
            }))
            .child(AgentLogo::new(AgentKind::ClaudeCode, 44.0, colors).badged(false))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(3.0))
                    .child(
                        div()
                            .text_size(px(Typo::ROW_EMPHASIZED.size))
                            .font_weight(Typo::ROW_EMPHASIZED.weight)
                            .text_color(colors.secondary)
                            .child("Bring up your first agent"),
                    )
                    .child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child("⌘T"),
                    ),
            )
            .child(
                div()
                    .id("empty-new-agent")
                    .px(px(10.0))
                    .h(px(28.0))
                    .flex()
                    .items_center()
                    .rounded(px(Radius::ROW))
                    .text_size(px(Typo::ROW.size))
                    .text_color(colors.secondary)
                    .cursor_pointer()
                    .hover(move |element| element.bg(colors.primary.alpha(0.06)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_new_agent_popover(None, cx);
                    }))
                    .gap(px(7.0))
                    .child(sf_symbol("square.and.pencil", 13.0, colors.secondary))
                    .child("New Agent"),
            )
            .into_any_element()
    }

    fn project_section(
        &mut self,
        group: &crate::store::SidebarProject,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = group.project.id.clone();
        let is_hovered = self.ui.hovered_project.as_ref() == Some(&id);
        let collapsed = self
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .sidebar_collapsed_projects
            .contains(&id);
        let project_for_click = group.project.clone();
        let project_root = group.project.root.clone();
        let project_host = group.host.clone();
        let project_is_remote = project_host.is_some();
        let project_host_label = group.host.as_deref().map(|host| {
            self.store
                .read()
                .expect("session store lock poisoned")
                .host_display_name(host)
        });
        let entity = cx.entity();
        let drag_label: SharedString = group.project.name.clone().into();
        let mut section = div().flex().flex_col().gap(px(1.0)).child(
            div()
                .id(format!("project:{}", id.0))
                .debug_selector({
                    let id = id.clone();
                    move || format!("PROJECT_{}", id.0)
                })
                .mt(px(6.0))
                .px(px(Space::ROW_H))
                .py(px(5.0))
                .min_h(px(Metrics::ROW_HEIGHT))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(Radius::ROW))
                .bg(Fill::hover(colors, is_hovered))
                .cursor_pointer()
                .on_hover(cx.listener({
                    let id = id.clone();
                    move |this, hovered: &bool, _, cx| {
                        this.ui.hovered_project = hovered.then(|| id.clone());
                        cx.notify();
                    }
                }))
                .on_click(cx.listener({
                    let id = id.clone();
                    move |this, _, _, cx| {
                        this.commit_rename();
                        let _ = this
                            .store
                            .write()
                            .expect("session store lock poisoned")
                            .toggle_project_collapsed(id.clone());
                        cx.notify();
                    }
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener({
                        let id = id.clone();
                        move |this, event: &gpui::MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.commit_rename();
                            this.ui.hover_card = None;
                            this.rename_focus.focus(window, cx);
                            this.ui.popover = Some(Popover::ProjectActions {
                                id: id.clone(),
                                position: Some(event.position),
                            });
                            cx.notify();
                        }
                    }),
                )
                .on_drag(
                    DraggedSidebarItem(DragItem::Project(id.clone())),
                    move |_, _, _, cx| {
                        cx.new(|_| DragPreview {
                            label: drag_label.clone(),
                            colors,
                        })
                    },
                )
                .drag_over::<DraggedSidebarItem>({
                    let id = id.clone();
                    move |element, dragged, _, cx| {
                        if let DragItem::Project(moved) = &dragged.0 {
                            entity.update(cx, |this, cx| {
                                this.reorder_project(moved, &id);
                                this.ui.drag_target = Some(format!("project:{}", id.0));
                                cx.notify();
                            });
                            element.bg(colors.primary.alpha(0.08))
                        } else {
                            element
                        }
                    }
                })
                .on_drop(cx.listener(|this, _: &DraggedSidebarItem, _, cx| {
                    this.finish_drag();
                    cx.notify();
                }))
                .drag_over::<ExternalPaths>(move |element, paths, _, _| {
                    if Self::can_accept_external_drop(
                        paths,
                        ExternalDropTarget::Project {
                            remote: project_is_remote,
                        },
                    ) {
                        element
                            .bg(Ink::FRESH.alpha(0.10))
                            .border_1()
                            .border_color(Ink::FRESH.alpha(0.38))
                    } else {
                        element
                    }
                })
                .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                    cx.stop_propagation();
                    this.external_drop(
                        paths,
                        ExternalDropTarget::Project {
                            remote: project_is_remote,
                        },
                        cx,
                    );
                }))
                .child(project_badge(colors))
                .child(
                    div()
                        .min_w(px(0.0))
                        .flex_1()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(px(Typo::ROW_EMPHASIZED.size))
                        .font_weight(Typo::ROW_EMPHASIZED.weight)
                        .text_color(colors.primary.alpha(0.90))
                        .child(group.project.name.clone()),
                )
                .when(group.pinned, |row| row.child(pin_mark(colors)))
                .when_some(project_host_label, |row, host| {
                    row.child(
                        div()
                            .max_w(px(72.0))
                            .px(px(5.0))
                            .py(px(1.0))
                            .rounded(px(Radius::CHIP))
                            .bg(Fill::subtle(colors))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(Typo::META.size - 1.0))
                            .text_color(colors.tertiary)
                            .child(host),
                    )
                })
                .child(
                    div()
                        .w(px(12.0))
                        .text_center()
                        .text_size(px(9.0))
                        .text_color(colors.secondary)
                        .child(sf_symbol_weighted(
                            if collapsed {
                                "chevron.right"
                            } else {
                                "chevron.down"
                            },
                            9.0,
                            SymbolWeight::Bold,
                            colors.secondary,
                        )),
                )
                .when(is_hovered, |row| {
                    row.child(
                        div()
                            .id(format!("project-menu:{}", id.0))
                            .w(px(20.0))
                            .h(px(20.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(Radius::CHIP))
                            .text_color(colors.secondary)
                            .child(sf_symbol_weighted(
                                "ellipsis",
                                12.0,
                                SymbolWeight::Semibold,
                                colors.secondary,
                            ))
                            .on_click(cx.listener({
                                let project = project_for_click.clone();
                                move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.ui.popover = Some(Popover::ProjectActions {
                                        id: project.id.clone(),
                                        position: None,
                                    });
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        div()
                            .id(format!("project-plus:{}", id.0))
                            .debug_selector({
                                let id = id.clone();
                                move || format!("PROJECT_ADD_{}", id.0)
                            })
                            .w(px(20.0))
                            .h(px(20.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(Radius::CHIP))
                            .text_color(colors.secondary)
                            .child(sf_symbol_weighted(
                                "plus",
                                12.0,
                                SymbolWeight::Medium,
                                colors.secondary,
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.open_new_agent_popover_at(
                                    Some(project_root.clone()),
                                    project_host.clone(),
                                    cx,
                                );
                            })),
                    )
                })
                .when(!is_hovered && collapsed, |row| {
                    row.child(AttentionDot::new(rollup_attention(&group.active), colors))
                }),
        );

        // The projection already folds a collapsed project away, so an empty
        // row list here means "collapsed" without asking a second source.
        for row in &group.sessions {
            let shortcut = self.shortcut_for(row.id());
            section = section.child(self.session_row(row, shortcut, colors, window, cx));
        }
        if !collapsed && !group.archived.is_empty() {
            section = section.child(self.archived_bucket(group, colors, window, cx));
        }
        section.into_any_element()
    }

    fn session_row(
        &mut self,
        row: &crate::store::SidebarRow,
        shortcut: Option<usize>,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session = &row.session;
        let id = session.id.clone();
        let (selected, multi, drag_selection, migrating) = {
            let mut store = self.store.write().expect("session store lock poisoned");
            (
                store.selected_session_id() == Some(&id),
                store.sidebar_selection().contains(&id),
                (store.sidebar_selection().len() > 1).then(|| store.sidebar_selection_ordered()),
                store.migrating().contains(&id),
            )
        };
        let hovered = self.ui.hovered_session.as_ref() == Some(&id);
        let archived = session.is_archived();
        let hibernated = session.hibernation.is_some();
        let session_is_remote = session.host.is_some();
        let ended = matches!(session.status, diri_proto::SessionStatus::Exited(_)) && !archived;
        let host_label = session.host.as_ref().map(|host| {
            self.store
                .read()
                .expect("session store lock poisoned")
                .host_display_name(host)
        });
        let title = display_title(session);
        let non_persistent =
            session.remote_persistence == Some(PersistenceCapability::NonPersistent);
        // Read before the title moves into the marquee below.
        let ended_chip = ended && title != ENDED_TITLE;
        let title_available_width = session_title_available_width(
            self.ui.width,
            row.depth,
            migrating,
            non_persistent,
            ended_chip,
            host_label.as_deref(),
            hibernated,
            row.pinned,
            !hovered && selected && shortcut.is_some(),
        );
        let title_marquee_id = format!("session-title-marquee:{}", id.0);
        let fill = if selected {
            RowFill::Selected
        } else if multi {
            RowFill::MultiSelected
        } else if hovered {
            RowFill::Hover
        } else {
            RowFill::Clear
        };

        if self.ui.renaming.as_ref() == Some(&id) {
            return div()
                .id(format!("rename:{}", id.0))
                .pl(px(Space::ROW_H))
                .pr(px(Space::ROW_H))
                .h(px(Metrics::ROW_HEIGHT))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(Radius::ROW))
                .bg(RowFill::Selected.color(colors))
                .drag_over::<ExternalPaths>({
                    let id = id.clone();
                    move |element, paths, _, _| {
                        if Self::can_accept_external_drop(
                            paths,
                            ExternalDropTarget::Session {
                                id: id.clone(),
                                remote: session_is_remote,
                            },
                        ) {
                            element
                                .bg(Palette::GEMINI_BLUE.alpha(0.12))
                                .border_1()
                                .border_color(Palette::GEMINI_BLUE.alpha(0.42))
                        } else {
                            element
                        }
                    }
                })
                .on_drop(cx.listener({
                    let id = id.clone();
                    move |this, paths: &ExternalPaths, _, cx| {
                        cx.stop_propagation();
                        this.commit_rename();
                        this.external_drop(
                            paths,
                            ExternalDropTarget::Session {
                                id: id.clone(),
                                remote: session_is_remote,
                            },
                            cx,
                        );
                    }
                }))
                .children(indent_rails(row, colors))
                // The fold control is inert mid-rename, but its column stays so
                // the text does not slide sideways the moment editing starts.
                .child(div().w(px(Space::INDENT)).flex_none())
                .child(
                    div()
                        .size(px(16.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(self.status_glyph(session, migrating, colors, window, cx)),
                )
                .child(
                    div()
                        .min_w(px(0.0))
                        .flex_1()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_size(px(Typo::ROW.size))
                        .text_color(colors.primary)
                        .child(query_label(&self.ui.rename_draft)),
                )
                .into_any_element();
        }

        let row_session = Arc::clone(session);
        let rename_session = Arc::clone(session);
        let close_id = id.clone();
        let hover_id = id.clone();
        let drag_item = if multi && let Some(selection) = drag_selection {
            DragItem::Sessions(selection)
        } else {
            DragItem::Session {
                id: id.clone(),
                project: session.project_id.clone(),
                parent: session.parent.clone(),
                archived,
            }
        };
        let drag_payload = DraggedSidebarItem(drag_item);
        let drag_label: SharedString = title.clone().into();
        let entity = cx.entity();
        div()
            .id(format!("session:{}", id.0))
            .pl(px(Space::ROW_H))
            .pr(px(Space::ROW_H))
            .h(px(Metrics::ROW_HEIGHT))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(Radius::ROW))
            .bg(fill.color(colors))
            .opacity(if archived {
                0.58
            } else if hibernated {
                0.74
            } else {
                1.0
            })
            .cursor_pointer()
            .on_hover(cx.listener(move |this, is_hovered: &bool, window, cx| {
                this.ui.hovered_session = is_hovered.then(|| hover_id.clone());
                this.schedule_hover_card(hover_id.clone(), *is_hovered, window, cx);
                cx.notify();
            }))
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    this.commit_rename();
                    if event.click_count() == 2 {
                        this.begin_rename(&row_session, window, cx);
                        return;
                    }
                    let modifiers = event.modifiers();
                    this.store
                        .write()
                        .expect("session store lock poisoned")
                        .sidebar_click(
                            row_session.id.clone(),
                            ClickModifiers {
                                command: modifiers.platform,
                                shift: modifiers.shift,
                            },
                        );
                    if !modifiers.platform && !modifiers.shift {
                        cx.emit(SidebarEvent::SessionActivated);
                    }
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, _, _, cx| {
                    this.close_sessions(vec![close_id.clone()], cx);
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.commit_rename();
                    this.ui.hover_card = None;
                    this.rename_focus.focus(window, cx);
                    this.ui.popover = Some(Popover::SessionActions {
                        id: rename_session.id.clone(),
                        position: event.position,
                    });
                    cx.notify();
                }),
            )
            .on_drag(drag_payload, move |_, _, _, cx| {
                cx.new(|_| DragPreview {
                    label: drag_label.clone(),
                    colors,
                })
            })
            .drag_over::<DraggedSidebarItem>({
                let target = id.clone();
                let target_project = session.project_id.clone();
                let target_parent = session.parent.clone();
                move |element, dragged, _, cx| {
                    // Siblings only. A row dropped on a cousin would shuffle
                    // the manual order without moving anything on screen,
                    // because each sibling run is sorted among itself.
                    if let DragItem::Session {
                        id: moved,
                        project,
                        parent,
                        archived: false,
                    } = &dragged.0
                        && project == &target_project
                        && parent == &target_parent
                    {
                        entity.update(cx, |this, cx| {
                            this.reorder_session(moved, &target);
                            this.ui.drag_target = Some(format!("session:{}", target.0));
                            cx.notify();
                        });
                        element.bg(colors.primary.alpha(0.08))
                    } else {
                        element
                    }
                }
            })
            .on_drop(cx.listener({
                let target_project = session.project_id.clone();
                move |this, dragged: &DraggedSidebarItem, _, cx| {
                    if let DragItem::Session {
                        id,
                        project,
                        archived: true,
                        ..
                    } = &dragged.0
                        && project == &target_project
                    {
                        this.store
                            .write()
                            .expect("session store lock poisoned")
                            .revive_sessions(vec![id.clone()]);
                    }
                    this.finish_drag();
                    cx.notify();
                }
            }))
            .drag_over::<ExternalPaths>({
                let id = id.clone();
                move |element, paths, _, _| {
                    if Self::can_accept_external_drop(
                        paths,
                        ExternalDropTarget::Session {
                            id: id.clone(),
                            remote: session_is_remote,
                        },
                    ) {
                        element
                            .bg(Palette::GEMINI_BLUE.alpha(0.12))
                            .border_1()
                            .border_color(Palette::GEMINI_BLUE.alpha(0.42))
                    } else {
                        element
                    }
                }
            })
            .on_drop(cx.listener({
                let id = id.clone();
                move |this, paths: &ExternalPaths, _, cx| {
                    cx.stop_propagation();
                    this.external_drop(
                        paths,
                        ExternalDropTarget::Session {
                            id: id.clone(),
                            remote: session_is_remote,
                        },
                        cx,
                    );
                }
            }))
            .children(indent_rails(row, colors))
            .child(self.disclosure(row, colors, cx))
            // The status glyph is the row's whole reason for existing at a
            // glance, so it no longer yields its slot to the close button on
            // hover -- pointing at a working agent used to hide the fact that
            // it was working. The ✕ lives at the trailing edge instead.
            .child(
                div()
                    .size(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(self.status_glyph(session, migrating, colors, window, cx)),
            )
            .child(
                HoverMarquee::new(
                    title_marquee_id,
                    title,
                    hovered,
                    title_available_width,
                    Typo::ROW.size,
                    colors.primary.alpha(if selected { 1.0 } else { 0.82 }),
                )
                .font_weight(Typo::ROW.weight),
            )
            .when(row.pinned, |element| element.child(pin_mark(colors)))
            // Chips, in descending order of how much they explain an otherwise
            // inert-looking row. Each is flex_none and the title absorbs the
            // remaining width, so a narrow sidebar truncates the title rather
            // than dropping the reason it is not moving.
            .when(migrating, |element| {
                element.child(StateChip::new("Moving…", colors.secondary, colors))
            })
            .when(non_persistent, |element| {
                // Louder than the rest of the lane on purpose: this session
                // cannot survive a detach, so closing the window loses it.
                element.child(AlertChip::new("No detach"))
            })
            .when(ended_chip, |element| {
                // An exited session with a real title otherwise looks alive:
                // the glyph goes quiet and nothing else says why.
                element.child(StateChip::new("Ended", colors.tertiary, colors))
            })
            .when(hibernated, |element| {
                // Hibernation chip. An 8px moon glyph was a smudge at this
                // size; the chip reads at a glance and matches the host badge.
                element.child(StateChip::new("Zzz", colors.tertiary, colors))
            })
            .when_some(host_label, |element, host| {
                // Remote-host chip: this session's agent runs on another machine.
                element.child(StateChip::new(host, colors.tertiary, colors))
            })
            .when(hovered, |element| {
                let close_id = id.clone();
                element.child(
                    div()
                        .id(format!("close:{}", id.0))
                        .size(px(16.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(Radius::CHIP))
                        .cursor_pointer()
                        .text_color(colors.secondary)
                        .hover(move |button| button.bg(Fill::subtle(colors)))
                        // The row is draggable, and a press that wanders
                        // 2px turns into a drag that swallows the click.
                        // Keeping mouse-down off the row makes every press
                        // on the ✕ a close.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation();
                        })
                        .child(sf_symbol_weighted(
                            "xmark",
                            8.5,
                            SymbolWeight::Bold,
                            colors.secondary,
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_sessions(vec![close_id.clone()], cx);
                            cx.notify();
                        })),
                )
            })
            // The hint and the ✕ share the trailing edge and never both apply:
            // hovering a row is the moment you want to close it, not the
            // moment you need to be told how to reach it from the keyboard.
            .when_some(
                (!hovered && selected).then_some(shortcut).flatten(),
                |element, index| {
                    element.child(
                        div()
                            .flex_none()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child(format!("⌘{index}")),
                    )
                },
            )
            .into_any_element()
    }

    /// The fold control for a row that spawned children, drawn in the same
    /// column a deeper row's rail occupies so titles stay on one axis whether
    /// or not a row has children.
    fn disclosure(
        &self,
        row: &crate::store::SidebarRow,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let slot = div().w(px(Space::INDENT)).flex_none().flex().items_center();
        if !row.has_children {
            return slot.into_any_element();
        }
        let id = row.id().clone();
        slot.id(format!("fold:{}", id.0))
            .justify_center()
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                let _ = this
                    .store
                    .write()
                    .expect("session store lock poisoned")
                    .toggle_session_collapsed(id.clone());
                cx.notify();
            }))
            .child(sf_symbol_weighted(
                if row.collapsed {
                    "chevron.right"
                } else {
                    "chevron.down"
                },
                8.0,
                SymbolWeight::Bold,
                colors.tertiary,
            ))
            .into_any_element()
    }

    fn archived_bucket(
        &mut self,
        group: &crate::store::SidebarProject,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let project_id = group.project.id.clone();
        let expanded = self
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .sidebar_expanded_archives
            .contains(&project_id);
        let targeted =
            self.ui.drag_target.as_deref() == Some(format!("archive:{}", project_id.0).as_str());
        let mut bucket = div()
            .id(format!("archive:{}", project_id.0))
            .flex()
            .flex_col()
            .rounded(px(Radius::ROW))
            .when(targeted, |element| {
                element
                    .bg(colors.primary.alpha(0.08))
                    .border_1()
                    .border_color(colors.primary.alpha(0.18))
            })
            .drag_over::<DraggedSidebarItem>({
                let entity = cx.entity();
                let project_id = project_id.clone();
                move |element, dragged, _, cx| {
                    let valid = match &dragged.0 {
                        DragItem::Session {
                            project, archived, ..
                        } => project == &project_id && !archived,
                        DragItem::Sessions(_) => true,
                        DragItem::Project(_) => false,
                    };
                    if valid {
                        entity.update(cx, |this, cx| {
                            this.ui.drag_target = Some(format!("archive:{}", project_id.0));
                            cx.notify();
                        });
                        element.bg(colors.primary.alpha(0.08))
                    } else {
                        element
                    }
                }
            })
            .on_drop(cx.listener({
                let project_id = project_id.clone();
                move |this, dragged: &DraggedSidebarItem, _, cx| {
                    let ids = match &dragged.0 {
                        DragItem::Session {
                            id,
                            project,
                            archived: false,
                            ..
                        } if project == &project_id => vec![id.clone()],
                        DragItem::Sessions(ids) => ids.clone(),
                        _ => Vec::new(),
                    };
                    this.archive_sessions(ids);
                    this.finish_drag();
                    cx.notify();
                }
            }))
            .child(
                div()
                    .mx(px(Space::ROW_H))
                    .mt(px(3.0))
                    .mb(px(1.0))
                    .h(px(1.0))
                    .bg(colors.primary.alpha(0.06)),
            )
            .child(
                div()
                    .id(format!("archive-header:{}", project_id.0))
                    .pl(px(Space::ROW_H + Space::INDENT))
                    .pr(px(Space::ROW_H))
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .cursor_pointer()
                    .text_size(px(Typo::SECTION_HEADER.size))
                    .font_weight(Typo::SECTION_HEADER.weight)
                    .text_color(colors.tertiary)
                    .on_click(cx.listener({
                        let project_id = project_id.clone();
                        move |this, _, _, cx| {
                            let _ = this
                                .store
                                .write()
                                .expect("session store lock poisoned")
                                .toggle_archive_expanded(project_id.clone());
                            cx.notify();
                        }
                    }))
                    .child(sf_symbol_weighted(
                        "archivebox",
                        9.0,
                        SymbolWeight::Semibold,
                        colors.tertiary,
                    ))
                    .child(format!("Archived · {}", group.archived.len()))
                    .child(sf_symbol_weighted(
                        if expanded {
                            "chevron.down"
                        } else {
                            "chevron.right"
                        },
                        8.0,
                        SymbolWeight::Bold,
                        colors.tertiary,
                    )),
            );
        if expanded {
            for session in &group.archived {
                bucket = bucket.child(self.archived_row(session, colors, window, cx));
            }
        }
        bucket.into_any_element()
    }

    fn archived_row(
        &mut self,
        session: &SessionRecord,
        colors: SemanticColors,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = session.id.clone();
        let hovered = self.ui.hovered_session.as_ref() == Some(&id);
        let selected = self
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session_id()
            == Some(&id);
        let row_session = session.clone();
        let revive_id = id.clone();
        let title = display_title(session);
        let drag_label: SharedString = title.clone().into();
        div()
            .id(format!("archived-session:{}", id.0))
            .pl(px(Space::ROW_H + Space::INDENT))
            .pr(px(Space::ROW_H))
            .h(px(Metrics::ROW_HEIGHT))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(Radius::ROW))
            .opacity(0.58)
            .bg(if selected {
                RowFill::Selected.color(colors)
            } else if hovered {
                RowFill::Hover.color(colors)
            } else {
                RowFill::Clear.color(colors)
            })
            .cursor_pointer()
            .on_hover(cx.listener({
                let id = id.clone();
                move |this, is_hovered: &bool, _, cx| {
                    this.ui.hovered_session = is_hovered.then(|| id.clone());
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                let modifiers = event.modifiers();
                this.store
                    .write()
                    .expect("session store lock poisoned")
                    .sidebar_click(
                        row_session.id.clone(),
                        ClickModifiers {
                            command: modifiers.platform,
                            shift: modifiers.shift,
                        },
                    );
                if !modifiers.platform && !modifiers.shift {
                    cx.emit(SidebarEvent::SessionActivated);
                }
                cx.notify();
            }))
            .on_drag(
                DraggedSidebarItem(DragItem::Session {
                    id: id.clone(),
                    project: session.project_id.clone(),
                    parent: session.parent.clone(),
                    archived: true,
                }),
                move |_, _, _, cx| {
                    cx.new(|_| DragPreview {
                        label: drag_label.clone(),
                        colors,
                    })
                },
            )
            .child(
                div()
                    .id(format!("revive:{}", id.0))
                    .size(px(16.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Radius::CHIP))
                    .bg(if hovered {
                        Fill::subtle(colors)
                    } else {
                        colors.primary.alpha(0.0)
                    })
                    .text_size(px(if hovered { 9.0 } else { 10.0 }))
                    .text_color(colors.secondary)
                    .child(sf_symbol_weighted(
                        if hovered {
                            "tray.and.arrow.up.fill"
                        } else {
                            "archivebox.fill"
                        },
                        if hovered { 8.0 } else { 10.0 },
                        if hovered {
                            SymbolWeight::Bold
                        } else {
                            SymbolWeight::Regular
                        },
                        colors.secondary,
                    ))
                    .when(hovered, |button| {
                        button.on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.store
                                .write()
                                .expect("session store lock poisoned")
                                .revive_sessions(vec![revive_id.clone()]);
                            cx.notify();
                        }))
                    }),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(Typo::ROW.size))
                    .text_color(colors.primary.alpha(if selected { 1.0 } else { 0.82 }))
                    .child(title),
            )
            .into_any_element()
    }

    /// The update indicator above the account row.
    ///
    /// This is the whole of diri's update UI in the main window, and it stays
    /// out of the way on purpose: a background check that finds something
    /// lights this row and nothing else. Clicking it advances one step —
    /// download, then restart — so an update never begins or completes without
    /// a deliberate click. Manual checks additionally show their outcome here
    /// so "Check for Updates…" is not a command that appears to do nothing.
    fn update_pill(&self, colors: SemanticColors, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.preview || !self.update.is_noteworthy() {
            return None;
        }
        let (symbol, tint, command) = match &self.update.phase {
            UpdatePhase::Available(_) => (
                "arrow.down.circle",
                diri_ui::Ink::FRESH,
                Some(UpdateCommand::Download),
            ),
            UpdatePhase::Downloading { .. } => ("arrow.down.circle", colors.secondary, None),
            UpdatePhase::Ready(_) => (
                "arrow.clockwise.circle",
                diri_ui::Ink::FRESH,
                Some(UpdateCommand::Install),
            ),
            UpdatePhase::Installing => ("arrow.clockwise.circle", colors.secondary, None),
            UpdatePhase::Failed(_) => (
                "exclamationmark.triangle",
                diri_ui::Ink::DANGER,
                Some(UpdateCommand::Dismiss),
            ),
            UpdatePhase::Checking => ("arrow.triangle.2.circlepath", colors.secondary, None),
            UpdatePhase::UpToDate => (
                "checkmark.circle",
                colors.secondary,
                Some(UpdateCommand::Dismiss),
            ),
            UpdatePhase::Idle | UpdatePhase::Unsupported(_) => return None,
        };
        let interactive = command.is_some();
        let hovered = interactive && self.ui.hovered_control == Some("update");
        let mut pill = div()
            .id("update-pill")
            .mb(px(3.0))
            .px(px(Space::ROW_H))
            .h(px(Metrics::ROW_HEIGHT))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(Radius::ROW))
            .bg(Fill::hover(colors, hovered))
            .child(
                div()
                    .w(px(16.0))
                    .text_center()
                    .child(sf_symbol(symbol, 12.5, tint)),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(Typo::ROW.size))
                    .text_color(if interactive { tint } else { colors.secondary })
                    .child(self.update.summary()),
            )
            .on_hover(cx.listener(move |this, is_hovered: &bool, _, cx| {
                this.ui.hovered_control = (interactive && *is_hovered).then_some("update");
                cx.notify();
            }));
        if let Some(command) = command {
            pill = pill.cursor_pointer().on_click(cx.listener(
                move |_, _, _, cx: &mut Context<Self>| {
                    cx.emit(SidebarEvent::Update(command.clone()));
                },
            ));
        }
        Some(pill.into_any_element())
    }

    fn account_footer(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let hovered = self.ui.hovered_control == Some("account");
        let cost = if self.preview {
            Some(PREVIEW_USAGE)
        } else {
            self.usage
                .map(|snapshot| snapshot.today().cost)
                .filter(|cost| *cost > 0.0)
        };
        div()
            .flex_none()
            .px(px(Space::INSET))
            .pt(px(5.0))
            .pb(px(10.0))
            .border_t_1()
            .border_color(colors.primary.alpha(0.06))
            .children(self.update_pill(colors, cx))
            .child(
                div()
                    .id("account")
                    .px(px(Space::ROW_H))
                    .h(px(Metrics::ROW_HEIGHT))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(Radius::ROW))
                    .bg(Fill::hover(colors, hovered))
                    .cursor_pointer()
                    .on_hover(cx.listener(|this, is_hovered: &bool, _, cx| {
                        this.ui.hovered_control = is_hovered.then_some("account");
                        cx.notify();
                    }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.ui.popover = Some(Popover::Account);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .w(px(16.0))
                            .text_center()
                            .text_size(px(13.0))
                            .text_color(colors.secondary)
                            .child(sf_symbol("person.crop.circle", 12.5, colors.secondary)),
                    )
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(Typo::ROW.size))
                            .text_color(colors.text(diri_ui::TextTone::Label))
                            .child(if self.preview {
                                "preview@dirijor.local"
                            } else {
                                "Local agents"
                            }),
                    )
                    .when_some(cost, |row, cost| {
                        row.child(
                            div()
                                .font_family(crate::fonts::mono_family())
                                .text_size(px(Typo::META_MONO.size))
                                .text_color(colors.tertiary)
                                .child(UsageFormat::money(cost)),
                        )
                    })
                    .child(div().text_size(px(9.0)).text_color(colors.tertiary).child(
                        sf_symbol_weighted(
                            "chevron.up.chevron.down",
                            8.5,
                            SymbolWeight::Semibold,
                            colors.tertiary,
                        ),
                    )),
            )
            .into_any_element()
    }

    fn popover(
        &self,
        colors: SemanticColors,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        match self.ui.popover.clone()? {
            Popover::NewAgent { directory, host } => {
                Some(self.new_agent_popover(directory, host, colors, cx))
            }
            Popover::Account => Some(self.account_popover(colors, window, cx)),
            Popover::ProjectActions { id, position } => {
                Some(self.project_actions_popover(id, position, colors, cx))
            }
            Popover::SessionActions { id, position } => {
                Some(self.session_actions_popover(id, position, colors, cx))
            }
        }
    }

    fn popover_shell(
        &self,
        top: f32,
        child: impl IntoElement,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.popover_shell_at(
            point(px(12.0), px(top)),
            Anchor::TopLeft,
            244.0,
            child,
            colors,
            cx,
        )
    }

    /// Anchors above the account footer (Swift: popover opens upward).
    fn popover_shell_above_footer(
        &self,
        child: impl IntoElement,
        colors: SemanticColors,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let footer_top = f32::from(window.viewport_size().height) - 44.0;
        self.popover_shell_at(
            point(px(12.0), px(footer_top)),
            Anchor::BottomLeft,
            244.0,
            child,
            colors,
            cx,
        )
    }

    /// Menu-style floating panel: the palette's FloatingSurface recipe, a
    /// sidebar-wide scrim so stray clicks only dismiss, and mouse-down-out so
    /// clicking anywhere else in the window also dismisses. The panel itself
    /// is deferred + anchored in window coordinates so it escapes the sidebar
    /// wrapper's overflow clip and never gets cut off at narrow widths.
    fn popover_shell_at(
        &self,
        position: Point<Pixels>,
        anchor: Anchor,
        width: f32,
        child: impl IntoElement,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.ui.popover = None;
                    cx.notify();
                }),
            )
            .child(
                deferred(
                    anchored()
                        .position(position)
                        .anchor(anchor)
                        .snap_to_window_with_margin(px(8.0))
                        .child(
                            div()
                                .w(px(width))
                                .occlude()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                )
                                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                    this.ui.popover = None;
                                    cx.notify();
                                }))
                                .child(FloatingSurface::new(
                                    colors,
                                    div()
                                        .rounded(px(Radius::PANEL))
                                        .overflow_hidden()
                                        .py(px(4.0))
                                        .child(child),
                                )),
                        ),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    fn new_agent_popover(
        &self,
        directory: Option<String>,
        host: Option<String>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        {
            let mut store = self.store.write().expect("session store lock poisoned");
            if store.agent_catalog(host.as_deref()).is_none() {
                store.request_agent_catalog(host.clone(), false);
            }
        }
        let (local_target, default_kind, hosts, active_session, repo_state, syncing, options) = {
            let store = self.store.read().expect("session store lock poisoned");
            let selected_host_id = host.as_deref();
            (
                directory
                    .clone()
                    .unwrap_or_else(|| store.default_new_agent_directory()),
                store.preferences().default_agent.clone(),
                store.hosts().to_vec(),
                store.selected_session().cloned(),
                store.repo_target(selected_host_id).cloned(),
                store.syncing_prefs().clone(),
                agent_picker_options(store.agent_catalog(host.as_deref())),
            )
        };
        let selected_host = host
            .clone()
            .and_then(|id| hosts.iter().find(|entry| entry.id == id).cloned());
        let active_host = active_session
            .as_ref()
            .and_then(|session| session.host.clone());
        // A selected remote host owns its cwd. Repo preservation is useful
        // only for returning from a remote session to the corresponding local
        // checkout; it must never override a remote host's default cwd.
        let preserve_repo = should_resolve_active_repo(
            directory.as_deref(),
            selected_host.as_ref().map(|host| host.id.as_str()),
            active_host.as_deref(),
        );
        // Fallback target when the repo isn't resolvable: the host's default
        // cwd remotely; locally the active project (or, for a remote active
        // session, the first project that exists on this machine).
        let fallback_target = match &selected_host {
            Some(host) => remote_picker_target(directory.as_deref(), host.default_cwd.as_deref()),
            None if directory.is_none() && active_host.is_some() => self
                .store
                .read()
                .expect("session store lock poisoned")
                .local_fallback_directory(),
            None => local_target,
        };
        let repo_name = active_session.as_ref().map(|session| {
            session
                .cwd
                .rsplit('/')
                .next()
                .unwrap_or(&session.cwd)
                .to_owned()
        });
        let (target, subtitle) = if preserve_repo {
            match repo_state {
                Some(crate::store::RepoTarget::Resolved(path)) => (path, None),
                Some(crate::store::RepoTarget::Pending) => {
                    (fallback_target, Some("locating repo…".to_owned()))
                }
                Some(crate::store::RepoTarget::NotCloned) => {
                    let place = selected_host
                        .as_ref()
                        .map_or_else(|| "this Mac".to_owned(), |h| h.display_name().to_owned());
                    let folder = fallback_target
                        .rsplit('/')
                        .next()
                        .unwrap_or(&fallback_target)
                        .to_owned();
                    (
                        fallback_target,
                        Some(format!(
                            "{} not on {place} — opens in {folder}",
                            repo_name.as_deref().unwrap_or("repo")
                        )),
                    )
                }
                _ => (fallback_target, None),
            }
        } else {
            (fallback_target, None)
        };
        let folder = target.rsplit('/').next().unwrap_or(&target).to_owned();
        let location = selected_host.as_ref().map_or_else(
            || "This Mac".to_owned(),
            |host| host.display_name().to_owned(),
        );
        let mut header = div()
            .px(px(12.0))
            .pt(px(10.0))
            .pb(px(8.0))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(px(Typo::ROW_EMPHASIZED.size))
                            .font_weight(Typo::ROW_EMPHASIZED.weight)
                            .text_color(colors.primary)
                            .child("New Agent"),
                    )
                    .child(
                        div()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.secondary)
                            .child(location),
                    ),
            )
            .child({
                let browse_host = selected_host.as_ref().map(|entry| entry.id.clone());
                let browse_target = target.clone();
                div()
                    .id("new-agent-directory")
                    .px(px(4.0))
                    .py(px(3.0))
                    .ml(px(-4.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .rounded(px(Radius::CHIP))
                    .cursor_pointer()
                    .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                    .text_size(px(Typo::META.size))
                    .text_color(colors.secondary)
                    .child(sf_symbol("folder.fill", 11.0, colors.secondary))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(folder),
                    )
                    .child(sf_symbol("chevron.right", 9.0, colors.tertiary))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.directory_picker_open = true;
                        this.directory_scroll = ScrollHandle::new();
                        // Pin the currently visible target. A concurrent repo
                        // lookup must not switch the directory underneath an
                        // open browser and leave it waiting on the wrong key.
                        this.ui.popover = Some(Popover::NewAgent {
                            directory: Some(browse_target.clone()),
                            host: browse_host.clone(),
                        });
                        this.store
                            .write()
                            .expect("session store lock poisoned")
                            .request_directory_listing(browse_host.clone(), browse_target.clone());
                        cx.notify();
                    }))
            });
        if let Some(subtitle) = subtitle {
            // Repo-resolution state: "locating repo…" or the visible fallback
            // ("anara not on Forge — opens in code").
            header = header.child(
                div()
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child(subtitle),
            );
        }
        let mut content = div()
            .flex()
            .flex_col()
            .child(header)
            .child(HairlineDivider::horizontal(colors));
        // Host selector — only when hosts.json configures remote hosts:
        // "This Mac" plus one row per host, checkmark on the selection.
        //
        // This list is the single surface that owns the persisted shortcut
        // destination, so it must always be able to undo itself: "This Mac" is
        // the first row and is a real target, not just the absence of one, and
        // it is worded exactly like the destination printed on the always-
        // visible New Agent row ("Claude Code · This Mac · ⌘T") so the label a
        // user reads is the label they come here to change.
        if !hosts.is_empty() {
            content = content.child(
                div()
                    .px(px(12.0))
                    .pt(px(7.0))
                    .pb(px(3.0))
                    .text_size(px(Typo::SECTION_HEADER.size))
                    .font_weight(Typo::SECTION_HEADER.weight)
                    .text_color(colors.tertiary)
                    .child("Run shortcuts on"),
            );
            let mut targets: Vec<(Option<String>, String, &'static str)> =
                vec![(None, "This Mac".to_owned(), "desktopcomputer")];
            for entry in &hosts {
                targets.push((
                    Some(entry.id.clone()),
                    entry.display_name().to_owned(),
                    "network",
                ));
            }
            for (index, (target_host, label, symbol)) in targets.into_iter().enumerate() {
                let selected =
                    target_host.as_deref() == selected_host.as_ref().map(|entry| entry.id.as_str());
                let directory = directory.clone();
                let previous_host = host.clone();
                let active_host = active_host.clone();
                let sync_host = target_host.clone();
                let is_syncing = sync_host.as_deref().is_some_and(|id| syncing.contains(id));
                content = content.child(
                    div()
                        .id(format!("host-option-{index}"))
                        .debug_selector(move || format!("HOST_OPTION_{index}"))
                        .mx(px(6.0))
                        .my(px(1.0))
                        .px(px(8.0))
                        .h(px(28.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .rounded(px(Radius::ROW))
                        .cursor_pointer()
                        .hover(move |element| element.bg(colors.primary.alpha(0.06)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            let next_directory = if target_host != previous_host {
                                None
                            } else {
                                directory.clone()
                            };
                            // Choosing a row here is an explicit, persistent
                            // setting, not last-used memory: one destination
                            // drives this spawn, ⌘T, ⌥⌘T and the palette, so
                            // the checkmark never disagrees with where a
                            // shortcut lands. Persisting is only defensible
                            // because the choice stays visible (the New Agent
                            // row and every palette title name the target) and
                            // reversible (the "This Mac" row above sets it
                            // back, and a host deleted from hosts.json is
                            // repaired to local on load).
                            this.store
                                .write()
                                .expect("session store lock poisoned")
                                .set_default_spawn_host(target_host.clone());
                            this.store
                                .write()
                                .expect("session store lock poisoned")
                                .request_agent_catalog(target_host.clone(), false);
                            // Only remote -> local needs a matching checkout.
                            // A remote destination starts at its configured cwd.
                            if should_resolve_active_repo(
                                next_directory.as_deref(),
                                target_host.as_deref(),
                                active_host.as_deref(),
                            ) {
                                this.store
                                    .write()
                                    .expect("session store lock poisoned")
                                    .request_repo_target(target_host.clone());
                            }
                            this.directory_picker_open = false;
                            this.ui.popover = Some(Popover::NewAgent {
                                directory: next_directory,
                                host: target_host.clone(),
                            });
                            cx.notify();
                        }))
                        .child(sf_symbol(symbol, 11.0, colors.secondary))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(Typo::ROW.size))
                                .text_color(colors.primary)
                                .child(label),
                        )
                        .when(selected && sync_host.is_some(), |row| {
                            // Push local agent prefs to this host (rsync over
                            // ssh, daemon-side). Spins tertiary while running.
                            let sync_host = sync_host.clone();
                            row.child(
                                div()
                                    .id(format!("host-sync-{index}"))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .size(px(18.0))
                                    .rounded(px(Radius::CHIP))
                                    .cursor_pointer()
                                    .hover(move |element| element.bg(colors.primary.alpha(0.08)))
                                    .child(sf_symbol(
                                        "arrow.triangle.2.circlepath",
                                        10.0,
                                        if is_syncing {
                                            colors.tertiary
                                        } else {
                                            colors.secondary
                                        },
                                    ))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        if let Some(host) = sync_host.clone() {
                                            this.store
                                                .write()
                                                .expect("session store lock poisoned")
                                                .sync_prefs(host);
                                        }
                                        cx.notify();
                                    })),
                            )
                        })
                        .when(selected, |row| {
                            row.child(sf_symbol_weighted(
                                "checkmark",
                                10.0,
                                SymbolWeight::Semibold,
                                colors.secondary,
                            ))
                        }),
                );
            }
            content = content.child(HairlineDivider::horizontal(colors));
        }
        if self.directory_picker_open {
            content = content.child(self.directory_picker(
                selected_host.as_ref().map(|entry| entry.id.clone()),
                target,
                colors,
                cx,
            ));
            return self.popover_shell_at(
                point(px(12.0), px(70.0)),
                Anchor::TopLeft,
                320.0,
                content.pb(px(6.0)),
                colors,
                cx,
            );
        }
        // Carried on repo-preserving spawns so the daemon re-resolves the
        // checkout itself (covers a click that lands while still "locating").
        let same_repo_reference = if preserve_repo {
            active_session.as_ref().map(|session| session.id.clone())
        } else {
            None
        };
        for (index, option) in options.into_iter().enumerate() {
            let row_id = format!("agent-option-{index}");
            let target = target.clone();
            let spawn_host = selected_host.as_ref().map(|entry| entry.id.clone());
            let same_repo_as = same_repo_reference.clone();
            // The picker selection is also the global shortcut destination,
            // so every shortcut stays visible and follows the checkmark.
            let shortcut = agent_picker_shortcut(&option.kind, &default_kind, option.shortcut);
            let shortcut = shortcut.to_owned();
            let agent_kind = ui_agent_kind(&option.kind);
            let spawn_kind = option.kind.clone();
            let available = selected_host.is_some() || option.available;
            let unavailable = (!available).then_some(option.unavailable_detail).flatten();
            let setup_url = (!available).then_some(option.setup_url).flatten();
            content = content.child(
                div()
                    .id(row_id)
                    .debug_selector(move || format!("AGENT_OPTION_{index}"))
                    .mx(px(6.0))
                    .my(px(1.0))
                    .px(px(8.0))
                    .h(px(32.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(Radius::ROW))
                    .when(available, |row| {
                        row.cursor_pointer()
                            .hover(move |element| element.bg(colors.primary.alpha(0.06)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.store
                                    .write()
                                    .expect("session store lock poisoned")
                                    .spawn_kind(
                                        spawn_kind.clone(),
                                        SpawnOptions {
                                            cwd: Some(target.clone()),
                                            host: spawn_host.clone(),
                                            same_repo_as: same_repo_as.clone(),
                                            ..SpawnOptions::default()
                                        },
                                    );
                                this.ui.popover = None;
                                cx.notify();
                            }))
                    })
                    .child(
                        div()
                            .w(px(24.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(AgentLogo::new(agent_kind, 20.0, colors).badged(false)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .text_size(px(Typo::ROW.size))
                            .text_color(if available {
                                colors.primary
                            } else {
                                colors.secondary
                            })
                            .child(option.title)
                            .when_some(unavailable, |label, unavailable| {
                                label.child(
                                    div()
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .text_size(px(Typo::META.size))
                                        .text_color(colors.tertiary)
                                        .child(unavailable),
                                )
                            }),
                    )
                    .when_some(setup_url, |row, url| {
                        row.child(
                            div()
                                .id(format!("agent-setup-{index}"))
                                .px(px(6.0))
                                .py(px(3.0))
                                .rounded(px(Radius::CHIP))
                                .cursor_pointer()
                                .text_size(px(Typo::META.size))
                                .text_color(colors.secondary)
                                .bg(Fill::subtle(colors))
                                .hover(move |button| button.bg(colors.primary.alpha(0.10)))
                                .on_click(move |_, _, cx| cx.open_url(&url))
                                .child("Setup…"),
                        )
                    })
                    .when(!shortcut.is_empty(), |row| {
                        row.child(
                            div()
                                .px(px(5.0))
                                .py(px(2.0))
                                .rounded(px(Radius::CHIP))
                                .bg(Fill::subtle(colors))
                                .text_size(px(Typo::META.size))
                                .font_weight(Typo::META.weight)
                                .text_color(colors.tertiary)
                                .child(shortcut),
                        )
                    }),
            );
        }
        let manage_host = selected_host.as_ref().map(|host| host.id.clone());
        content = content.child(HairlineDivider::horizontal(colors)).child(
            div()
                .id("manage-agents")
                .mx(px(6.0))
                .my(px(1.0))
                .px(px(8.0))
                .h(px(32.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(Radius::ROW))
                .cursor_pointer()
                .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.ui.popover = None;
                    cx.emit(SidebarEvent::OpenAgentSettings(manage_host.clone()));
                    cx.notify();
                }))
                .child(sf_symbol("gearshape", 11.0, colors.secondary))
                .child(
                    div()
                        .text_size(px(Typo::ROW.size))
                        .text_color(colors.secondary)
                        .child("Manage Agents…"),
                ),
        );
        self.popover_shell(70.0, content.pb(px(6.0)), colors, cx)
    }

    fn directory_picker(
        &self,
        host: Option<String>,
        requested_path: String,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self
            .store
            .read()
            .expect("session store lock poisoned")
            .directory_listing(host.as_deref(), &requested_path)
            .cloned();
        let mut panel = div().flex().flex_col();
        match state {
            Some(DirectoryListingState::Ready(result)) => {
                let use_path = result.path.clone();
                let use_host = host.clone();
                panel = panel.child(
                    div()
                        .px(px(10.0))
                        .py(px(7.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .min_w(px(0.0))
                                .flex_1()
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_size(px(Typo::META_MONO.size))
                                .font_family(crate::fonts::mono_family())
                                .text_color(colors.secondary)
                                .child(result.path.clone()),
                        )
                        .child(
                            div()
                                .id("use-new-agent-directory")
                                .px(px(8.0))
                                .py(px(4.0))
                                .rounded(px(Radius::CHIP))
                                .bg(Ink::FRESH)
                                .text_size(px(Typo::META.size))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.background)
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.directory_picker_open = false;
                                    this.ui.popover = Some(Popover::NewAgent {
                                        directory: Some(use_path.clone()),
                                        host: use_host.clone(),
                                    });
                                    cx.notify();
                                }))
                                .child("Use folder"),
                        ),
                );
                let mut rows = div()
                    .id("directory-picker-list")
                    .track_scroll(&self.directory_scroll)
                    .max_h(px(260.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col();
                if let Some(parent) = result.parent {
                    let parent_host = host.clone();
                    rows = rows.child(directory_row(
                        "arrow.up",
                        "Parent folder".to_owned(),
                        colors,
                        cx.listener(move |this, _, _, cx| {
                            this.directory_scroll = ScrollHandle::new();
                            this.ui.popover = Some(Popover::NewAgent {
                                directory: Some(parent.clone()),
                                host: parent_host.clone(),
                            });
                            this.store
                                .write()
                                .expect("session store lock poisoned")
                                .request_directory_listing(parent_host.clone(), parent.clone());
                            cx.notify();
                        }),
                    ));
                }
                for entry in result.entries {
                    let next_host = host.clone();
                    let next_path = entry.path;
                    rows = rows.child(directory_row(
                        "folder",
                        entry.name,
                        colors,
                        cx.listener(move |this, _, _, cx| {
                            this.directory_scroll = ScrollHandle::new();
                            this.ui.popover = Some(Popover::NewAgent {
                                directory: Some(next_path.clone()),
                                host: next_host.clone(),
                            });
                            this.store
                                .write()
                                .expect("session store lock poisoned")
                                .request_directory_listing(next_host.clone(), next_path.clone());
                            cx.notify();
                        }),
                    ));
                }
                if result.truncated {
                    rows = rows.child(
                        div()
                            .px(px(12.0))
                            .py(px(7.0))
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child("Showing the first 512 folders"),
                    );
                }
                panel = panel.child(HairlineDivider::horizontal(colors)).child(rows);
            }
            Some(DirectoryListingState::Error(error)) => {
                let retry_host = host.clone();
                let retry_path = requested_path.clone();
                panel = panel.child(
                    div()
                        .px(px(12.0))
                        .py(px(12.0))
                        .flex()
                        .items_start()
                        .gap(px(8.0))
                        .text_size(px(Typo::META.size))
                        .text_color(colors.secondary)
                        .child(sf_symbol(
                            "exclamationmark.triangle.fill",
                            12.0,
                            Ink::ATTENTION,
                        ))
                        .child(div().min_w(px(0.0)).flex_1().child(error))
                        .child(
                            div()
                                .id("retry-directory-listing")
                                .cursor_pointer()
                                .text_color(Ink::FRESH)
                                .child("Retry")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.store
                                        .write()
                                        .expect("session store lock poisoned")
                                        .request_directory_listing(
                                            retry_host.clone(),
                                            retry_path.clone(),
                                        );
                                    cx.notify();
                                })),
                        ),
                );
            }
            Some(DirectoryListingState::Loading) | None => {
                panel = panel.child(
                    div()
                        .px(px(12.0))
                        .py(px(16.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .text_size(px(Typo::META.size))
                        .text_color(colors.secondary)
                        .child(LoadingIndicator::new(
                            "directory-listing-loading",
                            14.0,
                            colors.tertiary,
                        ))
                        .child("Loading folders…"),
                );
            }
        }
        panel.into_any_element()
    }

    /// Version line in the account popover, doubling as the manual check.
    ///
    /// Whatever the pill is showing wins here, so the popover never contradicts
    /// the footer two pixels above it; with nothing pending it falls back to
    /// the running version and a click starts a check.
    fn update_menu_row(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let unsupported = matches!(self.update.phase, UpdatePhase::Unsupported(_));
        let command = match &self.update.phase {
            UpdatePhase::Available(_) => Some(UpdateCommand::Download),
            UpdatePhase::Ready(_) => Some(UpdateCommand::Install),
            UpdatePhase::Checking | UpdatePhase::Downloading { .. } | UpdatePhase::Installing => {
                None
            }
            _ if unsupported => None,
            _ => Some(UpdateCommand::Check {
                user_initiated: true,
            }),
        };
        let label = if self.preview {
            format!("diri {}", crate::updates::CURRENT_VERSION)
        } else {
            self.update.summary()
        };
        let mut row = div()
            .id("account-version")
            .mx(px(6.0))
            .px(px(8.0))
            .h(px(28.0))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .rounded(px(Radius::ROW))
            .text_size(px(Typo::ROW.size))
            .text_color(if unsupported {
                colors.tertiary
            } else {
                colors.primary
            })
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(label),
            );
        if let Some(command) = command {
            let action = match command {
                UpdateCommand::Download => "Download",
                UpdateCommand::Install => "Restart",
                _ => "Check",
            };
            row = row
                .cursor_pointer()
                .hover(move |element| element.bg(colors.primary.alpha(0.06)))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(Typo::META.size))
                        .text_color(colors.secondary)
                        .child(action),
                )
                .on_click(cx.listener(move |this, _, _, cx: &mut Context<Self>| {
                    cx.emit(SidebarEvent::Update(command.clone()));
                    this.ui.popover = None;
                    cx.notify();
                }));
        }
        row.into_any_element()
    }

    fn account_popover(
        &self,
        colors: SemanticColors,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut usage = div()
            .flex()
            .flex_col()
            .child(section_label("Usage", colors));
        if self.preview {
            usage = usage
                .child(usage_row("Session", "resets in 2h 14m", "$2.31", colors))
                .child(usage_row("Today", "1.8M tokens", "$4.82", colors))
                .child(usage_row("This month", "", "$86.40", colors));
        } else if let Some(snapshot) = self.usage {
            usage = usage
                .child(usage_row(
                    "Session",
                    snapshot
                        .session_remaining_seconds
                        .map(|seconds| format!("resets in {}", compact_duration(seconds)))
                        .as_deref()
                        .unwrap_or("idle"),
                    &snapshot
                        .session_cost
                        .map(UsageFormat::money)
                        .unwrap_or_else(|| "—".into()),
                    colors,
                ))
                .child(usage_row(
                    "Today",
                    &format!(
                        "{} tokens",
                        UsageFormat::tokens(snapshot.today().total_tokens())
                    ),
                    &UsageFormat::money(snapshot.today().cost),
                    colors,
                ))
                .child(usage_row(
                    "This month",
                    "",
                    &UsageFormat::money(snapshot.month().cost),
                    colors,
                ));
        } else {
            usage = usage.child(
                div()
                    .px(px(14.0))
                    .py(px(6.0))
                    .text_size(px(Typo::ROW.size))
                    .text_color(colors.tertiary)
                    .child("Measuring…"),
            );
        }
        let content = div()
            .flex()
            .flex_col()
            .child(usage)
            .child(div().mt(px(8.0)).h(px(1.0)).bg(colors.primary.alpha(0.06)))
            .child(section_label("Version", colors))
            .child(self.update_menu_row(colors, cx))
            .child(div().mt(px(8.0)).h(px(1.0)).bg(colors.primary.alpha(0.06)))
            .child(section_label("Remote", colors))
            .child(
                div()
                    .id("quick-add-remote-host")
                    .debug_selector(|| "quick-add-remote-host".into())
                    .mx(px(6.0))
                    .px(px(8.0))
                    .h(px(30.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(Radius::ROW))
                    .cursor_pointer()
                    .hover(move |element| element.bg(colors.primary.alpha(0.06)))
                    .text_size(px(Typo::ROW.size))
                    .text_color(colors.primary)
                    .child(sf_symbol("plus", 11.0, colors.secondary))
                    .child(div().flex_1().child("Add remote host"))
                    .child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child("SSH"),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.ui.popover = None;
                        cx.emit(SidebarEvent::AddRemoteHost);
                        cx.notify();
                    })),
            )
            .child(div().mt(px(8.0)).h(px(1.0)).bg(colors.primary.alpha(0.06)))
            .child(section_label("Account", colors))
            .child(
                div()
                    .id("account-active")
                    .mx(px(6.0))
                    .px(px(8.0))
                    .h(px(28.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(Radius::ROW))
                    .text_size(px(Typo::ROW.size))
                    .text_color(colors.primary)
                    .child(sf_symbol_weighted(
                        "checkmark",
                        10.0,
                        SymbolWeight::Semibold,
                        colors.secondary,
                    ))
                    .child(if self.preview {
                        "preview@dirijor.local"
                    } else {
                        "Local agents"
                    }),
            )
            .child(
                div()
                    .id("dismiss-account")
                    .mx(px(6.0))
                    .my(px(6.0))
                    .px(px(8.0))
                    .h(px(28.0))
                    .flex()
                    .items_center()
                    .rounded(px(Radius::ROW))
                    .cursor_pointer()
                    .hover(move |element| element.bg(colors.primary.alpha(0.06)))
                    .text_size(px(Typo::ROW.size))
                    .text_color(colors.secondary)
                    .child("Done")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.ui.popover = None;
                        cx.notify();
                    })),
            );
        self.popover_shell_above_footer(content, colors, window, cx)
    }

    fn project_actions_popover(
        &self,
        id: ProjectId,
        position: Option<Point<Pixels>>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (project, host, collapsed, pinned) = {
            let store = self.store.read().expect("session store lock poisoned");
            let Some(project) = store.projects().get(&id).cloned() else {
                return div().into_any_element();
            };
            (
                project,
                store
                    .sessions()
                    .values()
                    .find(|session| session.project_id == id)
                    .and_then(|session| session.host.clone()),
                store.preferences().sidebar_collapsed_projects.contains(&id),
                store.preferences().sidebar_pinned_projects.contains(&id),
            )
        };
        let content = div()
            .p(px(6.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(menu_row(
                "New Session Here",
                colors,
                cx.listener({
                    let root = project.root.clone();
                    let host = host.clone();
                    move |this, _, _, cx| {
                        this.open_new_agent_popover_at(Some(root.clone()), host.clone(), cx);
                    }
                }),
            ))
            .child(menu_row(
                if pinned {
                    "Unpin Project"
                } else {
                    "Pin Project"
                },
                colors,
                cx.listener({
                    let id = id.clone();
                    move |this, _, _, cx| {
                        let _ = this
                            .store
                            .write()
                            .expect("session store lock poisoned")
                            .toggle_project_pin(id.clone());
                        this.ui.popover = None;
                        cx.notify();
                    }
                }),
            ))
            .child(menu_row(
                if collapsed { "Expand" } else { "Collapse" },
                colors,
                cx.listener(move |this, _, _, cx| {
                    let _ = this
                        .store
                        .write()
                        .expect("session store lock poisoned")
                        .toggle_project_collapsed(id.clone());
                    this.ui.popover = None;
                    cx.notify();
                }),
            ));
        match position {
            Some(position) => {
                self.popover_shell_at(position, Anchor::TopLeft, 200.0, content, colors, cx)
            }
            None => self.popover_shell(96.0, content, colors, cx),
        }
    }

    /// Right-click context menu for a session row, anchored at the click.
    /// Mirrors the Swift SessionContextMenu, limited to actions the Rust
    /// store implements.
    fn session_actions_popover(
        &self,
        id: SessionId,
        position: Point<Pixels>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (session, pinned, bulk, hosts, migrating) = {
            let mut store = self.store.write().expect("session store lock poisoned");
            let Some(session) = store.sessions().get(&id).cloned() else {
                return div().into_any_element();
            };
            let pinned = store.preferences().sidebar_pinned_sessions.contains(&id);
            // The whole multi-selection, when the right-clicked row is part
            // of one (Swift: bulk actions split archive/revive honestly).
            let bulk =
                if store.sidebar_selection().len() > 1 && store.sidebar_selection().contains(&id) {
                    store.sidebar_selection_ordered()
                } else {
                    Vec::new()
                };
            let hosts = store.hosts().to_vec();
            let migrating = store.migrating().contains(&id);
            (session, pinned, bulk, hosts, migrating)
        };
        let mut content = div().p(px(6.0)).flex().flex_col().gap(px(2.0));
        if bulk.len() > 1 {
            let (active, parked): (Vec<SessionId>, Vec<SessionId>) = {
                let store = self.store.read().expect("session store lock poisoned");
                bulk.iter().cloned().partition(|session_id| {
                    store
                        .sessions()
                        .get(session_id)
                        .is_none_or(|session| !session.is_archived())
                })
            };
            if !active.is_empty() {
                content = content.child(menu_row(
                    count_label("Archive", active.len()),
                    colors,
                    cx.listener(move |this, _, _, cx| {
                        this.archive_sessions(active.clone());
                        this.ui.popover = None;
                        cx.notify();
                    }),
                ));
            }
            if !parked.is_empty() {
                content = content.child(menu_row(
                    count_label("Revive", parked.len()),
                    colors,
                    cx.listener(move |this, _, _, cx| {
                        this.store
                            .write()
                            .expect("session store lock poisoned")
                            .revive_sessions(parked.clone());
                        this.ui.popover = None;
                        cx.notify();
                    }),
                ));
            }
            content = content.child(menu_row(
                count_label("Close", bulk.len()),
                colors,
                cx.listener(move |this, _, _, cx| {
                    this.close_sessions(bulk.clone(), cx);
                    this.ui.popover = None;
                    cx.notify();
                }),
            ));
        } else if session.is_archived() {
            content = content
                .child(menu_row(
                    "Revive",
                    colors,
                    cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            this.store
                                .write()
                                .expect("session store lock poisoned")
                                .revive_sessions(vec![id.clone()]);
                            this.ui.popover = None;
                            cx.notify();
                        }
                    }),
                ))
                .child(menu_row(
                    "Remove from Sidebar",
                    colors,
                    cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            this.close_sessions(vec![id.clone()], cx);
                            this.ui.popover = None;
                            cx.notify();
                        }
                    }),
                ))
                .child(menu_divider(colors))
                .child(copy_session_id_row(id, colors, cx));
        } else {
            let running = !matches!(session.status, diri_proto::SessionStatus::Exited(_));
            if !running && session.resumability == diri_proto::Resumability::Resumable {
                content = content.child(menu_row(
                    "Resume",
                    colors,
                    cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            this.store
                                .read()
                                .expect("session store lock poisoned")
                                .resume(id.clone());
                            this.ui.popover = None;
                            cx.notify();
                        }
                    }),
                ));
            }
            // Session handoff (Claude only): local sessions offer "Move to
            // <host>", remote ones "Move to Local". Hidden while a move is
            // in flight so a double-click can't queue a second migration.
            if session.kind == ProtoAgentKind::CLAUDE_CODE && !hosts.is_empty() && !migrating {
                if let Some(current) = &session.host {
                    if hosts.iter().any(|entry| &entry.id == current) {
                        content = content.child(menu_row(
                            "Move to Local",
                            colors,
                            cx.listener({
                                let id = id.clone();
                                move |this, _, _, cx| {
                                    this.store
                                        .write()
                                        .expect("session store lock poisoned")
                                        .migrate_session(id.clone(), None);
                                    this.ui.popover = None;
                                    cx.notify();
                                }
                            }),
                        ));
                    }
                } else {
                    for entry in &hosts {
                        let target = entry.id.clone();
                        content = content.child(menu_row(
                            format!("Move to {}", entry.display_name()),
                            colors,
                            cx.listener({
                                let id = id.clone();
                                move |this, _, _, cx| {
                                    this.store
                                        .write()
                                        .expect("session store lock poisoned")
                                        .migrate_session(id.clone(), Some(target.clone()));
                                    this.ui.popover = None;
                                    cx.notify();
                                }
                            }),
                        ));
                    }
                }
            }
            let rename_session = session.clone();
            content = content
                .child(menu_row(
                    // Shells/Cursor can't resume a conversation — archiving
                    // still works, but say what reviving will get you.
                    if session.resumability == diri_proto::Resumability::NotResumable {
                        "Archive (won't be resumable)"
                    } else {
                        "Archive Session"
                    },
                    colors,
                    cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            this.archive_sessions(vec![id.clone()]);
                            this.ui.popover = None;
                            cx.notify();
                        }
                    }),
                ))
                .child(menu_row(
                    "Rename…",
                    colors,
                    cx.listener(move |this, _, window, cx| {
                        this.ui.popover = None;
                        this.begin_rename(&rename_session, window, cx);
                    }),
                ))
                .child(menu_row(
                    if pinned {
                        "Unpin Session"
                    } else {
                        "Pin Session"
                    },
                    colors,
                    cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            let _ = this
                                .store
                                .write()
                                .expect("session store lock poisoned")
                                .toggle_session_pin(id.clone());
                            this.ui.popover = None;
                            cx.notify();
                        }
                    }),
                ))
                .child(menu_row(
                    "Remove from Sidebar",
                    colors,
                    cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            this.close_sessions(vec![id.clone()], cx);
                            this.ui.popover = None;
                            cx.notify();
                        }
                    }),
                ))
                .child(menu_divider(colors))
                .child(copy_session_id_row(id, colors, cx));
        }
        self.popover_shell_at(position, Anchor::TopLeft, 220.0, content, colors, cx)
    }

    /// The sidebar's own translucent fill. Shared with the scroll fades so the
    /// two never drift apart.
    fn surface_fill(colors: SemanticColors) -> Rgba {
        colors.sidebar_surface()
    }

    /// Top/bottom gradient masks over the session list, each fading in over the
    /// first few pixels of travel so a list that fits shows neither.
    fn scroll_fades(&self, colors: SemanticColors) -> Vec<AnyElement> {
        const HEIGHT: f32 = 28.0;
        /// Scroll distance over which a mask reaches full strength.
        const RAMP: f32 = 14.0;

        let scrolled = f32::from(self.list_scroll.offset().y).min(0.0).abs();
        let remaining = (f32::from(self.list_scroll.max_offset().y) - scrolled).max(0.0);
        // Opaque at the edge: the sidebar's own fill is translucent, and
        // fading to it would leave a legible ghost of the clipped row.
        let fill = Hsla {
            a: 1.0,
            ..Self::surface_fill(colors).into()
        };
        let mut fades = Vec::new();
        for (strength, angle, edge) in [
            ((scrolled / RAMP).min(1.0), 180.0, true),
            ((remaining / RAMP).min(1.0), 0.0, false),
        ] {
            if strength <= 0.01 {
                continue;
            }
            let mask = div()
                .absolute()
                .left_0()
                .right_0()
                .h(px(HEIGHT))
                .opacity(strength)
                .bg(linear_gradient(
                    angle,
                    linear_color_stop(fill, 0.0),
                    linear_color_stop(fill.opacity(0.0), 1.0),
                ));
            fades.push(if edge {
                mask.top_0().into_any_element()
            } else {
                mask.bottom_0().into_any_element()
            });
        }
        fades
    }

    fn hover_card(&self, colors: SemanticColors) -> Option<AnyElement> {
        let (id, pointer_y) = self.ui.hover_card.as_ref()?;
        let (session, project) = {
            let store = self.store.read().expect("session store lock poisoned");
            let session = store.sessions().get(id)?.clone();
            let project = store.projects().get(&session.project_id).cloned();
            (session, project)
        };
        let mut details = div()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .px(px(12.0))
            .py(px(9.0));
        if let Some(project) = &project {
            details = details.child(hover_detail("folder.fill", &project.name, false, colors));
        }
        if let Some(branch) = &session.git_branch {
            details = details.child(hover_detail("arrow.branch", branch, true, colors));
        }
        details = details.child(hover_detail(
            if session.worktree_path.is_some() {
                "point.3.filled.connected.trianglepath.dotted"
            } else {
                "internaldrive"
            },
            &clamp_path(session.worktree_path.as_deref().unwrap_or(&session.cwd)),
            true,
            colors,
        ));
        if let Some(ports) = &session.listening_ports
            && !ports.is_empty()
        {
            details = details.child(hover_detail(
                "network",
                &ports
                    .iter()
                    .map(|port| format!(":{}", port.port))
                    .collect::<Vec<_>>()
                    .join(", "),
                false,
                colors,
            ));
        }
        let card = div()
            .w(px(260.0))
            .rounded(px(Radius::CARD))
            .bg(colors.background.alpha(0.98))
            .border_1()
            .border_color(colors.primary.alpha(0.08))
            .shadow_lg()
            .overflow_hidden()
            .child(
                div()
                    .px(px(12.0))
                    .pt(px(10.0))
                    .pb(px(8.0))
                    .text_size(px(Typo::ROW_EMPHASIZED.size))
                    .font_weight(Typo::ROW_EMPHASIZED.weight)
                    .text_color(colors.primary)
                    .child(display_title(&session)),
            )
            .child(HairlineDivider::horizontal(colors))
            .child(details);
        // Deferred + anchored so the card floats over the terminal instead of
        // being clipped at the sidebar edge. No mouse listeners: like the
        // Swift click-through panel, it never eats the first click on a row.
        Some(
            deferred(
                anchored()
                    .position(point(
                        px((self.ui.width - 4.0).max(0.0)),
                        px(pointer_y - 14.0),
                    ))
                    .snap_to_window_with_margin(px(8.0))
                    .child(card),
            )
            .into_any_element(),
        )
    }

    fn status_glyph(
        &mut self,
        session: &SessionRecord,
        migrating: bool,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<StatusGlyph> {
        let kind = ui_agent_kind(session.effective_kind());
        let state = status_state(session, migrating);
        let entity = self
            .glyphs
            .entry(session.id.clone())
            .or_insert_with(|| cx.new(|_| StatusGlyph::new(kind, state, 16.0, colors)))
            .clone();
        entity.update(cx, |glyph, cx| {
            glyph.set_kind(kind, cx);
            glyph.set_state(state, window, cx);
            glyph.set_colors(colors, cx);
        });
        entity
    }

    /// ⌘1–⌘8 address the first eight rows; ⌘9 always jumps to the last one,
    /// so the hint follows the same rule rather than labelling row nine.
    fn shortcut_for(&mut self, id: &SessionId) -> Option<usize> {
        self.shortcut_ranks.get(id).copied()
    }

    fn reorder_project(&mut self, moved: &ProjectId, target: &ProjectId) {
        let mut store = self.store.write().expect("session store lock poisoned");
        let mut order = store.sidebar_project_order();
        move_before(&mut order, moved, target);
        self.ui.order_dirty |= store.stage_project_order(order);
    }

    fn reorder_session(&mut self, moved: &SessionId, target: &SessionId) {
        let mut store = self.store.write().expect("session store lock poisoned");
        let mut order = store.sidebar_session_order();
        move_before(&mut order, moved, target);
        self.ui.order_dirty |= store.stage_session_order(order);
    }

    /// Ends a drag gesture: clears the visual state and writes any staged
    /// reorder to disk exactly once.
    fn finish_drag(&mut self) {
        self.ui.drag = None;
        self.ui.drag_target = None;
        if self.ui.order_dirty {
            self.ui.order_dirty = false;
            let _ = self
                .store
                .read()
                .expect("session store lock poisoned")
                .persist_preferences();
        }
    }

    /// Drops the moved session at the end of the manual order. The projection
    /// groups by project before it sorts, so "last overall" reads as "last in
    /// its own group" — which is what ⌃⌘↓ on the bottom-but-one row means.
    fn reorder_session_to_end(&mut self, moved: &SessionId) {
        let mut store = self.store.write().expect("session store lock poisoned");
        let mut order = store.sidebar_session_order();
        move_to_end(&mut order, moved);
        let _ = store.set_session_order(order);
    }

    fn archive_sessions(&mut self, ids: Vec<SessionId>) {
        self.store
            .write()
            .expect("session store lock poisoned")
            .archive_sessions(ids);
    }

    fn close_sessions(&mut self, ids: Vec<SessionId>, cx: &mut Context<Self>) {
        let mut store = self.store.write().expect("session store lock poisoned");
        store.request_close(ids.clone());
        let raised = store.pending_close().is_some();
        if self.preview && !raised {
            for id in ids {
                store.remove_session_record(&id);
            }
        }
        drop(store);
        if raised {
            // Wake RootView so the confirmation shows on this click, not the
            // next time something else happens to redraw the window.
            cx.emit(SidebarEvent::ConfirmationChanged);
        }
    }

    /// Selects the nth session (⌘1–⌘9 order, matching the row hints) and
    /// reports whether a session existed at that index.
    pub fn select_shortcut(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        self.commit_rename();
        let id = {
            let mut store = self.store.write().expect("session store lock poisoned");
            let id = store
                .ordered_sessions()
                .get(index)
                .map(|session| session.id.clone());
            if let Some(id) = &id {
                store.select(id.clone());
            }
            id
        };
        if id.is_none() {
            return false;
        }
        cx.emit(SidebarEvent::SessionActivated);
        cx.notify();
        true
    }

    /// Selects the last session in sidebar order (⌘9, matching the browser
    /// convention where the last digit jumps to the final tab).
    pub fn select_last(&mut self, cx: &mut Context<Self>) -> bool {
        let count = self
            .store
            .write()
            .expect("session store lock poisoned")
            .ordered_sessions()
            .len();
        if count == 0 {
            return false;
        }
        self.select_shortcut(count - 1, cx)
    }

    /// Moves the selection `delta` rows through the sidebar order (⌘↑/⌘↓ and
    /// ⌘←/⌘→), wrapping at both ends. Returns false when there are no
    /// sessions to move between.
    pub fn select_relative(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        self.commit_rename();
        {
            let mut store = self.store.write().expect("session store lock poisoned");
            let sessions = store.ordered_sessions();
            if sessions.is_empty() {
                return false;
            }
            let len = sessions.len() as isize;
            let current = store
                .selected_session_id()
                .and_then(|id| sessions.iter().position(|session| &session.id == id));
            let index = match current {
                Some(current) => (current as isize + delta).rem_euclid(len),
                // Nothing selected yet: ⌘↓ enters at the top, ⌘↑ at the bottom.
                None if delta >= 0 => 0,
                None => len - 1,
            } as usize;
            store.select(sessions[index].id.clone());
        }
        cx.emit(SidebarEvent::SessionActivated);
        cx.notify();
        true
    }

    /// ⌘J: select the next session waiting on a human, in sidebar order and
    /// wrapping past the current row. Returns false when nothing is waiting.
    pub fn select_next_needing_input(&mut self, cx: &mut Context<Self>) -> bool {
        self.commit_rename();
        {
            let mut store = self.store.write().expect("session store lock poisoned");
            let sessions = store.ordered_sessions();
            if sessions.is_empty() {
                return false;
            }
            let current = store
                .selected_session_id()
                .and_then(|id| sessions.iter().position(|session| &session.id == id));
            // Start one past the selection so repeated ⌘J walks the queue
            // instead of landing on the same row.
            let start = current.map_or(0, |index| index + 1);
            let Some(next) = (0..sessions.len())
                .map(|offset| &sessions[(start + offset) % sessions.len()])
                .find(|session| session.attention() == ProtoAttentionLevel::NeedsInput)
            else {
                return false;
            };
            store.select(next.id.clone());
        }
        cx.emit(SidebarEvent::SessionActivated);
        cx.notify();
        true
    }

    /// ⌃⌘↑/⌃⌘↓: move the selected session one place among its own siblings —
    /// the rows sharing its parent inside its project. Clamps at the ends of
    /// that run: a reorder that wrapped would teleport the row past every
    /// other project, and one that crossed levels would silently re-parent a
    /// session, which is the daemon's call to make, not a keystroke's.
    pub fn reorder_selected(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        self.commit_rename();
        let (moved, target) = {
            let mut store = self.store.write().expect("session store lock poisoned");
            let Some(selected) = store.selected_session_id().cloned() else {
                return false;
            };
            let projection = store.sidebar_projection();
            let Some(group) = projection
                .projects
                .iter()
                .find(|group| group.sessions.iter().any(|row| row.id() == &selected))
            else {
                return false;
            };
            let parent = group
                .sessions
                .iter()
                .find(|row| row.id() == &selected)
                .and_then(|row| row.session.parent.clone());
            let siblings: Vec<&SessionId> = group
                .sessions
                .iter()
                .filter(|row| row.session.parent == parent)
                .map(|row| row.id())
                .collect();
            let index = siblings
                .iter()
                .position(|id| *id == &selected)
                .expect("the group was found by this id");
            let destination = index as isize + delta;
            if destination < 0 || destination >= siblings.len() as isize {
                return false;
            }
            // Moving up lands before the sibling above; moving down lands
            // before the one two below, i.e. just after the row it swaps with.
            // Off the end there is no anchor, so the move goes to the tail.
            let target = if delta < 0 {
                siblings.get(destination as usize)
            } else {
                siblings.get(destination as usize + 1)
            }
            .map(|id| (*id).clone());
            (selected, target)
        };
        match target {
            Some(target) => self.reorder_session(&moved, &target),
            None => self.reorder_session_to_end(&moved),
        }
        cx.notify();
        true
    }

    /// ⌘R: start renaming the selected row inline, the same edit the context
    /// menu's "Rename…" opens. Returns false when nothing is selected.
    pub fn rename_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let selected = self
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session()
            .cloned();
        let Some(session) = selected else {
            return false;
        };
        self.begin_rename(&session, window, cx);
        true
    }

    /// ⌘⇧W: archive the selected session, where ⌘W removes it from the
    /// sidebar. Returns false when nothing is selected.
    pub fn archive_selected(&mut self, cx: &mut Context<Self>) -> bool {
        let selected = self
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session_id()
            .cloned();
        let Some(id) = selected else {
            return false;
        };
        self.archive_sessions(vec![id]);
        cx.notify();
        true
    }

    /// ⌘W: close the selected session, honoring the
    /// confirm-before-closing preference (a running session raises the
    /// confirmation dialog; an already-exited one closes at once). Returns
    /// false when nothing is selected so ⌘W falls through to closing the
    /// window.
    pub fn close_selected_now(&mut self, cx: &mut Context<Self>) -> bool {
        let selected = self
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session_id()
            .cloned();
        let Some(id) = selected else {
            return false;
        };
        if self.preview {
            self.close_sessions_immediately(vec![id]);
        } else {
            self.close_sessions(vec![id], cx);
        }
        cx.notify();
        true
    }

    /// Close that bypasses the confirm-before-closing preference entirely.
    fn close_sessions_immediately(&mut self, ids: Vec<SessionId>) {
        let mut store = self.store.write().expect("session store lock poisoned");
        store.remove_sessions(ids.clone());
        if self.preview {
            for id in ids {
                store.remove_session_record(&id);
            }
        }
    }
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors();
        let projection = {
            let mut store = self.store.write().expect("session store lock poisoned");
            store.sidebar_projection()
        };
        self.shortcut_ranks.clear();
        let session_count = projection.ordered_sessions.len();
        for (index, session) in projection.ordered_sessions.iter().enumerate() {
            let shortcut = if index < 8 {
                Some(index + 1)
            } else if index + 1 == session_count {
                Some(9)
            } else {
                None
            };
            if let Some(shortcut) = shortcut {
                self.shortcut_ranks.insert(session.id.clone(), shortcut);
            }
        }
        retain_live_glyphs(&mut self.glyphs, &projection.display_order);
        let mut list = div()
            .id("sidebar-list")
            .track_scroll(&self.list_scroll)
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .px(px(Space::INSET))
            .pt(px(2.0))
            .pb(px(Metrics::ROW_HEIGHT + 17.0))
            .flex()
            .flex_col()
            .gap(px(2.0));
        for group in &projection.projects {
            list = list.child(self.project_section(group, colors, window, cx));
        }
        list = list.child(self.empty_space_drop_target(cx));

        let mut root = div()
            .id("sidebar")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .text_color(colors.primary)
            .bg(Self::surface_fill(colors))
            .track_focus(&self.rename_focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .child(self.top_bar(colors, cx))
            .child(self.new_agent_row(colors, cx));
        if projection.projects.is_empty() {
            root = root.child(self.empty_state(colors, cx));
        } else {
            // Rows dissolve into the chrome at both ends of the scroll instead
            // of being sliced off by the container edge.
            root = root.child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_col()
                    .child(list)
                    .children(self.scroll_fades(colors)),
            );
        }
        if let Some(feedback) = self.external_drop_feedback(colors, cx) {
            root = root.child(feedback);
        }
        root = root.child(self.account_footer(colors, cx));
        if let Some(popover) = self.popover(colors, window, cx) {
            root = root.child(popover);
        }
        if let Some(card) = self.hover_card(colors) {
            root = root.child(card);
        }
        root
    }
}

fn icon_button(
    id: &'static str,
    system_image: &'static str,
    hovering: bool,
    colors: SemanticColors,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_hover: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Radius::BADGE))
        .bg(Fill::hover(colors, hovering))
        .cursor_pointer()
        .text_size(px(15.0))
        .text_color(colors.secondary)
        .on_click(on_click)
        .on_hover(on_hover)
        .child(sf_symbol(system_image, 15.0, colors.secondary))
        .into_any_element()
}

/// Title `display_title` gives a placeholder-named session that has exited.
/// The "Ended" chip stands down when the title already says it.
const ENDED_TITLE: &str = "Ended";

/// One leading column per ancestor level. A column is drawn full height while
/// that ancestor still has siblings below, and stops halfway on the last child
/// so a subtree visibly closes instead of trailing a rail into the next row.
fn indent_rails(row: &crate::store::SidebarRow, colors: SemanticColors) -> Vec<AnyElement> {
    (0..row.depth)
        .map(|column| {
            let continues = row.rails & (1u32 << column.min(31)) != 0;
            let last_column = column + 1 == row.depth;
            div()
                .w(px(Space::INDENT))
                .h(px(Metrics::ROW_HEIGHT))
                .flex_none()
                .flex()
                .justify_center()
                .child(
                    div()
                        .w(px(1.0))
                        // A rail that neither continues nor elbows into this
                        // row has no business being drawn at all.
                        .h(px(if continues {
                            Metrics::ROW_HEIGHT
                        } else if last_column {
                            Metrics::ROW_HEIGHT / 2.0
                        } else {
                            0.0
                        }))
                        .bg(colors.primary.alpha(0.10)),
                )
                .into_any_element()
        })
        .collect()
}

fn pin_mark(colors: SemanticColors) -> AnyElement {
    div()
        .flex_none()
        .flex()
        .items_center()
        .child(sf_symbol("pin.fill", 9.0, colors.tertiary))
        .into_any_element()
}

fn project_badge(colors: SemanticColors) -> AnyElement {
    div()
        .flex_none()
        .size(px(18.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Radius::CHIP))
        .bg(colors.primary.alpha(0.08))
        .text_size(px(9.0))
        .text_color(colors.secondary)
        .child(sf_symbol("folder.fill", 9.0, colors.secondary))
        .into_any_element()
}

fn menu_row(
    label: impl Into<SharedString>,
    colors: SemanticColors,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let label = label.into();
    div()
        .id(label.clone())
        .px(px(8.0))
        .h(px(28.0))
        .flex()
        .items_center()
        .rounded(px(Radius::ROW))
        .cursor_pointer()
        .hover(move |element| element.bg(colors.primary.alpha(0.06)))
        .text_size(px(Typo::ROW.size))
        .text_color(colors.primary)
        .child(label)
        .on_click(on_click)
        .into_any_element()
}

fn directory_row(
    symbol: &'static str,
    label: String,
    colors: SemanticColors,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let row_id = format!("directory-row-{label}");
    div()
        .id(row_id)
        .px(px(10.0))
        .h(px(30.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(Radius::ROW))
        .cursor_pointer()
        .hover(move |row| row.bg(colors.primary.alpha(0.06)))
        .child(sf_symbol(symbol, 11.0, colors.secondary))
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .text_size(px(Typo::ROW.size))
                .text_color(colors.primary)
                .child(label),
        )
        .on_click(on_click)
        .into_any_element()
}

fn remote_picker_target(explicit_directory: Option<&str>, host_default: Option<&str>) -> String {
    explicit_directory
        .or(host_default)
        .map(normalize_remote_picker_path)
        .unwrap_or_else(|| "~".to_owned())
}

fn normalize_remote_picker_path(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() {
        return "~".to_owned();
    }
    let without_trailing_slashes = path.trim_end_matches('/');
    if without_trailing_slashes.is_empty() {
        "/".to_owned()
    } else {
        without_trailing_slashes.to_owned()
    }
}

fn should_resolve_active_repo(
    explicit_directory: Option<&str>,
    target_host: Option<&str>,
    active_host: Option<&str>,
) -> bool {
    explicit_directory.is_none() && target_host.is_none() && active_host.is_some()
}

fn menu_divider(colors: SemanticColors) -> AnyElement {
    div()
        .my(px(3.0))
        .child(HairlineDivider::horizontal(colors))
        .into_any_element()
}

fn copy_session_id_row(
    id: SessionId,
    colors: SemanticColors,
    cx: &mut Context<Sidebar>,
) -> AnyElement {
    menu_row(
        "Copy Session ID",
        colors,
        cx.listener(move |this, _, _, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(id.0.clone()));
            this.ui.popover = None;
            cx.notify();
        }),
    )
}

fn count_label(verb: &str, count: usize) -> String {
    if count == 1 {
        format!("{verb} 1 Session")
    } else {
        format!("{verb} {count} Sessions")
    }
}

fn section_label(label: &'static str, colors: SemanticColors) -> AnyElement {
    div()
        .px(px(14.0))
        .pt(px(10.0))
        .pb(px(3.0))
        .text_size(px(Typo::SECTION_HEADER.size))
        .font_weight(Typo::SECTION_HEADER.weight)
        .text_color(colors.tertiary)
        .child(label)
        .into_any_element()
}

fn usage_row(label: &str, detail: &str, value: &str, colors: SemanticColors) -> AnyElement {
    div()
        .px(px(14.0))
        .h(px(24.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(px(Typo::ROW.size))
        .text_color(colors.text(diri_ui::TextTone::Label))
        .child(label.to_owned())
        .child(div().flex_1())
        .when(!detail.is_empty(), |row| {
            row.child(
                div()
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child(detail.to_owned()),
            )
        })
        .child(
            div()
                .font_family(crate::fonts::mono_family())
                .text_size(px(Typo::META_MONO.size))
                .text_color(colors.secondary)
                .child(value.to_owned()),
        )
        .into_any_element()
}

fn hover_detail(icon: &str, text: &str, mono: bool, colors: SemanticColors) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(7.0))
        .child(
            div()
                .w(px(13.0))
                .flex()
                .items_center()
                .justify_center()
                .child(sf_symbol(icon, 10.0, colors.secondary)),
        )
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .when(mono, |text| text.font_family(crate::fonts::mono_family()))
                .text_size(px(Typo::META.size))
                .text_color(colors.primary.alpha(0.82))
                .child(text.to_owned()),
        )
        .into_any_element()
}

fn display_title(session: &SessionRecord) -> String {
    if session.title_source == diri_proto::TitleSource::Placeholder {
        if matches!(
            session.status,
            diri_proto::SessionStatus::Starting
                | diri_proto::SessionStatus::Working
                | diri_proto::SessionStatus::NeedsInput(_)
        ) {
            "Untitled".into()
        } else {
            "Ended".into()
        }
    } else {
        session.title.clone()
    }
}

fn status_state(session: &SessionRecord, migrating: bool) -> StatusState {
    if migrating {
        return StatusState::Working;
    }
    if session.hibernation.is_some() {
        return StatusState::Hibernated;
    }
    match session.attention() {
        ProtoAttentionLevel::NeedsInput => StatusState::NeedsInput {
            destructive: session
                .needs_input
                .as_ref()
                .is_some_and(|detail| detail.risk_hint == diri_proto::RiskHint::Destructive),
        },
        ProtoAttentionLevel::DoneUnseen => StatusState::DoneUnseen,
        ProtoAttentionLevel::Working => StatusState::Working,
        ProtoAttentionLevel::IdleSeen => StatusState::IdleSeen,
        ProtoAttentionLevel::None | ProtoAttentionLevel::Unknown => StatusState::None,
    }
}

/// Rows for the new-agent picker come from the selected target's runtime
/// catalog. Unavailable and user-hidden Agents stay out of this high-frequency
/// surface; Settings remains the complete supported-Agent inventory.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentPickerOption {
    title: String,
    kind: ProtoAgentKind,
    shortcut: &'static str,
    binary: String,
    available: bool,
    setup_url: Option<String>,
    unavailable_detail: Option<String>,
}

fn agent_picker_options(
    catalog: Option<&diri_proto::AgentReadinessResult>,
) -> Vec<AgentPickerOption> {
    let mut options: Vec<_> = crate::agent_catalog::quick_agent_options(catalog)
        .into_iter()
        .filter(|option| option.kind != ProtoAgentKind::SHELL)
        .map(|option| AgentPickerOption {
            title: option.display_name,
            shortcut: if option.kind == ProtoAgentKind::CODEX {
                "⌘⇧N"
            } else {
                ""
            },
            kind: option.kind,
            binary: option.binary,
            available: true,
            setup_url: None,
            unavailable_detail: None,
        })
        .collect();
    // Terminal is last on purpose: it is the escape hatch, not an agent.
    options.push(AgentPickerOption {
        title: "Terminal".to_owned(),
        kind: ProtoAgentKind::SHELL,
        shortcut: "⌥⌘T",
        binary: "login shell".to_owned(),
        available: true,
        setup_url: None,
        unavailable_detail: None,
    });
    options
}

fn agent_picker_shortcut(
    kind: &ProtoAgentKind,
    default_kind: &ProtoAgentKind,
    fallback: &'static str,
) -> &'static str {
    if kind == default_kind {
        "⌘T"
    } else {
        fallback
    }
}

fn ui_agent_kind(kind: &ProtoAgentKind) -> AgentKind {
    // Brand vocabulary, not a protocol type: a manifest agent the client has
    // no hand-drawn mark for falls back to the generic terminal treatment.
    match kind.id() {
        ProtoAgentKind::CLAUDE_CODE_ID => AgentKind::ClaudeCode,
        ProtoAgentKind::CODEX_ID => AgentKind::Codex,
        ProtoAgentKind::CURSOR_ID => AgentKind::Cursor,
        ProtoAgentKind::GEMINI_ID => AgentKind::Gemini,
        ProtoAgentKind::SHELL_ID => AgentKind::Shell,
        _ => AgentKind::Generic,
    }
}

fn rollup_attention(sessions: &[Arc<SessionRecord>]) -> AttentionLevel {
    sessions
        .iter()
        .fold(AttentionLevel::None, |rollup, session| {
            let state = match status_state(session, false) {
                StatusState::NeedsInput { destructive } => {
                    AttentionLevel::NeedsInput { destructive }
                }
                StatusState::DoneUnseen => AttentionLevel::DoneUnseen,
                StatusState::Working => AttentionLevel::Working,
                StatusState::IdleSeen => AttentionLevel::IdleSeen,
                StatusState::Hibernated => AttentionLevel::Hibernated,
                StatusState::None => AttentionLevel::None,
            };
            if attention_rank(state) > attention_rank(rollup) {
                state
            } else {
                rollup
            }
        })
}

const fn attention_rank(level: AttentionLevel) -> u8 {
    match level {
        AttentionLevel::None | AttentionLevel::Hibernated => 0,
        AttentionLevel::IdleSeen => 1,
        AttentionLevel::Working => 2,
        AttentionLevel::DoneUnseen => 3,
        AttentionLevel::NeedsInput { .. } => 4,
    }
}

fn retain_live_glyphs<T>(glyphs: &mut HashMap<SessionId, T>, live: &[SessionId]) {
    let live: std::collections::HashSet<_> = live.iter().collect();
    glyphs.retain(|id, _| live.contains(id));
}

fn clamp_path(path: &str) -> String {
    if path.chars().count() <= 40 {
        return path.into();
    }
    let last = path.rsplit('/').next().unwrap_or(path);
    let head_budget = 40usize.saturating_sub(last.chars().count() + 2).max(4);
    format!(
        "{}…/{last}",
        path.chars().take(head_budget).collect::<String>()
    )
}

/// Overflow threshold for a session title. Individual badges reserve their
/// content estimate, padding, and following gap; HoverMarquee shapes the title
/// itself exactly. Rows carry a fixed disclosure column and one indent column
/// per ancestor, so nesting costs title width and has to be counted here or a
/// deep row marquees a title that was never actually clipped.
#[allow(clippy::too_many_arguments)]
fn session_title_available_width(
    sidebar_width: f32,
    depth: u16,
    migrating: bool,
    non_persistent: bool,
    ended: bool,
    host_label: Option<&str>,
    hibernated: bool,
    pinned: bool,
    shortcut_visible: bool,
) -> f32 {
    // Row insets + fold column + identity glyph + the gaps between them.
    let mut available = sidebar_width - 68.0 - f32::from(depth) * (Space::INDENT + 8.0);
    if migrating {
        available -= 66.0;
    }
    if non_persistent {
        available -= 72.0;
    }
    if ended {
        available -= 48.0;
    }
    if let Some(host) = host_label {
        available -= host.chars().count() as f32 * 6.2 + 18.0;
    }
    if hibernated {
        available -= 42.0;
    }
    if pinned {
        available -= 18.0;
    }
    // The close button and the shortcut hint share the trailing slot and are
    // near enough the same width that one reservation covers both.
    if shortcut_visible {
        available -= 28.0;
    }
    available.max(36.0)
}

fn compact_duration(seconds: i64) -> String {
    let minutes = (seconds / 60).max(0);
    if minutes >= 60 {
        format!("{}h {}m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Modifiers, TestAppContext};

    use super::*;

    struct SidebarPopoverHarness {
        sidebar: Entity<Sidebar>,
    }

    impl Render for SidebarPopoverHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(div().h_full().w(px(248.0)).child(self.sidebar.clone()))
        }
    }

    #[test]
    fn long_paths_keep_final_component() {
        let result = clamp_path("/Users/preview/Projects/a/very/long/path/settings-kit");
        assert!(result.ends_with("/settings-kit"));
        assert!(result.contains('…'));
    }

    #[test]
    fn compact_duration_matches_usage_copy() {
        assert_eq!(compact_duration(8_040), "2h 14m");
        assert_eq!(compact_duration(540), "9m");
    }

    #[test]
    fn title_overflow_threshold_accounts_for_sidebar_badges() {
        let plain =
            session_title_available_width(248.0, 0, false, false, false, None, false, false, false);
        let remote = session_title_available_width(
            248.0,
            0,
            false,
            false,
            false,
            Some("mini-b"),
            false,
            false,
            true,
        );
        assert!(plain > remote);
        // A nested row pays for every indent column it sits behind.
        let nested =
            session_title_available_width(248.0, 2, false, false, false, None, false, false, false);
        assert!(plain > nested);
        assert_eq!(
            session_title_available_width(
                200.0,
                1,
                true,
                true,
                true,
                Some("very-long-host"),
                true,
                true,
                true,
            ),
            36.0
        );
    }

    #[test]
    fn agent_shortcuts_remain_visible_when_the_execution_host_changes() {
        assert_eq!(
            agent_picker_shortcut(
                &ProtoAgentKind::CLAUDE_CODE,
                &ProtoAgentKind::CLAUDE_CODE,
                ""
            ),
            "⌘T"
        );
        assert_eq!(
            agent_picker_shortcut(&ProtoAgentKind::CODEX, &ProtoAgentKind::CLAUDE_CODE, "⌘⇧N"),
            "⌘⇧N"
        );
        assert_eq!(
            agent_picker_shortcut(&ProtoAgentKind::SHELL, &ProtoAgentKind::CLAUDE_CODE, "⌥⌘T"),
            "⌥⌘T"
        );
    }

    #[test]
    fn agent_picker_keeps_installed_manifest_agents_and_hides_unavailable_rows() {
        let catalog = diri_proto::AgentReadinessResult {
            agents: vec![
                diri_proto::AgentReadinessItem {
                    kind: ProtoAgentKind::new("amp"),
                    binary: "amp".into(),
                    path: Some("/bin/amp".into()),
                    show_in_quick_create: true,
                    descriptor: Some(diri_proto::AgentDescriptor {
                        id: "amp".into(),
                        display_name: "Amp".into(),
                        ..diri_proto::AgentDescriptor::default()
                    }),
                    ..diri_proto::AgentReadinessItem::default()
                },
                diri_proto::AgentReadinessItem {
                    kind: ProtoAgentKind::new("opencode"),
                    binary: "opencode".into(),
                    path: None,
                    descriptor: Some(diri_proto::AgentDescriptor {
                        id: "opencode".into(),
                        display_name: "OpenCode".into(),
                        setup: Some(diri_proto::AgentSetup {
                            url: Some("https://opencode.ai/docs".into()),
                            install_hint: Some("Install OpenCode.".into()),
                            sign_in_hint: Some("Run /connect.".into()),
                        }),
                        ..diri_proto::AgentDescriptor::default()
                    }),
                    ..diri_proto::AgentReadinessItem::default()
                },
            ],
            ..diri_proto::AgentReadinessResult::default()
        };
        let options = agent_picker_options(Some(&catalog));
        let amp = options
            .iter()
            .find(|option| option.kind == ProtoAgentKind::new("amp"))
            .expect("manifest-only option");
        assert!(amp.available);
        assert!(
            !options
                .iter()
                .any(|option| option.kind == ProtoAgentKind::new("opencode"))
        );
    }

    #[test]
    fn remote_directory_navigation_keeps_the_explicit_child_path() {
        assert_eq!(
            remote_picker_target(Some("/Users/remote/code/diri"), Some("~")),
            "/Users/remote/code/diri"
        );
    }

    #[test]
    fn remote_default_directory_has_a_visible_final_component() {
        assert_eq!(remote_picker_target(None, Some("~/")), "~");
        assert_eq!(remote_picker_target(None, Some("/srv/app/")), "/srv/app");
        assert_eq!(remote_picker_target(None, Some("/")), "/");
    }

    #[test]
    fn remote_new_agent_uses_the_selected_hosts_default_directory() {
        assert!(!should_resolve_active_repo(None, Some("forge"), None));
        assert!(!should_resolve_active_repo(
            None,
            Some("forge"),
            Some("studio")
        ));
        assert!(should_resolve_active_repo(None, None, Some("studio")));
        assert!(!should_resolve_active_repo(
            Some("/Users/me/code"),
            None,
            Some("studio")
        ));
    }

    #[test]
    fn migrating_session_uses_an_immediate_working_status() {
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let session = fixture.list.sessions.first().expect("preview session");

        assert_eq!(status_state(session, true), StatusState::Working);
    }

    /// A sidebar full of working Agents is diri's normal resting state, so a
    /// repeating timer here is a permanent wake, not an occasional one. The
    /// 10 Hz status ticker this replaces measured ~3% idle CPU and held
    /// ~240 MB of GPU memory that an idle window returns within seconds of its
    /// last frame. `diri-ui`'s `status_marks_never_sample_a_clock_while_rendering`
    /// guards the other half: a glyph that needs repainting to look right.
    #[test]
    fn the_sidebar_owns_no_repeating_clock() {
        let source = include_str!("view.rs");
        let periodic_timer = ["background_executor()", ".timer("].concat();
        let frame_request = ["request_animation", "_frame("].concat();

        assert!(
            !source.contains(&periodic_timer),
            "the sidebar must stay event-driven; a status clock here never stops, because \
             sessions are usually working"
        );
        assert!(
            !source.contains(&frame_request),
            "the sidebar must not drive the compositor from a render pass"
        );
    }

    #[test]
    fn status_glyph_lifecycle_follows_sidebar_projection() {
        let first = SessionId("first".into());
        let second = SessionId("second".into());
        let stale = SessionId("stale".into());
        let mut glyphs = HashMap::from([
            (first.clone(), ()),
            (second.clone(), ()),
            (stale.clone(), ()),
        ]);

        retain_live_glyphs(&mut glyphs, &[first.clone(), second.clone()]);

        assert_eq!(glyphs.len(), 2);
        assert!(glyphs.contains_key(&first));
        assert!(glyphs.contains_key(&second));
        assert!(!glyphs.contains_key(&stale));
    }

    #[gpui::test]
    fn sidebar_popovers_dismiss_when_clicking_elsewhere_in_the_window(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| {
                let mut sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
                sidebar.ui.popover = Some(Popover::NewAgent {
                    directory: None,
                    host: None,
                });
                sidebar
            });
            SidebarPopoverHarness { sidebar }
        });

        cx.simulate_click(point(px(500.0), px(320.0)), Modifiers::default());

        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar.ui.popover.clone()),
            None
        );
    }

    #[gpui::test]
    fn account_popover_exposes_the_remote_host_shortcut(cx: &mut TestAppContext) {
        let (_view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| {
                let mut sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
                sidebar.ui.popover = Some(Popover::Account);
                sidebar
            });
            SidebarPopoverHarness { sidebar }
        });

        assert!(cx.debug_bounds("quick-add-remote-host").is_some());
    }

    #[gpui::test]
    fn project_plus_opens_the_agent_kind_menu_in_that_project(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| {
                let sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
                let installed =
                    |kind: diri_proto::AgentKind, label: &str| diri_proto::AgentReadinessItem {
                        binary: kind.id().to_owned(),
                        kind,
                        path: Some(format!("/usr/local/bin/{label}")),
                        detected_path: None,
                        configured_path: None,
                        path_source: Some(diri_proto::AgentPathSource::SystemPath),
                        show_in_quick_create: true,
                        error: None,
                        descriptor: Some(diri_proto::AgentDescriptor {
                            display_name: label.to_owned(),
                            ..Default::default()
                        }),
                    };
                sidebar
                    .store
                    .write()
                    .expect("session store lock poisoned")
                    .set_agent_catalog(diri_proto::AgentReadinessResult {
                        host: None,
                        scanned_at: None,
                        agents: vec![
                            installed(diri_proto::AgentKind::CLAUDE_CODE, "Claude Code"),
                            installed(diri_proto::AgentKind::CODEX, "Codex"),
                        ],
                    });
                sidebar
            });
            SidebarPopoverHarness { sidebar }
        });
        let project = cx
            .debug_bounds("PROJECT_preview-dirijor")
            .expect("project row");
        cx.simulate_mouse_move(project.center(), None, Modifiers::default());
        let plus = cx
            .debug_bounds("PROJECT_ADD_preview-dirijor")
            .expect("project add button");

        cx.simulate_click(plus.center(), Modifiers::default());

        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar.ui.popover.clone()),
            Some(Popover::NewAgent {
                directory: Some("/Users/preview/Projects/dirijor".to_owned()),
                host: None,
            })
        );
        assert!(cx.debug_bounds("AGENT_OPTION_0").is_some());
        assert!(cx.debug_bounds("AGENT_OPTION_1").is_some());
    }

    #[gpui::test]
    fn choosing_a_host_in_new_agent_makes_its_shortcuts_the_default(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| {
                let mut sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
                sidebar
                    .store
                    .write()
                    .expect("session store lock poisoned")
                    .set_hosts(vec![diri_proto::HostEntry {
                        id: "forge".into(),
                        name: Some("Forge".into()),
                        ssh: "you@forge".into(),
                        default_cwd: None,
                        node: None,
                    }]);
                sidebar.ui.popover = Some(Popover::NewAgent {
                    directory: None,
                    host: None,
                });
                sidebar
            });
            SidebarPopoverHarness { sidebar }
        });
        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        let host = cx.debug_bounds("HOST_OPTION_1").expect("remote host row");

        cx.simulate_click(host.center(), Modifiers::default());

        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar
                .store
                .read()
                .expect("session store lock poisoned")
                .default_spawn_host()),
            Some("forge".into())
        );
    }

    /// The picker persists the shortcut destination, so the same picker has to
    /// be able to take it back: one click on the "This Mac" row must return
    /// ⌘T / ⌥⌘T / the palette to local, with nothing else to undo.
    #[gpui::test]
    fn the_new_agent_picker_can_send_shortcuts_back_to_this_mac(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| {
                let mut sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
                {
                    let mut store = sidebar.store.write().expect("session store lock poisoned");
                    store.set_hosts(vec![diri_proto::HostEntry {
                        id: "forge".into(),
                        name: Some("Forge".into()),
                        ssh: "you@forge".into(),
                        default_cwd: None,
                        node: None,
                    }]);
                    // Start from the regressed state: shortcuts already point
                    // at a remote host, as they would after an earlier click.
                    store.set_default_spawn_host(Some("forge".into()));
                }
                sidebar.ui.popover = Some(Popover::NewAgent {
                    directory: None,
                    host: Some("forge".into()),
                });
                sidebar
            });
            SidebarPopoverHarness { sidebar }
        });
        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar
                .store
                .read()
                .expect("session store lock poisoned")
                .default_spawn_host()),
            Some("forge".into())
        );

        let local = cx.debug_bounds("HOST_OPTION_0").expect("this-mac row");
        cx.simulate_click(local.center(), Modifiers::default());
        cx.run_until_parked();

        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar
                .store
                .read()
                .expect("session store lock poisoned")
                .default_spawn_host()),
            None
        );
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar.ui.popover.clone()),
            Some(Popover::NewAgent {
                directory: None,
                host: None,
            })
        );
    }
}
