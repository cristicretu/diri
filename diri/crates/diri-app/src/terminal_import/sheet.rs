//! "Bring your setup from Ghostty?": one screen that shows the terminal as it
//! will look, every preference that changes (from → to), and what is read
//! but not brought over. Import applies it all at once; Not Now changes
//! nothing.

use std::sync::Arc;

use diri_term::theme::TermTheme;
use diri_ui::{FloatingSurface, Radius, SemanticColors, Typo};
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, KeyDownEvent, MouseButton,
    Rgba, SharedString, Window, div, prelude::*, px,
};

use super::{Plan, Source, ThemeTarget};
use crate::i18n::{t, tf};
use crate::store::{StoreRuntime, CursorShapePref};
use crate::whats_new::sheet_button;

pub(crate) enum TerminalImportEvent {
    Close,
    Apply(Source),
}

pub(crate) struct TerminalImportSheet {
    plans: Vec<Plan>,
    index: usize,
    store: Arc<StoreRuntime>,
    focus: FocusHandle,
}

impl EventEmitter<TerminalImportEvent> for TerminalImportSheet {}

impl Focusable for TerminalImportSheet {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

const WIDTH: f32 = 480.0;
const PAD: f32 = 16.0;
/// Rows of "not imported" shown before the rest fold into "…".
const NOT_IMPORTED_ROWS: usize = 4;

/// What the terminal will look like after the import, for the preview.
struct Look {
    theme: TermTheme,
    spec_colors: Option<(Rgba, Rgba, [Rgba; 16], Option<Rgba>)>,
    family: String,
    size: f32,
    line_height: f32,
    cursor: CursorShapePref,
}

impl TerminalImportSheet {
    pub(crate) fn new(plans: Vec<Plan>, store: Arc<StoreRuntime>, cx: &mut Context<Self>) -> Self {
        Self {
            plans,
            index: 0,
            store,
            focus: cx.focus_handle(),
        }
    }

    pub(crate) fn source(&self) -> Option<Source> {
        self.plans.get(self.index).map(|plan| plan.source)
    }

    fn colors(&self) -> SemanticColors {
        crate::app_theme::colors_in(
            &self
                .store
                .store
                .read()
                .expect("session store lock poisoned"),
        )
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.plans.len() && index != self.index {
            self.index = index;
            cx.notify();
        }
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        if let Some(source) = self.source() {
            cx.emit(TerminalImportEvent::Apply(source));
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.modified() {
            return;
        }
        match keystroke.key.as_str() {
            "escape" => cx.emit(TerminalImportEvent::Close),
            "enter" => self.apply(cx),
            "left" => self.select(self.index.saturating_sub(1), cx),
            "right" => self.select(self.index + 1, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn look(&self, plan: &Plan) -> Look {
        let store = self.store.store.read().expect("session store lock poisoned");
        let prefs = store.preferences();
        let current = crate::settings::theme(&prefs.terminal_theme);
        let (theme, spec_colors) = match &plan.apply.theme {
            Some(ThemeTarget::Existing(id)) => (crate::settings::theme(id), None),
            Some(ThemeTarget::Import(spec)) => (
                current,
                Some((spec.background, spec.foreground, spec.ansi, spec.cursor)),
            ),
            None => (current, None),
        };
        Look {
            theme,
            spec_colors,
            family: crate::fonts::terminal_family(
                plan.apply
                    .font_family
                    .as_deref()
                    .unwrap_or(&prefs.terminal_font_family),
            )
            .to_owned(),
            size: plan.apply.font_size.unwrap_or(prefs.terminal_font_size),
            line_height: plan.apply.line_height.unwrap_or(prefs.terminal_line_height),
            cursor: plan
                .apply
                .cursor_shape
                .unwrap_or(prefs.terminal_cursor_shape),
        }
    }
}

/// A few lines of a shell, drawn in the imported font, colors and cursor.
fn preview(look: &Look, width: f32) -> impl IntoElement {
    let (background, foreground, ansi, cursor) = look.spec_colors.unwrap_or((
        look.theme.background,
        look.theme.foreground,
        look.theme.ansi,
        None,
    ));
    let cursor = cursor.unwrap_or(foreground);
    let line_height = look.size * 1.2 * look.line_height;
    let cell = look.size * 0.6;
    let span = |text: &'static str, color: Rgba| div().text_color(color).child(text);
    let line = || {
        div()
            .h(px(line_height))
            .flex()
            .items_center()
            .whitespace_nowrap()
    };
    let cursor_mark = match look.cursor {
        CursorShapePref::Block => div().w(px(cell)).h(px(look.size * 1.2)).bg(cursor),
        CursorShapePref::Bar => div().w(px(2.0)).h(px(look.size * 1.2)).bg(cursor),
        CursorShapePref::Underline => div()
            .w(px(cell))
            .h(px(look.size * 1.2))
            .flex()
            .items_end()
            .child(div().w_full().h(px(2.0)).bg(cursor)),
    };
    div()
        .id("terminal-import-preview")
        .w(px(width))
        .px(px(14.0))
        .py(px(10.0))
        .rounded(px(Radius::CARD))
        .overflow_hidden()
        .bg(background)
        .font_family(SharedString::from(look.family.clone()))
        .text_size(px(look.size))
        .line_height(px(line_height))
        .text_color(foreground)
        .child(
            line()
                .child(span("~/project", ansi[4]))
                .child(span(" on ", foreground))
                .child(span("main", ansi[5]))
                .child(span(" ❯ ", ansi[2]))
                .child(span("cargo test", foreground)),
        )
        .child(
            line()
                .child(span("ok", ansi[2]))
                .child(span(" 24 passed", foreground))
                .child(span(" · ", ansi[8]))
                .child(span("1 warning", ansi[3]))
                .child(span(" · ", ansi[8]))
                .child(span("0 failed", ansi[1])),
        )
        .child(
            line()
                .child(span("~/project", ansi[4]))
                .child(span(" ❯ ", ansi[2]))
                .child(cursor_mark),
        )
}

fn swatch(colors: [Rgba; 4]) -> impl IntoElement {
    let [background, foreground, red, blue] = colors;
    div()
        .flex_none()
        .w(px(28.0))
        .h(px(14.0))
        .rounded(px(3.0))
        .bg(background)
        .border_1()
        .border_color(gpui::rgba(0x8080803d))
        .flex()
        .items_center()
        .justify_center()
        .gap(px(2.0))
        .children([foreground, red, blue].map(|color| div().size(px(4.0)).rounded_full().bg(color)))
}

/// `~/…` for paths under the home folder.
fn display_path(path: &std::path::Path) -> String {
    let home = diri_platform::home_dir();
    match home.as_deref().and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

impl Render for TerminalImportSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors();
        let Some(plan) = self.plans.get(self.index).cloned() else {
            return div().into_any_element();
        };
        let viewport = window.viewport_size();
        let width = WIDTH.min(f32::from(viewport.width) - 64.0).max(320.0);
        let inner = width - 2.0 * PAD;
        let look = self.look(&plan);

        let sources = (self.plans.len() > 1).then(|| {
            div().flex().items_center().gap(px(4.0)).children(
                self.plans.iter().enumerate().map(|(index, other)| {
                    let selected = index == self.index;
                    div()
                        .id(("terminal-import-source", index))
                        .h(px(22.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .rounded(px(Radius::CHIP))
                        .text_size(px(11.0))
                        .text_color(if selected {
                            colors.primary
                        } else {
                            colors.secondary
                        })
                        .when(selected, |chip| chip.bg(colors.primary.alpha(0.08)))
                        .cursor_pointer()
                        .hover(move |style| style.bg(colors.primary.alpha(0.06)))
                        .child(other.source.name())
                        .on_click(cx.listener(move |this, _, _, cx| this.select(index, cx)))
                }),
            )
        });

        let label_width = 118.0;
        let changes = div().flex().flex_col().children(plan.changes.iter().enumerate().map(
            |(index, change)| {
                div()
                    .id(("terminal-import-change", index))
                    .min_h(px(28.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .when(index > 0, |row| {
                        row.border_t_1().border_color(colors.primary.alpha(0.06))
                    })
                    .text_size(px(12.0))
                    .child(
                        div()
                            .w(px(label_width))
                            .flex_none()
                            .text_color(colors.secondary)
                            .child(change.setting),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .text_color(colors.tertiary)
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .child(change.from.clone()),
                            )
                            .child(div().text_color(colors.tertiary).child("→"))
                            .when_some(change.swatch, |row, colors| row.child(swatch(colors)))
                            .child(
                                div()
                                    .text_color(colors.primary)
                                    .font_weight(FontWeight::MEDIUM)
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .child(change.to.clone()),
                            ),
                    )
            },
        ));

        let mut skipped = plan.not_imported.clone();
        if skipped.len() > NOT_IMPORTED_ROWS {
            skipped.truncate(NOT_IMPORTED_ROWS);
            skipped.push("…".to_owned());
        }
        let not_imported = (!skipped.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(px(Typo::SECTION_HEADER.size))
                        .font_weight(Typo::SECTION_HEADER.weight)
                        .text_color(colors.secondary)
                        .child(t("terminal_import.sheet.not_imported")),
                )
                .children(skipped.into_iter().map(|line| {
                    div()
                        .text_size(px(11.0))
                        .text_color(colors.tertiary)
                        .child(line)
                }))
        });
        let unchanged = (!plan.unchanged.is_empty()).then(|| {
            div()
                .text_size(px(11.0))
                .text_color(colors.tertiary)
                .child(tf(
                    "terminal_import.sheet.unchanged",
                    &[(
                        "settings",
                        &crate::herdr_import::join_list(
                            &plan
                                .unchanged
                                .iter()
                                .map(|setting| setting.to_lowercase())
                                .collect::<Vec<_>>(),
                        ),
                    )],
                ))
        });
        // The config itself, and how many files it pulled in.
        let read_from = plan.files.first().map(|first| {
            let mut files = display_path(first);
            if plan.files.len() > 1 {
                files.push_str(&format!(" +{}", plan.files.len() - 1));
            }
            div()
                .text_size(px(10.0))
                .text_color(colors.tertiary)
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(tf("terminal_import.sheet.read_from", &[("files", &files)]))
        });

        let max_height = (f32::from(viewport.height) - 48.0).max(240.0);
        let sheet = div()
            .id("terminal-import-sheet")
            .debug_selector(|| "terminal-import-sheet".into())
            .w(px(width))
            .max_h(px(max_height))
            .p(px(PAD))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(preview(&look, inner))
            .child(
                div()
                    .px(px(2.0))
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .children(sources)
                    .child(
                        div()
                            .text_size(px(Typo::DISPLAY_TITLE.size))
                            .font_weight(Typo::DISPLAY_TITLE.weight)
                            .text_color(colors.primary)
                            .child(tf(
                                "terminal_import.sheet.title",
                                &[("terminal", &plan.source.name())],
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(colors.secondary)
                            .child(t("terminal_import.sheet.subtitle")),
                    ),
            )
            // A short window scrolls the list, never the buttons.
            .child(
                div()
                    .id("terminal-import-body")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .px(px(2.0))
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(changes)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .children(not_imported)
                            .children(unchanged)
                            .children(read_from),
                    ),
            )
            .child(
                div()
                    .pt(px(2.0))
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        sheet_button(
                            "terminal-import-cancel",
                            t("terminal_import.sheet.cancel").into(),
                            None,
                            false,
                            colors,
                        )
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(TerminalImportEvent::Close))),
                    )
                    .child(
                        sheet_button(
                            "terminal-import-apply",
                            t("terminal_import.sheet.import").into(),
                            Some("↩".to_owned()),
                            true,
                            colors,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
                    ),
            );

        div()
            .id("terminal-import")
            .key_context("TerminalImport")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::rgba(0x00000055))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| {
                    cx.emit(TerminalImportEvent::Close);
                    cx.stop_propagation();
                }),
            )
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(FloatingSurface::new(colors, sheet))
            .into_any_element()
    }
}
