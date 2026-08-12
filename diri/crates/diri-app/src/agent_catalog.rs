//! The client-side view of the daemon's manifest/readiness catalog.
//!
//! Launch surfaces consume this module instead of each rebuilding a partial
//! four-agent list. It also centralizes unavailable/setup language so the
//! launcher, sidebar, Settings, and command palette tell the same story.

use diri_proto::{AgentKind, AgentReadinessItem, AgentReadinessResult};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentOption {
    pub kind: AgentKind,
    pub display_name: String,
    pub binary: String,
    pub available: bool,
    pub first_class: bool,
    pub setup_url: Option<String>,
    pub install_hint: String,
    pub sign_in_hint: Option<String>,
}

impl AgentOption {
    pub(crate) fn unavailable_label(&self) -> Option<String> {
        (!self.available).then(|| missing_binary_label(&self.binary))
    }

    pub(crate) fn unavailable_detail(&self) -> Option<String> {
        let mut detail = format!("{} · {}", self.unavailable_label()?, self.install_hint);
        if let Some(sign_in_hint) = &self.sign_in_hint {
            detail.push_str(" · ");
            detail.push_str(sign_in_hint);
        }
        Some(detail)
    }
}

/// Catalog rows in deterministic product order. An empty response means an
/// old daemon that predates descriptors, so retain the original four choices
/// as optimistic compatibility entries instead of making the app unusable.
pub(crate) fn agent_options(catalog: &AgentReadinessResult) -> Vec<AgentOption> {
    if catalog.agents.is_empty() {
        return legacy_options();
    }

    let mut options: Vec<_> = catalog
        .agents
        .iter()
        .filter(|item| !item.kind.is_terminal())
        .map(option_from_readiness)
        .collect();
    options.sort_by(|left, right| {
        option_order(left)
            .cmp(&option_order(right))
            .then_with(|| {
                left.display_name
                    .to_lowercase()
                    .cmp(&right.display_name.to_lowercase())
            })
            .then_with(|| left.kind.id().cmp(right.kind.id()))
    });
    options
}

/// Settings and the launcher must expose the shell whenever default resolution
/// can choose it. Available catalog extensions are valid explicit defaults,
/// but they do not replace the first-class-or-shell repair policy for a removed
/// preference.
pub(crate) fn default_agent_options(catalog: &AgentReadinessResult) -> Vec<AgentOption> {
    let mut options = agent_options(catalog);
    if !options
        .iter()
        .any(|option| option.available && option.first_class)
    {
        options.push(AgentOption {
            kind: AgentKind::SHELL,
            display_name: "Terminal".to_owned(),
            binary: "login shell".to_owned(),
            available: true,
            first_class: false,
            setup_url: None,
            install_hint: "Uses your login shell.".to_owned(),
            sign_in_hint: None,
        });
    }
    options
}

/// Keep a saved default only while it is launchable. Removed/unknown ids fall
/// back to an installed first-class agent, and finally to a shell session so
/// Command-T never becomes a dead shortcut.
pub(crate) fn resolved_default_agent(
    saved: &AgentKind,
    catalog: &AgentReadinessResult,
) -> AgentKind {
    let options = agent_options(catalog);
    if options
        .iter()
        .any(|option| option.available && option.kind == *saved)
    {
        return saved.clone();
    }
    options
        .iter()
        .find(|option| option.available && option.first_class)
        .map_or(AgentKind::SHELL, |option| option.kind.clone())
}

pub(crate) fn display_name(kind: &AgentKind, catalog: &AgentReadinessResult) -> String {
    agent_options(catalog)
        .into_iter()
        .find(|option| option.kind == *kind)
        .map(|option| option.display_name)
        .unwrap_or_else(|| builtin_name(kind).unwrap_or_else(|| title_case_id(kind.id())))
}

pub(crate) fn system_image(kind: &AgentKind) -> &'static str {
    match kind.id() {
        AgentKind::CLAUDE_CODE_ID => "sparkle",
        AgentKind::CODEX_ID => "chevron.left.forwardslash.chevron.right",
        AgentKind::CURSOR_ID => "cube",
        AgentKind::GEMINI_ID => "sparkles",
        AgentKind::SHELL_ID | AgentKind::GENERIC_ID => "terminal",
        _ => "terminal",
    }
}

/// Only ordinary browser URLs are handed to GPUI's established external-link
/// path. Whitespace/control characters are rejected as malformed input.
pub(crate) fn normal_web_url(url: &str) -> Option<String> {
    if url
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return None;
    }
    // `url::Url` deliberately treats `https:///setup` as host `setup`. Setup
    // metadata should not be repaired or guessed, so require a lexically
    // nonempty authority exactly where the manifest declared it.
    let authority = url
        .split_once("://")?
        .1
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.is_empty() {
        return None;
    }
    let parsed = url::Url::parse(url).ok()?;
    (matches!(parsed.scheme(), "http" | "https")
        && parsed.host_str().is_some_and(|host| !host.is_empty())
        && parsed.username().is_empty()
        && parsed.password().is_none())
    .then(|| url.to_owned())
}

pub(crate) fn missing_binary_label(binary: &str) -> String {
    format!("Missing {binary}")
}

fn option_from_readiness(item: &AgentReadinessItem) -> AgentOption {
    let descriptor = item.descriptor.as_ref();
    let setup = descriptor.and_then(|descriptor| descriptor.setup.as_ref());
    let display_name = descriptor
        .map(|descriptor| descriptor.display_name.trim())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .or_else(|| builtin_name(&item.kind))
        .unwrap_or_else(|| title_case_id(item.kind.id()));
    let install_hint = setup
        .and_then(|setup| setup.install_hint.as_deref())
        .map(str::trim)
        .filter(|hint| !hint.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Install {} and add it to PATH.", item.binary));
    AgentOption {
        kind: item.kind.clone(),
        display_name,
        binary: item.binary.clone(),
        available: item.available(),
        first_class: descriptor.is_some_and(|descriptor| descriptor.first_class)
            || is_legacy_first_class(&item.kind),
        setup_url: setup
            .and_then(|setup| setup.url.as_deref())
            .and_then(normal_web_url),
        install_hint,
        sign_in_hint: setup
            .and_then(|setup| setup.sign_in_hint.as_deref())
            .map(str::trim)
            .filter(|hint| !hint.is_empty())
            .map(str::to_owned),
    }
}

fn legacy_options() -> Vec<AgentOption> {
    [
        (AgentKind::CLAUDE_CODE, "Claude Code", "claude"),
        (AgentKind::CODEX, "Codex", "codex"),
        (AgentKind::CURSOR, "Cursor", "cursor-agent"),
        (AgentKind::GEMINI, "Gemini", "gemini"),
    ]
    .into_iter()
    .map(|(kind, display_name, binary)| AgentOption {
        kind,
        display_name: display_name.to_owned(),
        binary: binary.to_owned(),
        available: true,
        first_class: true,
        setup_url: None,
        install_hint: format!("Install {binary} and add it to PATH."),
        sign_in_hint: None,
    })
    .collect()
}

fn option_order(option: &AgentOption) -> (u8, usize) {
    let pinned = [
        AgentKind::CLAUDE_CODE_ID,
        AgentKind::CODEX_ID,
        AgentKind::CURSOR_ID,
        AgentKind::GEMINI_ID,
    ];
    pinned
        .iter()
        .position(|id| *id == option.kind.id())
        .map_or((1, usize::MAX), |index| (0, index))
}

fn is_legacy_first_class(kind: &AgentKind) -> bool {
    matches!(
        kind.id(),
        AgentKind::CLAUDE_CODE_ID
            | AgentKind::CODEX_ID
            | AgentKind::CURSOR_ID
            | AgentKind::GEMINI_ID
    )
}

fn builtin_name(kind: &AgentKind) -> Option<String> {
    match kind.id() {
        AgentKind::CLAUDE_CODE_ID => Some("Claude Code".to_owned()),
        AgentKind::CODEX_ID => Some("Codex".to_owned()),
        AgentKind::CURSOR_ID => Some("Cursor".to_owned()),
        AgentKind::GEMINI_ID => Some("Gemini".to_owned()),
        AgentKind::SHELL_ID => Some("Terminal".to_owned()),
        _ => None,
    }
}

pub(crate) fn title_case_id(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use diri_proto::{AgentDescriptor, AgentReadinessItem, AgentSetup};

    use super::*;

    fn item(id: &str, available: bool, first_class: bool) -> AgentReadinessItem {
        AgentReadinessItem {
            kind: AgentKind::new(id),
            binary: format!("{id}-bin"),
            path: available.then(|| format!("/bin/{id}")),
            descriptor: Some(AgentDescriptor {
                id: id.to_owned(),
                display_name: title_case_id(id),
                first_class,
                ..AgentDescriptor::default()
            }),
        }
    }

    #[test]
    fn unavailable_guidance_and_web_url_validation_are_shared() {
        let mut unavailable = item("amp", false, false);
        unavailable.descriptor.as_mut().unwrap().setup = Some(AgentSetup {
            url: Some("https://ampcode.com/manual".into()),
            install_hint: Some("Install Amp's CLI.".into()),
            sign_in_hint: Some("Sign in at ampcode.com, then run amp.".into()),
        });
        let options = agent_options(&AgentReadinessResult {
            agents: vec![unavailable],
        });
        assert_eq!(
            options[0].unavailable_label().as_deref(),
            Some("Missing amp-bin")
        );
        assert_eq!(options[0].install_hint, "Install Amp's CLI.");
        assert_eq!(
            options[0].unavailable_detail().as_deref(),
            Some("Missing amp-bin · Install Amp's CLI. · Sign in at ampcode.com, then run amp.")
        );
        assert_eq!(
            options[0].setup_url.as_deref(),
            Some("https://ampcode.com/manual")
        );
        assert_eq!(normal_web_url("javascript:alert(1)"), None);
        assert_eq!(normal_web_url("file:///tmp/setup"), None);
        assert_eq!(normal_web_url("https://?guide=1"), None);
        assert_eq!(normal_web_url("https:///setup"), None);
        assert_eq!(normal_web_url("https://example.com/\u{0}setup"), None);
        assert_eq!(normal_web_url("https://example.com/\nsetup"), None);
        assert_eq!(normal_web_url("https://user@example.com/setup"), None);
    }

    #[test]
    fn unknown_defaults_fall_back_to_first_class_then_shell() {
        let catalog = AgentReadinessResult {
            agents: vec![item("amp", true, false), item("codex", true, true)],
        };
        assert_eq!(
            resolved_default_agent(&AgentKind::new("removed"), &catalog),
            AgentKind::CODEX
        );
        let catalog = AgentReadinessResult {
            agents: vec![item("amp", true, false), item("codex", false, true)],
        };
        assert_eq!(
            resolved_default_agent(&AgentKind::new("removed"), &catalog),
            AgentKind::SHELL
        );
        let options = default_agent_options(&catalog);
        let resolved = resolved_default_agent(&AgentKind::new("removed"), &catalog);
        let selected = options
            .iter()
            .find(|option| option.kind == resolved)
            .expect("launcher/settings options represent the repaired default");
        assert_eq!(selected.display_name, "Terminal");
        assert!(selected.available);
    }

    #[test]
    fn old_daemon_entries_without_descriptors_keep_useful_availability_copy() {
        let catalog = AgentReadinessResult {
            agents: vec![AgentReadinessItem {
                kind: AgentKind::CODEX,
                binary: "codex".into(),
                path: None,
                descriptor: None,
            }],
        };
        let options = agent_options(&catalog);
        assert_eq!(options[0].display_name, "Codex");
        assert_eq!(
            options[0].unavailable_label().as_deref(),
            Some("Missing codex")
        );
        assert_eq!(options[0].install_hint, "Install codex and add it to PATH.");
        assert_eq!(options[0].setup_url, None);
    }
}
