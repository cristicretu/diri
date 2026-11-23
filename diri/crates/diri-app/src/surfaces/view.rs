//! Draws one Live Surface natively: the header (title, freshness, "Ask
//! agent", save and pin) and the component tree.
//!
//! The view owns only presentation state (the open tab of each `tabs`,
//! parsed Markdown, drawn diagrams, fetched pictures). Everything the person
//! changes goes through [`SurfacesModel`], which keeps it on disk and tells
//! the agent; everything live is read from the session store at draw time,
//! so a surface stays current with no agent running and no timer ticking.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use diri_proto::SessionId;
use diri_proto::surface::node::{ButtonStyle, Cell, ChecklistItem, Trend};
use diri_proto::surface::{Action, Interaction, Node, NodeKind, SurfaceDoc, Tone};
use diri_ui::{AgentLogo, Fill, Ink, Radius, SemanticColors};
use gpui::{
    AnyElement, ClickEvent, Context, Entity, FontWeight, MouseButton, MouseDownEvent, Render,
    Rgba, SharedString, Task, Window, div, img, prelude::*, px,
};

use super::SurfacesModel;
use super::render::{self, Resolved};
use crate::i18n::{t, tf};
use crate::icons::sf_symbol;
use crate::markdown::MarkdownDocument;
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
}

/// What a draw needs besides the view.
struct Ctx {
    colors: SemanticColors,
    surface: String,
    compact: bool,
    dark: bool,
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
        }
    }

    pub(crate) fn target(&self) -> &Target {
        &self.target
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

    fn render_header(&self, doc: &Arc<SurfaceDoc>, ctx: &Ctx, cx: &mut Context<Self>) -> AnyElement {
        let colors = ctx.colors;
        let page = self.mode == Mode::Page;
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
        let mut actions = div().flex().items_center().gap(px(6.0)).flex_none();
        if doc.owner.is_some() {
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
        div()
            .flex()
            .items_start()
            .gap(px(12.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(if page { 4.0 } else { 2.0 }))
                    .child(
                        div()
                            .text_size(px(if page { 26.0 } else { 15.0 }))
                            .line_height(px(if page { 32.0 } else { 20.0 }))
                            .font_weight(if page { FontWeight::BOLD } else { FontWeight::SEMIBOLD })
                            .text_color(colors.primary)
                            .child(doc.title.clone()),
                    )
                    .when_some(doc.subtitle.clone(), |el, subtitle| {
                        el.child(
                            div()
                                .text_size(px(if page { 14.0 } else { 12.5 }))
                                .text_color(colors.secondary)
                                .child(subtitle),
                        )
                    })
                    .child(
                        div()
                            .pt(px(2.0))
                            .text_size(px(11.5))
                            .text_color(colors.tertiary)
                            .child(meta.join("  ·  ")),
                    ),
            )
            .child(actions)
            .into_any_element()
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

    fn frame(colors: SemanticColors) -> gpui::Div {
        div()
            .rounded(px(Radius::CARD))
            .bg(alpha(colors.primary, 0.035))
            .border_1()
            .border_color(alpha(colors.primary, 0.075))
    }

    /// Dispatch only: the recursive path (containers) and the leaves live in
    /// separate frames, so a deep surface costs a small frame per level.
    fn node(&mut self, node: &Node, doc: &Arc<SurfaceDoc>, ctx: &Ctx, cx: &mut Context<Self>) -> AnyElement {
        if node.is_container() || matches!(node.kind, NodeKind::Tabs { .. }) {
            self.container(node, doc, ctx, cx)
        } else {
            self.leaf(node, doc, ctx, cx)
        }
    }

    #[inline(never)]
    fn container(&mut self, node: &Node, doc: &Arc<SurfaceDoc>, ctx: &Ctx, cx: &mut Context<Self>) -> AnyElement {
        let colors = ctx.colors;
        let node_id = node.id.clone().unwrap_or_default();
        match &node.kind {
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
                Self::frame(colors)
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
            } => {
                let live = bind.as_ref().and_then(|bind| self.resolve(bind, cx));
                let (text, color) = match live {
                    Some(resolved) => (resolved.text, render::tone_color(resolved.tone, colors)),
                    None => (
                        text.clone(),
                        tone.map_or(colors.primary, |tone| render::tone_color(tone, colors)),
                    ),
                };
                div()
                    .text_size(px(13.5))
                    .line_height(px(20.0))
                    .text_color(color)
                    .when(*mono, |el| el.font_family(crate::fonts::mono_family()).text_size(px(12.5)))
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
                Self::frame(colors)
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
                let fill = tone.map_or(crate::notes::editor_view::accent(), |tone| {
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
                let mut table = Self::frame(colors).flex().flex_col().overflow_hidden();
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
                                        el.cursor_pointer().text_color(crate::notes::editor_view::accent())
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
            NodeKind::Link { url, label, detail } => {
                self.link_row(&node_id, url, label.as_deref(), detail.as_deref(), ctx, cx)
            }
            NodeKind::Button {
                label,
                action,
                style,
            } => {
                let accent = crate::notes::editor_view::accent();
                let (bg, hover, fg) = match style {
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
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.act(&surface, &id, &action, cx)
                    }))
                    .child(label.clone())
                    .into_any_element()
            }
            NodeKind::Toggle { label, default, .. } => {
                let on = doc.toggle_value(&node_id, *default);
                let accent = crate::notes::editor_view::accent();
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
                ..
            } => {
                let value = doc
                    .select_value(&node_id, default.as_deref())
                    .map(str::to_owned);
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
            .when(link.is_some(), |el| el.cursor_pointer().text_color(crate::notes::editor_view::accent()))
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
        let accent = crate::notes::editor_view::accent();
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
                render::link_look(url, None, Some(&store))
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
                .child(sf_symbol(look.icon, 11.0, colors.secondary))
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

    fn link_row(
        &mut self,
        node_id: &str,
        url: &str,
        label: Option<&str>,
        detail: Option<&str>,
        ctx: &Ctx,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = ctx.colors;
        let look = {
            let store = self.runtime.store.read().expect("store");
            render::link_look(url, label, Some(&store))
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
                    .child(sf_symbol(look.icon, 14.0, colors.primary)),
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
                            .child(badge(look.badge.to_owned(), Tone::Muted, colors)),
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
        Self::frame(colors)
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
            None => Self::frame(colors)
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
        let ctx = Ctx {
            colors,
            surface: doc.id.clone(),
            compact: self.mode == Mode::Panel,
            dark,
        };
        let siblings = match &self.target {
            Target::Session(session) => self.model.read(cx).for_session(&session.0),
            Target::Surface(_) => Vec::new(),
        };
        let picker = (siblings.len() > 1).then(|| self.render_picker(&siblings, &doc.id, colors, cx));
        let header = self.render_header(&doc, &ctx, cx);
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
            .gap(px(if page { 18.0 } else { 12.0 }))
            .children(picker)
            .child(header)
            .children(body);
        div()
            .id("surface-view")
            .debug_selector(|| "surface-view".into())
            .size_full()
            .overflow_y_scroll()
            .when(page, |el| el.bg(colors.work_surface_nested()))
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
