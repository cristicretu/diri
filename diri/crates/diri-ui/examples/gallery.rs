use diri_ui::{
    AgentKind, AgentLogo, AttentionDot, AttentionLevel, BrandMark, BrandMarkKind, Fill,
    FloatingSurface, HairlineDivider, Icon, IconName, IconSize, Ink, Metrics, Motion, Palette,
    Radius, RowFill, SemanticColors, Space, StatusGlyph, StatusState, TextTone, Typo,
};
use gpui::{
    App, AppContext as _, Bounds, Context, Entity, FontWeight, Render, TitlebarOptions, Window,
    WindowBackgroundAppearance, WindowBounds, WindowOptions, div, point, prelude::*, px, size,
};
use gpui_platform::application;

const DARK: SemanticColors = SemanticColors::dark();
const LIGHT: SemanticColors = SemanticColors::light();

struct Gallery {
    status_glyphs: Vec<(AgentKind, StatusState, Entity<StatusGlyph>)>,
}

impl Gallery {
    fn new(cx: &mut Context<Self>) -> Self {
        let samples = [
            (AgentKind::ClaudeCode, StatusState::Working),
            (
                AgentKind::Codex,
                StatusState::NeedsInput { destructive: false },
            ),
            (
                AgentKind::Cursor,
                StatusState::NeedsInput { destructive: true },
            ),
            (AgentKind::Gemini, StatusState::DoneUnseen),
            (AgentKind::ClaudeCode, StatusState::IdleSeen),
            (AgentKind::Codex, StatusState::None),
            (AgentKind::Cursor, StatusState::Hibernated),
            (AgentKind::Shell, StatusState::Working),
        ];
        let status_glyphs = samples
            .into_iter()
            .map(|(kind, state)| {
                let glyph = cx.new(|_| StatusGlyph::new(kind, state, 22.0, DARK));
                (kind, state, glyph)
            })
            .collect();
        Self { status_glyphs }
    }

    fn heading(text: &'static str) -> gpui::Div {
        div()
            .text_size(px(11.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(DARK.text(TextTone::Label))
            .child(text.to_uppercase())
    }

    fn section(title: &'static str, content: impl IntoElement) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(Self::heading(title))
            .child(content)
    }

    fn swatch(label: impl Into<gpui::SharedString>, color: gpui::Rgba) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .child(div().size(px(42.0)).rounded(px(8.0)).bg(color))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(DARK.text(TextTone::Unselected))
                    .child(label.into()),
            )
    }

    fn marks_section() -> gpui::Div {
        let marks = BrandMarkKind::ALL.into_iter().map(|kind| {
            let agent = match kind {
                BrandMarkKind::Claude => AgentKind::ClaudeCode,
                BrandMarkKind::OpenAi => AgentKind::Codex,
                BrandMarkKind::Cursor => AgentKind::Cursor,
                BrandMarkKind::Gemini => AgentKind::Gemini,
                BrandMarkKind::Agent(mark) => AgentKind::Agent(mark),
            };
            let color = Ink::working(agent, DARK);
            div()
                .flex()
                .flex_col()
                .gap(px(9.0))
                .w(px(150.0))
                .child(
                    div()
                        .h(px(52.0))
                        .flex()
                        .items_center()
                        .gap(px(14.0))
                        .child(BrandMark::new(kind, 14.0, color))
                        .child(BrandMark::new(kind, 24.0, color))
                        .child(BrandMark::new(kind, 40.0, color)),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(DARK.text(TextTone::Unselected))
                        .child(kind.label()),
                )
        });
        Self::section(
            "Brand marks · 14 / 24 / 40",
            div().flex().flex_wrap().gap(px(24.0)).children(marks),
        )
    }

    fn logos_section() -> gpui::Div {
        let logos = AgentKind::ALL.into_iter().map(|kind| {
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(7.0))
                .w(px(72.0))
                .child(AgentLogo::new(kind, 34.0, DARK))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(DARK.text(TextTone::Unselected))
                        .child(kind.label()),
                )
        });
        Self::section(
            "AgentLogo · badged",
            div().flex().gap(px(14.0)).children(logos),
        )
    }

    fn icons_section() -> gpui::Div {
        let icons = IconName::ALL.into_iter().map(|name| {
            let label = name
                .asset_path()
                .strip_prefix("icons/")
                .and_then(|path| path.strip_suffix(".svg"))
                .unwrap_or(name.asset_path());
            div()
                .w(px(96.0))
                .h(px(58.0))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .rounded(px(Radius::ROW))
                .bg(Fill::subtle(DARK))
                .child(Icon::new(name, IconSize::LARGE, DARK.secondary))
                .child(
                    div()
                        .text_size(px(9.0))
                        .text_color(DARK.tertiary)
                        .child(label),
                )
        });
        Self::section(
            "Diri Line · embedded SVG icons",
            div().flex().flex_wrap().gap(px(8.0)).children(icons),
        )
    }

    fn status_section(&self) -> gpui::Div {
        let statuses = self.status_glyphs.iter().map(|(kind, state, glyph)| {
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.0))
                .w(px(116.0))
                .child(
                    div()
                        .size(px(40.0))
                        .rounded(px(Radius::BADGE))
                        .bg(Fill::subtle(DARK))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(glyph.clone()),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(DARK.text(TextTone::Unselected))
                        .child(state.label()),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(DARK.tertiary)
                        .child(kind.label()),
                )
        });

        let dots = [
            AttentionLevel::NeedsInput { destructive: false },
            AttentionLevel::NeedsInput { destructive: true },
            AttentionLevel::DoneUnseen,
            AttentionLevel::Working,
            AttentionLevel::IdleSeen,
            AttentionLevel::None,
            AttentionLevel::Hibernated,
        ]
        .into_iter()
        .map(|level| {
            div()
                .size(px(22.0))
                .flex()
                .items_center()
                .justify_center()
                .child(AttentionDot::new(level, DARK))
        });

        Self::section(
            "StatusGlyph · shared absolute phase · 10 fps",
            div()
                .flex()
                .flex_col()
                .gap(px(18.0))
                .child(div().flex().flex_wrap().gap(px(8.0)).children(statuses))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(DARK.tertiary)
                                .child("AttentionDot"),
                        )
                        .children(dots),
                ),
        )
    }

    fn type_section() -> gpui::Div {
        let rows = Typo::ALL.into_iter().map(|(role, style)| {
            div()
                .flex()
                .items_center()
                .h(px(30.0))
                .child(
                    div()
                        .w(px(150.0))
                        .text_size(px(11.0))
                        .text_color(DARK.tertiary)
                        .child(format!("{role:?}")),
                )
                .child(
                    div()
                        .text_size(px(style.size))
                        .font_weight(style.weight)
                        .text_color(DARK.primary)
                        .when(style.monospaced, |text| {
                            text.font_family(".AppleSystemUIFontMonospaced")
                        })
                        .child("The quick brown agent · 0123456789"),
                )
        });
        Self::section(
            "Type ramp · 11 / 13 / 15",
            div().flex().flex_col().children(rows),
        )
    }

    fn radii_and_fills_section() -> gpui::Div {
        let radii = [
            ("chip · 5", Radius::CHIP),
            ("badge · 6", Radius::BADGE),
            ("row · 7", Radius::ROW),
            ("card · 10", Radius::CARD),
            ("panel · 12", Radius::PANEL),
        ]
        .into_iter()
        .map(|(label, radius)| {
            div()
                .flex()
                .flex_col()
                .gap(px(7.0))
                .child(
                    div()
                        .w(px(84.0))
                        .h(px(44.0))
                        .rounded(px(radius))
                        .bg(DARK.primary.alpha(0.12)),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(DARK.tertiary)
                        .child(label),
                )
        });

        let fills = [
            ("clear", RowFill::Clear),
            ("hover · 6%", RowFill::Hover),
            ("multi · 8%", RowFill::MultiSelected),
            ("selected · 10%", RowFill::Selected),
        ]
        .into_iter()
        .map(|(label, fill)| {
            div()
                .w(px(132.0))
                .h(px(Metrics::ROW_HEIGHT))
                .px(px(Space::ROW_H))
                .rounded(px(Radius::ROW))
                .bg(fill.color(DARK))
                .border_1()
                .border_color(DARK.primary.alpha(0.06))
                .flex()
                .items_center()
                .text_size(px(11.0))
                .text_color(DARK.text(TextTone::Label))
                .child(label)
        });

        Self::section(
            "Radii + row fills",
            div()
                .flex()
                .flex_col()
                .gap(px(18.0))
                .child(div().flex().gap(px(12.0)).children(radii))
                .child(div().flex().gap(px(10.0)).children(fills)),
        )
    }

    fn colors_section() -> gpui::Div {
        let inks = [
            ("attention", Ink::ATTENTION),
            ("danger", Ink::DANGER),
            ("fresh", Ink::FRESH),
            ("clay", Palette::CLAY),
            ("gemini", Palette::GEMINI_BLUE),
        ]
        .into_iter()
        .map(|(label, color)| Self::swatch(label, color));

        let semantics = [
            ("light primary", LIGHT.primary),
            ("light secondary", LIGHT.secondary),
            ("light tertiary", LIGHT.tertiary),
            ("dark primary", DARK.primary),
            ("dark secondary", DARK.secondary),
            ("dark tertiary", DARK.tertiary),
        ]
        .into_iter()
        .map(|(label, color)| {
            div()
                .p(px(8.0))
                .rounded(px(8.0))
                .bg(if label.starts_with("light") {
                    LIGHT.background
                } else {
                    DARK.background
                })
                .child(Self::swatch(label, color))
        });

        Self::section(
            "Ink + semantic color",
            div()
                .flex()
                .flex_col()
                .gap(px(16.0))
                .child(div().flex().gap(px(18.0)).children(inks))
                .child(div().flex().gap(px(10.0)).children(semantics)),
        )
    }

    fn surfaces_section() -> gpui::Div {
        Self::section(
            "Shared surfaces",
            div()
                .flex()
                .items_center()
                .gap(px(38.0))
                .child(FloatingSurface::new(
                    DARK,
                    div()
                        .w(px(280.0))
                        .p(px(16.0))
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(DARK.primary)
                                .child("Floating surface"),
                        )
                        .child(HairlineDivider::horizontal(DARK))
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(DARK.text(TextTone::Unselected))
                                .child("12 pt panel · adaptive stroke · 24 pt shadow"),
                        ),
                ))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .text_size(px(11.0))
                        .text_color(DARK.tertiary)
                        .child(format!(
                            "Space  indent {} · rowH {} · inset {}",
                            Space::INDENT,
                            Space::ROW_H,
                            Space::INSET
                        ))
                        .child(format!(
                            "Metrics  title {} · row {} · footer {}",
                            Metrics::TITLE_BAR,
                            Metrics::ROW_HEIGHT,
                            Metrics::NEW_AGENT_FOOTER
                        ))
                        .child(format!(
                            "Motion  snap {:.2}/{:.2} · pop {:.2}/{:.2} · settle {:.2}/{:.2}",
                            Motion::SNAP.response,
                            Motion::SNAP.damping_fraction,
                            Motion::POP.response,
                            Motion::POP.damping_fraction,
                            Motion::SETTLE.response,
                            Motion::SETTLE.damping_fraction,
                        )),
                ),
        )
    }
}

impl Render for Gallery {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("gallery-scroll")
            .size_full()
            .bg(DARK.background)
            .font_family(".SystemUIFont")
            .text_color(DARK.primary)
            .overflow_y_scroll()
            .child(
                div()
                    .w_full()
                    .max_w(px(1120.0))
                    .mx_auto()
                    .pt(px(54.0))
                    .pb(px(80.0))
                    .px(px(34.0))
                    .flex()
                    .flex_col()
                    .gap(px(28.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .text_size(px(15.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("diri · design system gallery"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(DARK.tertiary)
                                    .child("Swift reference parity · GPUI native vectors"),
                            ),
                    )
                    .child(HairlineDivider::horizontal(DARK))
                    .child(Self::marks_section())
                    .child(Self::logos_section())
                    .child(Self::icons_section())
                    .child(self.status_section())
                    .child(Self::type_section())
                    .child(Self::radii_and_fills_section())
                    .child(Self::colors_section())
                    .child(Self::surfaces_section()),
            )
    }
}

fn main() {
    application()
        .with_assets(diri_ui::IconAssets)
        .run(|cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(1180.0), px(860.0)), cx);
            cx.open_window(
                WindowOptions {
                    focus: true,
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(900.0), px(560.0))),
                    window_background: WindowBackgroundAppearance::Opaque,
                    titlebar: Some(TitlebarOptions {
                        title: Some("diri-ui gallery".into()),
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(20.0), px(17.0))),
                    }),
                    ..Default::default()
                },
                |_, cx| cx.new(Gallery::new),
            )
            .expect("failed to open diri-ui gallery");
            cx.activate(true);
        });
}
