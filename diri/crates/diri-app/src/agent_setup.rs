//! Setup rows for coding agents that are not installed yet.
//!
//! The welcome screen, the launcher, and Settings all answer "how do I get an
//! agent?" with the same rows, so a newcomer sees one vocabulary: the Agent,
//! the exact installer Diri would run, and one button that runs it in a
//! Terminal tab they can watch.

use std::rc::Rc;

use diri_proto::AgentKind;
use diri_ui::{AgentLogo, Radius, SemanticColors};
use gpui::{AnyElement, App, Div, FontWeight, Role, Window, div, prelude::*, px};

use crate::agent_catalog::AgentOption;
use crate::icons::sf_symbol;

pub(crate) type InstallHandler = Rc<dyn Fn(&AgentOption, &mut Window, &mut App)>;
pub(crate) type ActionHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// What a setup surface knows about coding agents on the selected target.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum AgentSetupState {
    /// Detection has not answered yet; claim nothing.
    #[default]
    Checking,
    /// At least one Agent can launch. Carries display names in catalog order.
    Ready(Vec<AgentOption>),
    /// Nothing is installed; these are the Agents worth offering.
    Missing(Vec<AgentOption>),
}

impl AgentSetupState {
    /// The first rows are the ones Diri can install by itself; three keeps a
    /// welcome from turning into a catalog. Settings lists the rest.
    const CANDIDATES: usize = 3;

    pub(crate) fn from_catalog(catalog: Option<&diri_proto::AgentReadinessResult>) -> Self {
        let Some(catalog) = catalog else {
            return Self::Checking;
        };
        let ready: Vec<_> = crate::agent_catalog::agent_options(catalog)
            .into_iter()
            .filter(|option| option.available)
            .collect();
        if ready.is_empty() {
            Self::Missing(crate::agent_catalog::setup_candidates(
                catalog,
                Self::CANDIDATES,
            ))
        } else {
            Self::Ready(ready)
        }
    }
}

/// A bordered list of setup rows. `prefix` keeps element ids unique when two
/// surfaces are alive at once.
pub(crate) fn setup_list(
    prefix: &'static str,
    candidates: &[AgentOption],
    installing: Option<&AgentKind>,
    colors: SemanticColors,
    on_install: &InstallHandler,
) -> Div {
    let mut list = div()
        .w_full()
        .flex()
        .flex_col()
        .rounded(px(Radius::CARD))
        .border_1()
        .border_color(colors.primary.alpha(0.07))
        .bg(colors.primary.alpha(0.025))
        .overflow_hidden();
    for (index, option) in candidates.iter().enumerate() {
        if index > 0 {
            list = list.child(div().h(px(1.0)).bg(colors.primary.alpha(0.06)));
        }
        list = list.child(setup_row(
            prefix,
            option,
            index == 0,
            installing == Some(&option.kind),
            colors,
            on_install,
        ));
    }
    list
}

/// The command is shown whole, wrapping if it must: Install types exactly
/// this text, so an ellipsis would hide part of what the user is agreeing to.
fn setup_row(
    prefix: &'static str,
    option: &AgentOption,
    recommended: bool,
    installing: bool,
    colors: SemanticColors,
    on_install: &InstallHandler,
) -> AnyElement {
    let id = option.kind.id().to_owned();
    let detail = if installing {
        div()
            .text_size(px(11.0))
            .text_color(colors.secondary)
            .child("Installing in its own tab. Diri notices when it finishes.")
    } else if let Some(install) = &option.install {
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x(px(7.0))
            .gap_y(px(2.0))
            .when_some(install.requirement.clone(), |detail, requirement| {
                detail.child(
                    div()
                        .flex_none()
                        .text_size(px(11.0))
                        .text_color(colors.secondary)
                        .child(format!("Needs {requirement}")),
                )
            })
            .child(
                div()
                    .min_w(px(0.0))
                    .whitespace_normal()
                    .font_family(crate::fonts::mono_family())
                    .text_size(px(10.0))
                    .line_height(px(14.0))
                    .text_color(colors.tertiary)
                    .child(install.command.clone()),
            )
    } else {
        div()
            .text_size(px(11.0))
            .text_color(colors.tertiary)
            .child("Install it from the official guide, then check again.")
    };

    let mut actions = div().flex_none().flex().items_center().gap(px(4.0));
    if let Some(url) = option.setup_url.clone() {
        actions = actions.child(
            div()
                .id(format!("{prefix}-guide-{id}"))
                .debug_selector({
                    let selector = format!("{prefix}-guide-{id}");
                    move || selector.clone()
                })
                .role(Role::Button)
                .aria_label(format!("Open the {} setup guide", option.display_name))
                .h(px(24.0))
                .px(px(7.0))
                .rounded(px(Radius::CHIP))
                .flex()
                .items_center()
                .text_size(px(11.0))
                .text_color(colors.secondary)
                .cursor_pointer()
                .hover(move |button| {
                    button
                        .bg(colors.primary.alpha(0.06))
                        .text_color(colors.primary)
                })
                .on_click(move |_, _, cx| cx.open_url(&url))
                .child("Guide"),
        );
    }
    if option.install.is_some() && !installing {
        let handler = Rc::clone(on_install);
        let target = option.clone();
        actions = actions.child(
            div()
                .id(format!("{prefix}-install-{id}"))
                .debug_selector({
                    let selector = format!("{prefix}-install-{id}");
                    move || selector.clone()
                })
                .role(Role::Button)
                .aria_label(format!("Install {}", option.display_name))
                // The Settings control, not a call-to-action slab. The first
                // row is the shortest path, so it alone gets the stronger fill.
                .h(px(24.0))
                .px(px(9.0))
                .rounded(px(Radius::CHIP))
                .border_1()
                .border_color(colors.primary.alpha(if recommended { 0.16 } else { 0.10 }))
                .bg(colors.primary.alpha(if recommended { 0.10 } else { 0.04 }))
                .text_color(colors.primary)
                .text_size(px(11.0))
                .font_weight(FontWeight::MEDIUM)
                .flex()
                .items_center()
                .cursor_pointer()
                .hover(move |button| button.bg(colors.primary.alpha(0.14)))
                .active(|button| button.opacity(0.74))
                .on_click(move |_, window, cx| handler(&target, window, cx))
                .child("Install"),
        );
    }

    div()
        .px(px(11.0))
        .py(px(7.0))
        .flex()
        .gap(px(10.0))
        .child(
            div().h(px(24.0)).flex_none().flex().items_center().child(
                AgentLogo::new(crate::surface_shell::ui_agent(&option.kind), 18.0, colors)
                    .badged(false),
            ),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap(px(1.0))
                .child(
                    div()
                        .h(px(24.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(8.0))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(colors.primary)
                                .child(option.display_name.clone()),
                        )
                        .child(actions),
                )
                .child(detail),
        )
        .into_any_element()
}

/// "Claude Code and Codex are ready", for a line of meta text. The page is
/// an empty state, so installed Agents are a fact to mention, not a gallery.
pub(crate) fn ready_names(ready: &[AgentOption]) -> String {
    const NAMED: usize = 2;
    let names: Vec<_> = ready
        .iter()
        .take(NAMED)
        .map(|option| option.display_name.as_str())
        .collect();
    match (names.as_slice(), ready.len().saturating_sub(NAMED)) {
        ([], _) => "No agent is ready".to_owned(),
        ([only], 0) => format!("{only} is ready"),
        ([first, second], 0) => format!("{first} and {second} are ready"),
        (shown, more) => format!("{} and {more} more are ready", shown.join(", ")),
    }
}

/// A text-weight control for the quiet actions under a setup list.
pub(crate) fn quiet_link(
    id: &'static str,
    label: &'static str,
    symbol: Option<&'static str>,
    colors: SemanticColors,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .role(Role::Button)
        .aria_label(label)
        .flex()
        .items_center()
        .gap(px(5.0))
        .text_size(px(11.0))
        .text_color(colors.secondary)
        .cursor_pointer()
        .hover(move |link| link.text_color(colors.primary))
        .on_click(move |_, window, cx| on_click(window, cx))
        .when_some(symbol, |link, symbol| {
            link.child(sf_symbol(symbol, 10.0, colors.secondary))
        })
        .child(label)
        .into_any_element()
}

/// The shipped manifests as a readiness result, for fixtures and tests that
/// must show what a real newcomer sees rather than a hand-written roster.
#[cfg(test)]
pub(crate) fn bundled_catalog(installed: &[&str]) -> diri_proto::AgentReadinessResult {
    let (engine, failed) =
        diri_engine::detect::ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir())
            .expect("bundled manifests");
    assert!(failed.is_empty(), "manifests failed to decode: {failed:?}");
    let mut agents: Vec<_> = engine
        .ids()
        .into_iter()
        .filter_map(|id| {
            let raw = engine.raw_agent(id)?;
            let binary = raw.get("binary")?.as_str()?.to_owned();
            let order = raw
                .get("catalogOrder")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(u64::MAX);
            let ready = installed.contains(&id);
            Some((
                order,
                diri_proto::AgentReadinessItem {
                    kind: AgentKind::new(id),
                    path: ready.then(|| format!("/usr/local/bin/{binary}")),
                    path_source: ready.then_some(diri_proto::AgentPathSource::SystemPath),
                    binary,
                    show_in_quick_create: ready,
                    descriptor: serde_json::from_value(raw.clone()).ok(),
                    ..diri_proto::AgentReadinessItem::default()
                },
            ))
        })
        .collect();
    agents.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.kind.id().cmp(right.1.kind.id()))
    });
    diri_proto::AgentReadinessResult {
        agents: agents.into_iter().map(|(_, item)| item).collect(),
        ..diri_proto::AgentReadinessResult::default()
    }
}

#[cfg(test)]
mod tests {
    use diri_proto::{AgentDescriptor, AgentReadinessItem, AgentReadinessResult, AgentSetup};

    use super::*;

    fn item(id: &str, installed: bool, command: Option<&str>) -> AgentReadinessItem {
        AgentReadinessItem {
            kind: AgentKind::new(id),
            binary: id.to_owned(),
            path: installed.then(|| format!("/bin/{id}")),
            descriptor: Some(AgentDescriptor {
                id: id.to_owned(),
                display_name: id.to_owned(),
                first_class: true,
                setup: Some(AgentSetup {
                    url: Some(format!("https://example.com/{id}")),
                    install_command: command.map(str::to_owned),
                    ..AgentSetup::default()
                }),
                ..AgentDescriptor::default()
            }),
            ..AgentReadinessItem::default()
        }
    }

    #[test]
    fn a_mac_with_no_agents_is_offered_installers_with_claude_code_first() {
        let AgentSetupState::Missing(rows) =
            AgentSetupState::from_catalog(Some(&bundled_catalog(&[])))
        else {
            panic!("nothing installed means missing");
        };
        assert_eq!(rows.len(), AgentSetupState::CANDIDATES);
        assert_eq!(rows[0].kind.id(), "claude-code");
        assert!(
            rows.iter().all(|row| row.install.is_some()),
            "every offered row should be one click: {rows:?}"
        );
        assert!(
            rows.iter()
                .all(|row| row.install.as_ref().unwrap().requirement.is_none()),
            "a newcomer without Node.js can finish any offered install: {rows:?}"
        );
    }

    #[test]
    fn unknown_detection_claims_neither_ready_nor_missing() {
        assert_eq!(
            AgentSetupState::from_catalog(None),
            AgentSetupState::Checking
        );
    }

    #[test]
    fn one_installed_agent_ends_setup_and_hides_the_rest() {
        let catalog = AgentReadinessResult {
            agents: vec![item("amp", false, None), item("codex", true, None)],
            ..AgentReadinessResult::default()
        };
        let AgentSetupState::Ready(ready) = AgentSetupState::from_catalog(Some(&catalog)) else {
            panic!("an installed agent means ready");
        };
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].kind.id(), "codex");
    }

    #[test]
    fn installable_agents_lead_and_a_multiline_command_is_never_offered() {
        let catalog = AgentReadinessResult {
            agents: vec![
                item("amp", false, None),
                item("evil", false, Some("true\nrm -rf ~")),
                item("codex", false, Some("curl -fsSL https://example.com | sh")),
            ],
            ..AgentReadinessResult::default()
        };
        let AgentSetupState::Missing(rows) = AgentSetupState::from_catalog(Some(&catalog)) else {
            panic!("nothing installed means missing");
        };
        assert_eq!(rows[0].kind.id(), "codex");
        assert!(rows[0].install.is_some());
        let evil = rows.iter().find(|row| row.kind.id() == "evil").unwrap();
        assert_eq!(evil.install, None, "a control character voids the command");
    }
}
