//! Compact worktree inventory inside the shared Settings destination.
use super::*;

fn label(text: impl Into<SharedString>, size: f32, color: Rgba) -> gpui::Div {
    div()
        .text_size(px(size))
        .line_height(px(16.0))
        .text_color(color)
        .child(text.into())
}

fn disk_label(bytes: Option<u64>) -> String {
    match bytes {
        Some(n) if n >= 1024 * 1024 * 1024 => {
            format!("{:.1} GB", n as f64 / (1024.0 * 1024.0 * 1024.0))
        }
        Some(n) if n >= 1024 * 1024 => format!("{:.0} MB", n as f64 / (1024.0 * 1024.0)),
        Some(n) => format!("{} KB", n / 1024),
        None => "Size unavailable".into(),
    }
}

impl UtilitySurfaces {
    pub(super) fn worktree_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.settings_colors();
        let state = &self.worktrees;
        let ready: Vec<_> = state
            .entries
            .iter()
            .filter(|e| e.stale_suggestion)
            .collect();
        let known_bytes: u64 = ready.iter().filter_map(|e| e.health.disk_bytes).sum();
        let summary = if state.loading {
            "Checking worktrees…".into()
        } else {
            format!(
                "{} worktrees · {} ready to clean · {} cleanup estimate",
                state.entries.len(),
                ready.len(),
                disk_label(Some(known_bytes))
            )
        };
        let mut content = div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(5.0))
                            .child(label(summary, 13.0, colors.primary))
                            .child(label(
                                "Local projects · branches and commits are kept after cleanup",
                                11.0,
                                colors.tertiary,
                            )),
                    )
                    .when(!state.loading, |row| {
                        row.child(surface_button(
                            "Refresh",
                            "worktrees-refresh",
                            colors,
                            cx,
                            |this, cx| this.refresh_worktrees(cx),
                        ))
                    }),
            )
            .child(
                div().flex().items_center().gap(px(5.0)).children(
                    [
                        ("All", false, false),
                        ("Ready to clean", true, false),
                        ("Older than 30 days", false, true),
                    ]
                    .into_iter()
                    .map(|(title, ready, old)| {
                        div()
                            .id(SharedString::from(format!("worktrees-filter-{title}")))
                            .debug_selector(move || format!("worktrees-filter-{title}"))
                            .px(px(10.0))
                            .py(px(5.0))
                            .rounded(px(8.0))
                            .bg(if state.cleanup_only == ready && state.old_only == old {
                                colors.primary.alpha(0.10)
                            } else {
                                colors.primary.alpha(0.03)
                            })
                            .text_size(px(11.0))
                            .text_color(colors.secondary)
                            .cursor_pointer()
                            .hover(|s| s.bg(colors.primary.alpha(0.12)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.worktrees.cleanup_only = ready;
                                this.worktrees.old_only = old;
                                cx.notify();
                            }))
                            .child(title)
                    }),
                ),
            );
        if let Some(error) = &state.error {
            content = content.child(label(error.clone(), 12.0, Ink::DANGER));
        }
        let mut rows = div()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .p(px(5.0))
            .rounded(px(12.0))
            .bg(colors.primary.alpha(0.025));
        let mut visible = 0;
        let mut project = String::new();
        for entry in state.entries.iter().filter(|e| {
            (!state.cleanup_only || e.stale_suggestion) && (!state.old_only || e.age_days > 30)
        }) {
            visible += 1;
            if project != entry.project_root {
                project.clone_from(&entry.project_root);
                rows = rows.child(div().px(px(9.0)).pt(px(10.0)).pb(px(5.0)).child(label(
                    project.clone(),
                    11.0,
                    colors.tertiary,
                )));
            }
            let branch = entry
                .branch
                .clone()
                .unwrap_or_else(|| "Detached HEAD".into());
            let age = if entry.age_days < 0 {
                "Age unknown".into()
            } else {
                format!("{}d old", entry.age_days)
            };
            let health = &entry.health;
            let status = health
                .protection
                .clone()
                .unwrap_or_else(|| "Ready to clean".into());
            let pr_label = match health.pr_number {
                Some(n) => format!("#{n} {}", health.pr_state),
                None if health.pr_state == "Unavailable" => "PR unavailable".into(),
                None => health.pr_state.clone(),
            };
            let path = entry.path.clone();
            let mut row = div()
                .id(SharedString::from(format!("worktree-row-{path}")))
                .px(px(9.0))
                .py(px(6.0))
                .rounded(px(8.0))
                .hover(|s| s.bg(colors.primary.alpha(0.035)))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(12.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(7.0))
                                .min_w(px(0.0))
                                .flex_1()
                                .child(sf_symbol("arrow.branch", 12.0, colors.secondary))
                                .child(div().min_w(px(0.0)).truncate().child(label(
                                    branch.clone(),
                                    13.0,
                                    colors.primary,
                                ))),
                        ),
                )
                .child(div().min_w(px(0.0)).truncate().child(label(
                    path.clone(),
                    10.0,
                    colors.tertiary,
                )))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .flex_wrap()
                        .gap(px(8.0))
                        .child(label(
                            status,
                            11.0,
                            if entry.stale_suggestion {
                                colors.primary
                            } else {
                                colors.secondary
                            },
                        ))
                        .child(label(
                            format!(
                                "{} · {age} · {}",
                                if entry.dirty { "Changes" } else { "Clean" },
                                disk_label(health.disk_bytes)
                            ),
                            11.0,
                            colors.tertiary,
                        ))
                        .child(
                            div()
                                .id(SharedString::from(format!("worktree-pr-{path}")))
                                .text_size(px(11.0))
                                .text_color(colors.secondary)
                                .when_some(
                                    health.pr_url.clone().filter(|u| u.starts_with("https://")),
                                    |el, url| {
                                        el.cursor_pointer()
                                            .hover(|s| s.text_color(colors.primary))
                                            .on_click(move |_, _, cx| cx.open_url(&url))
                                    },
                                )
                                .child(pr_label),
                        )
                        .when(entry.stale_suggestion && !state.loading, |row| {
                            row.child(div().flex_1()).child(surface_button(
                                "Clean up…",
                                SharedString::from(format!("worktree-clean-{path}")),
                                colors,
                                cx,
                                move |this, cx| {
                                    this.worktrees.request_cleanup(&path);
                                    cx.notify();
                                },
                            ))
                        }),
                );
            if state
                .pending_cleanup
                .as_ref()
                .is_some_and(|p| p.path == entry.path)
            {
                row = row.child(div().mt(px(5.0)).p(px(10.0)).rounded(px(8.0)).bg(Ink::ATTENTION.alpha(0.08)).flex().flex_col().gap(px(8.0))
                    .child(label(format!("Remove {branch}?"), 12.0, colors.primary))
                    .child(label("Deletes this checkout and ignored files, including build output. Keeps the Git branch.", 11.0, colors.secondary))
                    .child(div().flex().gap(px(8.0))
                        .child(surface_button("Cancel", "worktree-clean-cancel", colors, cx, |this, cx| { this.worktrees.cancel_cleanup(); cx.notify(); }))
                        .child(surface_button("Remove worktree", "worktree-clean-confirm", colors, cx, |this, cx| { this.confirm_cleanup(cx); cx.notify(); }))));
            }
            rows = rows.child(row);
        }
        if visible == 0 {
            rows = rows.child(
                div()
                    .p(px(24.0))
                    .flex()
                    .flex_col()
                    .gap(px(7.0))
                    .child(label(
                        if state.loading {
                            "Scanning your local projects…"
                        } else if state.entries.is_empty() {
                            "Your worktrees will appear here"
                        } else {
                            "No worktrees match this view"
                        },
                        13.0,
                        colors.primary,
                    ))
                    .child(label(
                        "Add a local project to Diri, then refresh to review its linked worktrees.",
                        11.0,
                        colors.secondary,
                    )),
            );
        }
        content = content.child(rows)
            .child(label("Age is time since checkout creation, not last use. PRs use GitHub CLI access and the latest 1,000 repository PRs. Unavailable sizes are excluded from the total.", 11.0, colors.tertiary));
        settings_page("Worktrees", content, colors)
    }
}

#[cfg(test)]
pub(super) fn preview_entries() -> Vec<diri_proto::WorktreeOverviewEntry> {
    [
        ("main", "No recent PR", "Main checkout", 120, 800),
        ("feat/command-palette", "Merged", "", 42, 2400),
        ("fix/sidebar-spacing", "Merged", "", 18, 640),
        ("feat/phone-access", "Open", "Session in use", 5, 1200),
        ("experiment/layout", "Closed", "Local changes", 64, 320),
    ]
    .into_iter()
    .enumerate()
    .map(
        |(i, (branch, pr, protection, age, mb))| diri_proto::WorktreeOverviewEntry {
            path: format!("/Users/alex/Projects/diri/{}", branch.replace('/', "-")),
            branch: Some(branch.into()),
            project_root: "/Users/alex/Projects/diri".into(),
            session_id: None,
            session_status: None,
            dirty: i == 4,
            merged: pr == "Merged",
            age_days: age,
            stale_suggestion: protection.is_empty(),
            health: diri_proto::WorktreeHealth {
                head: Some("abc".into()),
                disk_bytes: Some(mb * 1024 * 1024),
                pr_number: (i > 0).then_some(190 + i as u64),
                pr_url: None,
                pr_state: pr.into(),
                protection: (!protection.is_empty()).then(|| protection.into()),
            },
        },
    )
    .collect()
}
