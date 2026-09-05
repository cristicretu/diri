//! Usage settings use the existing ledger and settings shell.
use super::*;
use crate::usage::dashboard::{UsageReport, date_label};
use crate::usage::{UsageFormat, UsageSnapshot};
use gpui::relative;

const PROVIDERS: [&str; 2] = ["Claude Code", "Codex"];
fn provider_color(provider: usize, colors: SemanticColors) -> Rgba {
    if provider == 0 {
        rgba(0xcf876dff)
    } else {
        colors.secondary
    }
}

impl UtilitySurfaces {
    pub(crate) fn set_usage(&mut self, usage: UsageSnapshot, cx: &mut Context<Self>) {
        self.usage = usage;
        if self.surface == Surface::Settings && self.settings_tab == SettingsTab::Usage {
            cx.notify();
        }
    }

    pub(super) fn usage_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.settings_colors();
        if self.usage.updated_at == 0 {
            return settings_page("Usage", div().flex().flex_col().gap(px(8.0)).py(px(24.0))
                .child(label("Reading local usage…", 14.0, colors.primary))
                .child(label("Preparing costs and token history from your Claude Code and Codex transcripts.", 12.0, colors.secondary)), colors).into_any_element();
        }
        let report = self
            .usage
            .history
            .report(self.usage.updated_at, self.usage_days);
        let total = report.total.totals();
        let loaded = self.usage.updated_at > 0;
        let subtitle = if loaded {
            format!(
                "{} — {} · UTC · Local transcripts",
                date_label(report.days[0].day),
                date_label(report.days.last().unwrap().day)
            )
        } else {
            "Reading local usage…".to_owned()
        };
        let mut ranges = div().flex().gap(px(3.0));
        for days in [7, 30, 90] {
            ranges = ranges.child(
                usage_control(
                    format!("usage-range-{days}"),
                    format!("{days} days"),
                    self.usage_days == days,
                    colors,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.usage_days = days;
                    cx.notify();
                })),
            );
        }
        let mut providers = div().flex().flex_col().gap(px(12.0));
        for (index, provider) in report.providers.iter().enumerate() {
            let share = ratio(provider.tokens.c, total.cost);
            providers = providers.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(5.0))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .gap(px(12.0))
                            .child(provider_label(index, colors))
                            .child(label(money(provider.tokens.c), 12.0, colors.primary)),
                    )
                    .child(
                        div()
                            .h(px(3.0))
                            .w_full()
                            .rounded(px(2.0))
                            .bg(colors.primary.alpha(0.06))
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(share as f32))
                                    .rounded(px(2.0))
                                    .bg(provider_color(index, colors)),
                            ),
                    )
                    .child(label(
                        format!(
                            "{:.1}% of cost · {} tokens",
                            share * 100.0,
                            UsageFormat::tokens(provider.totals().total_tokens())
                        ),
                        11.0,
                        colors.tertiary,
                    )),
            );
        }
        let hero = div()
            .flex()
            .flex_wrap()
            .gap(px(28.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(16.0))
                    .w(px(220.0))
                    .flex_grow(1.0)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(label("Estimated API cost", 12.0, colors.secondary))
                            .child(
                                label(
                                    if loaded {
                                        money(total.cost)
                                    } else {
                                        "—".into()
                                    },
                                    34.0,
                                    colors.primary,
                                )
                                .font_weight(FontWeight::MEDIUM),
                            )
                            .child(label(
                                "At model rates, not your subscription bill",
                                11.0,
                                colors.tertiary,
                            )),
                    )
                    .child(providers),
            )
            .child(self.usage_chart(&report, colors, cx));
        let input = total.input_tokens + total.cache_read_tokens + total.cache_write_tokens;
        let metrics = div()
            .flex()
            .flex_wrap()
            .gap(px(1.0))
            .rounded(px(8.0))
            .overflow_hidden()
            .bg(colors.primary.alpha(0.06))
            .child(metric(
                "Processed tokens",
                UsageFormat::tokens(total.total_tokens()),
                format!(
                    "{} per active day",
                    UsageFormat::tokens(total.total_tokens() / report.active_days.max(1) as i64)
                ),
                colors,
            ))
            .child(metric(
                "Cached input",
                UsageFormat::tokens(total.cache_read_tokens),
                format!(
                    "{:.1}% of input",
                    ratio(total.cache_read_tokens as f64, input as f64) * 100.0
                ),
                colors,
            ))
            .child(metric(
                "Uncached input",
                UsageFormat::tokens(total.input_tokens),
                format!(
                    "{} cache writes",
                    UsageFormat::tokens(total.cache_write_tokens)
                ),
                colors,
            ))
            .child(metric(
                "Output",
                UsageFormat::tokens(total.output_tokens),
                format!(
                    "{} reasoning reported",
                    UsageFormat::tokens(report.total.reasoning)
                ),
                colors,
            ))
            .child(metric(
                "Cache read savings",
                money(report.total.read_savings),
                "Estimated · excludes writes".into(),
                colors,
            ));
        let mut content = div().flex().flex_col().gap(px(24.0)).child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap(px(12.0))
                .child(label(subtitle, 11.0, colors.secondary))
                .child(ranges),
        );
        if loaded && total.total_tokens() == 0 {
            content = content.child(div().p(px(20.0)).rounded(px(8.0)).bg(colors.primary.alpha(0.035)).flex().flex_col().gap(px(6.0))
                .child(label("Your usage starts with a conversation", 14.0, colors.primary))
                .child(label("Use Claude Code or Codex on this Mac. Available transcript history appears here automatically.", 12.0, colors.secondary))
                .child(label("Try a longer date range to see earlier activity.", 12.0, colors.secondary)));
        }
        content = content.child(hero).child(metrics).child(self.usage_breakdown(&report, colors, cx))
            .child(div().flex().flex_col().gap(px(7.0))
                .child(label("About these estimates", 12.0, colors.primary).font_weight(FontWeight::MEDIUM))
                .child(label(format!("{:.1}% of tokens model priced · {} unpriced tokens · No provider billing data", ratio(report.total.priced_tokens as f64, total.total_tokens() as f64) * 100.0, UsageFormat::tokens(total.total_tokens() - report.total.priced_tokens)), 11.0, colors.secondary))
                .child(label("Uses Diri’s bundled model rates. Unpriced usage is excluded from cost. Cache read savings compare cached reads with uncached input rates; cache write premiums are excluded.", 11.0, colors.tertiary))
                .child(label("Updates automatically from available local Claude Code and Codex transcripts, including sessions outside Diri. Remote usage is not included in this detailed view.", 11.0, colors.tertiary)));
        settings_page("Usage", content, colors).into_any_element()
    }

    fn usage_chart(
        &self,
        report: &UsageReport,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tokens = self.usage_tokens;
        let value = |detail: crate::usage::dashboard::UsageDetail| {
            if tokens {
                detail.totals().total_tokens() as f64
            } else {
                detail.tokens.c
            }
        };
        let maximum = report
            .days
            .iter()
            .map(|day| value(day.total()))
            .fold(0.0_f64, f64::max);
        let mut bars = div()
            .flex()
            .items_end()
            .gap(px(if self.usage_days == 90 { 2.0 } else { 4.0 }))
            .h(px(156.0))
            .w_full()
            .border_b_1()
            .border_color(colors.primary.alpha(0.12));
        for day in &report.days {
            let mut bar = div()
                .id(SharedString::from(format!("usage-day-{}", day.day)))
                .flex_1()
                .min_w(px(0.0))
                .h_full()
                .flex()
                .flex_col()
                .justify_end()
                .rounded_t(px(2.0))
                .hover(|style| style.bg(colors.primary.alpha(0.06)));
            let tooltip = format!(
                "{} · {} · {} tokens",
                date_label(day.day),
                money(day.total().tokens.c),
                UsageFormat::tokens(day.total().totals().total_tokens())
            );
            bar =
                bar.tooltip(move |_, cx| cx.new(|_| UsageTooltip(tooltip.clone(), colors)).into());
            for provider in (0..2).rev() {
                bar = bar.child(
                    div()
                        .w_full()
                        .h(px(
                            (ratio(value(day.providers[provider]), maximum) * 148.0) as f32
                        ))
                        .flex_none()
                        .bg(provider_color(provider, colors)),
                );
            }
            bars = bars.child(bar);
        }
        let bars = if cx.reduce_motion() {
            bars.into_any_element()
        } else {
            bars.with_animation(
                SharedString::from(format!("usage-chart-{}-{tokens}", self.usage_days)),
                Animation::new(Duration::from_millis(160)).with_easing(ease_out_quint()),
                |bars, progress| bars.opacity(0.5 + 0.5 * progress),
            )
            .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .w(px(340.0))
            .min_w(px(240.0))
            .flex_grow(1.0)
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        label(
                            if tokens { "Daily tokens" } else { "Daily cost" },
                            13.0,
                            colors.primary,
                        )
                        .font_weight(FontWeight::MEDIUM),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(3.0))
                            .child(
                                usage_control("usage-cost", "Cost", !tokens, colors).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.usage_tokens = false;
                                        cx.notify();
                                    }),
                                ),
                            )
                            .child(
                                usage_control("usage-tokens", "Tokens", tokens, colors).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.usage_tokens = true;
                                        cx.notify();
                                    }),
                                ),
                            ),
                    ),
            )
            .child(label(
                format!(
                    "Peak {}",
                    if tokens {
                        UsageFormat::tokens(maximum as i64)
                    } else {
                        money(maximum)
                    }
                ),
                10.0,
                colors.tertiary,
            ))
            .child(bars)
            .child(
                div()
                    .flex()
                    .justify_between()
                    .child(label(date_label(report.days[0].day), 10.0, colors.tertiary))
                    .child(label(
                        date_label(report.days.last().unwrap().day),
                        10.0,
                        colors.tertiary,
                    )),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(12.0))
                    .child(provider_label(0, colors))
                    .child(provider_label(1, colors)),
            )
    }

    fn usage_breakdown(
        &self,
        report: &UsageReport,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut table = div().flex().flex_col().child(table_row(
            "",
            if self.usage_by_day {
                "Day (UTC)"
            } else {
                "Model"
            },
            "Cost",
            "Share",
            "Tokens",
            colors,
        ));
        if self.usage_by_day {
            for day in report.days.iter().rev() {
                let total = day.total().totals();
                table = table.child(table_row(
                    "",
                    &date_label(day.day),
                    &money(total.cost),
                    &format!("{:.1}%", ratio(total.cost, report.total.tokens.c) * 100.0),
                    &UsageFormat::tokens(total.total_tokens()),
                    colors,
                ));
            }
        } else {
            for row in &report.models {
                let total = row.detail.totals();
                table = table.child(table_row(
                    PROVIDERS[row.provider],
                    &row.model,
                    &if row.detail.priced_tokens == 0 {
                        "Unpriced".into()
                    } else {
                        money(total.cost)
                    },
                    &format!("{:.1}%", ratio(total.cost, report.total.tokens.c) * 100.0),
                    &UsageFormat::tokens(total.total_tokens()),
                    colors,
                ));
            }
        }
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(label("Breakdown", 13.0, colors.primary).font_weight(FontWeight::MEDIUM))
                    .child(
                        div()
                            .flex()
                            .gap(px(3.0))
                            .child(
                                usage_control("usage-model", "Model", !self.usage_by_day, colors)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.usage_by_day = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                usage_control("usage-by-day", "Day", self.usage_by_day, colors)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.usage_by_day = true;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(table)
    }
}

fn money(value: f64) -> String {
    format!("${value:.2}")
}
fn ratio(value: f64, total: f64) -> f64 {
    if total > 0.0 {
        (value / total).clamp(0.0, 1.0)
    } else {
        0.0
    }
}
fn label(text: impl Into<SharedString>, size: f32, color: Rgba) -> gpui::Div {
    div()
        .text_size(px(size))
        .text_color(color)
        .child(text.into())
}
fn usage_control(
    id: impl Into<SharedString>,
    text: impl Into<SharedString>,
    selected: bool,
    colors: SemanticColors,
) -> gpui::Stateful<gpui::Div> {
    let id = id.into();
    div()
        .id(id.clone())
        .debug_selector(move || id.to_string())
        .px(px(10.0))
        .h(px(27.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .border_1()
        .border_color(colors.primary.alpha(if selected { 0.1 } else { 0.0 }))
        .bg(colors.primary.alpha(if selected { 0.065 } else { 0.0 }))
        .text_size(px(11.0))
        .text_color(if selected {
            colors.primary
        } else {
            colors.secondary
        })
        .cursor_pointer()
        .hover(|style| style.bg(colors.primary.alpha(0.09)))
        .active(|style| style.bg(colors.primary.alpha(0.13)))
        .child(text.into())
}
fn provider_label(provider: usize, colors: SemanticColors) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .size(px(6.0))
                .rounded(px(3.0))
                .bg(provider_color(provider, colors)),
        )
        .child(label(PROVIDERS[provider], 11.0, colors.secondary))
}
fn metric(title: &str, value: String, detail: String, colors: SemanticColors) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(5.0))
        .p(px(12.0))
        .w(px(170.0))
        .flex_grow(1.0)
        .bg(colors.background)
        .child(label(title.to_owned(), 11.0, colors.secondary))
        .child(label(value, 20.0, colors.primary))
        .child(label(detail, 10.0, colors.tertiary))
}
fn table_row(
    provider: &str,
    name: &str,
    cost: &str,
    share: &str,
    tokens: &str,
    colors: SemanticColors,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(12.0))
        .min_h(px(39.0))
        .py(px(7.0))
        .border_b_1()
        .border_color(colors.primary.alpha(0.055))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(label(name.to_owned(), 12.0, colors.primary).truncate())
                .when(!provider.is_empty(), |row| {
                    row.child(label(provider.to_owned(), 10.0, colors.tertiary))
                }),
        )
        .child(
            label(cost.to_owned(), 12.0, colors.primary)
                .w(px(85.0))
                .text_right(),
        )
        .child(
            label(share.to_owned(), 12.0, colors.tertiary)
                .w(px(54.0))
                .text_right(),
        )
        .child(
            label(tokens.to_owned(), 12.0, colors.secondary)
                .w(px(64.0))
                .text_right(),
        )
}
struct UsageTooltip(String, SemanticColors);
impl Render for UsageTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        label(self.0.clone(), 11.0, self.1.primary)
            .px(px(10.0))
            .py(px(7.0))
            .rounded(px(6.0))
            .bg(self.1.background)
            .border_1()
            .border_color(self.1.primary.alpha(0.12))
    }
}
