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
use diri_ui::{AgentLogo, Radius, SemanticColors, Typo};
use gpui::{AnyElement, App, Div, IntoElement, Role, SharedString, Window, div, prelude::*, px};

use crate::agent_catalog::AgentOption;
use crate::agent_setup::{ActionHandler, AgentSetupState, InstallHandler, quiet_link, setup_list};
use crate::commands::{CommandId, NewDefaultSession, ShowAgentSettings, command};
use crate::i18n::{t, tf};
use crate::icons::sf_symbol;

pub(crate) struct EmptyWorkbench {
    pub has_sessions: bool,
    pub agents: AgentSetupState,
    /// The Agent the start button opens: the same one ⌘T would.
    pub default_agent: AgentKind,
    pub installing: Option<AgentKind>,
    /// A detection scan is in flight, so "Check again" reads as busy.
    pub scanning: bool,
    /// herdr sessions this Mac could bring over, as "5 sessions from herdr".
    pub herdr: Option<SharedString>,
    pub importing_herdr: bool,
    /// Another terminal whose setup this Mac could bring over ("Ghostty").
    pub terminal_setup: Option<&'static str>,
}

pub(crate) struct EmptyWorkbenchActions {
    pub install: InstallHandler,
    pub check_again: ActionHandler,
    /// Pick a project folder, then open the default agent there.
    pub start_in_folder: ActionHandler,
    /// Confirm, then open every herdr pane as a session.
    pub import_herdr: ActionHandler,
    /// Open one particular installed Agent in the home folder.
    pub start_agent: AgentHandler,
}

pub(crate) type AgentHandler = Rc<dyn Fn(&AgentKind, &mut Window, &mut App)>;

pub(crate) fn render(
    state: EmptyWorkbench,
    actions: EmptyWorkbenchActions,
    colors: SemanticColors,
) -> impl IntoElement {
    let column = if state.has_sessions {
        resting(&actions, colors)
    } else {
        welcome(&state, &actions, colors)
            .when_some(terminal_setup_link(&state, colors), |column, link| {
                column.child(div().pt(px(10.0)).child(link))
            })
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
    let title = t("empty.welcome.title");
    match &state.agents {
        AgentSetupState::Missing(candidates) => missing(state, candidates, actions, colors),
        AgentSetupState::Ready(ready) => {
            let lead = ready
                .iter()
                .find(|option| option.kind == state.default_agent);
            // A newcomer whose agent has never been signed in meets its login
            // screen first; saying so beats a button that promises a session.
            let signed_out = lead.filter(|option| option.signed_in == Some(false));
            let body: SharedString = match signed_out {
                Some(option) => tf(
                    "empty.welcome.sign_in_body",
                    &[("agent", &option.display_name)],
                )
                .into(),
                None => t("empty.welcome.body").into(),
            };
            let label: SharedString = match (lead, signed_out) {
                (_, Some(option)) => {
                    tf("empty.welcome.sign_in", &[("agent", &option.display_name)]).into()
                }
                (Some(option), None) => tf(
                    "empty.welcome.start_agent",
                    &[("agent", &option.display_name)],
                )
                .into(),
                (None, None) => t("empty.welcome.start_session").into(),
            };
            let others: Vec<&AgentOption> = ready
                .iter()
                .filter(|option| Some(option.kind.clone()) != lead.map(|lead| lead.kind.clone()))
                .take(MAX_OTHER_AGENTS)
                .collect();
            column()
                .gap(px(18.0))
                .child(heading(SYMBOL, title, body, colors))
                .child(start_controls(
                    label,
                    lead.map(|option| &option.kind),
                    actions,
                    colors,
                ))
                .when(!others.is_empty(), |column| {
                    column.child(other_agents(&others, actions, colors))
                })
                .when_some(herdr_link(state, actions, colors), |column, link| {
                    column.child(link)
                })
        }
        AgentSetupState::Checking => column()
            .gap(px(18.0))
            .child(heading(SYMBOL, title, t("empty.welcome.body"), colors))
            .child(start_controls(
                t("empty.welcome.start_session").into(),
                None,
                actions,
                colors,
            )),
    }
}

/// Installed Agents beyond the default, one quiet link each: the second agent
/// someone has is usually why they wanted Diri.
const MAX_OTHER_AGENTS: usize = 3;

fn other_agents(
    others: &[&AgentOption],
    actions: &EmptyWorkbenchActions,
    colors: SemanticColors,
) -> Div {
    let mut row = div()
        .flex()
        .flex_wrap()
        .justify_center()
        .items_center()
        .gap_x(px(10.0))
        .gap_y(px(6.0))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(colors.tertiary)
                .child(t("empty.welcome.or_start")),
        );
    for option in others {
        let start = Rc::clone(&actions.start_agent);
        let kind = option.kind.clone();
        let id = format!("empty-start-{}", option.kind.id());
        row = row.child(
            div()
                .id(SharedString::from(id.clone()))
                .debug_selector(move || id.clone())
                .role(Role::Button)
                .aria_label(SharedString::from(tf(
                    "empty.welcome.start_agent",
                    &[("agent", &option.display_name)],
                )))
                .flex()
                .items_center()
                .gap(px(4.0))
                .text_size(px(11.0))
                .text_color(colors.secondary)
                .cursor_pointer()
                .hover(move |link| link.text_color(colors.primary))
                .on_click(move |_, window, cx| start(&kind, window, cx))
                .child(
                    AgentLogo::new(crate::surface_shell::ui_agent(&option.kind), 14.0, colors)
                        .badged(false),
                )
                .child(option.display_name.clone()),
        );
    }
    row
}

fn missing(
    state: &EmptyWorkbench,
    candidates: &[AgentOption],
    actions: &EmptyWorkbenchActions,
    colors: SemanticColors,
) -> Div {
    const SYMBOL: &str = "rectangle.split.2x1";
    let title = t("empty.welcome.title");
    let check_again = Rc::clone(&actions.check_again);
    column()
        .gap(px(18.0))
        .child(heading(
            SYMBOL,
            title,
            t("empty.welcome.install_agent"),
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
                        t("empty.welcome.checking")
                    } else {
                        t("empty.welcome.check_again")
                    },
                    Some("arrow.counterclockwise"),
                    colors,
                    move |window, cx| check_again(window, cx),
                ))
                .child(quiet_link(
                    "welcome-agent-settings",
                    t("empty.welcome.more_agents"),
                    None,
                    colors,
                    |window, cx| window.dispatch_action(Box::new(ShowAgentSettings), cx),
                )),
        )
}

/// Someone arriving from another terminal wants it to look and feel the
/// same: one quiet line opens the import sheet, which shows every change.
fn terminal_setup_link(state: &EmptyWorkbench, colors: SemanticColors) -> Option<AnyElement> {
    let terminal = state.terminal_setup?;
    Some(quiet_link(
        "empty-import-terminal-setup",
        tf("terminal_import.welcome.link", &[("terminal", &terminal)]),
        Some("arrow.down"),
        colors,
        |window, cx| {
            window.dispatch_action(Box::new(crate::commands::ImportTerminalSetup), cx);
        },
    ))
}

/// Someone arriving from herdr already has work in flight: one quiet line
/// brings it over. It only appears once an agent is ready, because a
/// resumed conversation needs its agent installed.
fn herdr_link(
    state: &EmptyWorkbench,
    actions: &EmptyWorkbenchActions,
    colors: SemanticColors,
) -> Option<AnyElement> {
    if state.importing_herdr {
        return Some(quiet_link(
            "empty-import-herdr",
            t("empty.welcome.importing_herdr"),
            Some("arrow.down"),
            colors,
            |_, _| {},
        ));
    }
    let headline = state.herdr.as_ref()?;
    let import = Rc::clone(&actions.import_herdr);
    Some(quiet_link(
        "empty-import-herdr",
        tf("empty.welcome.import_herdr", &[("headline", headline)]),
        Some("arrow.down"),
        colors,
        move |window, cx| import(window, cx),
    ))
}

/// The app's standard bordered control, with the shortcut that does the same
/// thing set inside it the way a menu item carries its key equivalent. It
/// names the Agent it opens, with that Agent's mark, so the first press holds
/// no surprise.
fn start_button(
    label: SharedString,
    agent: Option<&AgentKind>,
    colors: SemanticColors,
) -> AnyElement {
    div()
        .id("empty-start-session")
        .debug_selector(|| "empty-start-session".into())
        .role(Role::Button)
        .aria_label(label.clone())
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
        .map(|button| match agent.filter(|kind| !kind.is_terminal()) {
            Some(kind) => button.child(
                AgentLogo::new(crate::surface_shell::ui_agent(kind), 16.0, colors).badged(false),
            ),
            None => button.child(sf_symbol("plus", 11.0, colors.primary)),
        })
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
    label: SharedString,
    agent: Option<&AgentKind>,
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
        .child(start_button(label, agent, colors))
        .child(quiet_link(
            "empty-start-in-folder",
            t("empty.choose_folder"),
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
                        .child(t("empty.resting.title")),
                ),
        )
        .child(start_controls(
            t("empty.resting.new_session").into(),
            None,
            actions,
            colors,
        ))
}
