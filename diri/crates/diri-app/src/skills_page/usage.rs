//! Settings › Skills › Usage: how often agents ran each skill. Each skill is
//! one ridge of daily runs, most used first, overlapping the row above like a
//! ridgeline plot; installed skills that never ran follow, so the least used
//! end of the catalogue is visible too.
//!
//! Runs come from `diri_usage::transcripts::skills`, scanned incrementally on
//! the background executor. Curves are smoothed when the data or the range
//! changes, so a frame only builds paths.

use super::*;
use diri_usage::transcripts::activity;
use diri_usage::transcripts::skills::{SkillRun, SkillSource, SkillUsage, SkillUsageStore, summarize};
use gpui::{Bounds, PathBuilder, Pixels, canvas, point};
use std::collections::HashMap;
use std::sync::Mutex;

pub(super) const RANGES: [usize; 3] = [30, 90, 365];
const ROW: f32 = 30.0;
/// How many rows tall a ridge at the overall peak is.
const OVERLAP: f32 = 2.2;
const NAME_W: f32 = 176.0;
const COUNT_W: f32 = 52.0;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum View {
    #[default]
    Catalog,
    Usage,
}

pub(super) struct UsageState {
    pub(super) days: usize,
    store: Arc<Mutex<Option<SkillUsageStore>>>,
    runs: Option<Arc<Vec<SkillRun>>>,
    loading: bool,
    task: Option<Task<()>>,
    pub(super) summary: Option<SkillUsage>,
    /// Smoothed daily runs per used skill, and the highest point among them.
    curves: Vec<Arc<Vec<f32>>>,
    peak: f32,
    /// Installed skills with no run in the range: name and catalogue index.
    unused: Vec<(String, usize)>,
    /// Lowercase skill name → catalogue index, to open a used skill.
    index: HashMap<String, usize>,
    scroll: ScrollHandle,
}

impl Default for UsageState {
    fn default() -> Self {
        Self {
            days: 90,
            store: Arc::default(),
            runs: None,
            loading: false,
            task: None,
            summary: None,
            curves: Vec::new(),
            peak: 0.0,
            unused: Vec::new(),
            index: HashMap::new(),
            scroll: ScrollHandle::new(),
        }
    }
}

/// Gaussian smoothing, wider for longer ranges so a year still reads as waves.
pub(super) fn smooth(values: &[u32]) -> Vec<f32> {
    let sigma = (values.len() as f32 / 40.0).max(1.2);
    let reach = (sigma * 3.0).ceil() as isize;
    (0..values.len() as isize)
        .map(|index| {
            let (mut sum, mut weight) = (0.0, 0.0);
            for offset in -reach..=reach {
                let Some(&value) = values.get((index + offset) as usize).filter(|_| index + offset >= 0)
                else {
                    continue;
                };
                let k = (-((offset * offset) as f32) / (2.0 * sigma * sigma)).exp();
                sum += value as f32 * k;
                weight += k;
            }
            if weight == 0.0 { 0.0 } else { sum / weight }
        })
        .collect()
}

/// Monday-zero weekday with the most runs, or `None` without any.
pub(super) fn busiest_weekday(usage: &SkillUsage) -> Option<usize> {
    let mut totals = [0_u32; 7];
    for skill in &usage.skills {
        for (day, count) in skill.daily.iter().enumerate() {
            totals[usize::from(activity::weekday(usage.first_day + day as i64))] += count;
        }
    }
    let best = *totals.iter().max()?;
    (best > 0).then(|| totals.iter().position(|&total| total == best).unwrap_or(0))
}

fn weekday_every(weekday: usize) -> &'static str {
    t(match weekday {
        0 => "settings.usage.weekday_every_mon",
        1 => "settings.usage.weekday_every_tue",
        2 => "settings.usage.weekday_every_wed",
        3 => "settings.usage.weekday_every_thu",
        4 => "settings.usage.weekday_every_fri",
        5 => "settings.usage.weekday_every_sat",
        _ => "settings.usage.weekday_every_sun",
    })
}

fn range_label(days: usize) -> &'static str {
    match days {
        30 => t("settings.usage.range_1m"),
        90 => t("settings.usage.range_3m"),
        _ => t("settings.skills.range_1y"),
    }
}

fn range_id(days: usize) -> &'static str {
    match days {
        30 => "skills-range-30",
        90 => "skills-range-90",
        _ => "skills-range-365",
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

impl SkillsPage {
    /// The usage view over a seeded catalogue and deterministic runs.
    #[cfg(test)]
    pub(crate) fn seed_usage_preview(&mut self) {
        self.seed_preview(false);
        let now = now();
        let mut runs = Vec::new();
        let skills: [(&str, u32, u32); 7] = [
            ("interface-craftsmanship", 9, 3),
            ("frontend-design", 7, 5),
            ("emil-design-eng", 6, 11),
            ("postgresql", 4, 17),
            ("dataviz", 3, 23),
            ("release-checklist", 2, 7),
            ("claude-api", 1, 29),
        ];
        let mut seed = 0x2545_f491_u32;
        for (name, weight, phase) in skills {
            for day in 0..90_u32 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                // Bursts that drift over the range, so ridges differ in shape.
                let wave = ((day + phase) as f32 / 9.0).sin().max(0.0);
                let count = ((wave * weight as f32) + (seed % 3) as f32 - 1.0).max(0.0) as u32;
                for run in 0..count {
                    runs.push(SkillRun {
                        at: now - i64::from(day) * 86_400 - i64::from(run) * 60,
                        name: name.into(),
                        source: SkillSource::Tool,
                    });
                }
            }
        }
        self.usage.runs = Some(Arc::new(runs));
        self.view = View::Usage;
        self.keyboard_target = 2;
        self.recompute_usage();
    }

    pub(super) fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        self.view = view;
        // Focus stays on the switch, which sits at a different stop per view.
        self.keyboard_target = match view {
            View::Catalog => 7,
            View::Usage => 0,
        };
        if view == View::Usage {
            self.refresh_usage(cx);
        }
        cx.notify();
    }

    pub(super) fn set_usage_days(&mut self, days: usize, cx: &mut Context<Self>) {
        if self.usage.days != days {
            self.usage.days = days;
            self.recompute_usage();
            self.usage.scroll.set_offset(point(px(0.0), px(0.0)));
        }
        cx.notify();
    }

    /// Reads transcript bytes appended since the last refresh.
    pub(super) fn refresh_usage(&mut self, cx: &mut Context<Self>) {
        if self.usage.loading {
            return;
        }
        let Some(home) = diri_platform::home_dir() else {
            self.usage.runs = Some(Arc::default());
            self.recompute_usage();
            return;
        };
        self.usage.loading = true;
        let store = Arc::clone(&self.usage.store);
        self.usage.task = Some(cx.spawn(async move |this, cx| {
            let runs = cx
                .background_executor()
                .spawn(async move {
                    let mut store = store.lock().unwrap_or_else(|poison| poison.into_inner());
                    store
                        .get_or_insert_with(|| SkillUsageStore::for_home(&home))
                        .refresh(now())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.usage.loading = false;
                this.usage.runs = Some(Arc::new(runs));
                this.recompute_usage();
                cx.notify();
            });
        }));
    }

    /// Rebuilds the summary from the runs, the range, and the catalogue.
    pub(super) fn recompute_usage(&mut self) {
        let Some(runs) = self.usage.runs.clone() else {
            return;
        };
        // A skill is known by its declared name and by its folder name: agents
        // that read SKILL.md files only see the folder.
        let mut index = HashMap::new();
        for (position, skill) in self.catalog.skills.iter().enumerate() {
            let folder = skill
                .path
                .parent()
                .and_then(|folder| folder.file_name())
                .map(|name| name.to_string_lossy().to_lowercase());
            for key in [Some(diri_usage::transcripts::skills::normalize(&skill.name).to_lowercase()), folder]
                .into_iter()
                .flatten()
            {
                index.entry(key).or_insert(position);
            }
        }
        let now = now();
        let today = (now + activity::system_offset(now)).div_euclid(86_400);
        // `/clear` and other built-in commands are not skills.
        let keep = |run: &SkillRun| {
            run.source != SkillSource::Slash || index.contains_key(&run.name.to_lowercase())
        };
        let summary = summarize(&runs, self.usage.days, today, &activity::system_offset, &keep);
        let used: Vec<usize> = summary
            .skills
            .iter()
            .filter_map(|skill| index.get(&skill.name.to_lowercase()).copied())
            .collect();
        let mut seen = std::collections::HashSet::new();
        let mut unused: Vec<(String, usize)> = self
            .catalog
            .skills
            .iter()
            .enumerate()
            .filter(|(position, skill)| {
                !used.contains(position)
                    && !summary.skills.iter().any(|used| used.name.eq_ignore_ascii_case(&skill.name))
                    && seen.insert(skill.name.to_lowercase())
            })
            .map(|(position, skill)| (skill.name.clone(), position))
            .collect();
        unused.sort_by_key(|(name, _)| name.to_lowercase());
        self.usage.curves = summary
            .skills
            .iter()
            .map(|skill| Arc::new(smooth(&skill.daily)))
            .collect();
        self.usage.peak = self
            .usage
            .curves
            .iter()
            .flat_map(|curve| curve.iter().copied())
            .fold(0.0001, f32::max);
        self.usage.unused = unused;
        self.usage.index = index;
        self.usage.summary = Some(summary);
    }

    /// The Catalog / Usage switch beside the page title.
    pub(super) fn view_switch(&self, colors: SemanticColors, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = match self.view {
            View::Catalog => self.keyboard_target == 7,
            View::Usage => self.keyboard_target == 0,
        };
        let mut switch = div()
            .id("skills-views")
            .role(gpui::Role::TabList)
            .aria_label(t("settings.skills.views"))
            .flex()
            .p(px(2.0))
            .gap(px(2.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(colors.primary.alpha(if focused { 0.28 } else { 0.08 }))
            .bg(colors.primary.alpha(0.025));
        for (view, id, label) in [
            (View::Catalog, "skills-view-catalog", t("settings.skills.view_catalog")),
            (View::Usage, "skills-view-usage", t("settings.skills.view_usage")),
        ] {
            let selected = self.view == view;
            switch = switch.child(
                div()
                    .id(id)
                    .role(gpui::Role::Tab)
                    .aria_label(label)
                    .aria_selected(selected)
                    .h(px(24.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .rounded(px(6.0))
                    .text_size(px(12.0))
                    .text_color(if selected { colors.primary } else { colors.secondary })
                    .when(selected, |tab| tab.bg(colors.primary.alpha(0.08)))
                    .cursor_pointer()
                    .hover(move |style| style.text_color(colors.primary))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.view != view {
                            this.set_view(view, cx);
                        }
                    })),
            );
        }
        switch
    }

    pub(super) fn render_usage(
        &self,
        mut page: gpui::Stateful<gpui::Div>,
        body_height: f32,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        page = page.child(
            div()
                .text_size(px(13.0))
                .text_color(colors.secondary)
                .child(tf(
                    "settings.skills.usage_intro",
                    &[("machine", &crate::platform::local_machine_label_lowercase())],
                )),
        );
        let mut ranges = div().flex().gap(px(5.0));
        for (position, days) in RANGES.into_iter().enumerate() {
            ranges = ranges.child(
                self.button(range_id(days), range_label(days), position + 1, colors)
                    .when(self.usage.days == days, |button| {
                        button
                            .bg(colors.primary.alpha(0.08))
                            .text_color(colors.primary)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.keyboard_target = position + 1;
                        this.set_usage_days(days, cx);
                    })),
            );
        }
        page = page.child(ranges);
        let message = |text: &'static str| div().text_size(px(12.0)).text_color(colors.secondary).child(text);
        let Some(summary) = &self.usage.summary else {
            return page.child(message(t("settings.skills.usage_loading")));
        };
        let total: u32 = summary.skills.iter().map(|skill| skill.total).sum();
        let used = summary.skills.len();
        let stat = |title: String, value: String| {
            div()
                .flex()
                .flex_col()
                .gap(px(5.0))
                .p(px(12.0))
                .min_w(px(140.0))
                .flex_1()
                .bg(colors.background)
                .child(div().text_size(px(11.0)).text_color(colors.secondary).child(title))
                .child(
                    div()
                        .text_size(px(20.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(value),
                )
        };
        page = page.child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(1.0))
                .rounded(px(8.0))
                .overflow_hidden()
                .bg(colors.primary.alpha(0.06))
                .child(stat(
                    tf("settings.skills.usage_runs_in", &[("range", &range_label(summary.days))]),
                    total.to_string(),
                ))
                .child(stat(
                    t("settings.skills.usage_skills_used").into(),
                    tf(
                        "settings.skills.usage_skills_used_value",
                        &[("used", &used), ("total", &(used + self.usage.unused.len()))],
                    ),
                ))
                .child(stat(
                    t("settings.skills.usage_busiest").into(),
                    busiest_weekday(summary).map_or_else(|| "—".to_owned(), |day| weekday_every(day).to_owned()),
                )),
        );
        if summary.skills.is_empty() {
            page = page.child(message(t("settings.skills.usage_empty")));
        }
        let header_label = |text: String| div().text_size(px(11.0)).text_color(colors.tertiary).child(text);
        let days = summary.days;
        let ticks = [days - 1, (days - 1) * 2 / 3, (days - 1) / 3, 0];
        let mut tick_row = div().flex_1().flex().justify_between();
        for ago in ticks {
            tick_row = tick_row.child(header_label(if ago == 0 {
                t("settings.skills.usage_today").into()
            } else {
                tf("settings.skills.usage_days_ago", &[("days", &ago)])
            }));
        }
        let header = div()
            .flex()
            .items_center()
            .gap(px(12.0))
            .pb(px(2.0))
            .border_b_1()
            .border_color(colors.primary.alpha(0.06))
            .child(div().w(px(NAME_W)).flex_shrink_0().text_right().child(header_label(t("settings.skills.usage_skill").into())))
            .child(tick_row)
            .child(div().w(px(COUNT_W)).flex_shrink_0().text_right().child(header_label(t("settings.skills.usage_runs").into())));
        let mut rows = div()
            .flex()
            .flex_col()
            .pt(px(if used > 0 { ROW * (OVERLAP - 1.0) } else { 0.0 }));
        let count = used.max(1) as f32;
        for (position, skill) in summary.skills.iter().enumerate() {
            let curve = Arc::clone(&self.usage.curves[position]);
            let peak = self.usage.peak;
            // Ridges further down the list are lighter, so the most used lead.
            let depth = position as f32 / count;
            let open = self.usage.index.get(&skill.name.to_lowercase()).copied();
            rows = rows.child(
                row_frame(("skill-usage", position), colors)
                    .aria_label(tf(
                        "settings.skills.usage_row",
                        &[("name", &skill.name), ("count", &skill.total)],
                    ))
                    .child(row_name(skill.name.clone(), colors.primary))
                    .child(
                        div().flex_1().h_full().relative().child(
                            canvas(|_, _, _| {}, move |bounds, _, window, _| {
                                paint_ridge(window, bounds, &curve, peak, depth, colors);
                            })
                            .absolute()
                            .inset_0(),
                        ),
                    )
                    .child(row_count(skill.total, colors.secondary))
                    .when_some(open, |row, index| {
                        row.role(gpui::Role::Button)
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| this.open_skill(index, cx)))
                    }),
            );
        }
        if !self.usage.unused.is_empty() {
            rows = rows.child(
                div()
                    .mt(px(if used > 0 { 20.0 } else { 0.0 }))
                    .mb(px(2.0))
                    .text_size(px(11.0))
                    .text_color(colors.tertiary)
                    .child(tf(
                        "settings.skills.usage_never",
                        &[("count", &self.usage.unused.len())],
                    )),
            );
            for (position, (name, index)) in self.usage.unused.iter().enumerate() {
                let index = *index;
                rows = rows.child(
                    row_frame(("skill-unused", position), colors)
                        .role(gpui::Role::Button)
                        .aria_label(tf("settings.skills.usage_row", &[("name", name), ("count", &0)]))
                        .cursor_pointer()
                        .child(row_name(name.clone(), colors.secondary))
                        .child(
                            div()
                                .flex_1()
                                .mb(px(7.0))
                                .border_b_1()
                                .border_color(colors.primary.alpha(0.08)),
                        )
                        .child(row_count(0, colors.tertiary))
                        .on_click(cx.listener(move |this, _, _, cx| this.open_skill(index, cx))),
                );
            }
        }
        page.child(header).child(
            div()
                .id("skills-usage-rows")
                .h(px((body_height - 92.0).max(160.0)))
                .overflow_y_scroll()
                .track_scroll(&self.usage.scroll)
                .child(rows),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(colors.tertiary)
                .child(t("settings.skills.usage_note")),
        )
    }
}

fn row_frame(id: impl Into<gpui::ElementId>, colors: SemanticColors) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(ROW))
        .flex_shrink_0()
        .flex()
        .items_end()
        .gap(px(12.0))
        .rounded(px(6.0))
        .hover(move |style| style.bg(colors.primary.alpha(0.035)))
}

fn row_name(name: String, color: gpui::Rgba) -> impl IntoElement {
    div()
        .w(px(NAME_W))
        .flex_shrink_0()
        .pb(px(6.0))
        .text_size(px(12.0))
        .text_color(color)
        .text_right()
        .truncate()
        .child(name)
}

fn row_count(count: u32, color: gpui::Rgba) -> impl IntoElement {
    div()
        .w(px(COUNT_W))
        .flex_shrink_0()
        .pb(px(6.0))
        .text_size(px(12.0))
        .text_color(color)
        .text_right()
        .child(count.to_string())
}

fn pt(x: f32, y: f32) -> gpui::Point<Pixels> {
    point(px(x), px(y))
}

/// One ridge: an opaque backdrop hides the ridges above where they pass
/// behind it, then a tint and a top line. It rises above its row.
fn paint_ridge(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    values: &[f32],
    peak: f32,
    depth: f32,
    colors: SemanticColors,
) {
    let left = f32::from(bounds.origin.x);
    let width = f32::from(bounds.size.width);
    let bottom = f32::from(bounds.origin.y) + f32::from(bounds.size.height);
    if values.len() < 2 || width < 4.0 {
        return;
    }
    let last = (values.len() - 1) as f32;
    let points: Vec<[f32; 2]> = values
        .iter()
        .enumerate()
        .map(|(x, value)| {
            [
                left + width * x as f32 / last,
                bottom - (value / peak).clamp(0.0, 1.0) * ROW * OVERLAP,
            ]
        })
        .collect();
    let area = |builder: PathBuilder| {
        let mut path = builder;
        path.move_to(pt(left, bottom));
        path.line_to(pt(points[0][0], points[0][1]));
        curve_through(&mut path, &points, bottom);
        path.line_to(pt(left + width, bottom));
        path.close();
        path.build().ok()
    };
    if let Some(path) = area(PathBuilder::fill()) {
        window.paint_path(path, colors.background);
    }
    if let Some(path) = area(PathBuilder::fill()) {
        window.paint_path(path, colors.primary.alpha(0.05 + 0.2 * (1.0 - depth)));
    }
    let mut line = PathBuilder::stroke(px(1.25));
    line.move_to(pt(points[0][0], points[0][1]));
    curve_through(&mut line, &points, bottom);
    if let Ok(path) = line.build() {
        window.paint_path(path, colors.primary.alpha(0.45 + 0.35 * (1.0 - depth)));
    }
}

/// Catmull-Rom through `points`, as cubic Béziers whose handles never dip
/// below the baseline.
fn curve_through(path: &mut PathBuilder, points: &[[f32; 2]], bottom: f32) {
    for i in 0..points.len() - 1 {
        let p0 = points[i.saturating_sub(1)];
        let p1 = points[i];
        let p2 = points[i + 1];
        let p3 = points[(i + 2).min(points.len() - 1)];
        let a = [p1[0] + (p2[0] - p0[0]) / 6.0, (p1[1] + (p2[1] - p0[1]) / 6.0).min(bottom)];
        let b = [p2[0] - (p3[0] - p1[0]) / 6.0, (p2[1] - (p3[1] - p1[1]) / 6.0).min(bottom)];
        path.cubic_bezier_to(pt(p2[0], p2[1]), pt(a[0], a[1]), pt(b[0], b[1]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_usage::transcripts::skills::SkillTally;

    #[test]
    fn smoothing_keeps_mass_near_the_spike_and_busiest_weekday_reads_days() {
        let mut daily = vec![0; 30];
        daily[15] = 10;
        let curve = smooth(&daily);
        assert!(curve[15] > curve[13] && curve[13] > curve[10]);
        assert!(curve[0] < 0.01);
        // Epoch day 0 was a Thursday (Monday-zero 3).
        let usage = SkillUsage {
            first_day: 0,
            days: 7,
            skills: vec![SkillTally {
                name: "a".into(),
                total: 3,
                daily: vec![0, 0, 0, 3, 0, 0, 0],
            }],
        };
        assert_eq!(busiest_weekday(&usage), Some(6));
        assert_eq!(busiest_weekday(&SkillUsage::default()), None);
    }
}
