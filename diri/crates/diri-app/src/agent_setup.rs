//! Setup rows for coding agents that are not installed yet.
//!
//! The welcome screen, the launcher, and Settings all answer "how do I get an
//! agent?" the same way: a row per Agent with one Install button. Pressing it
//! raises the system alert sheet with the exact installer Diri would run, and
//! confirming runs it in a Terminal tab the user can watch.

use std::rc::Rc;

use diri_proto::AgentKind;
use diri_ui::{AgentLogo, Radius, SemanticColors};
use gpui::{AnyElement, App, Div, FontWeight, Role, Window, div, prelude::*, px};

use crate::agent_catalog::AgentOption;
use crate::icons::sf_symbol;

/// Runs once the user has confirmed the sheet that showed the command.
pub(crate) type InstallHandler = Rc<dyn Fn(&AgentOption, &mut App)>;
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

/// The consent step for an installer: the system alert sheet, carrying the
/// exact text that will be typed. Rows stay one line because the command
/// lives here, where it is read at the moment of deciding, and nothing runs
/// on any answer but Install.
pub(crate) fn confirm_install(
    option: &AgentOption,
    window: &mut Window,
    cx: &mut App,
    on_install: InstallHandler,
) {
    let Some(install) = option.install.clone() else {
        return;
    };
    let requirement = install
        .requirement
        .map(|requirement| format!("\n\nNeeds {requirement}."))
        .unwrap_or_default();
    let answer = window.prompt(
        gpui::PromptLevel::Info,
        &format!("Install {}?", option.display_name),
        Some(&format!(
            "Diri runs its official installer in a new terminal tab:\n\n{}{requirement}",
            install.command
        )),
        &[
            gpui::PromptButton::ok("Install"),
            gpui::PromptButton::cancel("Cancel"),
        ],
        cx,
    );
    let option = option.clone();
    cx.spawn(async move |cx| {
        if answer.await.ok() == Some(0) {
            cx.update(|cx| on_install(&option, cx));
        }
    })
    .detach();
}

fn setup_row(
    prefix: &'static str,
    option: &AgentOption,
    recommended: bool,
    installing: bool,
    colors: SemanticColors,
    on_install: &InstallHandler,
) -> AnyElement {
    let id = option.kind.id().to_owned();
    let note = if installing {
        Some("Installing…".to_owned())
    } else {
        option
            .install
            .as_ref()
            .and_then(|install| install.requirement.as_ref())
            .map(|requirement| format!("Needs {requirement}"))
    };
    let mut row = div()
        .h(px(36.0))
        .px(px(11.0))
        .flex()
        .items_center()
        .gap(px(9.0))
        .child(
            AgentLogo::new(crate::surface_shell::ui_agent(&option.kind), 16.0, colors)
                .badged(false),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .text_size(px(12.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(colors.primary)
                .child(option.display_name.clone()),
        )
        .when_some(note, |row, note| {
            row.child(
                div()
                    .flex_none()
                    .text_size(px(11.0))
                    .text_color(colors.tertiary)
                    .child(note),
            )
        });
    if option.install.is_some() && !installing {
        let handler = Rc::clone(on_install);
        let target = option.clone();
        row = row.child(
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
                .flex_none()
                .h(px(22.0))
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
                .on_click(move |_, window, cx| {
                    confirm_install(&target, window, cx, Rc::clone(&handler));
                })
                .child("Install"),
        );
    } else if let Some(url) = option.setup_url.clone().filter(|_| !installing) {
        // No bundled installer: the vendor's guide is the only honest action.
        row = row.child(
            div()
                .id(format!("{prefix}-guide-{id}"))
                .role(Role::Button)
                .aria_label(format!("Open the {} setup guide", option.display_name))
                .flex_none()
                .h(px(22.0))
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
    row.into_any_element()
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
