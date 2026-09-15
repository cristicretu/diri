//! A split uses one ordinary vertical sidebar row, regardless of pane count.
use super::*;
use crate::split_workbench::drag::{DraggedWorkspace, WorkspacePreview};

impl Sidebar {
    pub(super) fn split_group_row(
        &mut self,
        row: &crate::store::SidebarRow,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let owner = row.id().clone();
        let (selected, tree, remembered, activity) = {
            let store = self.store.read().expect("session store lock poisoned");
            let activity = row
                .split_members
                .iter()
                .map(|session| {
                    sidebar_activity_state(
                        status_state(session, store.migrating().contains(&session.id)),
                        store.notifications().session_unread(&session.id),
                    )
                })
                .max_by_key(|state| match state {
                    StatusState::NeedsInput { .. } => 5,
                    StatusState::DoneUnseen => 4,
                    StatusState::Working => 3,
                    StatusState::IdleSeen => 2,
                    StatusState::Hibernated => 1,
                    StatusState::None => 0,
                })
                .unwrap_or(StatusState::None);
            (
                store.selected_session_id().cloned(),
                store
                    .preferences()
                    .split_layouts
                    .containing(&owner)
                    .cloned(),
                store
                    .preferences()
                    .split_layouts
                    .focus_history
                    .iter()
                    .find(|id| row.split_members.iter().any(|member| &member.id == *id))
                    .cloned()
                    .unwrap_or_else(|| owner.clone()),
                activity,
            )
        };
        let Some(tree) = tree else {
            return div().into_any_element();
        };
        let active = selected
            .as_ref()
            .is_some_and(|id| row.split_members.iter().any(|member| &member.id == id));
        self.working_row_rendered |= activity == StatusState::Working;
        let source = DraggedWorkspace {
            session: owner.clone(),
            whole_workspace: true,
        };
        let preview = WorkspacePreview {
            tree,
            labels: row
                .split_members
                .iter()
                .map(|session| (session.id.clone(), display_title(session)))
                .collect(),
            colors,
        };
        let title = row
            .split_members
            .iter()
            .map(|session| display_title(session))
            .collect::<Vec<_>>()
            .join(" · ");
        let focused =
            self.ui.focus_cursor.as_ref() == Some(&owner) && self.focus_handle.is_focused(window);
        let fill = if active {
            RowFill::Selected
        } else if focused {
            RowFill::Hover
        } else {
            RowFill::Clear
        };
        div()
            .id(SharedString::from(format!("split-group-{}", owner.0)))
            .debug_selector({
                let owner = owner.clone();
                move || format!("SPLIT_GROUP_{}", owner.0)
            })
            .h(px(SIDEBAR_NAV_ROW_HEIGHT))
            .pl(px(Space::ROW_H - 1.0))
            .pr(px(Space::ROW_H))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(SIDEBAR_ROW_RADIUS))
            .bg(fill.color(colors))
            .border_1()
            .border_color(colors.primary.alpha(0.0))
            .cursor_pointer()
            .hover(|style| {
                style.bg(if active {
                    RowFill::Selected.color(colors)
                } else {
                    RowFill::Hover.color(colors)
                })
            })
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.store
                    .write()
                    .expect("session store lock poisoned")
                    .select(remembered.clone());
                this.ui.focus_cursor = Some(owner.clone());
                cx.emit(SidebarEvent::SessionActivated);
                cx.notify();
            }))
            .on_drag(source, move |_, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| preview.clone())
            })
            .on_drag_move(cx.listener({
                let id = row.id().clone();
                move |_, event: &gpui::DragMoveEvent<DraggedWorkspace>, _, cx| {
                    if event.bounds.contains(&event.event.position) {
                        cx.emit(SidebarEvent::HoverWorkspace {
                            id: id.clone(),
                            bounds: event.bounds,
                        });
                    }
                }
            }))
            .drag_over::<DraggedWorkspace>(|style, _, _, _| {
                style
                    .border_color(Palette::CLAY.alpha(0.7))
                    .bg(Palette::CLAY.alpha(0.15))
            })
            .drag_over::<DraggedSidebarItem>(|style, _, _, _| {
                style
                    .border_color(Palette::CLAY.alpha(0.7))
                    .bg(Palette::CLAY.alpha(0.15))
            })
            .on_drop(cx.listener({
                let target = row.id().clone();
                move |_, source: &DraggedWorkspace, _, cx| {
                    cx.emit(SidebarEvent::GroupSessions {
                        target: target.clone(),
                        source: source.clone(),
                    });
                    cx.stop_propagation();
                }
            }))
            .on_drop(cx.listener({
                let target = row.id().clone();
                move |this, source: &DraggedSidebarItem, _, cx| {
                    if let Some(id) = source.session_id()
                        && this.ui.drag.is_some()
                    {
                        cx.emit(SidebarEvent::GroupSessions {
                            target: target.clone(),
                            source: DraggedWorkspace {
                                session: id.clone(),
                                whole_workspace: false,
                            },
                        });
                    }
                    this.finish_drag();
                    cx.notify();
                    cx.stop_propagation();
                }
            }))
            .children(indent_rails(row, colors))
            .child(activity_mark(activity, self.activity_frame, colors))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(Typo::ROW.size))
                    .text_color(colors.primary)
                    .child(title),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(Typo::META.size))
                    .text_color(colors.secondary)
                    .child(row.split_members.len().to_string()),
            )
            .into_any_element()
    }
}
