//! Draws one Live Surface natively: the header (title, freshness, "Ask
//! agent", save and pin) and the component tree.
//!
//! The view owns only presentation state (the open tab of each `tabs`,
//! parsed Markdown, drawn diagrams, fetched pictures). Everything the person
//! changes goes through [`SurfacesModel`], which keeps it on disk and tells
//! the agent; everything live is read from the session store at draw time,
//! so a surface stays current with no agent running and no timer ticking.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use diri_proto::SessionId;
use diri_proto::surface::node::{
    Align, BoxStyle, ButtonStyle, Cell, ChecklistItem, Color, Direction, Justify, Length, Shape,
    Trend, Weight,
};
use diri_proto::surface::{Action, Icon, Interaction, Node, NodeKind, SurfaceDoc, Tone};
use diri_ui::{AgentLogo, Fill, Ink, Radius, SemanticColors};
use gpui::{
    AnyElement, ClickEvent, Context, Entity, FocusHandle, FontWeight, MouseButton, MouseDownEvent, Render,
    Rgba, SharedString, Task, Window, div, img, prelude::*, px,
};

use super::{AppRun, SurfacesModel};
use super::render::{self, Resolved};
use crate::i18n::{t, tf};
use crate::icons::sf_symbol;
use crate::markdown::MarkdownDocument;
use crate::query_editor::QueryEditor;
use crate::text_input::Composition;
use crate::notes::editor_view::diagram::{self, Diagrams, Look};
use crate::notes::web_image::{self, WebImage, WebImages};
use crate::store::StoreRuntime;

/// Widest a page's content grows.
const PAGE_MEASURE: f32 = 1040.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A saved surface over the main pane.
    Page,
    /// A session's surface in its right panel: narrower, quieter header.
    Panel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Surface(String),
    /// The newest surface of this session (or the one picked).
    Session(SessionId),
}

pub(crate) struct SurfaceView {
    runtime: Arc<StoreRuntime>,
    model: Entity<SurfacesModel>,
    mode: Mode,
    target: Target,
    /// In session mode, the surface picked among several.
    picked: Option<String>,
    tabs: HashMap<String, String>,
    markdown: HashMap<u64, Arc<MarkdownDocument>>,
    diagrams: Diagrams,
    diagram_tasks: Vec<Task<()>>,
    images: WebImages,
    /// Fixture palette and clock; the live view follows the store.
    pub(crate) colors_override: Option<SemanticColors>,
    pub(crate) now_override: Option<u64>,
    /// Keyboard focus for an `input` being edited.
    focus: FocusHandle,
    input: Option<InputEdit>,
    /// The single timer that wakes this view for its app's next poll. Held
    /// only while the view draws: a hidden view schedules nothing.
    poll: Option<(String, Task<()>)>,
    /// Apps this view has opened, so `on_open` syncs once.
    opened: HashSet<String>,
    /// The approval review is open.
    review: bool,
}

/// An `input` the person is typing in.
struct InputEdit {
    surface: String,
    id: String,
    draft: QueryEditor,
    composition: Composition,
}

/// What a draw needs besides the view.
struct Ctx {
    colors: SemanticColors,
    surface: String,
    compact: bool,
    dark: bool,
    /// The agent's accent, or Diri's.
    accent: Rgba,
    /// The surface folder, for images and icons the app ships.
    folder: Option<std::path::PathBuf>,
    /// Card radius: the agent's style, or the theme's.
    radius: f32,
    /// 1 for comfortable, smaller for compact.
    scale: f32,
}

impl Ctx {
    fn paint(&self, color: &Color) -> Rgba {
        render::color(color, self.colors, self.accent)
    }
}

fn alpha(color: Rgba, a: f32) -> Rgba {
    Rgba {
        a: color.a * a,
        ..color
    }
}

fn hash_of(parts: &[&str]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut hasher);
    hasher.finish()
}

fn element_id(surface: &str, kind: &str, id: &str) -> SharedString {
    SharedString::from(format!("surface-{surface}-{kind}-{id}"))
}

impl SurfaceView {
    pub(crate) fn new(
        runtime: Arc<StoreRuntime>,
        model: Entity<SurfacesModel>,
        mode: Mode,
        target: Target,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        Self {
            runtime,
            model,
            mode,
            target,
            picked: None,
            tabs: HashMap::new(),
            markdown: HashMap::new(),
            diagrams: Diagrams::default(),
            diagram_tasks: Vec::new(),
            images: WebImages::default(),
            colors_override: None,
            now_override: None,
            focus: cx.focus_handle(),
            input: None,
            poll: None,
            opened: HashSet::new(),
            review: false,
        }
    }

    pub(crate) fn set_target(&mut self, target: Target, cx: &mut Context<Self>) {
        if self.target != target {
            self.target = target;
            self.picked = None;
            cx.notify();
        }
    }

    #[cfg(test)]
    pub(crate) fn select_tab_for_test(&mut self, tabs: &str, tab: &str, cx: &mut Context<Self>) {
        self.tabs.insert(tabs.to_owned(), tab.to_owned());
        cx.notify();
    }

    /// The surface this view shows right now.
    pub(crate) fn shown_surface(&self, cx: &gpui::App) -> Option<Arc<SurfaceDoc>> {
        let model = self.model.read(cx);
        match &self.target {
            Target::Surface(id) => model.get(id).map(|s| Arc::clone(&s.doc)),
            Target::Session(session) => {
                let all = model.for_session(&session.0);
                self.picked
                    .as_ref()
                    .and_then(|picked| all.iter().find(|doc| &doc.id == picked).cloned())
                    .or_else(|| all.first().cloned())
            }
        }
    }

    fn colors(&self) -> SemanticColors {
        self.colors_override.unwrap_or_else(|| {
            let store = self.runtime.store.read().expect("store");
            match self.mode {
                Mode::Page => crate::app_theme::colors_in(&store),
                Mode::Panel => crate::right_panel::panel_colors_in(&store),
            }
        })
    }

    fn now(&self) -> u64 {
        self.now_override.unwrap_or_else(diri_surfaces::now_ms)
    }

    // -- Interactions -----------------------------------------------------

    fn interact(&mut self, surface: &str, interaction: Interaction, cx: &mut Context<Self>) {
        let surface = surface.to_owned();
        self.model
            .update(cx, |model, cx| model.interact(&surface, interaction, cx));
    }

    fn act(&mut self, surface: &str, node: &str, action: &Action, cx: &mut Context<Self>) {
        let (surface, node, action) = (surface.to_owned(), node.to_owned(), action.clone());
        self.model
            .update(cx, |model, cx| model.act(&surface, &node, &action, cx));
    }

    fn open_link(&mut self, surface: &str, url: &str, cx: &mut Context<Self>) {
        let (surface, url) = (surface.to_owned(), url.to_owned());
        self.model
            .update(cx, |model, cx| model.open_link(&surface, &url, cx));
    }

    // -- Header -----------------------------------------------------------

    fn header_button(
        &self,
        id: &str,
        icon: &'static str,
        label: Option<&'static str>,
        colors: SemanticColors,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(SharedString::from(format!("surface-header-{id}")))
            .debug_selector(|| format!("surface-header-{id}"))
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(26.0))
            .px(px(if label.is_some() { 10.0 } else { 6.0 }))
            .rounded(px(Radius::BADGE))
            .bg(alpha(colors.primary, 0.055))
            .cursor_pointer()
            .hover(move |el| el.bg(alpha(colors.primary, 0.1)))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| on_click(this, cx)))
            .child(sf_symbol(icon, 12.0, colors.secondary))
            .when_some(label, |el, label| {
                el.child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(colors.primary)
                        .child(t(label)),
                )
            })
            .into_any_element()
    }

    fn render_header(
        &self,
        doc: &Arc<SurfaceDoc>,
        app: Option<&AppRun>,
        ctx: &Ctx,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = ctx.colors;
        let page = self.mode == Mode::Page;
        let minimal = doc.style.header == diri_proto::surface::identity::Header::Minimal;
        let (done, total) = doc.checklist_progress();
        let mut meta = vec![tf(
            "surfaces.updated",
            &[("when", &render::ago(doc.updated_ms, self.now()))],
        )];
        if let Some(by) = &doc.updated_by {
            meta.push(by.clone());
        }
        if total > 0 {
            meta.push(tf("surfaces.done", &[("done", &done), ("total", &total)]));
        }
        let syncs = doc.app.as_ref().is_some_and(|spec| spec.sync.is_some());
        let mut actions = div().flex().items_center().gap(px(6.0)).flex_none();
        if let (Some(run), true) = (app, syncs) {
            actions = actions.child(self.sync_chip(doc, run, ctx, cx));
        } else if doc.owner.is_some() {
            let id = doc.id.clone();
            actions = actions.child(self.header_button(
                "refresh",
                "arrow.clockwise.circle",
                page.then_some("surfaces.ask_refresh"),
                colors,
                move |this, cx| {
                    let id = id.clone();
                    this.model.update(cx, |model, cx| model.ask_refresh(&id, cx));
                },
                cx,
            ));
        }
        if !doc.scope.is_saved() {
            let id = doc.id.clone();
            actions = actions.child(self.header_button(
                "save",
                "square.and.arrow.up",
                Some("surfaces.save"),
                colors,
                move |this, cx| {
                    let id = id.clone();
                    this.model.update(cx, |model, cx| model.promote(&id, cx));
                },
                cx,
            ));
        } else {
            let id = doc.id.clone();
            let pinned = doc.pinned;
            actions = actions.child(self.header_button(
                "pin",
                if pinned { "pin.fill" } else { "pin" },
                None,
                colors,
                move |this, cx| {
                    let id = id.clone();
                    this.model
                        .update(cx, |model, cx| model.set_pinned(&id, !pinned, cx));
                },
                cx,
            ));
        }
        let tile = if page && !minimal { 44.0 } else { 28.0 };
        let icon = doc
            .icon
            .clone()
            .unwrap_or_else(|| Icon::Symbol("square.grid.2x2".into()));
        let title_size = match (page, minimal) {
            (true, false) => 26.0,
            (true, true) => 18.0,
            _ => 15.0,
        };
        let header = div()
            .flex()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .size(px(tile))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(tile * 0.28))
                    .bg(alpha(ctx.accent, 0.16))
                    .child(self.icon(&icon, tile * 0.55, ctx.accent, ctx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(if page { 3.0 } else { 1.0 }))
                    .child(
                        div()
                            .text_size(px(title_size))
                            .line_height(px((title_size * 1.25).round()))
                            .font_weight(if page && !minimal { FontWeight::BOLD } else { FontWeight::SEMIBOLD })
                            .text_color(colors.primary)
                            .truncate()
                            .child(doc.title.clone()),
                    )
                    .when(!minimal, |el| {
                        el.when_some(doc.subtitle.clone(), |el, subtitle| {
                            el.child(
                                div()
                                    .text_size(px(if page { 14.0 } else { 12.5 }))
                                    .text_color(colors.secondary)
                                    .child(subtitle),
                            )
                        })
                        .when(page, |el| {
                            el.child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(colors.tertiary)
                                    .child(meta.join("  ·  ")),
                            )
                        })
                    }),
            )
            .child(actions);
        let mut column = div().flex().flex_col().gap(px(10.0)).child(header);
        if let Some(run) = app {
            if self.review && !run.approved {
                column = column.child(self.review_panel(doc, run, ctx, cx));
            }
            if let Some(error) = &run.error {
                column = column.child(notice(t("surfaces.app_error"), error, Ink::DANGER, colors));
            } else if let Some(error) = &run.sync.error {
                column = column.child(notice(t("surfaces.sync_failed"), error, Ink::ATTENTION, colors));
            }
        }
        column.into_any_element()
    }

    /// The app's sync state, and its Refresh: "Synced 12s ago", "Syncing…",
    /// or, when its sync asked for programs or the network without
    /// approval, "Review to allow".
    fn sync_chip(&self, doc: &Arc<SurfaceDoc>, run: &AppRun, ctx: &Ctx, cx: &mut Context<Self>) -> AnyElement {
        let colors = ctx.colors;
        let needs = run.sync.needs_approval && !run.approved;
        let (icon, label, tint) = if run.sync.running {
            ("arrow.triangle.2.circlepath", t("surfaces.syncing").to_owned(), colors.secondary)
        } else if needs {
            ("lock", t("surfaces.needs_approval").to_owned(), Ink::ATTENTION)
        } else {
            let when = run
                .sync
                .last_ms
                .map(|last| render::ago(last, self.now()))
                .map_or_else(|| t("surfaces.sync_now").to_owned(), |when| tf("surfaces.synced", &[("when", &when)]));
            ("arrow.clockwise.circle", when, colors.secondary)
        };
        let id = doc.id.clone();
        div()
            .id(SharedString::from(format!("surface-sync-{}", doc.id)))
            .debug_selector(|| "surface-sync".into())
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(26.0))
            .px(px(10.0))
            .rounded(px(Radius::BADGE))
            .bg(if needs { alpha(Ink::ATTENTION, 0.14) } else { alpha(colors.primary, 0.055) })
            .cursor_pointer()
            .hover(move |el| el.bg(alpha(colors.primary, 0.1)))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                if needs {
                    this.review = !this.review;
                    cx.notify();
                } else {
                    let id = id.clone();
                    this.model.update(cx, |model, cx| model.sync_app(&id, cx));
                }
            }))
            .child(sf_symbol(icon, 12.0, tint))
            .child(div().text_size(px(12.0)).font_weight(FontWeight::MEDIUM).text_color(if needs { tint } else { colors.primary }).child(label))
            .into_any_element()
    }

    /// What the person approves: this exact app.lua (its digest), shown, so
    /// an app never runs programs or fetches without someone having seen it.
    fn review_panel(&self, doc: &Arc<SurfaceDoc>, run: &AppRun, ctx: &Ctx, cx: &mut Context<Self>) -> AnyElement {
        let colors = ctx.colors;
        let (id, digest) = (doc.id.clone(), run.digest.clone());
        Self::frame(colors, ctx.radius)
            .p(px(14.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .border_color(alpha(Ink::ATTENTION, 0.45))
            .child(
                div()
                    .text_size(px(13.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.primary)
                    .child(t("surfaces.review_title")),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(colors.secondary)
                    .child(tf("surfaces.review_body", &[("digest", &&run.digest[..12.min(run.digest.len())])])),
            )
            .child(
                div()
                    .id("surface-review-source")
                    .max_h(px(220.0))
                    .overflow_y_scroll()
                    .p(px(10.0))
                    .rounded(px(Radius::ROW))
                    .bg(alpha(colors.primary, 0.05))
                    .font_family(crate::fonts::mono_family())
                    .text_size(px(11.5))
                    .line_height(px(16.0))
                    .text_color(colors.primary)
                    .child(run.source_head.clone()),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(
                        div()
                            .id("surface-review-allow")
                            .debug_selector(|| "surface-review-allow".into())
                            .h(px(28.0))
                            .px(px(12.0))
                            .flex()
                            .items_center()
                            .rounded(px(Radius::ROW))
                            .bg(ctx.accent)
                            .cursor_pointer()
                            .text_size(px(12.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(gpui::white())
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.review = false;
                                let (id, digest) = (id.clone(), digest.clone());
                                this.model.update(cx, |model, cx| model.approve_app(&id, &digest, cx));
                            }))
                            .child(t("surfaces.allow")),
                    )
                    .child(
                        div()
                            .id("surface-review-cancel")
                            .h(px(28.0))
                            .px(px(12.0))
                            .flex()
                            .items_center()
                            .rounded(px(Radius::ROW))
                            .bg(alpha(colors.primary, 0.07))
                            .cursor_pointer()
                            .text_size(px(12.5))
                            .text_color(colors.primary)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.review = false;
                                cx.notify();
                            }))
                            .child(t("surfaces.not_now")),
                    ),
            )
            .into_any_element()
    }

    /// Syncs an app when this view first shows it (if it asks to), and keeps
    /// one timer for its next poll. The timer only wakes this view; if the
    /// view is no longer drawn, nothing renders, nothing reschedules, and the
    /// app is not polled. Zero work while hidden.
    fn schedule_sync(&mut self, id: &str, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let open = !self.opened.contains(id) && model.syncs_on_open(id);
        let due = model.poll_due(id, self.now());
        if self.opened.insert(id.to_owned()) && open {
            let id = id.to_owned();
            self.model.update(cx, |model, cx| model.sync_app(&id, cx));
            return;
        }
        match due {
            Some(wait) if wait.is_zero() => {
                self.poll = None;
                let id = id.to_owned();
                self.model.update(cx, |model, cx| model.sync_app(&id, cx));
            }
            Some(wait) => {
                if self.poll.as_ref().is_some_and(|(scheduled, _)| scheduled == id) {
                    return;
                }
                let task = cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(wait).await;
                    let _ = this.update(cx, |this, cx| {
                        this.poll = None;
                        cx.notify();
                    });
                });
                self.poll = Some((id.to_owned(), task));
            }
            None => self.poll = None,
        }
    }

    /// The next poll this view has asked to be woken for.
    #[cfg(test)]
    pub(crate) fn scheduled_poll(&self) -> Option<&str> {
        self.poll.as_ref().map(|(id, _)| id.as_str())
    }

    #[cfg(test)]
    pub(crate) fn open_review_for_test(&mut self, cx: &mut Context<Self>) {
        self.review = true;
        cx.notify();
    }

    // -- Components -------------------------------------------------------

    fn children(
        &mut self,
        nodes: &[Node],
        doc: &Arc<SurfaceDoc>,
        ctx: &Ctx,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        nodes.iter().map(|node| self.node(node, doc, ctx, cx)).collect()
    }

    fn frame(colors: SemanticColors, radius: f32) -> gpui::Div {
        div()
            .rounded(px(radius))
            .bg(alpha(colors.primary, 0.035))
            .border_1()
            .border_color(alpha(colors.primary, 0.075))
    }

    /// Dispatch only: the recursive path (containers) and the leaves live in
    /// separate frames, so a deep surface costs a small frame per level.
    fn node(&mut self, node: &Node, doc: &Arc<SurfaceDoc>, ctx: &Ctx, cx: &mut Context<Self>) -> AnyElement {
        let element = if node.is_container() || matches!(node.kind, NodeKind::Tabs { .. }) {
            self.container(node, doc, ctx, cx)
        } else {
            self.leaf(node, doc, ctx, cx)
        };
        match (&node.style, &node.kind) {
            // A box takes its style itself (gap, direction, alignment).
            (Some(_), NodeKind::Box { .. }) | (None, _) => element,
            (Some(style), _) => apply_style(div(), style, ctx).child(element).into_any_element(),
        }
    }

    fn icon(&self, icon: &Icon, size: f32, color: Rgba, ctx: &Ctx) -> AnyElement {
        icon_element(icon, size, color, ctx.folder.as_deref())
    }

    #[inline(never)]
    fn container(&mut self, node: &Node, doc: &Arc<SurfaceDoc>, ctx: &Ctx, cx: &mut Context<Self>) -> AnyElement {
        let colors = ctx.colors;
        let node_id = node.id.clone().unwrap_or_default();
        match &node.kind {
            NodeKind::Box {
                direction,
                wrap,
                children,
            } => {
                let base = div()
                    .flex()
                    .when(*direction == Direction::Row, |el| el.flex_row().items_center())
                    .when(*direction == Direction::Column, |el| el.flex_col())
                    // A narrow panel wraps rows rather than clipping them.
                    .when(*wrap || (ctx.compact && *direction == Direction::Row), |el| el.flex_wrap());
                let styled = match &node.style {
                    Some(style) => apply_style(base, style, ctx),
                    None => base,
                };
                styled
                    .children(self.children(children, doc, ctx, cx))
                    .into_any_element()
            }
            NodeKind::Scroll { height, children } => div()
                .id(element_id(&ctx.surface, "scroll", &node_id))
                .h(px(*height))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(8.0 * ctx.scale))
                .children(self.children(children, doc, ctx, cx))
                .into_any_element(),
            NodeKind::Stack { gap, children } => div()
                .flex()
                .flex_col()
                .gap(px(gap.unwrap_or(10.0)))
                .children(self.children(children, doc, ctx, cx))
                .into_any_element(),
            NodeKind::Row {
                gap,
                wrap,
                children,
            } => div()
                .flex()
                .items_center()
                .gap(px(gap.unwrap_or(8.0)))
                .when(*wrap || ctx.compact, |el| el.flex_wrap())
                .children(self.children(children, doc, ctx, cx))
                .into_any_element(),
            NodeKind::Grid { columns, children } => {
                let has_blocks = children.iter().any(|child| {
                    matches!(
                        child.kind,
                        NodeKind::Card { .. } | NodeKind::Stack { .. } | NodeKind::Table { .. }
                    )
                });
                let columns = if ctx.compact {
                    if has_blocks { 1 } else { (*columns).min(2) }
                } else {
                    *columns
                }
                .max(1) as usize;
                let mut rows = div().flex().flex_col().gap(px(12.0));
                let rendered = self.children(children, doc, ctx, cx);
                let mut iter = rendered.into_iter().peekable();
                while iter.peek().is_some() {
                    let mut row = div().flex().items_start().gap(px(12.0));
                    for _ in 0..columns {
                        let cell = div().flex_1().min_w_0();
                        row = row.child(match iter.next() {
                            Some(child) => cell.child(child),
                            None => cell,
                        });
                    }
                    rows = rows.child(row);
                }
                rows.into_any_element()
            }
            NodeKind::Card {
                title,
                subtitle,
                tone,
                children,
            } => {
                let border = tone.map(|tone| alpha(render::tone_color(tone, colors), 0.45));
                Self::frame(colors, ctx.radius)
                    .when_some(border, |el, border| el.border_color(border))
                    .p(px(if ctx.compact { 12.0 } else { 16.0 }))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .when(title.is_some() || subtitle.is_some(), |el| {
                        el.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .when_some(title.clone(), |el, title| {
                                    el.child(
                                        div()
                                            .text_size(px(13.5))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(colors.primary)
                                            .child(title),
                                    )
                                })
                                .when_some(subtitle.clone(), |el, subtitle| {
                                    el.child(
                                        div()
                                            .text_size(px(12.0))
                                            .text_color(colors.tertiary)
                                            .child(subtitle),
                                    )
                                }),
                        )
                    })
                    .children(self.children(children, doc, ctx, cx))
                    .into_any_element()
            }
            NodeKind::Section {
                title,
                detail,
                children,
            } => div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap(px(8.0))
                        .child(
                            div()
                                .text_size(px(11.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.tertiary)
                                .child(title.to_uppercase()),
                        )
                        .when_some(detail.clone(), |el, detail| {
                            el.child(div().text_size(px(11.5)).text_color(colors.tertiary).child(detail))
                        }),
                )
                .children(self.children(children, doc, ctx, cx))
                .into_any_element(),
            NodeKind::Tabs { tabs } => {
                let selected = self
                    .tabs
                    .get(&node_id)
                    .filter(|id| tabs.iter().any(|tab| &tab.id == *id))
                    .cloned()
                    .or_else(|| tabs.first().map(|tab| tab.id.clone()))
                    .unwrap_or_default();
                let mut strip = div()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .p(px(2.0))
                    .rounded(px(Radius::ROW + 1.0))
                    .bg(alpha(colors.primary, 0.05))
                    .self_start();
                for tab in tabs {
                    let active = tab.id == selected;
                    let (tabs_id, tab_id) = (node_id.clone(), tab.id.clone());
                    strip = strip.child(
                        div()
                            .id(element_id(&ctx.surface, "tab", &tab.id))
                            .px(px(12.0))
                            .h(px(24.0))
                            .flex()
                            .items_center()
                            .rounded(px(Radius::ROW))
                            .cursor_pointer()
                            .text_size(px(12.5))
                            .font_weight(FontWeight::MEDIUM)
                            .when(active, |el| {
                                el.bg(colors.background).text_color(colors.primary).shadow(vec![
                                    gpui::BoxShadow {
                                        color: gpui::hsla(0.0, 0.0, 0.0, 0.12),
                                        offset: gpui::point(px(0.0), px(1.0)),
                                        blur_radius: px(2.0),
                                        spread_radius: px(0.0),
                                        inset: false,
                                    },
                                ])
                            })
                            .when(!active, |el| {
                                el.text_color(colors.secondary)
                                    .hover(move |el| el.text_color(colors.primary))
                            })
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.tabs.insert(tabs_id.clone(), tab_id.clone());
                                cx.notify();
                            }))
                            .child(tab.label.clone()),
                    );
                }
                let body = tabs
                    .iter()
                    .find(|tab| tab.id == selected)
                    .map(|tab| self.children(&tab.children, doc, ctx, cx))
                    .unwrap_or_default();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(strip)
                    .children(body)
                    .into_any_element()
            }
            _ => self.leaf(node, doc, ctx, cx),
        }
    }

    #[inline(never)]
    fn leaf(&mut self, node: &Node, doc: &Arc<SurfaceDoc>, ctx: &Ctx, cx: &mut Context<Self>) -> AnyElement {
        let colors = ctx.colors;
        let node_id = node.id.clone().unwrap_or_default();
        match &node.kind {
            NodeKind::Divider => div()
                .h(px(1.0))
                .w_full()
                .bg(alpha(colors.primary, 0.08))
                .into_any_element(),
            NodeKind::Heading { text, level } => {
                let (size, weight) = match level {
                    1 => (22.0, FontWeight::BOLD),
                    2 => (17.0, FontWeight::SEMIBOLD),
                    _ => (14.0, FontWeight::SEMIBOLD),
                };
                div()
                    .text_size(px(size))
                    .line_height(px(size * 1.3))
                    .font_weight(weight)
                    .text_color(colors.primary)
                    .child(text.clone())
                    .into_any_element()
            }
            NodeKind::Text {
                text,
                tone,
                mono,
                bind,
                size,
                weight,
                color,
                truncate,
            } => {
                let live = bind.as_ref().and_then(|bind| self.resolve(bind, cx));
                let (text, ink) = match live {
                    Some(resolved) => (resolved.text, render::tone_color(resolved.tone, colors)),
                    None => (
                        text.clone(),
                        tone.map_or(colors.primary, |tone| render::tone_color(tone, colors)),
                    ),
                };
                let ink = color.as_ref().map_or(ink, |color| ctx.paint(color));
                let size = size.unwrap_or(if *mono { 12.5 } else { 13.5 });
                div()
                    .text_size(px(size))
                    .line_height(px((size * 1.42).round()))
                    .text_color(ink)
                    .font_weight(font_weight(*weight))
                    .when(*mono, |el| el.font_family(crate::fonts::mono_family()))
                    .when(*truncate, |el| el.min_w_0().truncate())
                    .child(text)
                    .into_any_element()
            }
            NodeKind::Markdown { text } => {
                let key = hash_of(&[text]);
                let document = self
                    .markdown
                    .entry(key)
                    .or_insert_with(|| Arc::new(MarkdownDocument::parse(text)))
                    .clone();
                crate::markdown_view::render_markdown(&document, colors)
            }
            NodeKind::Badge { label, tone, bind } => {
                let live = bind.as_ref().and_then(|bind| self.resolve(bind, cx));
                let (label, tone) = match live {
                    Some(resolved) => (resolved.text, resolved.tone),
                    None => (label.clone(), tone.unwrap_or_default()),
                };
                badge(label, tone, colors)
            }
            NodeKind::Metric {
                label,
                value,
                delta,
                trend,
                caption,
                bind,
            } => {
                let live = bind.as_ref().and_then(|bind| self.resolve(bind, cx));
                let (value, value_color) = match live {
                    Some(resolved) => (
                        resolved.text,
                        if resolved.tone == Tone::Muted {
                            colors.primary
                        } else {
                            render::tone_color(resolved.tone, colors)
                        },
                    ),
                    None => (value.clone(), colors.primary),
                };
                let trend_tone = match trend.unwrap_or_default() {
                    Trend::Up => Tone::Success,
                    Trend::Down => Tone::Danger,
                    Trend::Flat => Tone::Muted,
                };
                Self::frame(colors, ctx.radius)
                    .p(px(if ctx.compact { 12.0 } else { 14.0 }))
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(colors.secondary)
                            .truncate()
                            .child(label.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .text_size(px(if ctx.compact { 22.0 } else { 28.0 }))
                                    .line_height(px(if ctx.compact { 28.0 } else { 34.0 }))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(value_color)
                                    .child(value),
                            )
                            .when_some(delta.clone(), |el, delta| {
                                el.child(
                                    div()
                                        .text_size(px(12.0))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(render::tone_color(trend_tone, colors))
                                        .child(delta),
                                )
                            }),
                    )
                    .when_some(caption.clone(), |el, caption| {
                        el.child(div().text_size(px(11.5)).text_color(colors.tertiary).child(caption))
                    })
                    .into_any_element()
            }
            NodeKind::Progress {
                value,
                max,
                label,
                tone,
                track,
            } => {
                let tracked = track.as_deref().and_then(|id| doc.checklist_counts(id));
                let (fraction, label) = match tracked {
                    Some((done, total)) => (
                        if total == 0 { 0.0 } else { done as f32 / total as f32 },
                        label.clone().or_else(|| {
                            Some(tf("surfaces.done", &[("done", &done), ("total", &total)]))
                        }),
                    ),
                    None => ((value / max).clamp(0.0, 1.0) as f32, label.clone()),
                };
                let fill = tone.map_or(ctx.accent, |tone| {
                    render::tone_color(tone, colors)
                });
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .text_size(px(12.0))
                            .text_color(colors.secondary)
                            .child(label.unwrap_or_default())
                            .child(format!("{:.0}%", fraction * 100.0)),
                    )
                    .child(
                        div()
                            .h(px(6.0))
                            .w_full()
                            .rounded(px(3.0))
                            .bg(alpha(colors.primary, 0.08))
                            .child(
                                div()
                                    .h_full()
                                    .w(gpui::relative(fraction))
                                    .rounded(px(3.0))
                                    .bg(fill),
                            ),
                    )
                    .into_any_element()
            }
            NodeKind::Table { columns, rows } => {
                let mut table = Self::frame(colors, ctx.radius).flex().flex_col().overflow_hidden();
                let mut header = div()
                    .flex()
                    .px(px(12.0))
                    .py(px(8.0))
                    .bg(alpha(colors.primary, 0.03))
                    .border_b_1()
                    .border_color(alpha(colors.primary, 0.07));
                for column in columns {
                    header = header.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(11.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.tertiary)
                            .truncate()
                            .child(column.clone()),
                    );
                }
                table = table.child(header);
                for (index, row) in rows.iter().enumerate() {
                    let mut line = div()
                        .flex()
                        .items_center()
                        .px(px(12.0))
                        .py(px(7.0))
                        .when(index + 1 < rows.len(), |el| {
                            el.border_b_1().border_color(alpha(colors.primary, 0.05))
                        });
                    for column in 0..columns.len() {
                        let cell = row.get(column);
                        line = line.child(self.cell(cell, &format!("{node_id}-{index}-{column}"), ctx, cx));
                    }
                    table = table.child(line);
                }
                table.into_any_element()
            }
            NodeKind::KvList { items } => {
                let mut list = div().flex().flex_col().gap(px(4.0));
                for (index, item) in items.iter().enumerate() {
                    let value_color = item
                        .tone
                        .map_or(colors.primary, |tone| render::tone_color(tone, colors));
                    let link = item.link.clone();
                    let surface = ctx.surface.clone();
                    list = list.child(
                        div()
                            .id(element_id(&ctx.surface, "kv", &format!("{node_id}-{index}")))
                            .flex()
                            .items_baseline()
                            .gap(px(12.0))
                            .text_size(px(13.0))
                            .child(
                                div()
                                    .w(px(if ctx.compact { 96.0 } else { 140.0 }))
                                    .flex_none()
                                    .text_color(colors.secondary)
                                    .truncate()
                                    .child(item.key.clone()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_color(value_color)
                                    .when(link.is_some(), |el| {
                                        el.cursor_pointer().text_color(ctx.accent)
                                    })
                                    .child(item.value.clone()),
                            )
                            .when_some(link, |el, url| {
                                el.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.open_link(&surface, &url, cx)
                                }))
                            }),
                    );
                }
                list.into_any_element()
            }
            NodeKind::Mermaid { source } => self.mermaid(&node_id, source, ctx, cx),
            NodeKind::Image { src, alt, height } => self.image(src, alt.as_deref(), *height, ctx, cx),
            NodeKind::Checklist { title, items, .. } => {
                let mut list = div().flex().flex_col().gap(px(1.0));
                if let Some(title) = title {
                    list = list.child(
                        div()
                            .pb(px(4.0))
                            .text_size(px(12.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.secondary)
                            .child(title.clone()),
                    );
                }
                for item in items {
                    let checked = doc.is_checked(&node_id, item);
                    list = list.child(self.checklist_row(&node_id, item, checked, ctx, cx));
                }
                list.into_any_element()
            }
            NodeKind::Link {
                url,
                label,
                detail,
                icon,
                badge,
            } => self.link_row(
                &node_id,
                url,
                label.as_deref(),
                detail.as_deref(),
                icon.as_ref(),
                badge.as_deref(),
                ctx,
                cx,
            ),
            NodeKind::Button {
                label,
                action,
                variant,
                icon,
            } => {
                let accent = ctx.accent;
                let (bg, hover, fg) = match variant {
                    ButtonStyle::Primary => (accent, alpha(accent, 0.88), gpui::white().into()),
                    ButtonStyle::Secondary => (
                        alpha(colors.primary, 0.07),
                        alpha(colors.primary, 0.12),
                        colors.primary,
                    ),
                    ButtonStyle::Danger => (alpha(Ink::DANGER, 0.14), alpha(Ink::DANGER, 0.22), Ink::DANGER),
                };
                let fg: Rgba = fg;
                let (surface, id, action) = (ctx.surface.clone(), node_id.clone(), action.clone());
                div()
                    .id(element_id(&ctx.surface, "button", &node_id))
                    .debug_selector(|| format!("surface-button-{}", node_id))
                    .h(px(28.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .rounded(px(Radius::ROW))
                    .bg(bg)
                    .hover(move |el| el.bg(hover))
                    .cursor_pointer()
                    .text_size(px(12.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .gap(px(6.0))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.act(&surface, &id, &action, cx)
                    }))
                    .when_some(icon.as_ref(), |el, icon| el.child(self.icon(icon, 13.0, fg, ctx)))
                    .child(label.clone())
                    .into_any_element()
            }
            NodeKind::Toggle {
                label,
                default,
                value,
                ..
            } => {
                let on = value.unwrap_or_else(|| doc.toggle_value(&node_id, *default));
                let accent = ctx.accent;
                let (surface, id) = (ctx.surface.clone(), node_id.clone());
                div()
                    .id(element_id(&ctx.surface, "toggle", &node_id))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.interact(&surface, Interaction::Toggle { id: id.clone(), on: !on }, cx)
                    }))
                    .child(
                        div()
                            .w(px(28.0))
                            .h(px(16.0))
                            .flex_none()
                            .rounded(px(8.0))
                            .p(px(2.0))
                            .flex()
                            .when(on, |el| el.justify_end().bg(accent))
                            .when(!on, |el| el.bg(alpha(colors.primary, 0.16)))
                            .child(div().size(px(12.0)).rounded(px(6.0)).bg(gpui::white())),
                    )
                    .child(div().text_size(px(12.5)).text_color(colors.primary).child(label.clone()))
                    .into_any_element()
            }
            NodeKind::Select {
                label,
                options,
                default,
                value: controlled,
                ..
            } => {
                let value = controlled.clone().or_else(|| {
                    doc.select_value(&node_id, default.as_deref())
                        .map(str::to_owned)
                });
                let mut strip = div()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .p(px(2.0))
                    .rounded(px(Radius::ROW + 1.0))
                    .bg(alpha(colors.primary, 0.05))
                    .flex_wrap();
                for option in options {
                    let active = value.as_deref() == Some(option.value.as_str());
                    let (surface, id, chosen) =
                        (ctx.surface.clone(), node_id.clone(), option.value.clone());
                    strip = strip.child(
                        div()
                            .id(element_id(&ctx.surface, "select", &format!("{node_id}-{}", option.value)))
                            .px(px(10.0))
                            .h(px(22.0))
                            .flex()
                            .items_center()
                            .rounded(px(Radius::ROW))
                            .cursor_pointer()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::MEDIUM)
                            .when(active, |el| el.bg(colors.background).text_color(colors.primary))
                            .when(!active, |el| {
                                el.text_color(colors.secondary)
                                    .hover(move |el| el.text_color(colors.primary))
                            })
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.interact(
                                    &surface,
                                    Interaction::Select {
                                        id: id.clone(),
                                        value: chosen.clone(),
                                    },
                                    cx,
                                )
                            }))
                            .child(option.label.clone()),
                    );
                }
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .when_some(label.clone(), |el, label| {
                        el.child(div().text_size(px(12.5)).text_color(colors.secondary).child(label))
                    })
                    .child(strip)
                    .into_any_element()
            }
            NodeKind::Session { session, label } => self.session_row(&node_id, session, label.as_deref(), ctx, cx),
            NodeKind::Spacer => div().flex_1().into_any_element(),
            NodeKind::Code { text, language } => Self::frame(colors, ctx.radius)
                .p(px(12.0 * ctx.scale))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .when_some(language.clone(), |el, language| {
                    el.child(div().text_size(px(10.5)).text_color(colors.tertiary).child(language))
                })
                .child(
                    div()
                        .font_family(crate::fonts::mono_family())
                        .text_size(px(12.0))
                        .line_height(px(18.0))
                        .text_color(colors.primary)
                        .child(text.clone()),
                )
                .into_any_element(),
            NodeKind::Canvas {
                width,
                height,
                shapes,
            } => canvas_element(*width, *height, shapes.clone(), ctx),
            NodeKind::Media {
                image,
                title,
                subtitle,
                badge,
                url,
                aspect,
            } => self.media(&node_id, image.as_deref(), title, subtitle.as_deref(), badge.as_deref(), url.as_deref(), aspect.unwrap_or(16.0 / 9.0), ctx, cx),
            NodeKind::Checkbox { label, value } => {
                let on = if doc.app.is_some() {
                    *value
                } else {
                    doc.toggle_value(&node_id, *value)
                };
                let (surface, id) = (ctx.surface.clone(), node_id.clone());
                let accent = ctx.accent;
                div()
                    .id(element_id(&ctx.surface, "checkbox", &node_id))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.interact(&surface, Interaction::Toggle { id: id.clone(), on: !on }, cx)
                    }))
                    .child(tick_box(on, accent, colors))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(if on { colors.secondary } else { colors.primary })
                            .child(label.clone()),
                    )
                    .into_any_element()
            }
            NodeKind::Input { placeholder, value } => {
                let value = if doc.app.is_some() {
                    value.clone().unwrap_or_default()
                } else {
                    doc.state.selects.get(&node_id).cloned().or(value.clone()).unwrap_or_default()
                };
                self.input_field(&node_id, &value, placeholder.as_deref().unwrap_or_default(), ctx, cx)
            }
            NodeKind::Unsupported => div()
                .px(px(10.0))
                .py(px(8.0))
                .rounded(px(Radius::ROW))
                .border_1()
                .border_dashed()
                .border_color(alpha(colors.primary, 0.15))
                .text_size(px(12.0))
                .text_color(colors.tertiary)
                .child(t("surfaces.unsupported"))
                .into_any_element(),
            _ => div().into_any_element(),
        }
    }

    fn resolve(&mut self, binding: &diri_proto::surface::Binding, cx: &mut Context<Self>) -> Option<Resolved> {
        let todos = crate::notes::todos::TodosModel::global(&self.runtime, cx);
        let notes = todos.read(cx).notes();
        let store = self.runtime.store.read().expect("store");
        render::resolve(binding, &store, &|note| {
            notes.iter().find(|entry| entry.id == note).map(|entry| entry.open_todos)
        })
    }

    fn cell(&mut self, cell: Option<&Cell>, key: &str, ctx: &Ctx, cx: &mut Context<Self>) -> AnyElement {
        let colors = ctx.colors;
        let Some(cell) = cell else {
            return div().flex_1().min_w_0().into_any_element();
        };
        let tone = cell.tone();
        let link = cell.link().map(str::to_owned);
        let surface = ctx.surface.clone();
        div()
            .id(element_id(&ctx.surface, "cell", key))
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(6.0))
            .text_size(px(13.0))
            .text_color(tone.map_or(colors.primary, |tone| render::tone_color(tone, colors)))
            .when_some(tone.filter(|t| !matches!(t, Tone::Neutral | Tone::Muted)), |el, tone| {
                el.child(div().size(px(6.0)).flex_none().rounded(px(3.0)).bg(render::tone_color(tone, colors)))
            })
            .when(link.is_some(), |el| el.cursor_pointer().text_color(ctx.accent))
            .child(div().min_w_0().truncate().child(cell.text().to_owned()))
            .when_some(link, |el, url| {
                el.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.open_link(&surface, &url, cx)))
            })
            .into_any_element()
    }

    fn checklist_row(
        &mut self,
        list: &str,
        item: &ChecklistItem,
        checked: bool,
        ctx: &Ctx,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = ctx.colors;
        let accent = ctx.accent;
        let (surface, list_id, item_id) = (ctx.surface.clone(), list.to_owned(), item.id.clone());
        let tick = cx.listener(move |this, _: &MouseDownEvent, _, cx| {
            cx.stop_propagation();
            this.interact(
                &surface,
                Interaction::Check {
                    checklist: list_id.clone(),
                    item: item_id.clone(),
                    checked: !checked,
                },
                cx,
            );
        });
        let link = item.link.as_deref().map(|url| {
            let look = {
                let store = self.runtime.store.read().expect("store");
                render::link_look(url, item.link_label.as_deref(), item.icon.as_ref(), None, Some(&store))
            };
            let glyph = match &look.icon {
                Some(icon) => self.icon(icon, 11.0, colors.secondary, ctx),
                None => sf_symbol(look.default_symbol, 11.0, colors.secondary),
            };
            let (surface, url) = (ctx.surface.clone(), url.to_owned());
            div()
                .id(element_id(&ctx.surface, "check-link", &format!("{list}-{}", item.id)))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(4.0))
                .h(px(20.0))
                .px(px(6.0))
                .rounded(px(Radius::BADGE))
                .bg(alpha(colors.primary, 0.05))
                .hover(move |el| el.bg(alpha(colors.primary, 0.1)))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.open_link(&surface, &url, cx)))
                .child(glyph)
                .child(div().text_size(px(11.0)).text_color(colors.secondary).child(look.label))
        });
        div()
            .id(element_id(&ctx.surface, "check", &format!("{list}-{}", item.id)))
            .debug_selector(|| format!("surface-check-{list}-{}", item.id))
            .flex()
            .items_start()
            .gap(px(10.0))
            .px(px(6.0))
            .py(px(5.0))
            .rounded(px(Radius::ROW))
            .hover(move |el| el.bg(Fill::hover(colors, true)))
            .child(
                div()
                    .id(element_id(&ctx.surface, "tick", &format!("{list}-{}", item.id)))
                    .mt(px(2.0))
                    .size(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .when(checked, |el| el.bg(accent).child(sf_symbol("checkmark", 10.0, gpui::white().into())))
                    .when(!checked, |el| {
                        el.border(px(1.5))
                            .border_color(alpha(colors.primary, 0.3))
                            .hover(move |el| el.border_color(accent).bg(alpha(accent, 0.1)))
                    })
                    .on_mouse_down(MouseButton::Left, tick),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(
                        div()
                            .text_size(px(13.5))
                            .line_height(px(20.0))
                            .text_color(if checked { colors.tertiary } else { colors.primary })
                            .when(checked, |el| el.line_through())
                            .child(item.text.clone()),
                    )
                    .when_some(item.detail.clone(), |el, detail| {
                        el.child(div().text_size(px(11.5)).text_color(colors.tertiary).child(detail))
                    }),
            )
            .children(link)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn link_row(
        &mut self,
        node_id: &str,
        url: &str,
        label: Option<&str>,
        detail: Option<&str>,
        icon: Option<&Icon>,
        badge_word: Option<&str>,
        ctx: &Ctx,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = ctx.colors;
        let look = {
            let store = self.runtime.store.read().expect("store");
            render::link_look(url, label, icon, badge_word, Some(&store))
        };
        let glyph = match &look.icon {
            Some(icon) => self.icon(icon, 14.0, colors.primary, ctx),
            None => sf_symbol(look.default_symbol, 14.0, colors.primary),
        };
        let (surface, url) = (ctx.surface.clone(), url.to_owned());
        div()
            .id(element_id(&ctx.surface, "link", if node_id.is_empty() { &url } else { node_id }))
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(8.0))
            .py(px(6.0))
            .mx(px(-8.0))
            .rounded(px(Radius::ROW))
            .cursor_pointer()
            .hover(move |el| el.bg(Fill::hover(colors, true)))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.open_link(&surface, &url, cx)))
            .child(
                div()
                    .size(px(26.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Radius::BADGE + 1.0))
                    .bg(alpha(colors.primary, 0.06))
                    .child(glyph),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(13.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(colors.primary)
                                    .child(look.label),
                            )
                            .when_some(look.badge.clone(), |el, word| el.child(badge(word, Tone::Muted, colors))),
                    )
                    .when_some(detail.map(str::to_owned), |el, detail| {
                        el.child(div().truncate().text_size(px(12.0)).text_color(colors.tertiary).child(detail))
                    }),
            )
            .child(sf_symbol("chevron.right", 11.0, colors.tertiary))
            .into_any_element()
    }

    fn session_row(
        &mut self,
        node_id: &str,
        session: &str,
        label: Option<&str>,
        ctx: &Ctx,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = ctx.colors;
        let record = {
            let store = self.runtime.store.read().expect("store");
            store.sessions().get(&SessionId::new(session)).cloned()
        };
        let (title, status, logo) = match &record {
            Some(record) => (
                label.map(str::to_owned).unwrap_or_else(|| record.title.clone()),
                render::session_status(record),
                Some(crate::session_presentation::ui_agent_kind(record.effective_kind())),
            ),
            None => (
                label.map(str::to_owned).unwrap_or_else(|| session.to_owned()),
                Resolved {
                    text: t("surfaces.session_gone").to_owned(),
                    tone: Tone::Muted,
                },
                None,
            ),
        };
        let (surface, url) = (ctx.surface.clone(), format!("diri://session/{session}"));
        div()
            .id(element_id(&ctx.surface, "session", if node_id.is_empty() { session } else { node_id }))
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(8.0))
            .py(px(6.0))
            .mx(px(-8.0))
            .rounded(px(Radius::ROW))
            .cursor_pointer()
            .hover(move |el| el.bg(Fill::hover(colors, true)))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.open_link(&surface, &url, cx)))
            .child(match logo {
                Some(kind) => AgentLogo::new(kind, 20.0, colors).badged(false).into_any_element(),
                None => sf_symbol("terminal", 14.0, colors.tertiary),
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.5))
                    .text_color(colors.primary)
                    .child(title),
            )
            .child(badge(status.text, status.tone, colors))
            .into_any_element()
    }

    fn mermaid(&mut self, node_id: &str, source: &str, ctx: &Ctx, cx: &mut Context<Self>) -> AnyElement {
        let colors = ctx.colors;
        let key = hash_of(&[&ctx.surface, node_id]);
        let (look, start) = self.diagrams.look(key, source, ctx.dark);
        if let Some(draw_key) = start {
            let source = source.to_owned();
            let dark = ctx.dark;
            self.diagram_tasks.push(cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { diagram::draw(&source, dark) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if this.diagrams.landed(key, draw_key, result) {
                        cx.notify();
                    }
                });
            }));
        }
        let body = match look {
            Look::Drawn(drawing) => img(drawing.image.clone())
                .w(px(drawing.width))
                .max_w_full()
                .h(px(drawing.height))
                .into_any_element(),
            Look::Pending => div().h(px(80.0)).into_any_element(),
            Look::Failed(message, _) => div()
                .text_size(px(12.0))
                .text_color(colors.tertiary)
                .child(message)
                .into_any_element(),
        };
        Self::frame(colors, ctx.radius)
            .p(px(12.0))
            .flex()
            .justify_center()
            .child(body)
            .into_any_element()
    }

    fn image(
        &mut self,
        src: &str,
        alt: Option<&str>,
        height: Option<f32>,
        ctx: &Ctx,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = ctx.colors;
        let height = px(height.unwrap_or(180.0));
        let path = if let Some(name) = src.strip_prefix("assets/") {
            let store = self.model.read(cx).store().cloned();
            store.map(|store| store.dir().join("assets").join(name))
        } else if let Some(cache) = web_image::cache_dir() {
            let (state, start) = self.images.look(src, &cache);
            if start {
                let url = src.to_owned();
                cx.spawn(async move |this, cx| {
                    let file = cx
                        .background_executor()
                        .spawn({
                            let url = url.clone();
                            async move { web_image::fetch(&cache, &url) }
                        })
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        this.images.landed(&url, file);
                        cx.notify();
                    });
                })
                .detach();
            }
            match state {
                WebImage::Ready(path) => Some(path),
                _ => None,
            }
        } else {
            None
        };
        match path {
            Some(path) => img(path)
                .h(height)
                .max_w_full()
                .rounded(px(Radius::CARD))
                .into_any_element(),
            None => Self::frame(colors, ctx.radius)
                .h(height)
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(colors.tertiary)
                .child(alt.unwrap_or_default().to_owned())
                .into_any_element(),
        }
    }

    fn render_empty(&self, colors: SemanticColors) -> AnyElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.0))
            .p(px(24.0))
            .child(sf_symbol("square.grid.2x2", 24.0, colors.tertiary))
            .child(
                div()
                    .text_size(px(14.0))
                    .text_color(colors.secondary)
                    .child(t("surfaces.empty")),
            )
            .child(
                div()
                    .max_w(px(300.0))
                    .text_center()
                    .text_size(px(12.0))
                    .text_color(colors.tertiary)
                    .child(t("surfaces.empty_hint")),
            )
            .into_any_element()
    }

    /// Several surfaces on one session: a quiet chip row to pick one.
    fn render_picker(&self, docs: &[Arc<SurfaceDoc>], shown: &str, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let mut row = div().flex().flex_wrap().gap(px(4.0));
        for doc in docs {
            let active = doc.id == shown;
            let id = doc.id.clone();
            row = row.child(
                div()
                    .id(SharedString::from(format!("surface-pick-{}", doc.id)))
                    .px(px(8.0))
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .rounded(px(Radius::BADGE))
                    .cursor_pointer()
                    .text_size(px(11.5))
                    .when(active, |el| el.bg(alpha(colors.primary, 0.09)).text_color(colors.primary))
                    .when(!active, |el| el.text_color(colors.secondary).hover(move |el| el.bg(alpha(colors.primary, 0.05))))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.picked = Some(id.clone());
                        cx.notify();
                    }))
                    .child(doc.title.clone()),
            );
        }
        row.into_any_element()
    }
}

fn notice(title: &str, message: &str, tint: Rgba, colors: SemanticColors) -> AnyElement {
    div()
        .p(px(12.0))
        .rounded(px(Radius::ROW))
        .bg(alpha(tint, 0.1))
        .border_1()
        .border_color(alpha(tint, 0.35))
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(div().text_size(px(12.5)).font_weight(FontWeight::SEMIBOLD).text_color(tint).child(title.to_owned()))
        .child(
            div()
                .font_family(crate::fonts::mono_family())
                .text_size(px(11.5))
                .text_color(colors.primary)
                .child(message.to_owned()),
        )
        .into_any_element()
}

fn badge(label: String, tone: Tone, colors: SemanticColors) -> AnyElement {
    let ink = match tone {
        Tone::Neutral => colors.primary,
        Tone::Muted => colors.secondary,
        tone => render::tone_color(tone, colors),
    };
    div()
        .flex_none()
        .h(px(18.0))
        .px(px(6.0))
        .flex()
        .items_center()
        .rounded(px(Radius::BADGE))
        .bg(render::tone_fill(tone, colors))
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(ink)
        .child(label)
        .into_any_element()
}

fn font_weight(weight: Option<Weight>) -> FontWeight {
    match weight.unwrap_or_default() {
        Weight::Regular => FontWeight::NORMAL,
        Weight::Medium => FontWeight::MEDIUM,
        Weight::Semibold => FontWeight::SEMIBOLD,
        Weight::Bold => FontWeight::BOLD,
    }
}

/// The agent's box styling on top of `el`.
fn apply_style(el: gpui::Div, style: &BoxStyle, ctx: &Ctx) -> gpui::Div {
    let mut el = el;
    if let Some(padding) = style.padding {
        el = el.p(px(padding));
    }
    if let Some(x) = style.padding_x {
        el = el.px(px(x));
    }
    if let Some(y) = style.padding_y {
        el = el.py(px(y));
    }
    if let Some(gap) = style.gap {
        el = el.gap(px(gap));
    }
    if let Some(bg) = &style.bg {
        el = el.bg(ctx.paint(bg));
    }
    if let Some(border) = &style.border {
        el = el
            .border(px(style.border_width.unwrap_or(1.0)))
            .border_color(ctx.paint(border));
    }
    if let Some(radius) = style.radius {
        el = el.rounded(px(radius));
    }
    match &style.width {
        Some(Length::Points(w)) => el = el.w(px(*w)).flex_none(),
        Some(Length::Keyword(_)) => el = el.w_full(),
        None => {}
    }
    match &style.height {
        Some(Length::Points(h)) => el = el.h(px(*h)),
        Some(Length::Keyword(_)) => el = el.h_full(),
        None => {}
    }
    if let Some(min) = style.min_width {
        el = el.min_w(px(min));
    }
    if let Some(max) = style.max_width {
        el = el.max_w(px(max));
    }
    if let Some(grow) = style.grow {
        el = el.flex_grow(grow).flex_basis(px(0.0)).min_w_0();
    }
    el = match style.align {
        Some(Align::Start) => el.items_start(),
        Some(Align::Center) => el.items_center(),
        Some(Align::End) => el.items_end(),
        Some(Align::Stretch) => el.items_stretch(),
        None => el,
    };
    el = match style.justify {
        Some(Justify::Start) => el.justify_start(),
        Some(Justify::Center) => el.justify_center(),
        Some(Justify::End) => el.justify_end(),
        Some(Justify::Between) => el.justify_between(),
        Some(Justify::Around) => el.justify_around(),
        None => el,
    };
    if let Some(opacity) = style.opacity {
        el = el.opacity(opacity);
    }
    el
}

/// An agent-chosen glyph: an emoji, a built-in icon, or an image the app
/// ships in its folder.
pub(crate) fn icon_element(icon: &Icon, size: f32, color: Rgba, folder: Option<&std::path::Path>) -> AnyElement {
    match icon {
        Icon::Emoji(text) => div()
            .text_size(px(size * 0.95))
            .line_height(px(size))
            .child(text.clone())
            .into_any_element(),
        Icon::Symbol(name) => {
            if diri_ui::IconName::from_system_name(name).is_some() {
                sf_symbol(name, size, color)
            } else {
                sf_symbol("square.grid.2x2", size, color)
            }
        }
        Icon::Asset(file) => match folder {
            Some(folder) => img(folder.join(file)).size(px(size)).into_any_element(),
            None => sf_symbol("square.grid.2x2", size, color),
        },
    }
}

fn tick_box(on: bool, accent: Rgba, colors: SemanticColors) -> gpui::Div {
    div()
        .size(px(16.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.0))
        .when(on, |el| el.bg(accent).child(sf_symbol("checkmark", 10.0, gpui::white().into())))
        .when(!on, |el| {
            el.border(px(1.5)).border_color(alpha(colors.primary, 0.3))
        })
}

/// Vector shapes, painted directly: no image, no web view. The canvas
/// scales to the width it is given and keeps its height.
fn canvas_element(width: f32, height: f32, shapes: Vec<Shape>, ctx: &Ctx) -> AnyElement {
    let colors = ctx.colors;
    let accent = ctx.accent;
    let default = colors.secondary;
    gpui::canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let sx = f32::from(bounds.size.width) / width.max(1.0);
            let sy = f32::from(bounds.size.height) / height.max(1.0);
            let at = |x: f32, y: f32| {
                gpui::point(bounds.origin.x + px(x * sx), bounds.origin.y + px(y * sy))
            };
            let paint = |color: &Option<Color>| {
                color
                    .as_ref()
                    .map_or(default, |color| render::color(color, colors, accent))
            };
            for shape in &shapes {
                match shape {
                    Shape::Rect { x, y, w, h, color, radius } => {
                        let rect = gpui::Bounds::new(at(*x, *y), gpui::size(px(w * sx), px(h * sy)));
                        window.paint_quad(gpui::quad(
                            rect,
                            px(radius.unwrap_or(0.0)),
                            paint(color),
                            px(0.0),
                            gpui::transparent_black(),
                            gpui::BorderStyle::default(),
                        ));
                    }
                    Shape::Circle { x, y, r, color } => {
                        let scale = sx.min(sy);
                        let rect = gpui::Bounds::new(
                            at(*x - r * scale / sx, *y - r * scale / sy),
                            gpui::size(px(2.0 * r * scale), px(2.0 * r * scale)),
                        );
                        window.paint_quad(gpui::quad(
                            rect,
                            px(r * scale),
                            paint(color),
                            px(0.0),
                            gpui::transparent_black(),
                            gpui::BorderStyle::default(),
                        ));
                    }
                    Shape::Line { points, color, width } => {
                        if points.len() < 2 {
                            continue;
                        }
                        let mut path = gpui::PathBuilder::stroke(px(width.unwrap_or(1.5)));
                        path.move_to(at(points[0][0], points[0][1]));
                        for point in &points[1..] {
                            path.line_to(at(point[0], point[1]));
                        }
                        if let Ok(path) = path.build() {
                            window.paint_path(path, paint(color));
                        }
                    }
                    Shape::Area { points, color } => {
                        if points.len() < 3 {
                            continue;
                        }
                        let mut path = gpui::PathBuilder::fill();
                        path.move_to(at(points[0][0], points[0][1]));
                        for point in &points[1..] {
                            path.line_to(at(point[0], point[1]));
                        }
                        path.close();
                        if let Ok(path) = path.build() {
                            window.paint_path(path, paint(color));
                        }
                    }
                }
            }
        },
    )
    .w(px(width))
    .max_w_full()
    .h(px(height))
    .flex_none()
    .into_any_element()
}

impl SurfaceView {
    /// A media card: a thumbnail (an https picture or a file the app
    /// ships), a corner badge, and the title; a click opens `url` through
    /// the same gate as any link.
    #[allow(clippy::too_many_arguments)]
    fn media(
        &mut self,
        node_id: &str,
        image: Option<&str>,
        title: &str,
        subtitle: Option<&str>,
        badge_text: Option<&str>,
        url: Option<&str>,
        aspect: f32,
        ctx: &Ctx,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = ctx.colors;
        let picture = image.map(|src| self.picture_path(src, cx));
        let thumb = div()
            .relative()
            .w_full()
            .aspect_ratio(aspect)
            .rounded(px(ctx.radius))
            .overflow_hidden()
            .bg(alpha(colors.primary, 0.08))
            .child(match picture.flatten() {
                Some(path) => img(path)
                    .size_full()
                    .object_fit(gpui::ObjectFit::Cover)
                    .into_any_element(),
                None => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(sf_symbol("photo", 20.0, colors.tertiary))
                    .into_any_element(),
            })
            .when_some(badge_text.map(str::to_owned), |el, text| {
                el.child(
                    div()
                        .absolute()
                        .right(px(6.0))
                        .bottom(px(6.0))
                        .px(px(5.0))
                        .py(px(1.0))
                        .rounded(px(4.0))
                        .bg(gpui::rgba(0x000000c0))
                        .text_size(px(11.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(gpui::white())
                        .child(text),
                )
            });
        let url = url.map(str::to_owned);
        let surface = ctx.surface.clone();
        div()
            .id(element_id(&ctx.surface, "media", node_id))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .when(url.is_some(), |el| el.cursor_pointer())
            .when_some(url, |el, url| {
                el.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.open_link(&surface, &url, cx)))
            })
            .child(thumb)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(px(13.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.primary)
                            .line_height(px(19.0))
                            .child(title.to_owned()),
                    )
                    .when_some(subtitle.map(str::to_owned), |el, subtitle| {
                        el.child(div().text_size(px(12.0)).text_color(colors.tertiary).truncate().child(subtitle))
                    }),
            )
            .into_any_element()
    }

    /// A picture on disk: a file the app ships, or a web picture once the
    /// shared cache has it (fetched off the main thread, then redrawn).
    fn picture_path(&mut self, src: &str, cx: &mut Context<Self>) -> Option<std::path::PathBuf> {
        if !src.starts_with("https://") {
            let surface = self.shown_surface(cx)?;
            let folder = self.model.read(cx).store()?.folder_for(&surface.id).ok()?;
            return Some(folder.join(src));
        }
        let cache = web_image::cache_dir()?;
        let (state, start) = self.images.look(src, &cache);
        if start {
            let url = src.to_owned();
            cx.spawn(async move |this, cx| {
                let file = cx
                    .background_executor()
                    .spawn({
                        let url = url.clone();
                        async move { web_image::fetch(&cache, &url) }
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.images.landed(&url, file);
                    cx.notify();
                });
            })
            .detach();
        }
        match state {
            WebImage::Ready(path) => Some(path),
            _ => None,
        }
    }

    /// A one-line field. Clicking starts editing; Enter sends the text to
    /// the app (or keeps it, for a plain surface); Escape drops it.
    fn input_field(&mut self, node_id: &str, value: &str, placeholder: &str, ctx: &Ctx, cx: &mut Context<Self>) -> AnyElement {
        let colors = ctx.colors;
        let editing = self
            .input
            .as_ref()
            .is_some_and(|input| input.surface == ctx.surface && input.id == node_id);
        let (surface, id, start) = (ctx.surface.clone(), node_id.to_owned(), value.to_owned());
        let field: AnyElement = if editing {
            let input = self.input.as_ref().expect("editing");
            crate::text_input::render(
                input.draft.clone(),
                &input.composition,
                SharedString::from(placeholder.to_owned()),
                crate::text_input::Style {
                    colors,
                    selection: alpha(ctx.accent, 0.35),
                },
                self.focus.clone(),
                InputOwner {
                    view: cx.entity().downgrade(),
                    epoch: input.composition.epoch,
                },
            )
        } else {
            div()
                .text_size(px(13.0))
                .text_color(if value.is_empty() { colors.tertiary } else { colors.primary })
                .truncate()
                .child(if value.is_empty() { placeholder.to_owned() } else { value.to_owned() })
                .into_any_element()
        };
        div()
            .id(element_id(&ctx.surface, "input", node_id))
            .debug_selector(|| format!("surface-input-{node_id}"))
            .h(px(30.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .rounded(px(Radius::ROW))
            .bg(alpha(colors.primary, 0.05))
            .border_1()
            .border_color(if editing { ctx.accent } else { alpha(colors.primary, 0.1) })
            .cursor_text()
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                let mut draft = QueryEditor::default();
                draft.insert(&start);
                this.input = Some(InputEdit {
                    surface: surface.clone(),
                    id: id.clone(),
                    draft,
                    composition: Composition::default(),
                });
                window.focus(&this.focus, cx);
                cx.notify();
            }))
            .child(field)
            .into_any_element()
    }

    fn input_key(&mut self, event: &gpui::KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.input.as_mut() else { return };
        match event.keystroke.key.as_str() {
            "enter" => {
                let input = self.input.take().expect("editing");
                let text = input.draft.text().to_owned();
                self.interact(&input.surface, Interaction::Text { id: input.id, value: text }, cx);
                cx.stop_propagation();
            }
            "escape" => {
                self.input = None;
                cx.stop_propagation();
            }
            _ => {
                match crate::query_editor::edit_for(&event.keystroke) {
                    Some(crate::query_editor::Edit::Local(edit)) => {
                        input.draft.apply(edit);
                        cx.stop_propagation();
                    }
                    Some(crate::query_editor::Edit::Clipboard(edit)) => {
                        use crate::query_editor::ClipboardEdit;
                        match edit {
                            ClipboardEdit::Copy => crate::query_editor::copy_selection(&input.draft, cx),
                            ClipboardEdit::Cut => {
                                crate::query_editor::cut_selection(&mut input.draft, cx);
                            }
                            ClipboardEdit::Paste => {
                                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                                    input.draft.insert(&text);
                                }
                            }
                        }
                        cx.stop_propagation();
                    }
                    None => {}
                }
            }
        }
        cx.notify();
    }
}

#[derive(Clone)]
struct InputOwner {
    view: gpui::WeakEntity<SurfaceView>,
    epoch: u64,
}

impl crate::text_input::Owner for InputOwner {
    fn read<T>(
        &self,
        window: &mut Window,
        cx: &mut gpui::App,
        f: impl FnOnce(&QueryEditor, &Composition, &Window) -> T,
    ) -> Option<T> {
        let view = self.view.upgrade()?;
        let view = view.read(cx);
        let input = view.input.as_ref().filter(|input| input.composition.epoch == self.epoch)?;
        Some(f(&input.draft, &input.composition, window))
    }

    fn edit(
        &self,
        window: &mut Window,
        cx: &mut gpui::App,
        edit: impl FnOnce(&mut Composition, &mut QueryEditor),
    ) {
        let epoch = self.epoch;
        let _ = self.view.update(cx, |view, cx| {
            if let Some(input) = view.input.as_mut()
                && input.composition.epoch == epoch
            {
                edit(&mut input.composition, &mut input.draft);
                window.invalidate_character_coordinates();
                cx.notify();
            }
        });
    }
}

impl Render for SurfaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.model.update(cx, |model, cx| model.sync(cx));
        let colors = self.colors();
        let dark = matches!(colors.appearance, diri_ui::Appearance::Dark);
        let _ = window;
        let Some(doc) = self.shown_surface(cx) else {
            return div()
                .id("surface-view")
                .size_full()
                .bg(if self.mode == Mode::Page {
                    colors.work_surface_nested()
                } else {
                    gpui::transparent_black().into()
                })
                .child(self.render_empty(colors))
                .into_any_element();
        };
        self.model.update(cx, |model, cx| model.set_dark(dark, cx));
        let app = self.model.read(cx).get(&doc.id).and_then(|s| s.app.clone());
        let folder = self
            .model
            .read(cx)
            .store()
            .and_then(|store| store.folder_for(&doc.id).ok());
        let compact_style = doc.style.density == diri_proto::surface::identity::Density::Compact;
        let ctx = Ctx {
            colors,
            surface: doc.id.clone(),
            compact: self.mode == Mode::Panel,
            dark,
            accent: render::accent_of(doc.accent.as_deref()),
            folder,
            radius: doc.style.radius.unwrap_or(Radius::CARD),
            scale: if compact_style { 0.75 } else { 1.0 },
        };
        if app.is_some() {
            self.schedule_sync(&doc.id, cx);
        }
        let siblings = match &self.target {
            Target::Session(session) => self.model.read(cx).for_session(&session.0),
            Target::Surface(_) => Vec::new(),
        };
        let picker = (siblings.len() > 1).then(|| self.render_picker(&siblings, &doc.id, colors, cx));
        let header = self.render_header(&doc, app.as_ref(), &ctx, cx);
        let body = self.children(&doc.root, &doc, &ctx, cx);
        // Drop drawings and Markdown this surface no longer shows.
        if self.markdown.len() > 256 {
            self.markdown.clear();
        }
        let page = self.mode == Mode::Page;
        let column = div()
            .w_full()
            .when(page, |el| el.max_w(px(PAGE_MEASURE)))
            .flex()
            .flex_col()
            .gap(px(if page { 18.0 } else { 12.0 } * ctx.scale))
            .children(picker)
            .child(header)
            .children(body);
        let tinted = doc.style.canvas == diri_proto::surface::identity::Canvas::Tinted;
        let canvas_fill = if tinted {
            diri_ui::composite(alpha(ctx.accent, 0.07), colors.work_surface_nested())
        } else {
            colors.work_surface_nested()
        };
        div()
            .id("surface-view")
            .debug_selector(|| "surface-view".into())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::input_key))
            .size_full()
            .overflow_y_scroll()
            .when(page || tinted, |el| el.bg(canvas_fill))
            .font_family(crate::fonts::ui_family())
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .when(page, |el| el.px(px(40.0)).pt(px(40.0)).pb(px(120.0)))
                    .when(!page, |el| el.px(px(14.0)).pt(px(12.0)).pb(px(40.0)))
                    .child(column),
            )
            .into_any_element()
    }
}
