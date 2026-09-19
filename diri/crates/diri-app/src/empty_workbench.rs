//! The resting workbench: what Diri is, whether this machine can run an agent
//! yet, and the one action that moves a newcomer forward.
//!
//! A first launch has no sessions and often no agent. Telling that person to
//! "pick a coding agent installed on your computer" strands them, so the page
//! reads detection facts and leads with whichever step is actually next:
//! install an agent, or start a session.

use std::rc::Rc;

use diri_proto::AgentKind;
use diri_ui::{Palette, Radius, SemanticColors};
use gpui::{Div, FontWeight, IntoElement, Role, div, prelude::*, px};

use crate::agent_setup::{
    ActionHandler, AgentSetupState, InstallHandler, quiet_link, ready_line, setup_list,
};
use crate::commands::{CommandId, FocusSidebar, OpenLauncher, ShowAgentSettings, command};
use crate::icons::sf_symbol;

pub(crate) struct EmptyWorkbench {
    pub has_sessions: bool,
    pub agents: AgentSetupState,
    pub installing: Option<AgentKind>,
    /// A detection scan is in flight, so "Check again" reads as busy.
    pub scanning: bool,
}

pub(crate) struct EmptyWorkbenchActions {
    pub install: InstallHandler,
    pub check_again: ActionHandler,
}

pub(crate) fn render(
    state: EmptyWorkbench,
    actions: EmptyWorkbenchActions,
    colors: SemanticColors,
) -> impl IntoElement {
    let column = if state.has_sessions {
        resting(colors)
    } else {
        welcome(&state, &actions, colors)
    };
    div()
        .id("empty-workbench")
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .px(px(28.0))
        .py(px(32.0))
        .flex()
        .flex_col()
        .items_center()
        // `justify_center` would clip the top of a column taller than the
        // pane; auto margins center it and still let it scroll from the top.
        .child(div().flex_1())
        .child(column)
        .child(div().flex_1())
}

fn column() -> Div {
    div()
        .w_full()
        .max_w(px(460.0))
        .flex_none()
        .flex()
        .flex_col()
}

fn mark() -> Div {
    div()
        .size(px(44.0))
        .flex_none()
        .rounded(px(Radius::PANEL))
        .bg(Palette::CLAY.alpha(0.12))
        .flex()
        .items_center()
        .justify_center()
        .child(sf_symbol("rectangle.stack", 21.0, Palette::CLAY))
}

fn headline(title: &'static str, body: &'static str, colors: SemanticColors) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(
            div()
                .text_size(px(26.0))
                .line_height(px(32.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(colors.primary)
                .child(title),
        )
        .child(
            div()
                .text_size(px(14.0))
                .line_height(px(22.0))
                .text_color(colors.secondary)
                .child(body),
        )
}

fn welcome(state: &EmptyWorkbench, actions: &EmptyWorkbenchActions, colors: SemanticColors) -> Div {
    column()
        .gap(px(24.0))
        .child(headline(
            "Run coding agents side by side.",
            "Diri is a workspace for AI coding agents like Claude Code and Codex. \
             Hand each task to its own session, keep several going at once, and \
             see which one is working, finished, or waiting on you.",
            colors,
        ))
        .child(
            div().flex().flex_col().gap(px(10.0)).children(
                [
                    (
                        "rectangle.stack",
                        "One session per task, in any project folder",
                    ),
                    (
                        "bell",
                        "A heads-up when an agent finishes or needs your answer",
                    ),
                    (
                        "moon.fill",
                        "Sessions keep running after you close the window",
                    ),
                ]
                .into_iter()
                .map(|(icon, label)| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(12.0))
                        .child(
                            div()
                                .w(px(16.0))
                                .flex_none()
                                .flex()
                                .justify_center()
                                .child(sf_symbol(icon, 14.0, colors.secondary)),
                        )
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(colors.secondary)
                                .child(label),
                        )
                }),
            ),
        )
        .child(agents_section(state, actions, colors))
}

fn eyebrow(label: impl Into<gpui::SharedString>, colors: SemanticColors) -> Div {
    div()
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(colors.tertiary)
        .child(label.into())
}

fn agents_section(
    state: &EmptyWorkbench,
    actions: &EmptyWorkbenchActions,
    colors: SemanticColors,
) -> Div {
    let machine = crate::platform::local_machine_label_lowercase();
    let section = div()
        .pt(px(22.0))
        .border_t_1()
        .border_color(colors.primary.alpha(0.07))
        .flex()
        .flex_col()
        .gap(px(14.0));
    match &state.agents {
        AgentSetupState::Checking => section
            .child(eyebrow(
                format!("Looking for coding agents on {machine}…"),
                colors,
            ))
            .child(start_button("Start a session", true, colors)),
        AgentSetupState::Ready(ready) => section
            .child(eyebrow(format!("Ready on {machine}"), colors))
            .child(ready_line(ready, colors))
            .child(
                div()
                    .pt(px(6.0))
                    .child(start_button("Start your first session", true, colors)),
            ),
        AgentSetupState::Missing(candidates) => {
            let check_again = Rc::clone(&actions.check_again);
            section
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(5.0))
                        .child(
                            div()
                                .text_size(px(14.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(colors.primary)
                                .child("First, install a coding agent"),
                        )
                        .child(
                            div()
                                .text_size(px(12.0))
                                .line_height(px(18.0))
                                .text_color(colors.secondary)
                                .child(format!(
                                    "None found on {machine} yet. Each official installer \
                                     runs in a tab you can watch."
                                )),
                        ),
                )
                .child(setup_list(
                    "welcome",
                    candidates,
                    state.installing.as_ref(),
                    colors,
                    &actions.install,
                ))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(18.0))
                        .child(quiet_link(
                            "welcome-check-again",
                            if state.scanning {
                                "Checking…"
                            } else {
                                "Check again"
                            },
                            Some("arrow.counterclockwise"),
                            colors,
                            move |window, cx| check_again(window, cx),
                        ))
                        .child(quiet_link(
                            "welcome-agent-settings",
                            "More agents…",
                            None,
                            colors,
                            |window, cx| window.dispatch_action(Box::new(ShowAgentSettings), cx),
                        ))
                        .child(quiet_link(
                            "welcome-skip",
                            "Open a terminal instead",
                            None,
                            colors,
                            |window, cx| window.dispatch_action(Box::new(OpenLauncher), cx),
                        )),
                )
        }
    }
}

fn start_button(label: &'static str, show_shortcut: bool, colors: SemanticColors) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(14.0))
        .child(
            div()
                .id("empty-start-session")
                .debug_selector(|| "empty-start-session".into())
                .role(Role::Button)
                .aria_label(label)
                .h(px(38.0))
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
                .child(label)
                .child(sf_symbol("chevron.right", 12.0, colors.background)),
        )
        .when(show_shortcut, |row| {
            row.child(
                div()
                    .text_size(px(12.0))
                    .text_color(colors.secondary)
                    .child(
                        command(CommandId::OpenLauncher)
                            .shortcut_label()
                            .unwrap_or_default(),
                    ),
            )
        })
}

/// Sessions exist but none is open in this pane.
fn resting(colors: SemanticColors) -> Div {
    column()
        .gap(px(24.0))
        .child(mark())
        .child(headline(
            "Ready for your next task?",
            "Pick up a session from the sidebar, or start something new.",
            colors,
        ))
        .child(start_button("Start a session", true, colors))
        .child(
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
}
