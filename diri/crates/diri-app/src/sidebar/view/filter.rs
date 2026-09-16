use super::*;

impl Sidebar {
    pub(super) fn handle_filter_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match event.keystroke.key.as_str() {
            "escape" => {
                if self.filter_query.text().is_empty() {
                    self.filter_open = false;
                    self.focus_handle.focus(window, cx);
                } else {
                    self.filter_query.clear();
                }
            }
            "down" | "up" | "enter" => {
                // Editing does not activate a result. Arrow/Return enters the
                // result list; a subsequent Return deliberately selects it.
                self.focus_handle.focus(window, cx);
                let (rows, selected) = self.focus_rows_snapshot();
                self.ui
                    .reconcile_focus_cursor(&focus_row_ids(&rows), selected.as_ref());
                self.scroll_focus_cursor_into_view(window);
            }
            _ => {
                let Some(edit) = query_editor::edit_for(&event.keystroke) else {
                    return false;
                };
                match edit {
                    Edit::Local(local) => {
                        self.filter_query.apply(local);
                    }
                    Edit::Clipboard(ClipboardEdit::Copy) => {
                        query_editor::copy_selection(&self.filter_query, cx)
                    }
                    Edit::Clipboard(ClipboardEdit::Cut) => {
                        query_editor::cut_selection(&mut self.filter_query, cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Paste) => {
                        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                            self.filter_query.insert(&text);
                        }
                    }
                }
            }
        }
        self.dismiss_hover_card(cx);
        cx.stop_propagation();
        cx.notify();
        true
    }

    pub(super) fn filter_control(
        &self,
        colors: SemanticColors,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut control = div()
            .id("sidebar-filter")
            .debug_selector(|| "sidebar-filter".into())
            .mx(px(Space::INSET))
            .mb(px(6.0))
            .h(px(30.0))
            .flex_none()
            .rounded(px(SIDEBAR_ROW_RADIUS))
            .flex()
            .items_center()
            .gap(px(7.0))
            .px(px(9.0))
            .text_size(px(Typo::META.size))
            .text_color(colors.secondary)
            .cursor_text()
            .track_focus(&self.filter_focus)
            .when(self.filter_open, |row| row.bg(colors.primary.alpha(0.06)))
            .on_click(cx.listener(|this, _, window, cx| {
                if !this.filter_open {
                    this.filter_generation += 1;
                }
                this.filter_open = true;
                this.filter_focus.focus(window, cx);
                cx.notify();
            }))
            .child(sf_symbol("magnifyingglass", 11.0, colors.secondary));
        if self.filter_open {
            control = control.child(
                div()
                    .id("sidebar-filter-input")
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .role(Role::TextInput)
                    .aria_label("Filter session labels")
                    .child(if self.filter_focus.is_focused(window) {
                        query_label(&self.filter_query)
                    } else {
                        div()
                            .child(self.filter_query.text().to_owned())
                            .into_any_element()
                    }),
            );
            control = control.child(
                div()
                    .id("clear-sidebar-filter")
                    .debug_selector(|| "clear-sidebar-filter".into())
                    .role(Role::Button)
                    .aria_label("Clear filter")
                    .size(px(20.0))
                    .flex_none()
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.0))
                    .hover(move |button| button.bg(colors.primary.alpha(0.08)))
                    .child(sf_symbol("xmark", 9.0, colors.tertiary))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.filter_query.clear();
                        this.filter_open = false;
                        this.focus_handle.focus(window, cx);
                        cx.stop_propagation();
                        cx.notify();
                    })),
            );
        } else {
            control = control
                .role(Role::Button)
                .aria_label("Filter sessions")
                .child("Filter sessions");
        }
        let expanded = (self.ui.width - Space::INSET * 2.0).max(0.0);
        if self.filter_open && !cx.reduce_motion() {
            control
                .with_animation(
                    format!("sidebar-filter-open-{}", self.filter_generation),
                    Animation::new(Duration::from_millis(180)),
                    move |control, progress| {
                        control.w(px(
                            128.0 + (expanded - 128.0) * Motion::SETTLE.settle(progress)
                        ))
                    },
                )
                .into_any_element()
        } else {
            control
                .w(px(if self.filter_open { expanded } else { 128.0 }))
                .into_any_element()
        }
    }
}
