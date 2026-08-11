//! State shared by the four settings tabs.
//!
//! General, Terminal, and Resources mutate diri's preferences. Remote manages
//! the shared execution-host catalog used by the SSH Remote Holder transport.

use diri_proto::{HostEntry, HostNodeConfig};
use diri_term::theme::TermTheme;

use crate::store::{DefaultAgent, Prefs};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsTab {
    #[default]
    General,
    Terminal,
    Resources,
    Remote,
}

impl SettingsTab {
    pub const ALL: [Self; 4] = [Self::General, Self::Terminal, Self::Resources, Self::Remote];

    pub const fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Terminal => "Terminal",
            Self::Resources => "Resources",
            Self::Remote => "Remote",
        }
    }

    pub const fn subtitle(self) -> &'static str {
        match self {
            Self::General => "Startup, sessions, and updates",
            Self::Terminal => "Appearance and text size",
            Self::Resources => "Idle sessions and memory",
            Self::Remote => "SSH execution hosts",
        }
    }

    pub const fn icon(self) -> &'static str {
        match self {
            Self::General => "gearshape",
            Self::Terminal => "terminal",
            Self::Resources => "server.rack",
            Self::Remote => "network",
        }
    }
}

/// User-editable host fields. `original_id` is intentionally not presented as
/// a form field: it is generated once and then preserved because session
/// records refer to it as their stable machine identity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostDraft {
    pub original_id: Option<String>,
    pub name: String,
    pub ssh: String,
    pub default_cwd: String,
    pub node_endpoint: String,
    pub node_token_file: String,
    pub node_id: String,
}

impl HostDraft {
    pub fn new() -> Self {
        Self {
            default_cwd: "~".to_owned(),
            ..Self::default()
        }
    }

    pub fn editing(host: &HostEntry) -> Self {
        Self {
            original_id: Some(host.id.clone()),
            name: host.display_name().to_owned(),
            ssh: host.ssh.clone(),
            default_cwd: host.default_cwd.clone().unwrap_or_default(),
            node_endpoint: host
                .node
                .as_ref()
                .map_or_else(String::new, |node| node.endpoint.clone()),
            node_token_file: host
                .node
                .as_ref()
                .map_or_else(String::new, |node| node.token_file.clone()),
            node_id: host
                .node
                .as_ref()
                .and_then(|node| node.node_id.clone())
                .unwrap_or_default(),
        }
    }

    pub fn entry(&self, existing: &[HostEntry]) -> Result<HostEntry, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Give this host a name.".to_owned());
        }
        let ssh = self.ssh.trim();
        if ssh.is_empty() {
            return Err("Enter an SSH destination, such as you@forge.".to_owned());
        }
        if ssh.chars().any(char::is_whitespace) {
            return Err("The SSH destination cannot contain spaces.".to_owned());
        }

        let id = self
            .original_id
            .clone()
            .unwrap_or_else(|| unique_host_id(name, existing.iter().map(|host| host.id.as_str())));
        let default_cwd = self.default_cwd.trim();
        let node_endpoint = self.node_endpoint.trim();
        let node_token_file = self.node_token_file.trim();
        let node_id = self.node_id.trim();
        if node_endpoint.is_empty() != node_token_file.is_empty() {
            return Err("Enter both the node endpoint and its local token file.".to_owned());
        }
        if !node_endpoint.is_empty()
            && (node_endpoint.chars().any(char::is_whitespace)
                || !node_endpoint
                    .strip_prefix("tcp://")
                    .unwrap_or(node_endpoint)
                    .contains(':'))
        {
            return Err("Use a node endpoint like tcp://100.64.0.2:7337.".to_owned());
        }
        Ok(HostEntry {
            id,
            name: Some(name.to_owned()),
            ssh: ssh.to_owned(),
            default_cwd: (!default_cwd.is_empty()).then(|| default_cwd.to_owned()),
            node: (!node_endpoint.is_empty()).then(|| HostNodeConfig {
                endpoint: node_endpoint.to_owned(),
                token_file: node_token_file.to_owned(),
                node_id: (!node_id.is_empty()).then(|| node_id.to_owned()),
            }),
        })
    }
}

fn unique_host_id<'a>(name: &str, existing: impl Iterator<Item = &'a str>) -> String {
    let mut base = String::new();
    let mut separating = false;
    for character in name.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if separating && !base.is_empty() {
                base.push('-');
            }
            base.push(character);
            separating = false;
        } else {
            separating = true;
        }
    }
    if base.is_empty() {
        base.push_str("host");
    }
    let occupied: std::collections::HashSet<&str> = existing.collect();
    if !occupied.contains(base.as_str()) {
        return base;
    }
    for suffix in 2.. {
        let candidate = format!("{base}-{suffix}");
        if !occupied.contains(candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!()
}

pub fn default_agent_label(agent: DefaultAgent) -> &'static str {
    match agent {
        DefaultAgent::ClaudeCode => "Claude Code",
        DefaultAgent::Codex => "Codex",
        DefaultAgent::Cursor => "Cursor",
        DefaultAgent::Gemini => "Gemini",
    }
}

pub fn theme(id: &str) -> TermTheme {
    crate::app_theme::terminal_theme(id)
}

pub fn next_theme(id: &str) -> TermTheme {
    let index = TermTheme::CATALOG
        .iter()
        .position(|theme| theme.id == id)
        .unwrap_or(0);
    TermTheme::CATALOG[(index + 1) % TermTheme::CATALOG.len()]
}

pub fn cycle_hibernate_minutes(value: u32) -> u32 {
    match value {
        0 => 5,
        5 => 15,
        15 => 30,
        30 => 60,
        _ => 0,
    }
}

pub fn cycle_memory_limit(value: u64) -> u64 {
    match value {
        2 => 4,
        4 => 6,
        6 => 8,
        _ => 2,
    }
}

pub fn terminal_font_label(prefs: &Prefs) -> String {
    format!("{:.0} pt", prefs.terminal_font_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_choices_match_swift() {
        assert_eq!(cycle_hibernate_minutes(0), 5);
        assert_eq!(cycle_hibernate_minutes(60), 0);
        assert_eq!(cycle_memory_limit(2), 4);
        assert_eq!(cycle_memory_limit(8), 2);
        assert_eq!(DefaultAgent::ALL.len(), 4);
        assert!(
            SettingsTab::ALL
                .into_iter()
                .all(|tab| !tab.subtitle().is_empty() && !tab.icon().is_empty())
        );
    }

    #[test]
    fn terminal_theme_falls_back_and_wraps() {
        assert_eq!(theme("missing").id, TermTheme::DIRIJOR_DARK.id);
        let last = TermTheme::CATALOG.last().unwrap();
        assert_eq!(next_theme(last.id).id, TermTheme::DIRIJOR_DARK.id);
    }

    #[test]
    fn host_drafts_generate_unique_ids_and_preserve_them_when_edited() {
        let forge = HostEntry {
            id: "forge".into(),
            name: Some("Forge".into()),
            ssh: "you@forge".into(),
            default_cwd: Some("~/code".into()),
            node: None,
        };
        let draft = HostDraft {
            name: "Forge".into(),
            ssh: "other@forge".into(),
            default_cwd: String::new(),
            ..HostDraft::default()
        };
        let added = draft
            .entry(std::slice::from_ref(&forge))
            .expect("valid host");
        assert_eq!(added.id, "forge-2");
        assert_eq!(added.default_cwd, None);

        let mut editing = HostDraft::editing(&forge);
        editing.name = "The Forge".into();
        let edited = editing.entry(&[]).expect("valid edit");
        assert_eq!(edited.id, "forge");
        assert_eq!(edited.name.as_deref(), Some("The Forge"));
    }

    #[test]
    fn new_hosts_default_to_the_remote_home_directory() {
        assert_eq!(HostDraft::new().default_cwd, "~");
    }

    #[test]
    fn host_drafts_reject_missing_or_malformed_destinations() {
        let mut draft = HostDraft::new();
        draft.name = "Builder".into();
        assert!(draft.entry(&[]).unwrap_err().contains("SSH destination"));
        draft.ssh = "ssh builder".into();
        assert!(draft.entry(&[]).unwrap_err().contains("spaces"));

        draft.ssh = "builder".into();
        draft.node_endpoint = "tcp://100.64.0.2:7337".into();
        assert!(draft.entry(&[]).unwrap_err().contains("both"));
        draft.node_token_file = "~/.config/dirijor/builder.token".into();
        draft.node_id = "node-builder".into();
        let host = draft.entry(&[]).expect("first-party node host");
        let node = host.node.expect("node config");
        assert_eq!(node.endpoint, "tcp://100.64.0.2:7337");
        assert_eq!(node.node_id.as_deref(), Some("node-builder"));
    }
}
