//! Working and Settled folds (Settings → General → Fold working and settled
//! sessions). Each project keeps the sessions that need you at the top:
//! those waiting on you or holding news you have not seen, plus pinned rows
//! and the one on screen. Sessions that are working fold into a collapsed
//! Working row under them, and idle sessions you have already seen fold into
//! a collapsed Settled row. A session moves back up the moment it finishes,
//! fails, or asks for something.
//!
//! A row moves with its whole subtree, so a parent and the agents it spawned
//! never split across folds. The subtree stays up while any row in it needs
//! you.
//!
//! The folds are a view of the store's projection: nothing is written, and
//! turning the setting off restores the exact projection.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum FoldKind {
    Working,
    Settled,
}

impl FoldKind {
    pub(super) const ALL: [FoldKind; 2] = [FoldKind::Working, FoldKind::Settled];

    fn key(self) -> &'static str {
        match self {
            FoldKind::Working => "working",
            FoldKind::Settled => "settled",
        }
    }

    fn label(self) -> &'static str {
        match self {
            FoldKind::Working => t("sidebar.fold.working"),
            FoldKind::Settled => t("sidebar.fold.settled"),
        }
    }

    pub(super) fn rows(self, group: &crate::store::SidebarProject) -> &[crate::store::SidebarRow] {
        match self {
            FoldKind::Working => &group.working,
            FoldKind::Settled => &group.settled,
        }
    }
}

/// Where a session tree belongs, from the states of its visible rows.
pub(super) fn classify(
    states: impl IntoIterator<Item = StatusState>,
    keep_up: bool,
) -> Option<FoldKind> {
    if keep_up {
        return None;
    }
    let mut working = false;
    for state in states {
        match state {
            StatusState::NeedsInput { .. } | StatusState::DoneUnseen => return None,
            StatusState::Working => working = true,
            StatusState::IdleSeen | StatusState::None | StatusState::Hibernated => {}
        }
    }
    Some(if working {
        FoldKind::Working
    } else {
        FoldKind::Settled
    })
}

/// The projection with each project's working and settled trees moved out
/// of `sessions` into its folds.
pub(super) fn fold_projection(
    projection: Arc<crate::store::SidebarProjection>,
    store: &crate::store::SessionStore,
) -> Arc<crate::store::SidebarProjection> {
    let selected = store.selected_session_id();
    let mut folded = (*projection).clone();
    for group in &mut folded.projects {
        let rows = std::mem::take(&mut group.sessions);
        let mut start = 0;
        while start < rows.len() {
            let end = rows[start + 1..]
                .iter()
                .position(|row| row.depth == 0)
                .map_or(rows.len(), |offset| start + 1 + offset);
            let tree = &rows[start..end];
            let keep_up = tree
                .iter()
                .any(|row| row.pinned || selected == Some(row.id()));
            let states = tree
                .iter()
                .map(|row| super::activity::row_state(store, &row.session));
            let target = match classify(states, keep_up) {
                None => &mut group.sessions,
                Some(FoldKind::Working) => &mut group.working,
                Some(FoldKind::Settled) => &mut group.settled,
            };
            target.extend_from_slice(tree);
            start = end;
        }
    }
    Arc::new(folded)
}

impl Sidebar {
    /// Whether the folds apply: the setting is on, the sidebar groups by
    /// project, and no filter is narrowing the list.
    pub(super) fn folds_enabled(&self, store: &crate::store::SessionStore) -> bool {
        let prefs = store.preferences();
        prefs.sidebar_fold_sessions
            && prefs.sidebar_grouping == SidebarGrouping::Project
            && !self.filtering()
    }

    pub(super) fn fold_expanded(&self, project: &ProjectId, kind: FoldKind) -> bool {
        self.expanded_folds.contains(&(project.clone(), kind))
    }

    fn toggle_fold(&mut self, project: ProjectId, kind: FoldKind, cx: &mut Context<Self>) {
        let key = (project, kind);
        if !self.expanded_folds.remove(&key) {
            self.expanded_folds.insert(key);
        }
        cx.notify();
    }

    /// One fold under a project's rows: a header with its count, then its
    /// rows while open. Returns the element and its height.
    pub(super) fn session_fold(
        &mut self,
        group: &crate::store::SidebarProject,
        kind: FoldKind,
        interactive: bool,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (AnyElement, f32) {
        let project_id = group.project.id.clone();
        let rows = kind.rows(group).to_vec();
        let expanded = self.fold_expanded(&project_id, kind);
        let key = format!("{}:{}", kind.key(), project_id.0);
        let mut fold = div().id(key.clone()).flex().flex_col().flex_none().child(
            div()
                .id(format!("{key}:header"))
                .debug_selector({
                    let key = key.clone();
                    move || format!("FOLD_{key}")
                })
                .role(Role::Button)
                .aria_label(kind.label())
                .mt(px(4.0))
                .pl(px(Space::ROW_H))
                .pr(px(Space::ROW_H))
                .h(px(SIDEBAR_NAV_ROW_HEIGHT))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(SIDEBAR_ROW_RADIUS))
                .cursor_pointer()
                .text_size(px(Typo::SECTION_HEADER.size))
                .font_weight(Typo::SECTION_HEADER.weight)
                .text_color(colors.tertiary)
                .hover(move |header| header.bg(colors.primary.alpha(0.05)))
                .on_click(cx.listener({
                    let project_id = project_id.clone();
                    move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_fold(project_id.clone(), kind, cx);
                    }
                }))
                .child(project_disclosure(!expanded, colors))
                .child(div().min_w(px(0.0)).flex_1().child(kind.label()))
                .child(archive_count(rows.len(), colors)),
        );
        let now = Instant::now();
        let motion = self
            .fold_disclosures
            .entry((project_id, kind))
            .or_insert_with(|| Disclosure::new(expanded, rows.len(), now));
        let frame = motion.update(expanded, rows.len(), now, cx.reduce_motion());
        self.disclosure_animating |= frame.animating;
        self.row_window.advance(4.0 + SIDEBAR_NAV_ROW_HEIGHT);
        let body_top = self.row_window.cursor();
        let mut natural = 0.0;
        if frame.reveal > 0.0 {
            let settled = !frame.animating && frame.reveal >= 1.0 && interactive;
            let mut children = Vec::new();
            for row in &rows {
                let question = self.observe_question(row, cx.reduce_motion());
                let height = question.1;
                natural += height + 2.0;
                if !self.place_row(Some(row.id()), 2.0, height, !settled) {
                    children.push(Slot::Skipped {
                        height,
                        margin: 2.0,
                    });
                    continue;
                }
                let shortcut = self.shortcut_for(row.id());
                let rendered = self.mount_session_row(
                    row,
                    shortcut,
                    None,
                    group.host.is_some(),
                    colors,
                    question,
                    !settled,
                    window,
                    cx,
                );
                let rendered = if expanded && interactive {
                    self.track_row_bounds(row.id().clone(), rendered, None)
                } else {
                    rendered
                };
                children.push(Slot::Built {
                    row: rendered,
                    height,
                    margin: 2.0,
                });
            }
            fold = fold.child(disclosure_body(
                windowing::body_rows(children),
                &frame,
                expanded && interactive,
            ));
        }
        let body_height = natural * frame.reveal;
        self.row_window.set_cursor(body_top + body_height);
        (
            fold.into_any_element(),
            SIDEBAR_NAV_ROW_HEIGHT + 4.0 + body_height,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tree_stays_up_while_any_row_needs_you() {
        use StatusState as S;
        let asks = S::NeedsInput { destructive: false };
        assert_eq!(classify([S::Working, asks], false), None);
        assert_eq!(classify([S::IdleSeen, S::DoneUnseen], false), None);
        assert_eq!(classify([S::Working, S::IdleSeen], false), Some(FoldKind::Working));
        assert_eq!(
            classify([S::IdleSeen, S::None, S::Hibernated], false),
            Some(FoldKind::Settled)
        );
        assert_eq!(classify([S::Working], true), None, "pinned or on screen");
    }
}
