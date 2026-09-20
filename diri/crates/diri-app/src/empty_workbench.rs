//! The resting workbench: what Diri is, whether this machine can run an agent
//! yet, and the one action that moves a newcomer forward.
//!
//! A first launch has no sessions and often no agent. Telling that person to
//! "pick a coding agent installed on your computer" strands them, so the page
//! reads detection facts and leads with whichever step is actually next:
//! install an agent, or start a session.
//!
//! Starting is direct: the default agent opens in the home folder, the same
//! launch as the New Agent shortcut, and a project folder is optional. The
//! agent's own prompt is where the task gets typed, so nothing here composes
//! or injects one.
//!
//! It is an empty state, not a landing page. It borrows the sidebar's empty
//! state and the Settings controls: a tertiary symbol, row-sized type, and the
//! same quiet bordered buttons, centered in the pane like any macOS
//! "nothing selected" view.

use std::rc::Rc;

use diri_proto::AgentKind;
use diri_ui::{Radius, SemanticColors, Typo};
use gpui::{AnyElement, Div, IntoElement, Role, SharedString, div, prelude::*, px};

use crate::agent_setup::{ActionHandler, AgentSetupState, InstallHandler, quiet_link, setup_list};
use crate::commands::{CommandId, NewDefaultSession, ShowAgentSettings, command};
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
        .py(px(28.0))
        .flex()
        .flex_col()
        .items_center()
        // `justify_center` would clip the top of a column taller than the
        // pane; spacers center it and still let it scroll from the top.
        .child(div().flex_1())
        .child(column)
        .child(div().flex_1())
}

fn column() -> Div {
    div()
        .w_full()
        .max_w(px(440.0))
        .flex_none()
        .flex()
        .flex_col()
        .items_center()
}

/// Symbol, title, and a sentence or two: the same three parts as the
/// sidebar's empty state, one step up in size because this is the main pane.
fn heading(
    symbol: &'static str,
    title: &'static str,
    body: impl Into<SharedString>,
    colors: SemanticColors,
) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(12.0))
        .child(sf_symbol(symbol, 28.0, colors.tertiary))
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(px(Typo::DISPLAY_TITLE.size))
                        .font_weight(Typo::DISPLAY_TITLE.weight)
                        .text_color(colors.primary)
                        .child(title),
                )
                .child(
                    div()
                        .max_w(px(440.0))
                        .text_center()
                        .text_size(px(Typo::ROW.size))
                        .line_height(px(19.0))
                        .text_color(colors.secondary)
                        .child(body.into()),
                ),
        )
}

fn welcome(state: &EmptyWorkbench, actions: &EmptyWorkbenchActions, colors: SemanticColors) -> Div {
    // Not the sidebar's stack: on a first launch the two empty states sit
    // side by side, and twin symbols read as a rendering mistake.
    const SYMBOL: &str = "rectangle.split.2x1";
    const TITLE: &str = "Run coding agents side by side";
    let AgentSetupState::Missing(candidates) = &state.agents else {
        return column()
            .gap(px(18.0))
            .child(heading(
                SYMBOL,
                TITLE,
                "Each task gets its own session. Diri tells you when one needs you.",
                colors,
            ))
            .child(start_controls("Start a session", actions, colors));
    };
    let check_again = Rc::clone(&actions.check_again);
    column()
        .gap(px(18.0))
        .child(heading(
            SYMBOL,
            TITLE,
            "Install a coding agent to get started.",
            colors,
        ))
        // The sentence above may run wide; the list stays the width of a
        // Settings group so its Install buttons sit near their names.
        .child(div().w_full().max_w(px(320.0)).child(setup_list(
            "welcome",
            candidates,
            state.installing.as_ref(),
            colors,
            &actions.install,
        )))
        .child(
            div()
                .flex()
                .justify_center()
                .items_center()
                .gap(px(16.0))
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
                )),
        )
}

/// The app's standard bordered control, with the shortcut that does the same
/// thing set inside it the way a menu item carries its key equivalent.
fn start_button(label: &'static str, colors: SemanticColors) -> AnyElement {
    div()
        .id("empty-start-session")
        .debug_selector(|| "empty-start-session".into())
        .role(Role::Button)
        .aria_label(label)
        .h(px(28.0))
        .px(px(11.0))
        .rounded(px(Radius::BADGE))
        .border_1()
        .border_color(colors.primary.alpha(0.12))
        .bg(colors.primary.alpha(0.06))
        .flex()
        .items_center()
        .gap(px(7.0))
        .text_size(px(12.0))
        .font_weight(Typo::ROW_EMPHASIZED.weight)
        .text_color(colors.primary)
        .cursor_pointer()
        .hover(move |button| button.bg(colors.primary.alpha(0.10)))
        .active(move |button| button.bg(colors.primary.alpha(0.14)))
        .on_click(|_, window, cx| window.dispatch_action(Box::new(NewDefaultSession), cx))
        .child(sf_symbol("plus", 11.0, colors.primary))
        .child(label)
        .when_some(
            command(CommandId::NewDefaultSession).shortcut_label(),
            |button, shortcut| {
                button.child(
                    div()
                        .pl(px(3.0))
                        .text_size(px(Typo::META.size))
                        .text_color(colors.tertiary)
                        .child(shortcut),
                )
            },
        )
        .into_any_element()
}

/// One press starts working: the default agent opens where the New Agent
/// shortcut would put it. A project folder is an option, not a gate, because
/// plenty of first tasks have no project yet.
fn start_controls(
    label: &'static str,
    actions: &EmptyWorkbenchActions,
    colors: SemanticColors,
) -> Div {
    let start_in_folder = Rc::clone(&actions.start_in_folder);
    div()
        .flex()
        .flex_wrap()
        .justify_center()
        .items_center()
        .gap_x(px(14.0))
        .gap_y(px(8.0))
        .child(start_button(label, colors))
        .child(quiet_link(
            "empty-start-in-folder",
            "Choose a folder…",
            Some("folder"),
            colors,
            move |window, cx| start_in_folder(window, cx),
        ))
}

/// Sessions exist but none is open in this pane.
fn resting(actions: &EmptyWorkbenchActions, colors: SemanticColors) -> Div {
    column()
        .gap(px(16.0))
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(12.0))
                .child(sf_symbol("square.stack.3d.up", 28.0, colors.tertiary))
                .child(
                    div()
                        .text_size(px(Typo::DISPLAY_TITLE.size))
                        .font_weight(Typo::DISPLAY_TITLE.weight)
                        .text_color(colors.primary)
                        .child("No session open"),
                ),
        )
        .child(start_controls("New session", actions, colors))
}

#[cfg(test)]
mod tests {
    use diri_ui::SemanticColors;
    use gpui::{Context, IntoElement, Render, Window, div, prelude::*, px};

    use super::*;

    /// The pane's pages on their own, in both themes, without a daemon or
    /// personal state. `render_first_run_window_screenshots` in `root.rs`
    /// shows the same pages inside the whole window.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes first-experience visual review artifacts"]
    fn render_first_experience_screenshots() {
        use gpui::AppContext as _;

        struct Page {
            colors: SemanticColors,
            installed: &'static [&'static str],
            installing: Option<AgentKind>,
            has_sessions: bool,
        }
        impl Render for Page {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let catalog = crate::agent_setup::bundled_catalog(self.installed);
                div()
                    .size_full()
                    .bg(self.colors.background)
                    .font_family(crate::fonts::ui_family())
                    .flex()
                    .flex_col()
                    .child(render(
                        EmptyWorkbench {
                            has_sessions: self.has_sessions,
                            agents: AgentSetupState::from_catalog(Some(&catalog)),
                            installing: self.installing.clone(),
                            scanning: false,
                        },
                        EmptyWorkbenchActions {
                            install: Rc::new(|_, _| {}),
                            check_again: Rc::new(|_, _| {}),
                            start_in_folder: Rc::new(|_, _| {}),
                        },
                        self.colors,
                    ))
            }
        }

        let output = std::path::PathBuf::from(
            std::env::var_os("DIRI_VISUAL_OUTPUT").expect("set DIRI_VISUAL_OUTPUT directory"),
        );
        std::fs::create_dir_all(&output).unwrap();
        let ready: &[&str] = &["claude-code", "codex"];
        for (theme, width, height) in [
            ("dirijor-dark", 1100.0, 700.0),
            ("dirijor-light", 760.0, 560.0),
        ] {
            let platform = gpui_platform::current_platform(true);
            let mut cx = gpui::HeadlessAppContext::with_platform(
                platform.text_system(),
                std::sync::Arc::new(diri_ui::IconAssets),
                gpui_platform::current_headless_renderer,
            );
            cx.update(|cx| {
                crate::fonts::init(cx);
                cx.set_reduce_motion(true);
            });
            for (name, installed, installing, has_sessions) in [
                ("welcome", &[][..], None, false),
                (
                    "welcome-installing",
                    &[][..],
                    Some(AgentKind::CLAUDE_CODE),
                    false,
                ),
                ("welcome-ready", ready, None, false),
                ("resting", ready, None, true),
            ] {
                let page = cx
                    .open_window(gpui::size(px(width), px(height)), |_, cx| {
                        cx.new(|_| Page {
                            colors: crate::app_theme::colors(theme),
                            installed,
                            installing,
                            has_sessions,
                        })
                    })
                    .unwrap();
                cx.run_until_parked();
                cx.capture_screenshot(page.into())
                    .unwrap()
                    .save(output.join(format!("{theme}-{name}.png")))
                    .unwrap();
                page.update(&mut cx, |_, window, _| window.remove_window())
                    .unwrap();
                cx.run_until_parked();
            }
        }
    }
}
