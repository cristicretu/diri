//! The resting workbench: what Diri is, whether this machine can run an agent
//! yet, and the one action that moves a newcomer forward.
//!
//! A first launch has no sessions and often no agent. Telling that person to
//! "pick a coding agent installed on your computer" strands them, so the page
//! reads detection facts and leads with whichever step is actually next:
//! install an agent, or start a session.
//!
//! Starting is direct: choose a folder and the default agent opens in it, the
//! same launch as the New Agent shortcut. The agent's own prompt is where the
//! task gets typed, so nothing here composes or injects one.

use std::rc::Rc;

use diri_proto::AgentKind;
use diri_ui::{Palette, Radius, SemanticColors};
use gpui::{Div, FontWeight, IntoElement, Role, div, prelude::*, px};

use crate::agent_setup::{
    ActionHandler, AgentSetupState, InstallHandler, quiet_link, ready_line, setup_list,
};
use crate::commands::{
    CommandId, FocusSidebar, NewDefaultSession, NewTerminal, ShowAgentSettings, command,
};
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
    /// Pick a project folder, then open the default agent there.
    pub start_in_folder: ActionHandler,
}

pub(crate) fn render(
    state: EmptyWorkbench,
    actions: EmptyWorkbenchActions,
    colors: SemanticColors,
) -> impl IntoElement {
    let column = if state.has_sessions {
        resting(&actions, colors)
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
            .child(choose_folder(actions, colors)),
        AgentSetupState::Ready(ready) => section
            .child(eyebrow(format!("Ready on {machine}"), colors))
            .child(ready_line(ready, colors))
            .child(div().pt(px(6.0)).child(choose_folder(actions, colors))),
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
                            |window, cx| window.dispatch_action(Box::new(NewTerminal), cx),
                        )),
                )
        }
    }
}

/// The filled button a page leads with, and the shortcut that does the same.
fn start_button(
    label: &'static str,
    symbol: &'static str,
    shortcut: Option<CommandId>,
    colors: SemanticColors,
    on_click: ActionHandler,
) -> Div {
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
                .gap(px(9.0))
                .cursor_pointer()
                .hover(|button| button.opacity(0.88))
                .active(|button| button.opacity(0.74))
                .on_click(move |_, window, cx| on_click(window, cx))
                .child(sf_symbol(symbol, 14.0, colors.background))
                .child(label),
        )
        .when_some(
            shortcut.and_then(|id| command(id).shortcut_label()),
            |row, shortcut| {
                row.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(colors.secondary)
                        .child(shortcut),
                )
            },
        )
}

/// Choosing the folder is the whole first step: the agent opens there.
fn choose_folder(actions: &EmptyWorkbenchActions, colors: SemanticColors) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(start_button(
            "Choose a project folder",
            "folder",
            None,
            colors,
            Rc::clone(&actions.start_in_folder),
        ))
        .child(
            div()
                .text_size(px(12.0))
                .line_height(px(18.0))
                .text_color(colors.secondary)
                .child(
                    "Your agent opens in that folder. Tell it what you want done, in plain words.",
                ),
        )
}

/// Sessions exist but none is open in this pane.
fn resting(actions: &EmptyWorkbenchActions, colors: SemanticColors) -> Div {
    let start_in_folder = Rc::clone(&actions.start_in_folder);
    column()
        .gap(px(24.0))
        .child(mark())
        .child(headline(
            "Ready for your next task?",
            "Pick up a session from the sidebar, or start a new one.",
            colors,
        ))
        .child(start_button(
            "New session",
            "plus",
            Some(CommandId::NewDefaultSession),
            colors,
            Rc::new(|window, cx| window.dispatch_action(Box::new(NewDefaultSession), cx)),
        ))
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(18.0))
                .child(quiet_link(
                    "empty-start-in-folder",
                    "Start in another folder…",
                    None,
                    colors,
                    move |window, cx| start_in_folder(window, cx),
                ))
                .child(quiet_link(
                    "empty-browse-sessions",
                    "Show sessions in the sidebar",
                    None,
                    colors,
                    |window, cx| window.dispatch_action(Box::new(FocusSidebar), cx),
                )),
        )
}
