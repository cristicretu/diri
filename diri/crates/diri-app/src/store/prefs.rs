use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use diri_proto::paths::DirijorPaths;
use diri_proto::{AgentKind, ProjectId, SessionId};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::launch_recipe::{LaunchRecipeBook, deserialize_recipe_book};

const DEFAULT_THEME: &str = "dirijor-dark";

/// The interface language. `System` follows the reader's preferred languages
/// (see `crate::i18n`); a pick persists as its tag. A tag this build has no
/// catalog for, written by a newer diri, reads as `System`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UiLanguage {
    #[default]
    System,
    Fixed(diri_i18n::Language),
}

impl UiLanguage {
    pub const fn tag(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Fixed(language) => language.tag(),
        }
    }
}

/// Where one kind of session news is announced, besides the session's own
/// row: a system notification (only while Diri is in the background), a
/// sound, the Dock badge, and one bounce of the Dock icon (also only in the
/// background).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AttentionRoute {
    pub notify: bool,
    pub sound: bool,
    pub badge: bool,
    pub bounce: bool,
}

impl Default for AttentionRoute {
    fn default() -> Self {
        Self {
            notify: true,
            sound: true,
            badge: true,
            bounce: false,
        }
    }
}

/// Per-state routing. Working has no entry: it only ever marks the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "RawAttentionRoutes")]
pub struct AttentionRoutes {
    /// A permission prompt or question is waiting.
    pub blocked: AttentionRoute,
    /// The program failed or exited with an error.
    pub error: AttentionRoute,
    /// A turn finished.
    pub done: AttentionRoute,
}

impl Default for AttentionRoutes {
    /// Every channel on, except the Dock bounce, which only a session that
    /// is waiting on you earns.
    fn default() -> Self {
        Self {
            blocked: AttentionRoute {
                bounce: true,
                ..AttentionRoute::default()
            },
            error: AttentionRoute::default(),
            done: AttentionRoute::default(),
        }
    }
}

/// [`AttentionRoutes`] as stored, so a channel missing from saved prefs takes
/// its own state's default (a blocked route saved before the bounce channel
/// existed still bounces).
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawAttentionRoutes {
    blocked: RawAttentionRoute,
    error: RawAttentionRoute,
    done: RawAttentionRoute,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawAttentionRoute {
    notify: Option<bool>,
    sound: Option<bool>,
    badge: Option<bool>,
    bounce: Option<bool>,
}

impl RawAttentionRoute {
    fn or(self, default: AttentionRoute) -> AttentionRoute {
        AttentionRoute {
            notify: self.notify.unwrap_or(default.notify),
            sound: self.sound.unwrap_or(default.sound),
            badge: self.badge.unwrap_or(default.badge),
            bounce: self.bounce.unwrap_or(default.bounce),
        }
    }
}

impl From<RawAttentionRoutes> for AttentionRoutes {
    fn from(raw: RawAttentionRoutes) -> Self {
        let defaults = Self::default();
        Self {
            blocked: raw.blocked.or(defaults.blocked),
            error: raw.error.or(defaults.error),
            done: raw.done.or(defaults.done),
        }
    }
}

/// What the Dock badge counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DockBadge {
    /// Sessions that are blocked, failed or finished and not looked at yet:
    /// one per session however many notifications it posted.
    #[default]
    Sessions,
    /// Every unread entry in the notification inbox (the behavior before
    /// per-session counting).
    Notifications,
}

/// Who listens for a spoken answer after a reading.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TalkAnswer {
    /// Wispr Flow's hands-free dictation, through its URL scheme.
    #[default]
    WisprFlow,
    /// Diri's own on-device recognition: the answer is sent to the agent
    /// directly, so no window needs to be in front.
    Diri,
    /// Any dictation app (macOS Dictation, Superwhisper, …), started and
    /// stopped by pressing its shortcut.
    Shortcut,
}

/// A recorded shortcut: its label as Diri writes shortcuts (`cmd-alt-d`)
/// and the macOS virtual key code to press.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DictationShortcut {
    pub label: String,
    pub key_code: u16,
}

impl DictationShortcut {
    /// The modifiers in the label, as `diri-voice press` takes them.
    ///
    /// A side is kept (`alt-right`, `cmd-left`) so a bare right Option key is
    /// not pressed as whichever Option happens to be the generic one.
    pub fn modifiers(&self) -> Vec<&'static str> {
        self.label.split('-').filter_map(press_modifier).collect()
    }
}

fn press_modifier(part: &str) -> Option<&'static str> {
    Some(match part {
        "cmd" => "cmd",
        "leftcmd" => "cmd-left",
        "rightcmd" => "cmd-right",
        "ctrl" => "ctrl",
        "leftctrl" => "ctrl-left",
        "rightctrl" => "ctrl-right",
        "alt" => "alt",
        "leftalt" => "alt-left",
        "rightalt" => "alt-right",
        "shift" => "shift",
        "leftshift" => "shift-left",
        "rightshift" => "shift-right",
        "fn" => "fn",
        _ => return None,
    })
}

impl Serialize for UiLanguage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.tag())
    }
}

impl<'de> Deserialize<'de> for UiLanguage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let tag = String::deserialize(deserializer)?;
        Ok(diri_i18n::Language::from_tag(&tag).map_or(Self::System, Self::Fixed))
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WindowMode {
    #[default]
    Windowed,
    Maximized,
    Fullscreen,
}

/// How the main window sits over the desktop. Stored as its own enum so the
/// preferences file never depends on `diri-ui` types.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WindowMaterial {
    /// Blur the desktop behind the window and paint chrome as translucent
    /// tints over it.
    #[default]
    Glass,
    /// A solid window. Cheaper for the compositor: no backdrop is retained.
    Opaque,
}

/// How the whole interface is painted: where chrome takes its hues from and
/// how much desktop shows through. The palette's Change Interface Style page
/// and Settings › Appearance offer these four, in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InterfaceStyle {
    /// Clear glass in neutral hues: the desktop reads through every surface,
    /// the terminal included.
    Modern,
    /// Solid neutral grays, as native macOS windows paint. The terminal keeps
    /// its theme's text colors on a system background.
    System,
    /// Solid, with the color theme tinting every surface.
    Themed,
    /// The color theme's tints laid over a blurred desktop.
    Blended,
}

impl InterfaceStyle {
    pub const ALL: [Self; 4] = [Self::Modern, Self::System, Self::Themed, Self::Blended];

    /// The platform window material the style needs.
    pub const fn material(self) -> WindowMaterial {
        match self {
            Self::Modern | Self::Blended => WindowMaterial::Glass,
            Self::System | Self::Themed => WindowMaterial::Opaque,
        }
    }

    /// The style a preferences file written before styles existed meant.
    pub const fn from_material(material: WindowMaterial) -> Self {
        match material {
            WindowMaterial::Glass => Self::Blended,
            WindowMaterial::Opaque => Self::Themed,
        }
    }

    /// Chrome hues come from neutral system grays rather than the theme.
    pub const fn neutral(self) -> bool {
        matches!(self, Self::Modern | Self::System)
    }

    pub const fn clear_glass(self) -> bool {
        matches!(self, Self::Modern)
    }

    pub fn title(self) -> &'static str {
        crate::i18n::t(match self {
            Self::Modern => "interface_style.modern",
            Self::System => "interface_style.system",
            Self::Themed => "interface_style.themed",
            Self::Blended => "interface_style.blended",
        })
    }
}

/// How terminal panes sit in the window. Comfortable is the default, for
/// files written before density existed too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PaneDensity {
    /// Panes fill the window edge to edge, split by single hairlines.
    Compact,
    /// Each pane is a rounded island with a gap of window chrome around it.
    /// Last because serde only takes `other` on the final variant.
    #[default]
    #[serde(other)]
    Comfortable,
}

impl PaneDensity {
    pub const ALL: [Self; 2] = [Self::Comfortable, Self::Compact];

    /// Chrome between neighbouring islands and between an island and the
    /// window or sidebar edge; zero when compact.
    pub const fn island_gap(self) -> f32 {
        match self {
            Self::Comfortable => 6.0,
            Self::Compact => 0.0,
        }
    }

    pub const fn islands(self) -> bool {
        matches!(self, Self::Comfortable)
    }

    pub fn title(self) -> &'static str {
        crate::i18n::t(match self {
            Self::Comfortable => "density.comfortable",
            Self::Compact => "density.compact",
        })
    }
}

/// The terminal cursor's resting shape. A name this build does not know
/// reads as a block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CursorShapePref {
    Bar,
    Underline,
    #[default]
    #[serde(other)]
    Block,
}

impl CursorShapePref {
    pub const ALL: [Self; 3] = [Self::Block, Self::Bar, Self::Underline];

    pub const fn shape(self) -> diri_proto::grid::CursorShape {
        match self {
            Self::Block => diri_proto::grid::CursorShape::Block,
            Self::Bar => diri_proto::grid::CursorShape::Bar,
            Self::Underline => diri_proto::grid::CursorShape::Underline,
        }
    }

    pub fn title(self) -> &'static str {
        crate::i18n::t(match self {
            Self::Block => "cursor.shape.block",
            Self::Bar => "cursor.shape.bar",
            Self::Underline => "cursor.shape.underline",
        })
    }
}

/// How fast the terminal cursor blinks once the user stops typing. A name
/// this build does not know reads as normal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CursorBlinkPref {
    Off,
    Slow,
    Fast,
    #[default]
    #[serde(other)]
    Normal,
}

impl CursorBlinkPref {
    pub const ALL: [Self; 4] = [Self::Off, Self::Slow, Self::Normal, Self::Fast];

    pub const fn rate(self) -> diri_term::cursor_motion::BlinkRate {
        use diri_term::cursor_motion::BlinkRate;
        match self {
            Self::Off => BlinkRate::Off,
            Self::Slow => BlinkRate::Slow,
            Self::Normal => BlinkRate::Normal,
            Self::Fast => BlinkRate::Fast,
        }
    }

    pub fn title(self) -> &'static str {
        crate::i18n::t(match self {
            Self::Off => "cursor.blink.off",
            Self::Slow => "cursor.blink.slow",
            Self::Normal => "cursor.blink.normal",
            Self::Fast => "cursor.blink.fast",
        })
    }
}

/// Where a new Agent opened from a project (⌘T, New Agent) starts. Stored
/// per project; a missing entry, or a name this build does not know, is the
/// current checkout, which is what every project did before the choice.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum NewAgentStart {
    /// A fresh Diri worktree and branch from the remote default branch,
    /// fetched first (`origin/HEAD`, else `origin/main`, `origin/master`).
    FreshWorktree,
    /// The checkout the source Session works in. Last because serde's
    /// catch-all must be.
    #[default]
    #[serde(other)]
    CurrentCheckout,
}

/// Where a terminal `file:line` link opens. Stored by name; a name this
/// build does not know reads as `Automatic` rather than failing the file.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FileEditor {
    Cursor,
    VsCode,
    Zed,
    /// The app macOS opens the file with; it receives no line number.
    DefaultApp,
    /// The first of Cursor, VS Code and Zed that is installed, otherwise the
    /// file's default app. Last because serde's catch-all must be.
    #[default]
    #[serde(other)]
    Automatic,
}

/// Which Option keys a terminal treats as Meta (`ESC` + key, readline's
/// `M-b`) rather than as the key that types the layout's characters.
/// Stored by name; an unknown name reads as `Automatic`.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OptionAsMeta {
    /// Left Option is Meta, right Option types characters.
    Left,
    /// Right Option is Meta, left Option types characters.
    Right,
    /// Both Option keys are Meta.
    Both,
    /// Neither: Option types the layout's characters.
    Off,
    /// Meta on US layouts, characters on layouts that need Option to type
    /// ASCII. Last because serde's catch-all must be.
    #[default]
    #[serde(other)]
    Automatic,
}

impl WindowMaterial {
    pub const fn to_ui(self) -> diri_ui::Material {
        match self {
            Self::Glass => diri_ui::Material::Glass,
            Self::Opaque => diri_ui::Material::Opaque,
        }
    }

    pub const fn toggled(self) -> Self {
        match self {
            Self::Glass => Self::Opaque,
            Self::Opaque => Self::Glass,
        }
    }
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

impl WindowPlacement {
    /// Repair a placement read from disk. Returns false when it cannot be
    /// trusted at all and should be dropped.
    pub fn normalize(&mut self) -> bool {
        let valid = self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width > 0.0
            && self.height > 0.0;
        if valid {
            self.width = self.width.max(900.0);
            self.height = self.height.max(560.0);
        }
        valid
    }
}

/// A window that was open beside the key window when diri last quit, with
/// enough of its view state to bring it back as it was.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedWindow {
    pub placement: WindowPlacement,
    #[serde(default)]
    pub workspace: Option<diri_proto::workspace::WorkspaceId>,
    #[serde(default)]
    pub selected_session: Option<SessionId>,
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

/// How the leading sidebar presents sessions. This is deliberately a view
/// preference: projects remain attached to every session even when their
/// headers are hidden by the recency view.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SidebarGrouping {
    #[default]
    Project,
    Recency,
}

/// Navigation placement is local presentation only. Both orientations use
/// the same project/session identities and saved ordering.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TabOrientation {
    #[default]
    Vertical,
    Horizontal,
}

impl TabOrientation {
    pub fn toggled(self) -> Self {
        match self {
            Self::Vertical => Self::Horizontal,
            Self::Horizontal => Self::Vertical,
        }
    }
}

/// Sort policy for sidebar sessions. `Custom` preserves the long-standing
/// drag order; the chronological choices never overwrite that saved order.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SidebarOrdering {
    #[default]
    Custom,
    NewestFirst,
    OldestFirst,
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

fn sidebar_lineage_highlights_default() -> bool {
    true
}

/// A channel this build does not know (written by a newer one) reads as unset
/// rather than failing the whole preferences file.
fn deserialize_update_channel<'de, D>(
    deserializer: D,
) -> Result<Option<diri_updater::UpdateChannel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()))
}

impl Prefs {
    /// The chosen update channel, or the one this build came from: a nightly
    /// build keeps following nightlies until the user says otherwise.
    pub fn effective_update_channel(&self) -> diri_updater::UpdateChannel {
        self.update_channel.unwrap_or_else(|| {
            diri_updater::UpdateChannel::for_version(crate::updates::CURRENT_VERSION)
        })
    }
}

fn terminal_follows_last_directory_default() -> bool {
    true
}

fn terminal_ligatures_default() -> bool {
    true
}

const fn terminal_font_weight_default() -> u16 {
    400
}

const fn terminal_bold_weight_default() -> u16 {
    700
}

const fn terminal_cell_width_default() -> u16 {
    100
}

const fn terminal_font_smoothing_default() -> bool {
    true
}

/// A style this build does not know, written by a newer diri, reads as
/// unset (the material decides) rather than failing the whole file.
fn deserialize_interface_style<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<InterfaceStyle>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()))
}

const fn window_transparency_default() -> f32 {
    1.0
}

fn terminal_theme_light_default() -> String {
    "dirijor-light".to_owned()
}

fn terminal_theme_dark_default() -> String {
    "dirijor-dark".to_owned()
}

const fn terminal_line_height_default() -> f32 {
    1.0
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
    /// Interface language, persisted as `"system"` or a catalog tag such as
    /// `"en"`, `"zh-Hans"` or `"es"`.
    pub ui_language: UiLanguage,
    pub confirm_before_closing_session: bool,
    pub status_sounds: bool,
    pub status_notifications: bool,
    /// Which channels blocked, failed and finished sessions reach.
    pub attention_routes: AttentionRoutes,
    /// What the Dock badge counts.
    pub dock_badge: DockBadge,
    /// Read the relevant part of a finished turn's reply aloud (`crate::voice`).
    #[serde(default)]
    pub talk_with_agent: bool,
    /// Agents name their session after the task (`name_session`). The Engine
    /// and the MCP server read it as an Agent starts, under
    /// [`diri_proto::session_naming::PREF_KEY`], so it reaches new sessions.
    pub agents_name_sessions: bool,
    /// After a reading, start Wispr Flow's hands-free dictation for the
    /// session in front of the user.
    #[serde(default)]
    pub talk_dictation: bool,
    /// The system voice for readings, by identifier; empty picks the best
    /// installed voice for each reply's language; `command` pipes readings
    /// to [`Self::talk_voice_command`].
    #[serde(default)]
    pub talk_voice: String,
    /// A shell command that speaks its standard input, for any TTS a user
    /// has (`say -v Ava`, Piper, a script calling a speech API).
    #[serde(default)]
    pub talk_voice_command: String,
    /// What takes the spoken answer when [`Self::talk_dictation`] is on.
    #[serde(default)]
    pub talk_answer: TalkAnswer,
    /// The start/stop shortcut of the user's dictation app, for
    /// [`TalkAnswer::Shortcut`].
    #[serde(default)]
    pub talk_answer_shortcut: Option<DictationShortcut>,
    pub muted_notification_sessions: std::collections::BTreeSet<String>,
    /// Whether "Talk with your agent" reads a tab unless it is one of
    /// [`Self::talk_exceptions`]. False with one exception is "read only
    /// this tab"; new tabs follow this default.
    pub talk_reads_by_default: bool,
    /// Tabs that do the opposite of [`Self::talk_reads_by_default`]: the
    /// title bar's speaker button and the palette's read-aloud commands.
    pub talk_exceptions: std::collections::BTreeSet<String>,
    /// Names the user gave conversations, by the Agent's own conversation id.
    /// History reads them after the tab that carried the rename has closed:
    /// provider transcripts never learn a Diri rename.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub conversation_titles: BTreeMap<String, String>,
    /// Check, download, and verify releases in the background. A staged update
    /// installs on quit or when the user requests a restart.
    pub automatic_updates: bool,
    /// A release the user chose not to install. Persisted so "Skip" outlives
    /// the session that clicked it; empty means nothing is skipped.
    pub skipped_update_version: String,
    /// The feed updates follow. `None` until the user picks one; read it
    /// through [`Prefs::effective_update_channel`].
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_update_channel"
    )]
    pub update_channel: Option<diri_updater::UpdateChannel>,
    pub hibernate_after_minutes: u32,
    pub memory_hard_limit_gb: u64,
    /// Which generation of hibernation defaults this file was last brought
    /// up to. Prefs are written wholesale, so an old default is
    /// indistinguishable from a choice; this lets a raised default reach
    /// users who never touched the setting, once, without ever moving a
    /// value that differs from the old default. Field-level default so a
    /// file written before the field existed reads as revision 0, not as
    /// whatever `Prefs::default()` currently carries.
    #[serde(default)]
    pub hibernation_defaults_revision: u32,
    pub terminal_theme: String,
    /// Follow native appearance changes; terminal_theme stores the resolved palette.
    pub follow_system_theme: bool,
    /// The theme System appearance uses while macOS is light, and the one it
    /// uses while dark. Field-level defaults, so a file from before the pair
    /// existed follows the system exactly as it did.
    #[serde(default = "terminal_theme_light_default")]
    pub terminal_theme_light: String,
    #[serde(default = "terminal_theme_dark_default")]
    pub terminal_theme_dark: String,
    pub terminal_font_size: f32,
    /// Terminal font family by name. Empty follows diri's platform default,
    /// so a file never pins a font that only existed on another Mac.
    #[serde(default)]
    pub terminal_font_family: String,
    /// Row height as a multiple of the font's own line height.
    #[serde(default = "terminal_line_height_default")]
    pub terminal_line_height: f32,
    /// Let the terminal font form its ligatures (`=>`, `!=`). On is the
    /// font's own default, so files written before the field read as on.
    #[serde(default = "terminal_ligatures_default")]
    pub terminal_ligatures: bool,
    /// CSS weight of the regular face (400 Regular, 500 Medium, 300 Light).
    /// The family's nearest face is used, so a weight it lacks is harmless.
    #[serde(default = "terminal_font_weight_default")]
    pub terminal_font_weight: u16,
    /// CSS weight of the face bold cells use.
    #[serde(default = "terminal_bold_weight_default")]
    pub terminal_bold_weight: u16,
    /// CSS weight of the italic face; unset follows the regular weight.
    #[serde(default)]
    pub terminal_italic_weight: Option<u16>,
    /// Cell width as a percentage of the font's advance.
    #[serde(default = "terminal_cell_width_default")]
    pub terminal_cell_width: u16,
    /// Platform font smoothing (macOS thickens light-on-dark text by its
    /// luminance). On is what the terminal always did.
    #[serde(default = "terminal_font_smoothing_default")]
    pub terminal_font_smoothing: bool,
    /// Extra stroke around every glyph, in quarters of a device pixel, for
    /// thin fonts on low-resolution displays. 0 is off.
    #[serde(default)]
    pub terminal_font_thicken: u8,
    pub terminal_copy_on_select: bool,
    /// Open a terminal URL on a plain click. When off, links still open with
    /// Command- or Control-click.
    pub terminal_open_links_on_click: bool,
    /// Link schemes other than the web's that open without asking
    /// (`ssh`, `vscode`, …), added by "Always Open" on the ask sheet.
    #[serde(skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub terminal_trusted_link_schemes: std::collections::BTreeSet<String>,
    pub terminal_hide_pointer: bool,
    /// The cursor a terminal draws until a program asks for another.
    pub terminal_cursor_shape: CursorShapePref,
    pub terminal_cursor_blink: CursorBlinkPref,
    /// Let programs change the cursor's shape and blink (vim's bar in
    /// insert mode). Off, every terminal keeps the shape above.
    pub terminal_cursor_follows_programs: bool,
    /// Give new zsh and bash terminals Diri's OSC 133 prompt and command
    /// marks. The Engine applies it; the store pushes it on every change.
    pub terminal_shell_integration: bool,
    /// Where a clicked `file:line` reference in terminal output opens.
    #[serde(default)]
    pub terminal_file_editor: FileEditor,
    /// Which Option keys are Meta in a terminal.
    #[serde(default)]
    pub terminal_option_as_meta: OptionAsMeta,
    /// Start a new terminal in the directory the last terminal was in,
    /// instead of its project's root. Missing files pick this up as on.
    #[serde(default = "terminal_follows_last_directory_default")]
    pub terminal_follows_last_directory: bool,
    pub terminal_paste_protection: bool,
    /// Whether the window blurs the desktop behind it. Field-level default so
    /// files written before it existed pick up glass.
    #[serde(default)]
    pub window_material: WindowMaterial,
    /// How much desktop the glass lets through, as a multiple of the shipped
    /// tint (1.0). Missing files keep the shipped look.
    #[serde(default = "window_transparency_default")]
    pub window_transparency: f32,
    /// The chosen interface style. `None` in files written before styles
    /// existed; read it through [`Prefs::interface_style`], which falls back
    /// to `window_material`. Stored beside the material rather than in place
    /// of it so an older diri reading this file keeps the right blur.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_interface_style"
    )]
    pub interface_style: Option<InterfaceStyle>,
    /// Islands (comfortable) or flush panes (compact).
    #[serde(default)]
    pub pane_density: PaneDensity,
    /// Last size, position, and presentation mode of the key window.
    pub window_placement: Option<WindowPlacement>,
    /// The other windows open at the last quit, in no particular order. They
    /// come back only when macOS keeps windows across a quit.
    #[serde(default)]
    pub additional_windows: Vec<SavedWindow>,
    /// Whether the leading sidebar was mounted when the app last ran.
    pub sidebar_visible: bool,
    pub sidebar_width: f32,
    pub sidebar_grouping: SidebarGrouping,
    /// Mark a session's parent and children while the pointer or keyboard
    /// cursor rests on it. Missing files pick this up as on.
    #[serde(default = "sidebar_lineage_highlights_default")]
    pub sidebar_lineage_highlights: bool,
    pub tab_orientation: TabOrientation,
    /// Visibility of the top tab strip, independent of the vertical sidebar.
    pub horizontal_tabs_visible: bool,
    /// Initial workspace for new windows; each open window keeps its own selection.
    pub active_workspace: Option<diri_proto::workspace::WorkspaceId>,
    pub sidebar_ordering: SidebarOrdering,
    /// The projectless recency view has one shared archive disclosure rather
    /// than one disclosure per hidden project header.
    pub sidebar_recency_archives_expanded: bool,
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
    /// Presentation-only disclosure state for named workspace headings.
    pub sidebar_collapsed_workspaces: Vec<diri_proto::workspace::WorkspaceId>,
    /// Sessions whose spawned children are folded away.
    pub sidebar_collapsed_sessions: Vec<SessionId>,
    pub sidebar_expanded_archives: Vec<ProjectId>,
    /// Versioned, locally owned one-action Agent workflows.
    #[serde(default, deserialize_with = "deserialize_recipe_book")]
    pub launch_recipes: LaunchRecipeBook,
    /// Per-command keyboard overrides keyed by the command registry's stable
    /// id. A missing entry uses the shipped binding, `null` leaves the command
    /// unassigned, and a string contains a GPUI keystroke such as `cmd-shift-p`.
    /// Unknown ids are retained so opening these preferences in an older diri
    /// build does not erase settings written by a newer one.
    pub shortcut_overrides: BTreeMap<String, Option<String>>,
    /// Physical macOS key code recorded with the global Show App binding.
    /// Keeping the native code avoids assuming a US keyboard layout.
    pub global_shortcut_key_code: Option<u16>,
    /// Session that should regain focus after the daemon's initial hydrate.
    pub last_selected_session: Option<SessionId>,
    /// herdr panes and conversations already brought over, so importing
    /// again only offers what is new. See `crate::herdr_import`.
    pub herdr_imported: std::collections::BTreeSet<String>,
    /// The newest release whose What's New highlights were shown or
    /// dismissed. A new install starts at the running version, so it never
    /// sees highlights; a file written before this field existed reads as
    /// empty, so an update from those versions shows them once.
    #[serde(default)]
    pub whats_new_seen_version: String,
    /// Per-project [`NewAgentStart`], keyed by project id. Only opted-in
    /// projects are written; everything else starts in the current checkout.
    #[serde(default)]
    pub new_agent_start: BTreeMap<String, NewAgentStart>,
    /// Developer frame meter overlay (⌥⌘P). See `crate::perf_overlay`.
    #[serde(default)]
    pub perf_overlay: bool,
    /// Developer per-view render counters (⌥⌘R).
    #[serde(default)]
    pub render_counters: bool,
}

impl Prefs {
    /// The cursor preferences every terminal pane draws with.
    pub fn cursor_preferences(&self) -> diri_term::cursor_motion::CursorPreferences {
        diri_term::cursor_motion::CursorPreferences {
            shape: self.terminal_cursor_shape.shape(),
            blink: self.terminal_cursor_blink.rate(),
            follow_program: self.terminal_cursor_follows_programs,
        }
    }
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            default_agent: AgentKind::CLAUDE_CODE,
            default_spawn_host: None,
            start_at_login: false,
            ui_language: UiLanguage::System,
            confirm_before_closing_session: true,
            status_sounds: true,
            status_notifications: true,
            attention_routes: AttentionRoutes::default(),
            dock_badge: DockBadge::default(),
            talk_with_agent: false,
            agents_name_sessions: true,
            talk_dictation: false,
            talk_voice: String::new(),
            talk_voice_command: String::new(),
            talk_answer: TalkAnswer::default(),
            talk_answer_shortcut: None,
            muted_notification_sessions: Default::default(),
            talk_reads_by_default: true,
            talk_exceptions: Default::default(),
            conversation_titles: BTreeMap::new(),
            automatic_updates: true,
            skipped_update_version: String::new(),
            update_channel: None,
            hibernate_after_minutes: 60,
            memory_hard_limit_gb: 16,
            hibernation_defaults_revision: Self::HIBERNATION_DEFAULTS_REVISION,
            terminal_theme: DEFAULT_THEME.to_owned(),
            follow_system_theme: false,
            terminal_theme_light: terminal_theme_light_default(),
            terminal_theme_dark: terminal_theme_dark_default(),
            terminal_font_size: 13.0,
            terminal_font_family: String::new(),
            terminal_line_height: terminal_line_height_default(),
            terminal_ligatures: true,
            terminal_font_weight: terminal_font_weight_default(),
            terminal_bold_weight: terminal_bold_weight_default(),
            terminal_italic_weight: None,
            terminal_cell_width: terminal_cell_width_default(),
            terminal_font_smoothing: true,
            terminal_font_thicken: 0,
            terminal_copy_on_select: false,
            terminal_open_links_on_click: true,
            terminal_trusted_link_schemes: Default::default(),
            terminal_hide_pointer: true,
            terminal_cursor_shape: CursorShapePref::Block,
            terminal_cursor_blink: CursorBlinkPref::Normal,
            terminal_cursor_follows_programs: true,
            terminal_shell_integration: true,
            terminal_file_editor: FileEditor::Automatic,
            terminal_option_as_meta: OptionAsMeta::Automatic,
            terminal_follows_last_directory: true,
            terminal_paste_protection: false,
            window_material: WindowMaterial::Glass,
            window_transparency: window_transparency_default(),
            interface_style: None,
            pane_density: PaneDensity::Comfortable,
            window_placement: None,
            additional_windows: Vec::new(),
            sidebar_visible: false,
            sidebar_width: 248.0,
            sidebar_grouping: SidebarGrouping::Project,
            sidebar_lineage_highlights: true,
            tab_orientation: TabOrientation::Vertical,
            horizontal_tabs_visible: true,
            active_workspace: None,
            sidebar_ordering: SidebarOrdering::Custom,
            sidebar_recency_archives_expanded: false,
            inspector_open: false,
            inspector_width: 440.0,
            inspector_tab: InspectorTab::Info,
            workbench_primary_fraction: crate::workbench::DEFAULT_PRIMARY_FRACTION,
            quick_open_roots: String::new(),
            sidebar_project_order: Vec::new(),
            sidebar_session_order: Vec::new(),
            sidebar_pinned_projects: Vec::new(),
            sidebar_pinned_sessions: Vec::new(),
            sidebar_collapsed_projects: Vec::new(),
            sidebar_collapsed_workspaces: Vec::new(),
            sidebar_collapsed_sessions: Vec::new(),
            sidebar_expanded_archives: Vec::new(),
            launch_recipes: LaunchRecipeBook::default(),
            shortcut_overrides: BTreeMap::new(),
            global_shortcut_key_code: None,
            last_selected_session: None,
            herdr_imported: Default::default(),
            whats_new_seen_version: crate::updates::CURRENT_VERSION.to_owned(),
            new_agent_start: BTreeMap::new(),
            perf_overlay: false,
            render_counters: false,
        }
    }
}

impl Prefs {
    pub fn new_agent_start(&self, project: &ProjectId) -> NewAgentStart {
        self.new_agent_start
            .get(&project.0)
            .copied()
            .unwrap_or_default()
    }

    /// Stores `start` for `project`, dropping the default so the file only
    /// lists projects that opted in.
    pub fn set_new_agent_start(&mut self, project: &ProjectId, start: NewAgentStart) {
        if start == NewAgentStart::CurrentCheckout {
            self.new_agent_start.remove(&project.0);
        } else {
            self.new_agent_start.insert(project.0.clone(), start);
        }
    }

    pub const MIN_TERMINAL_FONT_SIZE: f32 = 10.0;
    pub const MAX_TERMINAL_FONT_SIZE: f32 = 20.0;
    pub const MIN_TERMINAL_LINE_HEIGHT: f32 = 1.0;
    pub const MAX_TERMINAL_LINE_HEIGHT: f32 = 2.0;
    pub const MIN_TERMINAL_CELL_WIDTH: u16 = 80;
    pub const MAX_TERMINAL_CELL_WIDTH: u16 = 150;
    pub const TERMINAL_CELL_WIDTH_STEP: u16 = 5;
    pub const MAX_TERMINAL_FONT_THICKEN: u8 = gpui::GlyphRasterStyle::MAX_STROKE;

    pub fn path() -> PathBuf {
        let home = diri_platform::home_dir()
            .map(|p| p.into_os_string())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/nonexistent"));
        Self::path_in_home(&home)
    }

    pub fn path_in_home(home: &Path) -> PathBuf {
        DirijorPaths::prefs_file(home)
    }

    /// Bump when a hibernation default changes, and teach
    /// [`Self::migrate_hibernation_defaults`] the old value to move.
    pub const HIBERNATION_DEFAULTS_REVISION: u32 = 1;

    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read(path) {
            Ok(bytes) => {
                let mut prefs: Self = serde_json::from_slice(&bytes)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                prefs.migrate_hibernation_defaults();
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
        diri_platform::private::create_dir_all(parent)?;
        let bytes = serde_json::to_vec_pretty(&normalized)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        diri_platform::private::replace_via(&path.with_extension("json.tmp"), path, bytes)
    }

    pub fn zoom_terminal(&mut self, delta: f32) {
        self.terminal_font_size = (self.terminal_font_size + delta)
            .clamp(Self::MIN_TERMINAL_FONT_SIZE, Self::MAX_TERMINAL_FONT_SIZE);
    }

    /// Chooses the terminal family. The faces chosen for the previous family
    /// go back to Regular and Bold: another family's Medium may not exist.
    pub fn set_terminal_font_family(&mut self, family: String) {
        if family != self.terminal_font_family {
            self.terminal_font_weight = terminal_font_weight_default();
            self.terminal_bold_weight = terminal_bold_weight_default();
            self.terminal_italic_weight = None;
        }
        self.terminal_font_family = family;
    }

    pub fn reset_terminal_zoom(&mut self) {
        self.terminal_font_size = 13.0;
    }

    /// Moves values still sitting on a superseded default onto the current
    /// one. Anything the user changed from the old default is left alone.
    pub fn migrate_hibernation_defaults(&mut self) {
        if self.hibernation_defaults_revision < 1 {
            // Revision 1 (2026-09): 15 min → 1 h, 6 GB → 16 GB. Sessions
            // were being frozen mid-work far too readily.
            if self.hibernate_after_minutes == 15 {
                self.hibernate_after_minutes = 60;
            }
            if self.memory_hard_limit_gb == 6 {
                self.memory_hard_limit_gb = 16;
            }
        }
        self.hibernation_defaults_revision = Self::HIBERNATION_DEFAULTS_REVISION;
    }

    /// The interface style in effect. A stored style whose material no
    /// longer matches `window_material` lost to a later material change (an
    /// older diri, or a direct write), so the material decides.
    pub fn interface_style(&self) -> InterfaceStyle {
        match self.interface_style {
            Some(style) if style.material() == self.window_material => style,
            _ => InterfaceStyle::from_material(self.window_material),
        }
    }

    pub fn set_interface_style(&mut self, style: InterfaceStyle) {
        self.interface_style = Some(style);
        self.window_material = style.material();
    }

    pub fn apply_system_theme(&mut self, dark: bool) -> bool {
        if !self.follow_system_theme {
            return false;
        }
        let id = if dark {
            &self.terminal_theme_dark
        } else {
            &self.terminal_theme_light
        };
        if self.terminal_theme == *id {
            return false;
        }
        self.terminal_theme = id.clone();
        true
    }

    /// The theme System appearance shows on one side of the OS appearance.
    pub fn system_theme(&self, dark: bool) -> &str {
        if dark {
            &self.terminal_theme_dark
        } else {
            &self.terminal_theme_light
        }
    }

    /// Chooses `id` for System appearance's light or dark side, and shows it
    /// now when the OS is on that side. System appearance stays on.
    pub fn set_system_theme(&mut self, dark: bool, id: &str, system_is_dark: bool) {
        if dark {
            self.terminal_theme_dark = id.to_owned();
        } else {
            self.terminal_theme_light = id.to_owned();
        }
        if self.follow_system_theme && dark == system_is_dark {
            self.terminal_theme = id.to_owned();
        }
    }

    /// Chooses one theme for every OS appearance. It also becomes the
    /// remembered theme for its own side (`dark` is the theme's appearance),
    /// so switching to System later keeps the family the person picked.
    pub fn set_fixed_theme(&mut self, id: &str, dark: bool) {
        self.follow_system_theme = false;
        self.terminal_theme = id.to_owned();
        if dark {
            self.terminal_theme_dark = id.to_owned();
        } else {
            self.terminal_theme_light = id.to_owned();
        }
    }

    pub fn normalize(&mut self) {
        self.window_transparency = if self.window_transparency.is_finite() {
            self.window_transparency
                .clamp(0.0, diri_ui::SemanticColors::MAX_TRANSPARENCY)
        } else {
            window_transparency_default()
        };
        if !self.terminal_font_size.is_finite() {
            self.terminal_font_size = 13.0;
        }
        self.terminal_font_size = self
            .terminal_font_size
            .clamp(Self::MIN_TERMINAL_FONT_SIZE, Self::MAX_TERMINAL_FONT_SIZE);
        self.terminal_line_height = if self.terminal_line_height.is_finite() {
            // Tenths, so stepping by 0.1 never accumulates float drift.
            ((self.terminal_line_height * 10.0).round() / 10.0).clamp(
                Self::MIN_TERMINAL_LINE_HEIGHT,
                Self::MAX_TERMINAL_LINE_HEIGHT,
            )
        } else {
            terminal_line_height_default()
        };
        let weight = |weight: u16| weight.clamp(100, 900);
        self.terminal_font_weight = weight(self.terminal_font_weight);
        self.terminal_bold_weight = weight(self.terminal_bold_weight);
        self.terminal_italic_weight = self.terminal_italic_weight.map(weight);
        self.terminal_cell_width = self
            .terminal_cell_width
            .clamp(Self::MIN_TERMINAL_CELL_WIDTH, Self::MAX_TERMINAL_CELL_WIDTH);
        self.terminal_font_thicken = self
            .terminal_font_thicken
            .min(Self::MAX_TERMINAL_FONT_THICKEN);
        let family = self.terminal_font_family.trim();
        if family.len() != self.terminal_font_family.len() {
            self.terminal_font_family = family.to_owned();
        }
        if self
            .window_placement
            .as_mut()
            .is_some_and(|placement| !placement.normalize())
        {
            self.window_placement = None;
        }
        self.additional_windows
            .retain_mut(|window| window.placement.normalize());
        if !self.sidebar_width.is_finite() {
            self.sidebar_width = 248.0;
        }
        self.sidebar_width = self.sidebar_width.clamp(200.0, 400.0);
        if !self.inspector_width.is_finite() {
            self.inspector_width = 440.0;
        }
        self.inspector_width = self.inspector_width.clamp(300.0, 720.0);
        if self.sidebar_grouping == SidebarGrouping::Recency
            && self.sidebar_ordering == SidebarOrdering::Custom
        {
            self.sidebar_ordering = SidebarOrdering::NewestFirst;
        }
        if !self.workbench_primary_fraction.is_finite() {
            self.workbench_primary_fraction = crate::workbench::DEFAULT_PRIMARY_FRACTION;
        }
        self.workbench_primary_fraction = self.workbench_primary_fraction.clamp(0.0, 1.0);
        if self.terminal_theme.is_empty() {
            self.terminal_theme = DEFAULT_THEME.to_owned();
        }
        if self.terminal_theme_light.is_empty() {
            self.terminal_theme_light = terminal_theme_light_default();
        }
        if self.terminal_theme_dark.is_empty() {
            self.terminal_theme_dark = terminal_theme_dark_default();
        }
        self.launch_recipes.normalize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch_recipe::{LaunchRecipe, RecipeProject};

    #[test]
    fn only_a_waiting_session_bounces_the_dock_by_default_and_saved_routes_keep_it() {
        let fresh: Prefs = serde_json::from_str("{}").unwrap();
        let routes = fresh.attention_routes;
        assert!(routes.blocked.bounce);
        assert!(!routes.error.bounce && !routes.done.bounce);
        // Routes saved before the bounce channel existed: each state takes
        // its own default for the missing channel and keeps what was saved.
        let saved: Prefs = serde_json::from_str(
            r#"{"attentionRoutes":{"blocked":{"notify":false},"done":{"sound":false}}}"#,
        )
        .unwrap();
        let routes = saved.attention_routes;
        assert!(!routes.blocked.notify && routes.blocked.bounce && routes.blocked.badge);
        assert!(!routes.done.sound && !routes.done.bounce && routes.done.notify);
        let mut changed = Prefs::default();
        changed.attention_routes.blocked.bounce = false;
        changed.attention_routes.done.bounce = true;
        let read: Prefs = serde_json::from_str(&serde_json::to_string(&changed).unwrap()).unwrap();
        assert_eq!(read.attention_routes, changed.attention_routes);
    }

    #[test]
    fn agent_session_names_default_on_and_are_read_where_the_engine_looks() {
        let older: Prefs = serde_json::from_str("{}").unwrap();
        assert!(older.agents_name_sessions);
        let mut prefs = Prefs::default();
        let written = serde_json::to_vec(&prefs).unwrap();
        assert!(diri_proto::session_naming::enabled(&written));
        prefs.agents_name_sessions = false;
        let written = serde_json::to_vec(&prefs).unwrap();
        assert!(!diri_proto::session_naming::enabled(&written));
    }

    #[test]
    fn files_from_before_interface_styles_keep_their_material() {
        let glass: Prefs = serde_json::from_str("{}").unwrap();
        assert_eq!(glass.interface_style(), InterfaceStyle::Blended);
        assert_eq!(glass.pane_density, PaneDensity::Comfortable);
        let opaque: Prefs = serde_json::from_str(r#"{"windowMaterial":"opaque"}"#).unwrap();
        assert_eq!(opaque.interface_style(), InterfaceStyle::Themed);
    }

    #[test]
    fn a_chosen_style_round_trips_and_carries_its_material() {
        for style in InterfaceStyle::ALL {
            let mut prefs = Prefs::default();
            prefs.set_interface_style(style);
            prefs.pane_density = PaneDensity::Compact;
            assert_eq!(prefs.window_material, style.material());
            let read: Prefs =
                serde_json::from_str(&serde_json::to_string(&prefs).unwrap()).unwrap();
            assert_eq!(read.interface_style(), style);
            assert_eq!(read.pane_density, PaneDensity::Compact);
        }
    }

    #[test]
    fn a_later_material_change_outranks_a_stale_style() {
        // An older diri only knows the material; flipping it there must win.
        let mut prefs = Prefs::default();
        prefs.set_interface_style(InterfaceStyle::Modern);
        prefs.window_material = WindowMaterial::Opaque;
        assert_eq!(prefs.interface_style(), InterfaceStyle::Themed);
    }

    #[test]
    fn unknown_density_and_style_names_fall_back() {
        let prefs: Prefs = serde_json::from_str(r#"{"paneDensity":"airy"}"#).unwrap();
        assert_eq!(prefs.pane_density, PaneDensity::Comfortable);
        let prefs: Prefs = serde_json::from_str(r#"{"interfaceStyle":"neon"}"#).unwrap();
        assert_eq!(prefs.interface_style(), InterfaceStyle::Blended);
    }

    #[test]
    fn cursor_preferences_default_to_the_shipped_cursor_and_round_trip() {
        // A file written before the settings existed changes nothing.
        let older: Prefs = serde_json::from_str("{}").unwrap();
        assert_eq!(
            older.cursor_preferences(),
            diri_term::cursor_motion::CursorPreferences::default()
        );
        let prefs: Prefs = serde_json::from_str(
            r#"{"terminalCursorShape":"bar","terminalCursorBlink":"off","terminalCursorFollowsPrograms":false}"#,
        )
        .unwrap();
        let cursor = prefs.cursor_preferences();
        assert_eq!(cursor.shape, diri_proto::grid::CursorShape::Bar);
        assert_eq!(cursor.blink, diri_term::cursor_motion::BlinkRate::Off);
        assert!(!cursor.follow_program);
        let again: Prefs =
            serde_json::from_str(&serde_json::to_string(&prefs).unwrap()).unwrap();
        assert_eq!(again.cursor_preferences(), cursor);
        // Names from a newer build fall back instead of failing the file.
        let newer: Prefs = serde_json::from_str(
            r#"{"terminalCursorShape":"hollow","terminalCursorBlink":"pulse"}"#,
        )
        .unwrap();
        assert_eq!(newer.terminal_cursor_shape, CursorShapePref::Block);
        assert_eq!(newer.terminal_cursor_blink, CursorBlinkPref::Normal);
    }

    #[test]
    fn new_agents_start_in_the_current_checkout_unless_a_project_opts_in() {
        let project = ProjectId("p".into());
        let legacy: Prefs = serde_json::from_str("{}").unwrap();
        assert_eq!(
            legacy.new_agent_start(&project),
            NewAgentStart::CurrentCheckout
        );
        assert_eq!(
            Prefs::default().new_agent_start(&project),
            NewAgentStart::CurrentCheckout
        );

        let mut prefs = Prefs::default();
        prefs.set_new_agent_start(&project, NewAgentStart::FreshWorktree);
        let json = serde_json::to_value(&prefs).unwrap();
        assert_eq!(
            json["newAgentStart"],
            serde_json::json!({ "p": "freshWorktree" })
        );
        let restored: Prefs = serde_json::from_value(json).unwrap();
        assert_eq!(
            restored.new_agent_start(&project),
            NewAgentStart::FreshWorktree
        );
        assert_eq!(
            restored.new_agent_start(&ProjectId("other".into())),
            NewAgentStart::CurrentCheckout
        );

        // Turning it off removes the entry instead of writing the default.
        prefs.set_new_agent_start(&project, NewAgentStart::CurrentCheckout);
        assert!(prefs.new_agent_start.is_empty());
        // A value from a newer build reads as the current checkout.
        let future: Prefs =
            serde_json::from_str(r#"{"newAgentStart":{"p":"somethingNew"}}"#).unwrap();
        assert_eq!(
            future.new_agent_start(&project),
            NewAgentStart::CurrentCheckout
        );
    }

    #[test]
    fn system_appearance_is_opt_in_persisted_and_only_changes_with_the_os() {
        let mut prefs: Prefs = serde_json::from_str(r#"{"terminalTheme":"vesper"}"#).unwrap();
        assert!(!prefs.follow_system_theme);
        assert!(!prefs.apply_system_theme(false));
        assert_eq!(prefs.terminal_theme, "vesper");

        prefs.follow_system_theme = true;
        assert!(prefs.apply_system_theme(false));
        assert_eq!(prefs.terminal_theme, "dirijor-light");
        assert!(!prefs.apply_system_theme(false));
        let mut restored: Prefs =
            serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
        assert!(restored.follow_system_theme);
        assert!(restored.apply_system_theme(true));
        assert_eq!(restored.terminal_theme, "dirijor-dark");
        assert!(!restored.apply_system_theme(true));
        restored.follow_system_theme = false;
        assert!(!restored.apply_system_theme(false));
        assert_eq!(restored.terminal_theme, "dirijor-dark");
    }

    #[test]
    fn system_appearance_uses_the_chosen_light_and_dark_pair() {
        // A file from before the pair follows the system as it always did.
        let mut prefs: Prefs =
            serde_json::from_str(r#"{"terminalTheme":"dirijor-dark","followSystemTheme":true}"#)
                .unwrap();
        assert_eq!(prefs.system_theme(false), "dirijor-light");
        assert_eq!(prefs.system_theme(true), "dirijor-dark");

        // Choosing the light side while the OS is dark keeps the dark theme
        // on screen until the OS turns light.
        prefs.set_system_theme(false, "github-light-default", true);
        assert!(prefs.follow_system_theme);
        assert_eq!(prefs.terminal_theme, "dirijor-dark");
        prefs.set_system_theme(true, "vesper", true);
        assert_eq!(prefs.terminal_theme, "vesper", "the visible side applies now");
        assert!(prefs.apply_system_theme(false));
        assert_eq!(prefs.terminal_theme, "github-light-default");
        assert!(prefs.apply_system_theme(true));
        assert_eq!(prefs.terminal_theme, "vesper");

        let restored: Prefs = serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
        assert_eq!(restored.terminal_theme_light, "github-light-default");
        assert_eq!(restored.terminal_theme_dark, "vesper");

        // A fixed choice turns System off and is remembered for its side.
        prefs.set_fixed_theme("solarized-light", false);
        assert!(!prefs.follow_system_theme);
        assert_eq!(prefs.terminal_theme, "solarized-light");
        assert_eq!(prefs.system_theme(false), "solarized-light");
        assert_eq!(prefs.system_theme(true), "vesper");
        assert!(!prefs.apply_system_theme(true), "fixed themes ignore the OS");
    }

    #[test]
    fn option_as_meta_defaults_to_automatic_and_round_trips() {
        let legacy: Prefs = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.terminal_option_as_meta, OptionAsMeta::Automatic);
        let unknown: Prefs =
            serde_json::from_str(r#"{"terminalOptionAsMeta":"middle"}"#).unwrap();
        assert_eq!(unknown.terminal_option_as_meta, OptionAsMeta::Automatic);
        for choice in [
            OptionAsMeta::Left,
            OptionAsMeta::Right,
            OptionAsMeta::Both,
            OptionAsMeta::Off,
        ] {
            let prefs = Prefs {
                terminal_option_as_meta: choice,
                ..Prefs::default()
            };
            let restored: Prefs =
                serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
            assert_eq!(restored.terminal_option_as_meta, choice);
        }
    }

    #[test]
    fn ui_language_persists_as_a_tag_and_unknown_tags_follow_the_system() {
        let legacy: Prefs = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.ui_language, UiLanguage::System);
        let chinese = Prefs {
            ui_language: UiLanguage::Fixed(diri_i18n::Language::SimplifiedChinese),
            ..Prefs::default()
        };
        let json = serde_json::to_value(&chinese).unwrap();
        assert_eq!(json["uiLanguage"], "zh-Hans");
        let restored: Prefs = serde_json::from_value(json).unwrap();
        assert_eq!(restored.ui_language, chinese.ui_language);
        // A language a newer diri added reads as System here, not an error
        // that would discard every other preference.
        let newer: Prefs =
            serde_json::from_str(r#"{"uiLanguage":"ja","statusSounds":false}"#).unwrap();
        assert_eq!(newer.ui_language, UiLanguage::System);
        assert!(!newer.status_sounds);
    }

    #[test]
    fn terminal_typography_defaults_round_trips_and_repairs() {
        let legacy: Prefs = serde_json::from_str(r#"{"terminalFontSize":14}"#).unwrap();
        assert_eq!(legacy.terminal_font_family, "");
        assert_eq!(legacy.terminal_line_height, 1.0);
        assert!(legacy.terminal_ligatures, "a file without the field keeps ligatures");

        let mut prefs = Prefs {
            terminal_font_family: "  JetBrains Mono ".to_owned(),
            terminal_line_height: 1.2999,
            terminal_ligatures: false,
            ..legacy
        };
        prefs.normalize();
        assert_eq!(prefs.terminal_font_family, "JetBrains Mono");
        assert_eq!(prefs.terminal_line_height, 1.3);
        let restored: Prefs = serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
        assert_eq!(restored, prefs);

        for (stored, repaired) in [(f32::NAN, 1.0), (0.4, 1.0), (9.0, 2.0)] {
            let mut prefs = Prefs {
                terminal_line_height: stored,
                ..Prefs::default()
            };
            prefs.normalize();
            assert_eq!(prefs.terminal_line_height, repaired);
        }
    }

    #[test]
    fn font_faces_cell_width_and_thickening_default_round_trip_and_repair() {
        // A file from before these settings paints exactly as it did.
        let legacy: Prefs = serde_json::from_str(r#"{"terminalFontSize":14}"#).unwrap();
        assert_eq!(legacy.terminal_font_weight, 400);
        assert_eq!(legacy.terminal_bold_weight, 700);
        assert_eq!(legacy.terminal_italic_weight, None);
        assert_eq!(legacy.terminal_cell_width, 100);
        assert!(legacy.terminal_font_smoothing);
        assert_eq!(legacy.terminal_font_thicken, 0);

        let prefs = Prefs {
            terminal_font_weight: 500,
            terminal_bold_weight: 800,
            terminal_italic_weight: Some(300),
            terminal_cell_width: 110,
            terminal_font_smoothing: false,
            terminal_font_thicken: 2,
            ..Prefs::default()
        };
        let json = serde_json::to_value(&prefs).unwrap();
        assert_eq!(json["terminalFontWeight"], 500);
        assert_eq!(json["terminalCellWidth"], 110);
        assert_eq!(json["terminalFontThicken"], 2);
        let restored: Prefs = serde_json::from_value(json).unwrap();
        assert_eq!(restored, prefs);

        let mut wild = Prefs {
            terminal_font_weight: 0,
            terminal_bold_weight: 5000,
            terminal_italic_weight: Some(50),
            terminal_cell_width: 400,
            terminal_font_thicken: 200,
            ..Prefs::default()
        };
        wild.normalize();
        assert_eq!(wild.terminal_font_weight, 100);
        assert_eq!(wild.terminal_bold_weight, 900);
        assert_eq!(wild.terminal_italic_weight, Some(100));
        assert_eq!(wild.terminal_cell_width, Prefs::MAX_TERMINAL_CELL_WIDTH);
        assert_eq!(wild.terminal_font_thicken, Prefs::MAX_TERMINAL_FONT_THICKEN);
        let mut narrow = Prefs {
            terminal_cell_width: 10,
            ..Prefs::default()
        };
        narrow.normalize();
        assert_eq!(narrow.terminal_cell_width, Prefs::MIN_TERMINAL_CELL_WIDTH);

        // Faces belong to a family; choosing another one resets them, and
        // choosing the same one again keeps them.
        let mut chosen = prefs.clone();
        chosen.set_terminal_font_family(String::new());
        assert_eq!(chosen.terminal_font_weight, 500);
        chosen.set_terminal_font_family("Menlo".to_owned());
        assert_eq!(chosen.terminal_font_family, "Menlo");
        assert_eq!(
            (
                chosen.terminal_font_weight,
                chosen.terminal_bold_weight,
                chosen.terminal_italic_weight
            ),
            (400, 700, None)
        );
        assert_eq!(chosen.terminal_cell_width, 110, "cell width is not a face");
    }

    #[test]
    fn tab_orientation_defaults_migrates_and_round_trips_without_changing_order() {
        let legacy: Prefs = serde_json::from_str(r#"{"sidebarVisible":true}"#).unwrap();
        assert_eq!(legacy.tab_orientation, TabOrientation::Vertical);
        let prefs = Prefs {
            tab_orientation: TabOrientation::Horizontal,
            sidebar_session_order: vec![SessionId::new("second"), SessionId::new("first")],
            last_selected_session: Some(SessionId::new("first")),
            ..legacy
        };
        let restored: Prefs = serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
        assert_eq!(restored, prefs);
    }

    fn prefs_with_hibernation(minutes: u32, gb: u64, revision: Option<u32>) -> Prefs {
        let mut value = serde_json::to_value(Prefs::default()).expect("serialize prefs");
        value["hibernateAfterMinutes"] = serde_json::json!(minutes);
        value["memoryHardLimitGb"] = serde_json::json!(gb);
        match revision {
            Some(revision) => value["hibernationDefaultsRevision"] = serde_json::json!(revision),
            None => {
                value
                    .as_object_mut()
                    .expect("prefs object")
                    .remove("hibernationDefaultsRevision");
            }
        }
        let mut prefs: Prefs = serde_json::from_value(value).expect("readable");
        prefs.migrate_hibernation_defaults();
        prefs
    }

    #[test]
    fn stale_hibernation_defaults_move_to_the_current_ones_once() {
        // A file written before the revision field existed, still on the
        // old defaults: both move.
        let migrated = prefs_with_hibernation(15, 6, None);
        assert_eq!(migrated.hibernate_after_minutes, 60);
        assert_eq!(migrated.memory_hard_limit_gb, 16);
        assert_eq!(
            migrated.hibernation_defaults_revision,
            Prefs::HIBERNATION_DEFAULTS_REVISION
        );
        // A deliberate choice away from the old defaults is untouched.
        let chosen = prefs_with_hibernation(30, 8, None);
        assert_eq!(chosen.hibernate_after_minutes, 30);
        assert_eq!(chosen.memory_hard_limit_gb, 8);
        // Choosing the old default AFTER the migration ran sticks.
        let rechosen = prefs_with_hibernation(15, 6, Some(1));
        assert_eq!(rechosen.hibernate_after_minutes, 15);
        assert_eq!(rechosen.memory_hard_limit_gb, 6);
    }

    #[test]
    fn file_editor_choice_round_trips_and_unknown_names_fall_back() {
        let fresh: Prefs = serde_json::from_str("{}").unwrap();
        assert_eq!(fresh.terminal_file_editor, FileEditor::Automatic);
        let saved: Prefs = serde_json::from_str(r#"{"terminalFileEditor":"zed"}"#).unwrap();
        assert_eq!(saved.terminal_file_editor, FileEditor::Zed);
        let future: Prefs =
            serde_json::from_str(r#"{"terminalFileEditor":"sublime","terminalCopyOnSelect":true}"#)
                .unwrap();
        assert_eq!(future.terminal_file_editor, FileEditor::Automatic);
        assert!(
            future.terminal_copy_on_select,
            "the rest of the file survives"
        );
    }

    #[test]
    fn fresh_preferences_close_panels_but_saved_choices_survive() {
        let fresh: Prefs = serde_json::from_str("{}").expect("missing preferences use defaults");
        assert!(!fresh.sidebar_visible);
        assert!(fresh.sidebar_lineage_highlights);
        assert!(!fresh.inspector_open);
        assert_eq!(fresh.sidebar_grouping, SidebarGrouping::Project);
        assert_eq!(fresh.sidebar_ordering, SidebarOrdering::Custom);
        for sidebar in [false, true] {
            for inspector in [false, true] {
                let saved = Prefs {
                    sidebar_visible: sidebar,
                    inspector_open: inspector,
                    ..Prefs::default()
                };
                let restored: Prefs =
                    serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
                assert_eq!(restored.sidebar_visible, sidebar);
                assert_eq!(restored.inspector_open, inspector);
            }
        }

        let saved = Prefs {
            sidebar_grouping: SidebarGrouping::Recency,
            sidebar_ordering: SidebarOrdering::OldestFirst,
            sidebar_recency_archives_expanded: true,
            ..Prefs::default()
        };
        let restored: Prefs = serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
        assert_eq!(restored.sidebar_grouping, SidebarGrouping::Recency);
        assert_eq!(restored.sidebar_ordering, SidebarOrdering::OldestFirst);
        assert!(restored.sidebar_recency_archives_expanded);
    }

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
    fn older_preferences_migrate_to_default_shortcuts() {
        let mut value = serde_json::to_value(Prefs::default()).expect("serialize prefs");
        value
            .as_object_mut()
            .expect("prefs object")
            .remove("shortcutOverrides");
        let prefs: Prefs = serde_json::from_value(value).expect("old preferences remain readable");
        assert!(prefs.shortcut_overrides.is_empty());
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

    #[test]
    fn update_channel_defaults_to_the_build_and_persists_a_choice() {
        use diri_updater::UpdateChannel;

        let legacy: Prefs = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.update_channel, None);
        assert_eq!(
            legacy.effective_update_channel(),
            UpdateChannel::for_version(crate::updates::CURRENT_VERSION)
        );
        assert!(
            serde_json::to_value(&legacy)
                .unwrap()
                .get("updateChannel")
                .is_none()
        );

        let prefs = Prefs {
            update_channel: Some(UpdateChannel::Nightly),
            ..Prefs::default()
        };
        let json = serde_json::to_value(&prefs).unwrap();
        assert_eq!(json["updateChannel"], "nightly");
        let restored: Prefs = serde_json::from_value(json).unwrap();
        assert_eq!(restored.effective_update_channel(), UpdateChannel::Nightly);

        let future: Prefs =
            serde_json::from_str(r#"{"updateChannel":"beta","automaticUpdates":false}"#).unwrap();
        assert_eq!(future.update_channel, None);
        assert!(
            !future.automatic_updates,
            "the rest of the file still loads"
        );
    }
}
