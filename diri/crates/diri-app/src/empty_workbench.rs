//! The resting workbench explains the next action without requiring a sidebar.

use diri_ui::{Palette, Radius, SemanticColors};
use gpui::{FontWeight, IntoElement, Role, div, prelude::*, px};

use crate::commands::{CommandId, FocusSidebar, OpenLauncher, command};
use crate::icons::sf_symbol;

pub(crate) fn render(has_sessions: bool, colors: SemanticColors) -> impl IntoElement {
    div()
        .id("empty-workbench")
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .px(px(28.0))
        .py(px(32.0))
        .flex()
        .flex_col()
        .justify_center()
        .items_center()
        .child(
            div()
                .w_full()
                .max_w(px(420.0))
                .flex()
                .flex_col()
                .gap(px(24.0))
                .child(
                    div()
                        .size(px(48.0))
                        .flex_none()
                        .rounded(px(Radius::PANEL))
                        .bg(Palette::CLAY.alpha(0.12))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(sf_symbol("square.and.pencil", 23.0, Palette::CLAY)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .child(
                            div()
                                .text_size(px(28.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(colors.primary)
                                .child(if has_sessions { "Ready for your next task?" } else { "Your agents, one workspace." }),
                        )
                        .child(
                            div()
                                .text_size(px(14.0))
                                .line_height(px(22.0))
                                .text_color(colors.secondary)
                                .child(if has_sessions {
                                    "Pick up a session from the sidebar, or start something new."
                                } else {
                                    "Work with coding agents in your own projects. Give each task a session, and keep everything together in Diri."
                                }),
                        ),
                )
                .when(!has_sessions, |view| {
                    view.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(14.0))
                            .children([
                                ("folder", "Choose a project folder"),
                                ("terminal", "Pick a coding agent installed on your computer"),
                                ("bubble.left", "Describe what you want to work on"),
                            ].into_iter().map(|(icon, label)| {
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(12.0))
                                    .child(sf_symbol(icon, 14.0, colors.secondary))
                                    .child(div().text_size(px(13.0)).text_color(colors.secondary).child(label))
                            })),
                    )
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(14.0))
                        .child(
                            div()
                                .id("empty-start-session")
                                .debug_selector(|| "empty-start-session".into())
                                .role(Role::Button)
                                .aria_label("Start a session")
                                .h(px(40.0))
                                .px(px(16.0))
                                .rounded(px(Radius::ROW))
                                .bg(colors.primary)
                                .text_color(colors.background)
                                .text_size(px(13.0))
                                .font_weight(FontWeight::MEDIUM)
                                .flex()
                                .items_center()
                                .gap(px(10.0))
                                .cursor_pointer()
                                .hover(|button| button.opacity(0.88))
                                .active(|button| button.opacity(0.74))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(OpenLauncher), cx);
                                })
                                .child("Start a session")
                                .child(sf_symbol("chevron.right", 12.0, colors.background)),
                        )
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(colors.secondary)
                                .child(command(CommandId::OpenLauncher).shortcut_label().unwrap_or_default()),
                        ),
                )
                .when(has_sessions, |view| {
                    view.child(
                        div()
                            .id("empty-browse-sessions")
                            .role(Role::Button)
                            .aria_label("Show sessions")
                            .py(px(6.0))
                            .text_size(px(13.0))
                            .text_color(colors.secondary)
                            .cursor_pointer()
                            .hover(move |button| button.text_color(colors.primary))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(FocusSidebar), cx);
                            })
                            .child("Show sessions in the sidebar"),
                    )
                }),
        )
}
