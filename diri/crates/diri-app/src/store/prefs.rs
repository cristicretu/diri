use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use diri_proto::paths::DirijorPaths;
use diri_proto::{AgentKind, ProjectId, SessionId};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::launch_recipe::{LaunchRecipeBook, deserialize_recipe_book};

const DEFAULT_THEME: &str = "dirijor-dark";

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WindowMode {
    #[default]
    Windowed,
    Maximized,
    Fullscreen,
}

/// The last desktop window placement, stored without GPUI types so the
/// preferences file stays a plain, forwards-compatible JSON document.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowPlacement {
    #[serde(default)]
    pub display_uuid: Option<String>,
    pub mode: WindowMode,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InspectorTab {
    #[default]
    Info,
    Changes,
    Code,
    Artifacts,
}

/// Preferences intentionally persist the manifest id as a plain string. The
/// four pre-catalog enum spellings are accepted forever because prefs survive
/// upgrades; new saves use the canonical manifest ids (for example
/// `"claude-code"` and `"opencode"`).
fn serialize_default_agent<S>(agent: &AgentKind, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(agent.id())
}

fn deserialize_default_agent<'de, D>(deserializer: D) -> Result<AgentKind, D::Error>
where
    D: Deserializer<'de>,
{
    let saved = String::deserialize(deserializer)?;
    Ok(match saved.as_str() {
        "claudeCode" | AgentKind::CLAUDE_CODE_ID => AgentKind::CLAUDE_CODE,
        "codex" => AgentKind::CODEX,
        "cursor" => AgentKind::CURSOR,
        "gemini" => AgentKind::GEMINI,
        "shell" => AgentKind::SHELL,
        _ => AgentKind::new(saved),
    })
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Prefs {
    #[serde(
        serialize_with = "serialize_default_agent",
        deserialize_with = "deserialize_default_agent"
    )]
    pub default_agent: AgentKind,
    /// Persistent destination for global new-session shortcuts. `None` means
    /// this Mac; a host id means that configured remote host. The alias
    /// migrates preferences written by the earlier last-used implementation.
    #[serde(alias = "lastSpawnHost")]
    pub default_spawn_host: Option<String>,
    pub start_at_login: bool,
    pub confirm_before_closing_session: bool,
    pub status_sounds: bool,
    /// Check, download, and verify releases in the background. A staged update
    /// installs on quit or when the user requests a restart.
    pub automatic_updates: bool,
    /// A release the user chose not to install. Persisted so "Skip" outlives
    /// the session that clicked it; empty means nothing is skipped.
    pub skipped_update_version: String,
    pub hibernate_after_minutes: u32,
    pub memory_hard_limit_gb: u64,
    pub terminal_theme: String,
    pub terminal_font_size: f32,
    /// Last size, position, and presentation mode of the main window.
    pub window_placement: Option<WindowPlacement>,
    /// Whether the leading sidebar was mounted when the app last ran.
    pub sidebar_visible: bool,
    pub sidebar_width: f32,
    /// Whether the trailing workbench inspector is mounted.
    pub inspector_open: bool,
    /// Width of the trailing workbench inspector in points.
    pub inspector_width: f32,
    /// Last selected tab in the trailing workbench inspector.
    pub inspector_tab: InspectorTab,
    /// Fraction of the terminal workbench reserved for the primary pane when
    /// the lower terminal is open.
    pub workbench_primary_fraction: f32,
    /// Newline-separated roots, matching the Swift settings text field.
    pub quick_open_roots: String,
    pub sidebar_project_order: Vec<ProjectId>,
    pub sidebar_session_order: Vec<SessionId>,
    pub sidebar_pinned_projects: Vec<ProjectId>,
    pub sidebar_pinned_sessions: Vec<SessionId>,
    pub sidebar_collapsed_projects: Vec<ProjectId>,
    /// Sessions whose spawned children are folded away.
    pub sidebar_collapsed_sessions: Vec<SessionId>,
    pub sidebar_expanded_archives: Vec<ProjectId>,
    /// Versioned, locally owned one-action Agent workflows.
    #[serde(default, deserialize_with = "deserialize_recipe_book")]
    pub launch_recipes: LaunchRecipeBook,
    /// Session that should regain focus after the daemon's initial hydrate.
    pub last_selected_session: Option<SessionId>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            default_agent: AgentKind::CLAUDE_CODE,
            default_spawn_host: None,
            start_at_login: false,
            confirm_before_closing_session: true,
            status_sounds: true,
            automatic_updates: true,
            skipped_update_version: String::new(),
            hibernate_after_minutes: 15,
            memory_hard_limit_gb: 6,
            terminal_theme: DEFAULT_THEME.to_owned(),
            terminal_font_size: 13.0,
            window_placement: None,
            sidebar_visible: true,
            sidebar_width: 248.0,
            inspector_open: true,
            inspector_width: 440.0,
            inspector_tab: InspectorTab::Info,
            workbench_primary_fraction: crate::workbench::DEFAULT_PRIMARY_FRACTION,
            quick_open_roots: String::new(),
            sidebar_project_order: Vec::new(),
            sidebar_session_order: Vec::new(),
            sidebar_pinned_projects: Vec::new(),
            sidebar_pinned_sessions: Vec::new(),
            sidebar_collapsed_projects: Vec::new(),
            sidebar_collapsed_sessions: Vec::new(),
            sidebar_expanded_archives: Vec::new(),
            launch_recipes: LaunchRecipeBook::default(),
            last_selected_session: None,
        }
    }
}

impl Prefs {
    pub const MIN_TERMINAL_FONT_SIZE: f32 = 10.0;
    pub const MAX_TERMINAL_FONT_SIZE: f32 = 20.0;

    pub fn path() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/nonexistent"));
        Self::path_in_home(&home)
    }

    pub fn path_in_home(home: &Path) -> PathBuf {
        DirijorPaths::prefs_file(home)
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read(path) {
            Ok(bytes) => {
                let mut prefs: Self = serde_json::from_slice(&bytes)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                prefs.normalize();
                Ok(prefs)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut normalized = self.clone();
        normalized.normalize();
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "preference path has no parent")
        })?;
        fs::create_dir_all(parent)?;
        let bytes = serde_json::to_vec_pretty(&normalized)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, path)
    }

    pub fn zoom_terminal(&mut self, delta: f32) {
        self.terminal_font_size = (self.terminal_font_size + delta)
            .clamp(Self::MIN_TERMINAL_FONT_SIZE, Self::MAX_TERMINAL_FONT_SIZE);
    }

    pub fn reset_terminal_zoom(&mut self) {
        self.terminal_font_size = 13.0;
    }

    pub fn normalize(&mut self) {
        if !self.terminal_font_size.is_finite() {
            self.terminal_font_size = 13.0;
        }
        self.terminal_font_size = self
            .terminal_font_size
            .clamp(Self::MIN_TERMINAL_FONT_SIZE, Self::MAX_TERMINAL_FONT_SIZE);
        if let Some(placement) = &mut self.window_placement {
            let valid = placement.x.is_finite()
                && placement.y.is_finite()
                && placement.width.is_finite()
                && placement.height.is_finite()
                && placement.width > 0.0
                && placement.height > 0.0;
            if valid {
                placement.width = placement.width.max(900.0);
                placement.height = placement.height.max(560.0);
            } else {
                self.window_placement = None;
            }
        }
        if !self.sidebar_width.is_finite() {
            self.sidebar_width = 248.0;
        }
        self.sidebar_width = self.sidebar_width.clamp(200.0, 400.0);
        if !self.inspector_width.is_finite() {
            self.inspector_width = 440.0;
        }
        self.inspector_width = self.inspector_width.clamp(300.0, 720.0);
        if !self.workbench_primary_fraction.is_finite() {
            self.workbench_primary_fraction = crate::workbench::DEFAULT_PRIMARY_FRACTION;
        }
        self.workbench_primary_fraction = self.workbench_primary_fraction.clamp(0.0, 1.0);
        if self.terminal_theme.is_empty() {
            self.terminal_theme = DEFAULT_THEME.to_owned();
        }
        self.launch_recipes.normalize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch_recipe::{LaunchRecipe, RecipeProject};

    #[test]
    fn older_preferences_migrate_to_an_empty_recipe_book() {
        let mut value = serde_json::to_value(Prefs::default()).expect("serialize prefs");
        value
            .as_object_mut()
            .expect("prefs object")
            .remove("launchRecipes");
        let prefs: Prefs = serde_json::from_value(value).expect("old preferences remain readable");
        assert!(prefs.launch_recipes.items().is_empty());
    }

    #[test]
    fn malformed_recipe_data_does_not_discard_other_preferences() {
        let mut value = serde_json::to_value(Prefs {
            status_sounds: false,
            ..Prefs::default()
        })
        .expect("serialize prefs");
        value["launchRecipes"] = serde_json::json!({"version": 1, "items": "broken"});
        let prefs: Prefs =
            serde_json::from_value(value).expect("malformed recipe field is isolated");
        assert!(!prefs.status_sounds);
        assert!(prefs.launch_recipes.items().is_empty());
    }

    #[test]
    fn recipe_book_round_trips_through_preferences() {
        let mut prefs = Prefs::default();
        prefs
            .launch_recipes
            .add(LaunchRecipe::draft(
                "Review",
                AgentKind::CODEX,
                RecipeProject::Path {
                    path: "/tmp".into(),
                },
                None,
                "Review this branch",
            ))
            .expect("add recipe");
        let json = serde_json::to_vec(&prefs).expect("serialize prefs");
        let restored: Prefs = serde_json::from_slice(&json).expect("deserialize prefs");
        assert_eq!(
            restored.launch_recipes.items(),
            prefs.launch_recipes.items()
        );
    }
}
