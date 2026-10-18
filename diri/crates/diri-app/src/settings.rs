//! State shared by the settings tabs.
//!
//! General, Appearance, Terminal, Voice, and Resources mutate diri's preferences. Remote manages
//! the shared execution-host catalog used by the SSH Remote Holder transport.

use diri_proto::{HostEntry, HostNodeConfig};
use diri_term::theme::TermTheme;

use crate::query_editor::QueryEditor;
use crate::store::Prefs;

/// The groups the navigation list is split into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsSection {
    App,
    Agents,
    Machines,
}

impl SettingsSection {
    pub fn label(self) -> &'static str {
        crate::i18n::t(match self {
            Self::App => "settings.section.app",
            Self::Agents => "settings.section.agents",
            Self::Machines => "settings.section.machines",
        })
    }
}

/// Everything the window sidebar needs to paint settings navigation.
///
/// Settings state itself stays in `UtilitySurfaces`, which owns the page the
/// navigation drives. The sidebar renders this projection rather than keeping
/// a second copy, so the list and the page can never disagree about which tab
/// is selected or what the search field says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsNav {
    pub tabs: Vec<SettingsTab>,
    pub active: SettingsTab,
    pub search: QueryEditor,
    pub search_active: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsTab {
    #[default]
    General,
    Appearance,
    Terminal,
    Shortcuts,
    Voice,
    WhatsNew,
    Agents,
    Accounts,
    Skills,
    Schedules,
    Usage,
    Worktrees,
    Resources,
    Remote,
    Phone,
}

impl SettingsTab {
    /// Navigation order: grouped by [`SettingsSection`], most used first.
    pub const ALL: [Self; 15] = [
        Self::General,
        Self::Appearance,
        Self::Terminal,
        Self::Shortcuts,
        Self::Voice,
        Self::WhatsNew,
        Self::Agents,
        Self::Accounts,
        Self::Skills,
        Self::Schedules,
        Self::Usage,
        Self::Worktrees,
        Self::Resources,
        Self::Remote,
        Self::Phone,
    ];

    /// Pages this build can show. Voice is macOS-only.
    pub fn available(self) -> bool {
        self != Self::Voice || cfg!(target_os = "macos")
    }

    pub fn label(self) -> &'static str {
        crate::i18n::t(match self {
            Self::General => "settings.tab.general",
            Self::Appearance => "settings.tab.appearance",
            Self::Terminal => "settings.tab.terminal",
            Self::Shortcuts => "settings.tab.shortcuts",
            Self::Voice => "settings.tab.voice",
            Self::WhatsNew => "settings.tab.whats_new",
            Self::Agents => "settings.tab.agents",
            Self::Accounts => "settings.tab.accounts",
            Self::Skills => "settings.tab.skills",
            Self::Schedules => "settings.tab.schedules",
            Self::Usage => "settings.tab.usage",
            Self::Worktrees => "settings.tab.worktrees",
            Self::Resources => "settings.tab.resources",
            Self::Remote => "settings.tab.remote",
            Self::Phone => "settings.tab.phone",
        })
    }

    pub fn subtitle(self) -> &'static str {
        crate::i18n::t(match self {
            Self::General => "settings.subtitle.general",
            Self::Appearance => "settings.subtitle.appearance",
            Self::Terminal => "settings.subtitle.terminal",
            Self::Shortcuts => "settings.subtitle.shortcuts",
            Self::Voice => "settings.subtitle.voice",
            Self::WhatsNew => "settings.subtitle.whats_new",
            Self::Agents => "settings.subtitle.agents",
            Self::Accounts => "settings.subtitle.accounts",
            Self::Skills => "settings.subtitle.skills",
            Self::Schedules => "settings.subtitle.schedules",
            Self::Usage => "settings.subtitle.usage",
            Self::Worktrees => "settings.subtitle.worktrees",
            Self::Resources => "settings.subtitle.resources",
            Self::Remote => "settings.subtitle.remote",
            Self::Phone => "settings.subtitle.phone",
        })
    }

    /// App pages shape diri itself, Agents pages the agents and what they
    /// cost, and Machines pages where sessions run.
    pub const fn section(self) -> SettingsSection {
        match self {
            Self::General
            | Self::Appearance
            | Self::Terminal
            | Self::Shortcuts
            | Self::Voice
            | Self::WhatsNew => SettingsSection::App,
            Self::Agents | Self::Accounts | Self::Skills | Self::Schedules | Self::Usage => {
                SettingsSection::Agents
            }
            Self::Worktrees | Self::Resources | Self::Remote | Self::Phone => {
                SettingsSection::Machines
            }
        }
    }

    pub const fn icon(self) -> &'static str {
        match self {
            Self::General => "gearshape",
            Self::Appearance => "paintpalette",
            Self::Terminal => "terminal",
            Self::Shortcuts => "keyboard",
            Self::Voice => "speaker.wave.2",
            Self::WhatsNew => "gift",
            Self::Agents => "sparkles",
            Self::Accounts => "account.circle",
            Self::Skills => "doc.text",
            Self::Schedules => "clock.fill",
            Self::Usage => "chart.bar.xaxis",
            Self::Worktrees => "arrow.branch",
            Self::Resources => "server.rack",
            Self::Remote => "network",
            Self::Phone => "iphone",
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
            return Err(crate::i18n::t("settings.remote.error_name").to_owned());
        }
        let ssh = self.ssh.trim();
        if ssh.is_empty() {
            return Err(crate::i18n::t("settings.remote.error_ssh").to_owned());
        }
        if ssh.chars().any(char::is_whitespace) {
            return Err(crate::i18n::t("settings.remote.error_ssh_spaces").to_owned());
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
            return Err(crate::i18n::t("settings.remote.error_node_pair").to_owned());
        }
        if !node_endpoint.is_empty()
            && (node_endpoint.chars().any(char::is_whitespace)
                || !node_endpoint
                    .strip_prefix("tcp://")
                    .unwrap_or(node_endpoint)
                    .contains(':'))
        {
            return Err(crate::i18n::t("settings.remote.error_endpoint").to_owned());
        }
        Ok(HostEntry {
            transport: Default::default(),
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

pub fn theme(id: &str) -> TermTheme {
    crate::app_theme::terminal_theme(id)
}

pub fn next_theme(id: &str) -> TermTheme {
    let all = diri_term::themes::all();
    let index = all.iter().position(|theme| theme.id == id);
    index.map_or(TermTheme::DIRIJOR_DARK, |index| {
        all[(index + 1) % all.len()]
    })
}

/// A group of the theme picker, in the order it lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeGroup {
    Imported,
    Dark,
    Light,
}

/// One row of the theme picker: a group header or a theme.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ThemePickerRow {
    Header(ThemeGroup),
    Theme(&'static TermTheme),
}

/// The picker's rows for `query`: imported themes, then dark, then light,
/// each in catalog order (curated first), empty groups left out. Every word
/// of the query must appear in the theme's name or in "dark"/"light".
pub fn theme_picker_rows(query: &str) -> Vec<ThemePickerRow> {
    use diri_term::theme::ThemeAppearance;
    let query = query.to_lowercase();
    let words = query.split_whitespace().collect::<Vec<_>>();
    let matches = |theme: &TermTheme| {
        words.is_empty() || {
            let haystack = format!(
                "{} {}",
                theme.name.to_lowercase(),
                theme.appearance.label().to_lowercase()
            );
            words.iter().all(|word| haystack.contains(word))
        }
    };
    let imported = diri_term::themes::imported();
    let catalog = diri_term::themes::catalog();
    let groups: [(ThemeGroup, Box<dyn Iterator<Item = &'static TermTheme>>); 3] = [
        (ThemeGroup::Imported, Box::new(imported.iter().copied())),
        (
            ThemeGroup::Dark,
            Box::new(
                catalog
                    .iter()
                    .filter(|theme| theme.appearance == ThemeAppearance::Dark),
            ),
        ),
        (
            ThemeGroup::Light,
            Box::new(
                catalog
                    .iter()
                    .filter(|theme| theme.appearance == ThemeAppearance::Light),
            ),
        ),
    ];
    let mut rows = Vec::new();
    for (group, themes) in groups {
        let header = rows.len();
        rows.extend(
            themes
                .filter(|theme| matches(theme))
                .map(ThemePickerRow::Theme),
        );
        if rows.len() > header {
            rows.insert(header, ThemePickerRow::Header(group));
        }
    }
    rows
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
        assert_eq!(SettingsTab::Appearance.label(), "Appearance");
        assert_eq!(SettingsTab::Terminal.label(), "Terminal");
        assert!(
            SettingsTab::ALL
                .into_iter()
                .all(|tab| !tab.subtitle().is_empty() && !tab.icon().is_empty())
        );
    }

    #[test]
    fn terminal_theme_falls_back_and_wraps() {
        assert_eq!(theme("missing").id, TermTheme::DIRIJOR_DARK.id);
        let all = diri_term::themes::all();
        let last = all.last().unwrap();
        assert_eq!(next_theme(last.id).id, all[0].id);
        assert_eq!(next_theme("missing").id, TermTheme::DIRIJOR_DARK.id);
        // Vendored and curated ids both resolve.
        assert_eq!(theme("3024-night").name, "3024 Night");
        assert_eq!(theme("vesper"), TermTheme::VESPER);
    }

    #[test]
    fn theme_picker_groups_and_filters_the_whole_catalog() {
        let headers = |rows: &[ThemePickerRow]| {
            rows.iter()
                .filter_map(|row| match row {
                    ThemePickerRow::Header(group) => Some(*group),
                    ThemePickerRow::Theme(_) => None,
                })
                .filter(|group| *group != ThemeGroup::Imported)
                .collect::<Vec<_>>()
        };
        let all = theme_picker_rows("");
        assert_eq!(headers(&all), [ThemeGroup::Dark, ThemeGroup::Light]);
        assert!(all.len() > 700, "{}", all.len());
        let dark_first = all.iter().find_map(|row| match row {
            // Other tests import themes into the shared registry.
            ThemePickerRow::Theme(theme)
                if theme.appearance == diri_term::theme::ThemeAppearance::Dark
                    && !diri_term::themes::is_imported(theme.id) =>
            {
                Some(theme.id)
            }
            _ => None,
        });
        assert_eq!(
            dark_first,
            Some(TermTheme::DIRIJOR_DARK.id),
            "curated themes lead"
        );

        let gruvbox = theme_picker_rows("GRUVBOX light");
        assert_eq!(headers(&gruvbox), [ThemeGroup::Light]);
        assert!(gruvbox.iter().all(|row| match row {
            ThemePickerRow::Theme(theme) => theme.name.to_lowercase().contains("gruvbox"),
            ThemePickerRow::Header(_) => true,
        }));
        assert!(gruvbox.iter().any(
            |row| matches!(row, ThemePickerRow::Theme(theme) if theme.id == TermTheme::GRUVBOX_LIGHT.id)
        ));
        assert!(theme_picker_rows("no such theme anywhere").is_empty());
    }

    /// The picker rebuilds its rows on every keystroke and frame it is open.
    #[test]
    fn theme_picker_rows_are_cheap_to_rebuild() {
        let _ = theme_picker_rows("");
        let start = std::time::Instant::now();
        for query in ["", "a", "night", "solarized dark"] {
            let _ = theme_picker_rows(query);
        }
        // Generous for a debug build on a busy machine; release is ~50x faster.
        assert!(
            start.elapsed() < std::time::Duration::from_millis(250),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn host_drafts_generate_unique_ids_and_preserve_them_when_edited() {
        let forge = HostEntry {
            transport: Default::default(),
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
