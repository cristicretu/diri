use super::*;
use crate::store::{WorkspaceSpawnState, WorkspaceSpawnTarget};

impl RootView {
    pub(super) fn workspace_spawn_target(&self) -> Option<WorkspaceSpawnTarget> {
        let workspace = self.active_workspace.clone()?;
        let store = self.window_store.read().expect("store");
        let selected_tab = store
            .workspace_catalog()
            .snapshot()?
            .workspaces
            .iter()
            .find(|w| w.id == workspace)?
            .selected_tab
            .clone();
        Some(WorkspaceSpawnTarget {
            owner: self.spawn_owner,
            workspace,
            selected_tab,
        })
    }

    pub(super) fn sync_workspace_spawn_context(&self, cx: &mut Context<Self>) {
        let target = self.workspace_spawn_target();
        if let Some(navigation) = &self.navigation {
            navigation.update(cx, |navigation, cx| {
                navigation.set_workspace_spawn_target(target);
                navigation.set_workspace_palette_context(self.active_workspace.clone(), cx)
            });
        }
    }

    pub(super) fn workspace_launches(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let store = self.window_store.read().expect("store");
        let receipts: Vec<_> = store
            .workspace_spawn_receipts()
            .filter(|r| {
                !matches!(
                    r.state,
                    WorkspaceSpawnState::Placed { .. } | WorkspaceSpawnState::Created { .. }
                )
            })
            .cloned()
            .collect();
        if receipts.is_empty() && !self.launches_expanded {
            return None;
        }
        let pending = receipts.iter().filter(|r| r.state.pending()).count();
        let names: std::collections::HashMap<_, _> = store
            .workspace_catalog()
            .snapshot()
            .into_iter()
            .flat_map(|s| s.workspaces.iter())
            .map(|w| (w.id.clone(), w.name.clone()))
            .collect();
        drop(store);
        let label = if receipts.is_empty() {
            "No pending launches".to_owned()
        } else if pending == receipts.len() {
            format!(
                "Creating {pending} {}…",
                if pending == 1 { "session" } else { "sessions" }
            )
        } else {
            format!(
                "Review {} {}",
                receipts.len() - pending,
                if receipts.len() - pending == 1 {
                    "launch"
                } else {
                    "launches"
                }
            )
        };
        let selected = self
            .launch_cursor
            .filter(|id| receipts.iter().any(|r| r.id == *id))
            .or_else(|| receipts.last().map(|r| r.id));
        let mut panel = div()
            .track_focus(&self.launches_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.workspace_launch_key(event, window, cx)
            }))
            .absolute()
            .right(px(12.0))
            .top(px(44.0))
            .w(px(310.0))
            .max_w_full()
            .rounded(px(Radius::PANEL))
            .bg(colors.background)
            .border_1()
            .border_color(colors.floating_stroke())
            .shadow_lg()
            .occlude()
            .text_color(colors.primary)
            .text_size(px(Typo::META.size));
        panel = panel.child(
            div()
                .id("workspace-launches-toggle")
                .debug_selector(|| "workspace-launches-toggle".into())
                .p(px(12.0))
                .cursor_pointer()
                .flex()
                .items_center()
                .justify_between()
                .child(label)
                .child(sf_symbol(
                    if self.launches_expanded {
                        "chevron.up"
                    } else {
                        "chevron.down"
                    },
                    10.0,
                    colors.secondary,
                ))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.launches_expanded = !this.launches_expanded;
                    if this.launches_expanded {
                        window.focus(&this.launches_focus, cx);
                    } else {
                        this.restore_launch_focus(window, cx);
                    }
                    cx.notify();
                    cx.stop_propagation();
                })),
        );
        if self.launches_expanded {
            let mut rows = div()
                .id("workspace-launches-scroll")
                .track_scroll(&self.launch_scroll)
                .max_h(px(340.0))
                .overflow_y_scroll()
                .flex()
                .flex_col();
            for receipt in receipts.iter().rev() {
                let id = receipt.id;
                let (detail, session, retry) = match &receipt.state {
                    WorkspaceSpawnState::Creating => ("Creating session…".to_owned(), None, false),
                    WorkspaceSpawnState::Placing(session) => (
                        "Adding session to workspace…".to_owned(),
                        Some(session.clone()),
                        false,
                    ),
                    WorkspaceSpawnState::Unplaced { session, detail } => {
                        (detail.clone(), Some(session.clone()), true)
                    }
                    WorkspaceSpawnState::Unconfirmed(detail) => (detail.clone(), None, false),
                    WorkspaceSpawnState::Placed { .. } | WorkspaceSpawnState::Created { .. } => {
                        continue;
                    }
                };
                let mut buttons = div().flex().gap(px(6.0)).text_color(colors.primary);
                if let Some(session) = session {
                    buttons = buttons.child(
                        div()
                            .id(("workspace-launch-open", id))
                            .px(px(4.0))
                            .py(px(3.0))
                            .rounded(px(Radius::CHIP))
                            .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                            .cursor_pointer()
                            .child("Open session")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_workspace_launch_session(session.clone(), window, cx);
                                cx.stop_propagation();
                            })),
                    );
                }
                if retry {
                    buttons = buttons.child(
                        div()
                            .id(("workspace-launch-retry", id))
                            .px(px(4.0))
                            .py(px(3.0))
                            .rounded(px(Radius::CHIP))
                            .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                            .cursor_pointer()
                            .child("Retry placement")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.window_store
                                    .write()
                                    .expect("store")
                                    .retry_workspace_placement(id);
                                cx.notify();
                                cx.stop_propagation();
                            })),
                    );
                }
                if !receipt.state.pending() {
                    buttons = buttons.child(
                        div()
                            .id(("workspace-launch-dismiss", id))
                            .px(px(4.0))
                            .py(px(3.0))
                            .rounded(px(Radius::CHIP))
                            .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                            .cursor_pointer()
                            .child("Dismiss")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.window_store
                                    .write()
                                    .expect("store")
                                    .dismiss_workspace_spawn(id);
                                cx.notify();
                                cx.stop_propagation();
                            })),
                    );
                }
                rows = rows.child(
                    div()
                        .id(("workspace-launch-row", id))
                        .debug_selector(move || format!("workspace-launch-row-{id}"))
                        .when(selected == Some(id), |row| {
                            row.bg(colors.primary.alpha(0.045))
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                this.launch_cursor = Some(id);
                                window.focus(&this.launches_focus, cx);
                                cx.notify();
                            }),
                        )
                        .p(px(12.0))
                        .border_t_1()
                        .border_color(colors.floating_stroke())
                        .flex()
                        .flex_col()
                        .gap(px(7.0))
                        .child(div().font_weight(FontWeight::MEDIUM).child(
                            receipt.target.workspace().map_or_else(
                                || "All sessions".into(),
                                |id| {
                                    names
                                        .get(id)
                                        .cloned()
                                        .unwrap_or_else(|| "Removed workspace".into())
                                },
                            ),
                        ))
                        .child(
                            div()
                                .text_color(colors.secondary)
                                .child(bounded_notice_body(&detail)),
                        )
                        .child(buttons),
                );
            }
            panel = panel.child(rows).child(
                div()
                    .p(px(10.0))
                    .text_color(colors.secondary)
                    .child("↑↓ choose · Return open · R retry · Delete dismiss · Esc close"),
            );
        }
        Some(panel.into_any_element())
    }
    pub(super) fn open_workspace_launch_session(
        &mut self,
        session: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.window_store.write().expect("store").select(session);
        self.launches_expanded = false;
        let opening_project = self
            .sidebar
            .update(cx, |sidebar, cx| sidebar.open_selected_project_agent(cx));
        if !opening_project {
            self.sidebar
                .update(cx, |sidebar, cx| sidebar.activate_workspace(None, cx));
            self.activate_saved_workspace(None, window, cx);
        }
        self.services.store.publish_local_change();
        cx.notify();
    }

    fn restore_launch_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_workspace.is_some()
            && let Some(workbench) = &self.workspace_workbench
        {
            workbench.update(cx, |view, cx| view.focus(window, cx));
        } else if let Some(terminal) = &self.terminal {
            terminal.update(cx, |view, cx| view.focus(window, cx));
        }
    }

    fn workspace_launch_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.launches_focus.contains_focused(window, cx) || !self.launches_expanded {
            return;
        }
        let receipts: Vec<_> = self
            .window_store
            .read()
            .expect("store")
            .workspace_spawn_receipts()
            .filter(|r| {
                !matches!(
                    r.state,
                    WorkspaceSpawnState::Placed { .. } | WorkspaceSpawnState::Created { .. }
                )
            })
            .cloned()
            .collect();
        let index = self
            .launch_cursor
            .and_then(|id| receipts.iter().position(|r| r.id == id))
            .unwrap_or(receipts.len().saturating_sub(1));
        match event.keystroke.key.as_str() {
            "up" | "down" if !receipts.is_empty() => {
                let index = if event.keystroke.key == "down" {
                    index.saturating_sub(1)
                } else {
                    (index + 1).min(receipts.len() - 1)
                };
                self.launch_cursor = Some(receipts[index].id);
                self.launch_scroll
                    .scroll_to_item(receipts.len() - 1 - index);
            }
            "escape" => {
                self.launches_expanded = false;
                self.restore_launch_focus(window, cx);
            }
            "r" => {
                if let Some(receipt) = receipts.get(index) {
                    self.window_store
                        .write()
                        .expect("store")
                        .retry_workspace_placement(receipt.id);
                }
            }
            "backspace" | "delete" => {
                if let Some(receipt) = receipts.get(index) {
                    self.window_store
                        .write()
                        .expect("store")
                        .dismiss_workspace_spawn(receipt.id);
                }
            }
            "enter" => {
                if let Some(receipt) = receipts.get(index)
                    && let WorkspaceSpawnState::Placing(session)
                    | WorkspaceSpawnState::Unplaced { session, .. } = &receipt.state
                {
                    self.open_workspace_launch_session(session.clone(), window, cx);
                }
            }
            _ => return,
        }
        cx.notify();
        cx.stop_propagation();
    }
}
