//! Portable profile sync and Mac-to-Mac migration: the control-plane shapes.
//!
//! The Engine syncs the portable part of a Diri setup (notes, App Skills,
//! recipes, schedules, preferences, `init.lua`, projects, manifest
//! overrides) with a private Git repository or a synced folder, and exports
//! or imports a migration bundle that adds resumable sessions and their
//! Claude Code and Codex transcripts. Credentials, sockets, Holder state and
//! terminal swap files never enter either.

use serde::{Deserialize, Serialize};

use crate::model::DateMillis;

/// Where a profile syncs to.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum SyncBackendKind {
    /// A Git repository (a URL or a local repository path). Diri keeps its
    /// own clone and commits one change set per sync.
    Git,
    /// Any folder another tool keeps in sync: iCloud Drive, Syncthing,
    /// Dropbox, a network share.
    #[default]
    Folder,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SyncConfig {
    pub backend: SyncBackendKind,
    /// Folder path (`~/` allowed) or Git repository URL/path.
    pub location: String,
    /// Sync shortly after local profile files change. Off means only
    /// "Sync Now" syncs; nothing watches or polls.
    #[serde(default)]
    pub auto: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SyncConfigureParams {
    /// `None` turns sync off and forgets the location (the synced copy is
    /// left alone).
    #[serde(default)]
    pub config: Option<SyncConfig>,
}

/// What one sync moved.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSummary {
    /// Files brought into this Mac's profile.
    pub pulled: u32,
    /// Files written to the synced copy.
    pub pushed: u32,
    /// Deletions applied on either side.
    #[serde(default)]
    pub deleted: u32,
    /// Profile paths both sides changed. Local kept; the other side's copy is
    /// saved under `conflicts/` in the synced copy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    /// The Git commit this sync made, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Preferences arrived and wait for the app to apply them.
    #[serde(default)]
    pub prefs_pending: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<SyncConfig>,
    /// This Mac's name as it signs sync commits and conflict copies.
    pub device: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_synced_at: Option<DateMillis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_summary: Option<SyncSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub running: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MigrateExportParams {
    /// Destination file. A directory gets `Diri-<device>-<date>.diribundle`.
    pub path: String,
    /// Seals the bundle with AES-256-GCM under an Argon2id key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// Leave Claude Code and Codex transcripts out (sessions then import as
    /// not resumable).
    #[serde(default)]
    pub skip_transcripts: bool,
}

/// How many of each thing a bundle carries, or an import changes.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableCounts {
    pub sessions: u32,
    pub transcripts: u32,
    pub notes: u32,
    pub skills: u32,
    pub recipes: u32,
    pub schedules: u32,
    pub projects: u32,
}

/// Something deliberately left out, with the reason.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedItem {
    pub item: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateExportResult {
    pub path: String,
    pub bytes: u64,
    pub encrypted: bool,
    pub counts: PortableCounts,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedItem>,
}

/// One `OLD=NEW` path prefix rule. `~/` is allowed on either side.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PathMapping {
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MigrateImportParams {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// Plan only: nothing on this Mac changes.
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub maps: Vec<PathMapping>,
}

/// How an exported project root was found on this Mac.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProjectMatch {
    /// The same path (after the home folder moved) exists here.
    #[default]
    Same,
    /// A `--map` rule or a row the user mapped.
    Mapped,
    /// A checkout with the same `origin` remote.
    Origin,
    /// Not found: sessions keep the home-remapped path and the user can map it.
    Unresolved,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedProject {
    /// As exported, `~/...` under the old home.
    pub from: String,
    /// Absolute path on this Mac.
    pub to: String,
    #[serde(rename = "match")]
    pub how: ProjectMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_origin: Option<String>,
    pub sessions: u32,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PlannedAction {
    #[default]
    Add,
    Update,
    /// Already here and at least as new, or live here.
    Keep,
    Skip,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedSession {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub cwd: String,
    pub action: PlannedAction,
    /// The conversation resumes here (its transcript is or will be in place).
    pub resumable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleSource {
    pub device: String,
    pub home: String,
    pub exported_at: Option<DateMillis>,
    pub diri_version: String,
    pub encrypted: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateImportPlan {
    pub source: BundleSource,
    pub dry_run: bool,
    /// What the bundle carries.
    pub bundle: PortableCounts,
    /// What this import adds (or, for a dry run, would add).
    pub added: PortableCounts,
    /// Sessions refreshed from a newer exported copy.
    pub updated: u32,
    /// Items already here, kept as they are.
    pub kept: u32,
    /// Files that differ from what is already here: the local copy is kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    pub projects: Vec<PlannedProject>,
    pub sessions: Vec<PlannedSession>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedItem>,
    /// Preferences arrived and wait for the app to apply them.
    #[serde(default)]
    pub prefs_pending: bool,
}

/// Name of the file the Engine leaves beside `prefs.json` for the app.
pub const PENDING_PREFS_FILE_NAME: &str = "prefs.pending-import.json";
/// Extension of a migration bundle.
pub const BUNDLE_EXTENSION: &str = "diribundle";

/// The `prefs.json` keys that travel. Everything else (window placement,
/// update channel, login item, phone access, voice commands that may carry a
/// key, machine limits, telemetry overlays) stays on the Mac that set it.
/// The Engine exports only these keys and the app applies only these keys.
pub const PORTABLE_PREF_KEYS: &[&str] = &[
    "agentsNameSessions",
    "attentionRoutes",
    "autoBalancePanes",
    "conversationTitles",
    "defaultAgent",
    "dockBadge",
    "followSystemTheme",
    "globalShortcutKeyCode",
    "hibernateAfterMinutes",
    "interfaceStyle",
    "launchRecipes",
    "mutedNotificationSessions",
    "newAgentStart",
    "paneDensity",
    "reduceMotion",
    "shortcutOverrides",
    "sidebarCollapsedProjects",
    "sidebarGrouping",
    "sidebarLineageHighlights",
    "sidebarOrdering",
    "sidebarPinnedProjects",
    "sidebarPinnedSessions",
    "sidebarProjectOrder",
    "sidebarSessionOrder",
    "sidebarUsageIndicator",
    "statusNotifications",
    "statusSounds",
    "tabOrientation",
    "talkAnswer",
    "talkExceptions",
    "talkReadsByDefault",
    "talkVoice",
    "talkWithAgent",
    "terminalBoldWeight",
    "terminalCellWidth",
    "terminalCopyOnSelect",
    "terminalCursorBlink",
    "terminalCursorFollowsPrograms",
    "terminalCursorShape",
    "terminalFileEditor",
    "terminalFollowsLastDirectory",
    "terminalFontFamily",
    "terminalFontSize",
    "terminalFontSmoothing",
    "terminalFontThicken",
    "terminalFontWeight",
    "terminalHidePointer",
    "terminalItalicWeight",
    "terminalLigatures",
    "terminalLineHeight",
    "terminalOpenLinksOnClick",
    "terminalOptionAsMeta",
    "terminalPasteProtection",
    "terminalShellIntegration",
    "terminalTheme",
    "terminalThemeDark",
    "terminalThemeLight",
    "terminalTrustedLinkSchemes",
    "uiLanguage",
    "warnOnSessionClose",
    "windowMaterial",
    "windowTransparency",
];

/// Overlays the portable keys of `incoming` onto `current` (both
/// `prefs.json` objects). Keys outside [`PORTABLE_PREF_KEYS`] are ignored
/// whatever `incoming` carries. Returns whether anything changed.
pub fn overlay_portable_prefs(
    current: &mut serde_json::Map<String, serde_json::Value>,
    incoming: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    let mut changed = false;
    for key in PORTABLE_PREF_KEYS {
        if let Some(value) = incoming.get(*key)
            && current.get(*key) != Some(value)
        {
            current.insert((*key).to_owned(), value.clone());
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_ignores_keys_that_do_not_travel() {
        let mut current = serde_json::json!({"terminalTheme": "a", "windowPlacement": 1});
        let incoming = serde_json::json!({
            "terminalTheme": "b",
            "windowPlacement": 2,
            "talkVoiceCommand": "curl -H 'Authorization: x'",
        });
        let current = current.as_object_mut().unwrap();
        assert!(overlay_portable_prefs(
            current,
            incoming.as_object().unwrap()
        ));
        assert_eq!(current["terminalTheme"], "b");
        assert_eq!(current["windowPlacement"], 1);
        assert!(!current.contains_key("talkVoiceCommand"));
        assert!(!overlay_portable_prefs(
            current,
            incoming.as_object().unwrap()
        ));
    }

    #[test]
    fn portable_keys_are_sorted_and_unique() {
        assert!(PORTABLE_PREF_KEYS.windows(2).all(|w| w[0] < w[1]));
    }
}
