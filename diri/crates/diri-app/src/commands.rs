//! The application's command registry.
//!
//! A command's typed GPUI action, default key binding, context, and palette
//! presentation live together here. Views execute actions; they do not decode
//! keystrokes or maintain their own copies of shortcut labels.

use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

use gpui::{Action, App, KeyBinding, Keystroke, actions};

pub const APP_CONTEXT: &str = "Diri";
pub const SESSION_NAVIGATION_CONTEXT: &str = "DiriSessionNavigation";
pub const TERMINAL_CONTEXT: &str = "DiriTerminal";
pub const NAVIGATION_CONTEXT: &str = "DiriNavigation";
pub const UTILITY_CONTEXT: &str = "DiriUtility";

pub type ShortcutOverrides = BTreeMap<String, Option<String>>;

static ACTIVE_SHORTCUT_OVERRIDES: OnceLock<RwLock<ShortcutOverrides>> = OnceLock::new();

actions!(diri_app, [Quit, HideApp, CloseWindow]);

actions!(
    diri,
    [
        CloseSession,
        ReopenSession,
        OpenLauncher,
        NewDefaultSession,
        NewTerminal,
        NewCodexSession,
        ToggleCommandPalette,
        ToggleQuickOpen,
        ToggleHistory,
        ToggleNotifications,
        ToggleOverview,
        OpenWorktrees,
        OpenSettings,
        ToggleSidebar,
        FocusSidebar,
        ToggleInspector,
        ToggleAuxiliaryTerminal,
        QuoteSelection,
        QuoteSelectionToSession,
        ArchiveSelectedSession,
        RenameSelectedSession,
        DelegateSelectedSession,
        SelectNextAttentionSession,
        CheckForUpdates,
        SelectPreviousSession,
        SelectNextSession,
        MoveSelectedSessionUp,
        MoveSelectedSessionDown,
        SelectSession1,
        SelectSession2,
        SelectSession3,
        SelectSession4,
        SelectSession5,
        SelectSession6,
        SelectSession7,
        SelectSession8,
        SelectLastSession,
        CloseSurface,
        MoveUp,
        MoveDown,
        Activate,
    ]
);

actions!(
    diri_terminal,
    [
        OpenFind,
        FindNext,
        FindPrevious,
        CloseFind,
        ZoomIn,
        ZoomOut,
        ResetZoom,
        Paste,
        CopySelection,
    ]
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommandId {
    Quit,
    HideApp,
    CloseWindow,
    CloseSession,
    ReopenSession,
    OpenLauncher,
    NewDefaultSession,
    NewTerminal,
    NewCodexSession,
    ToggleCommandPalette,
    ToggleQuickOpen,
    ToggleHistory,
    ToggleNotifications,
    ToggleOverview,
    OpenWorktrees,
    OpenSettings,
    ToggleSidebar,
    FocusSidebar,
    ToggleInspector,
    ToggleAuxiliaryTerminal,
    QuoteSelection,
    QuoteSelectionToSession,
    ArchiveSelectedSession,
    RenameSelectedSession,
    DelegateSelectedSession,
    SelectNextAttentionSession,
    CheckForUpdates,
    SelectPreviousSession,
    SelectNextSession,
    MoveSelectedSessionUp,
    MoveSelectedSessionDown,
    SelectSession1,
    SelectSession2,
    SelectSession3,
    SelectSession4,
    SelectSession5,
    SelectSession6,
    SelectSession7,
    SelectSession8,
    SelectLastSession,
    OpenFind,
    FindNext,
    FindPrevious,
    ZoomIn,
    ZoomOut,
    ResetZoom,
    Paste,
    CopySelection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShortcutCategory {
    Sessions,
    Navigation,
    Workspace,
    Terminal,
    Application,
}

impl ShortcutCategory {
    pub const ALL: [Self; 5] = [
        Self::Sessions,
        Self::Navigation,
        Self::Workspace,
        Self::Terminal,
        Self::Application,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Sessions => "Sessions",
            Self::Navigation => "Navigation",
            Self::Workspace => "Workspace",
            Self::Terminal => "Terminal",
            Self::Application => "Application",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShortcutMetadata {
    pub title: &'static str,
    pub description: &'static str,
    pub category: ShortcutCategory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaletteMetadata {
    pub title: &'static str,
    pub system_image: &'static str,
    pub keywords: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub id: CommandId,
    pub stable_id: &'static str,
    pub keystroke: Option<&'static str>,
    pub alternate_keystrokes: &'static [&'static str],
    pub shortcut: Option<&'static str>,
    pub context: Option<&'static str>,
    pub palette: Option<PaletteMetadata>,
}

macro_rules! spec {
    ($id:ident, $stable_id:literal, $key:expr, $shortcut:expr, $context:expr) => {
        CommandSpec {
            id: CommandId::$id,
            stable_id: $stable_id,
            keystroke: $key,
            alternate_keystrokes: &[],
            shortcut: $shortcut,
            context: $context,
            palette: None,
        }
    };
    ($id:ident, $stable_id:literal, $key:expr, $shortcut:expr, $context:expr, $title:literal, $image:literal, $keywords:literal) => {
        CommandSpec {
            id: CommandId::$id,
            stable_id: $stable_id,
            keystroke: $key,
            alternate_keystrokes: &[],
            shortcut: $shortcut,
            context: $context,
            palette: Some(PaletteMetadata {
                title: $title,
                system_image: $image,
                keywords: $keywords,
            }),
        }
    };
}

macro_rules! spec_with_alternates {
    ($id:ident, $stable_id:literal, $key:expr, [$($alternate:literal),+], $shortcut:expr, $context:expr) => {
        CommandSpec {
            id: CommandId::$id,
            stable_id: $stable_id,
            keystroke: $key,
            alternate_keystrokes: &[$($alternate),+],
            shortcut: $shortcut,
            context: $context,
            palette: None,
        }
    };
}

pub const COMMANDS: &[CommandSpec] = &[
    spec!(Quit, "quit", Some("cmd-q"), Some("⌘Q"), None),
    spec!(HideApp, "hide-app", Some("cmd-h"), Some("⌘H"), None),
    spec!(CloseWindow, "close-window", None, None, None),
    spec!(
        CloseSession,
        "close-session",
        Some("cmd-w"),
        Some("⌘W"),
        Some(APP_CONTEXT)
    ),
    spec!(
        ReopenSession,
        "reopen-session",
        Some("cmd-shift-t"),
        Some("⇧⌘T"),
        Some(APP_CONTEXT)
    ),
    spec!(
        OpenLauncher,
        "open-launcher",
        Some("cmd-n"),
        Some("⌘N"),
        Some(APP_CONTEXT)
    ),
    spec!(
        NewDefaultSession,
        "new-default",
        Some("cmd-t"),
        Some("⌘T"),
        Some(APP_CONTEXT)
    ),
    spec!(
        NewTerminal,
        "new-terminal",
        Some("cmd-alt-t"),
        Some("⌥⌘T"),
        Some(APP_CONTEXT),
        "New Terminal",
        "terminal",
        "shell console zsh bash tty"
    ),
    spec!(
        NewCodexSession,
        "new-codex",
        Some("cmd-shift-n"),
        Some("⇧⌘N"),
        Some(APP_CONTEXT)
    ),
    spec!(
        ToggleCommandPalette,
        "command-palette",
        Some("cmd-k"),
        Some("⌘K"),
        Some(APP_CONTEXT)
    ),
    spec!(
        ToggleQuickOpen,
        "quick-open",
        Some("cmd-p"),
        Some("⌘P"),
        Some(APP_CONTEXT),
        "Open Folder…",
        "magnifyingglass",
        "folder project directory jump goto find"
    ),
    spec!(
        ToggleHistory,
        "history",
        Some("cmd-shift-h"),
        Some("⇧⌘H"),
        Some(APP_CONTEXT)
    ),
    spec!(
        ToggleOverview,
        "session-overview",
        Some("cmd-shift-o"),
        Some("⇧⌘O"),
        Some(APP_CONTEXT),
        "Session Overview",
        "square.grid.2x2",
        "board grid switcher all sessions"
    ),
    spec!(
        OpenWorktrees,
        "worktrees",
        Some("cmd-alt-w"),
        Some("⌥⌘W"),
        Some(APP_CONTEXT),
        "Worktrees Overview",
        "square.stack.3d.up",
        "git branch checkout"
    ),
    spec!(
        OpenSettings,
        "settings",
        Some("cmd-,"),
        Some("⌘,"),
        Some(APP_CONTEXT),
        "Settings…",
        "gearshape",
        "preferences config options"
    ),
    spec!(
        ToggleSidebar,
        "toggle-sidebar",
        Some("cmd-b"),
        Some("⌘B"),
        Some(APP_CONTEXT),
        "Toggle Sidebar",
        "sidebar.left",
        "hide show panel"
    ),
    spec!(
        FocusSidebar,
        "focus-sidebar",
        Some("cmd-shift-b"),
        Some("⇧⌘B"),
        Some(APP_CONTEXT),
        "Focus Sidebar",
        "sidebar.left",
        "keyboard sessions navigation focus"
    ),
    spec!(
        ToggleInspector,
        "toggle-inspector",
        Some("cmd-shift-d"),
        Some("⇧⌘D"),
        Some(APP_CONTEXT)
    ),
    spec!(
        ToggleAuxiliaryTerminal,
        "toggle-auxiliary-terminal",
        Some("cmd-j"),
        Some("⌘J"),
        Some(APP_CONTEXT)
    ),
    spec!(
        QuoteSelection,
        "quote-selection",
        Some("cmd-shift-c"),
        Some("⇧⌘C"),
        Some(APP_CONTEXT),
        "Quote Selection",
        "text.quote",
        "append cite composer draft context"
    ),
    spec!(
        QuoteSelectionToSession,
        "quote-selection-to-session",
        Some("cmd-alt-shift-c"),
        Some("⌥⇧⌘C"),
        Some(APP_CONTEXT),
        "Quote Selection to Session…",
        "sidebar.left",
        "append cite composer draft target another agent"
    ),
    spec!(
        ArchiveSelectedSession,
        "archive-selected-session",
        Some("cmd-shift-w"),
        Some("⇧⌘W"),
        Some(APP_CONTEXT)
    ),
    spec!(
        RenameSelectedSession,
        "rename-selected-session",
        Some("cmd-r"),
        Some("⌘R"),
        Some(APP_CONTEXT)
    ),
    spec!(
        DelegateSelectedSession,
        "delegate-selected-session",
        Some("cmd-ctrl-d"),
        Some("⌃⌘D"),
        Some(APP_CONTEXT),
        "Delegate Selected Session",
        "arrowshape.turn.up.right",
        "handoff delegate context agent session"
    ),
    spec!(
        SelectNextAttentionSession,
        "select-next-attention-session",
        Some("cmd-shift-j"),
        Some("⇧⌘J"),
        Some(APP_CONTEXT)
    ),
    spec!(
        ToggleNotifications,
        "toggle-notifications",
        Some("cmd-shift-i"),
        Some("⇧⌘I"),
        Some(APP_CONTEXT),
        "Notifications",
        "bell",
        "inbox unread alerts attention"
    ),
    spec!(
        CheckForUpdates,
        "check-for-updates",
        None,
        None,
        Some(APP_CONTEXT),
        "Check for Updates…",
        "arrow.triangle.2.circlepath",
        "upgrade version release"
    ),
    spec_with_alternates!(
        SelectPreviousSession,
        "select-previous-session",
        Some("cmd-alt-left"),
        ["cmd-alt-up", "cmd-["],
        Some("⌥⌘←"),
        Some(SESSION_NAVIGATION_CONTEXT)
    ),
    spec_with_alternates!(
        SelectNextSession,
        "select-next-session",
        Some("cmd-alt-right"),
        ["cmd-alt-down", "cmd-]"],
        Some("⌥⌘→"),
        Some(SESSION_NAVIGATION_CONTEXT)
    ),
    spec!(
        MoveSelectedSessionUp,
        "move-selected-session-up",
        Some("cmd-ctrl-up"),
        Some("⌃⌘↑"),
        Some(SESSION_NAVIGATION_CONTEXT)
    ),
    spec!(
        MoveSelectedSessionDown,
        "move-selected-session-down",
        Some("cmd-ctrl-down"),
        Some("⌃⌘↓"),
        Some(SESSION_NAVIGATION_CONTEXT)
    ),
    spec!(
        SelectSession1,
        "select-session-1",
        Some("cmd-1"),
        Some("⌘1"),
        Some(APP_CONTEXT)
    ),
    spec!(
        SelectSession2,
        "select-session-2",
        Some("cmd-2"),
        Some("⌘2"),
        Some(APP_CONTEXT)
    ),
    spec!(
        SelectSession3,
        "select-session-3",
        Some("cmd-3"),
        Some("⌘3"),
        Some(APP_CONTEXT)
    ),
    spec!(
        SelectSession4,
        "select-session-4",
        Some("cmd-4"),
        Some("⌘4"),
        Some(APP_CONTEXT)
    ),
    spec!(
        SelectSession5,
        "select-session-5",
        Some("cmd-5"),
        Some("⌘5"),
        Some(APP_CONTEXT)
    ),
    spec!(
        SelectSession6,
        "select-session-6",
        Some("cmd-6"),
        Some("⌘6"),
        Some(APP_CONTEXT)
    ),
    spec!(
        SelectSession7,
        "select-session-7",
        Some("cmd-7"),
        Some("⌘7"),
        Some(APP_CONTEXT)
    ),
    spec!(
        SelectSession8,
        "select-session-8",
        Some("cmd-8"),
        Some("⌘8"),
        Some(APP_CONTEXT)
    ),
    spec!(
        SelectLastSession,
        "select-last-session",
        Some("cmd-9"),
        Some("⌘9"),
        Some(APP_CONTEXT)
    ),
    spec!(
        OpenFind,
        "terminal-find",
        Some("cmd-f"),
        Some("⌘F"),
        Some(TERMINAL_CONTEXT)
    ),
    spec!(
        FindNext,
        "terminal-find-next",
        Some("cmd-g"),
        Some("⌘G"),
        Some(TERMINAL_CONTEXT)
    ),
    spec!(
        FindPrevious,
        "terminal-find-previous",
        Some("cmd-shift-g"),
        Some("⇧⌘G"),
        Some(TERMINAL_CONTEXT)
    ),
    spec_with_alternates!(
        ZoomIn,
        "terminal-zoom-in",
        Some("cmd-="),
        ["cmd-+"],
        Some("⌘+"),
        Some(TERMINAL_CONTEXT)
    ),
    spec!(
        ZoomOut,
        "terminal-zoom-out",
        Some("cmd--"),
        Some("⌘−"),
        Some(TERMINAL_CONTEXT)
    ),
    spec!(
        ResetZoom,
        "terminal-reset-zoom",
        Some("cmd-0"),
        Some("⌘0"),
        Some(TERMINAL_CONTEXT)
    ),
    spec!(
        Paste,
        "terminal-paste",
        Some("cmd-v"),
        Some("⌘V"),
        Some(TERMINAL_CONTEXT)
    ),
    spec!(
        CopySelection,
        "terminal-copy",
        Some("cmd-c"),
        Some("⌘C"),
        Some(TERMINAL_CONTEXT)
    ),
];

pub fn command(id: CommandId) -> &'static CommandSpec {
    COMMANDS
        .iter()
        .find(|command| command.id == id)
        .expect("every CommandId must have a registry entry")
}

/// Returns whether a platform key event is one of this command's registered
/// bindings. Modal surfaces use this to decide which application commands may
/// continue through capture without duplicating raw shortcut strings.
pub fn matches_keystroke(id: CommandId, keystroke: &Keystroke) -> bool {
    let overrides = active_shortcut_overrides()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    command(id)
        .effective_keystrokes(&overrides)
        .into_iter()
        .filter_map(|binding| Keystroke::parse(&binding).ok())
        .any(|binding| {
            binding.modifiers == keystroke.modifiers
                && (binding.key == keystroke.key
                    || keystroke.key_char.as_ref() == Some(&binding.key))
        })
}

/// Installs the shipped keymap plus persisted user overrides at app startup.
pub fn bind_keys(cx: &mut App, overrides: &ShortcutOverrides) {
    set_active_shortcut_overrides(overrides);
    bind_active_keys(cx, overrides);
}

/// Replaces the live keymap after an edit. Diri is the only owner of GPUI key
/// bindings in this application, so rebuilding avoids leaving stale custom
/// bindings active after a user changes or clears one.
pub fn rebind_keys(cx: &mut App, overrides: &ShortcutOverrides) {
    set_active_shortcut_overrides(overrides);
    cx.clear_key_bindings();
    bind_active_keys(cx, overrides);
}

fn bind_active_keys(cx: &mut App, overrides: &ShortcutOverrides) {
    cx.bind_keys(
        COMMANDS
            .iter()
            .flat_map(|command| command.key_bindings(overrides)),
    );
}

fn active_shortcut_overrides() -> &'static RwLock<ShortcutOverrides> {
    ACTIVE_SHORTCUT_OVERRIDES.get_or_init(|| RwLock::new(ShortcutOverrides::new()))
}

fn set_active_shortcut_overrides(overrides: &ShortcutOverrides) {
    *active_shortcut_overrides()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = overrides.clone();
}

impl CommandSpec {
    fn key_bindings(&self, overrides: &ShortcutOverrides) -> Vec<KeyBinding> {
        self.effective_keystrokes(overrides)
            .into_iter()
            .map(|key| self.key_binding(&key))
            .collect()
    }

    pub fn shortcut_label(&self) -> Option<String> {
        let overrides = active_shortcut_overrides()
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.shortcut_label_for(&overrides)
    }

    pub fn shortcut_label_for(&self, overrides: &ShortcutOverrides) -> Option<String> {
        match overrides.get(self.stable_id) {
            Some(None) => None,
            Some(Some(binding)) if Keystroke::parse(binding).is_ok() => Keystroke::parse(binding)
                .ok()
                .map(|key| shortcut_label_for_keystroke(&key)),
            _ => self
                .keystroke
                .and_then(|binding| platform_keystroke(self.id, binding))
                .and_then(|_| self.shortcut.map(platform_shortcut_label)),
        }
    }

    pub fn is_overridden(&self, overrides: &ShortcutOverrides) -> bool {
        overrides.contains_key(self.stable_id)
    }

    pub fn effective_keystrokes(&self, overrides: &ShortcutOverrides) -> Vec<String> {
        match overrides.get(self.stable_id) {
            Some(None) => Vec::new(),
            Some(Some(binding)) if Keystroke::parse(binding).is_ok() => vec![binding.clone()],
            _ => self
                .keystroke
                .into_iter()
                .chain(self.alternate_keystrokes.iter().copied())
                .filter_map(|key| platform_keystroke(self.id, key))
                .collect(),
        }
    }

    fn key_binding(&self, key: &str) -> KeyBinding {
        let context = self.context;
        match self.id {
            CommandId::Quit => KeyBinding::new(key, Quit, context),
            CommandId::HideApp => KeyBinding::new(key, HideApp, context),
            CommandId::CloseWindow => KeyBinding::new(key, CloseWindow, context),
            CommandId::CloseSession => KeyBinding::new(key, CloseSession, context),
            CommandId::ReopenSession => KeyBinding::new(key, ReopenSession, context),
            CommandId::OpenLauncher => KeyBinding::new(key, OpenLauncher, context),
            CommandId::NewDefaultSession => KeyBinding::new(key, NewDefaultSession, context),
            CommandId::NewTerminal => KeyBinding::new(key, NewTerminal, context),
            CommandId::NewCodexSession => KeyBinding::new(key, NewCodexSession, context),
            CommandId::ToggleCommandPalette => KeyBinding::new(key, ToggleCommandPalette, context),
            CommandId::ToggleQuickOpen => KeyBinding::new(key, ToggleQuickOpen, context),
            CommandId::ToggleHistory => KeyBinding::new(key, ToggleHistory, context),
            CommandId::ToggleNotifications => KeyBinding::new(key, ToggleNotifications, context),
            CommandId::ToggleOverview => KeyBinding::new(key, ToggleOverview, context),
            CommandId::OpenWorktrees => KeyBinding::new(key, OpenWorktrees, context),
            CommandId::OpenSettings => KeyBinding::new(key, OpenSettings, context),
            CommandId::ToggleSidebar => KeyBinding::new(key, ToggleSidebar, context),
            CommandId::FocusSidebar => KeyBinding::new(key, FocusSidebar, context),
            CommandId::ToggleInspector => KeyBinding::new(key, ToggleInspector, context),
            CommandId::ToggleAuxiliaryTerminal => {
                KeyBinding::new(key, ToggleAuxiliaryTerminal, context)
            }
            CommandId::QuoteSelection => KeyBinding::new(key, QuoteSelection, context),
            CommandId::QuoteSelectionToSession => {
                KeyBinding::new(key, QuoteSelectionToSession, context)
            }
            CommandId::ArchiveSelectedSession => {
                KeyBinding::new(key, ArchiveSelectedSession, context)
            }
            CommandId::RenameSelectedSession => {
                KeyBinding::new(key, RenameSelectedSession, context)
            }
            CommandId::DelegateSelectedSession => {
                KeyBinding::new(key, DelegateSelectedSession, context)
            }
            CommandId::SelectNextAttentionSession => {
                KeyBinding::new(key, SelectNextAttentionSession, context)
            }
            CommandId::CheckForUpdates => KeyBinding::new(key, CheckForUpdates, context),
            CommandId::SelectPreviousSession => {
                KeyBinding::new(key, SelectPreviousSession, context)
            }
            CommandId::SelectNextSession => KeyBinding::new(key, SelectNextSession, context),
            CommandId::MoveSelectedSessionUp => {
                KeyBinding::new(key, MoveSelectedSessionUp, context)
            }
            CommandId::MoveSelectedSessionDown => {
                KeyBinding::new(key, MoveSelectedSessionDown, context)
            }
            CommandId::SelectSession1 => KeyBinding::new(key, SelectSession1, context),
            CommandId::SelectSession2 => KeyBinding::new(key, SelectSession2, context),
            CommandId::SelectSession3 => KeyBinding::new(key, SelectSession3, context),
            CommandId::SelectSession4 => KeyBinding::new(key, SelectSession4, context),
            CommandId::SelectSession5 => KeyBinding::new(key, SelectSession5, context),
            CommandId::SelectSession6 => KeyBinding::new(key, SelectSession6, context),
            CommandId::SelectSession7 => KeyBinding::new(key, SelectSession7, context),
            CommandId::SelectSession8 => KeyBinding::new(key, SelectSession8, context),
            CommandId::SelectLastSession => KeyBinding::new(key, SelectLastSession, context),
            CommandId::OpenFind => KeyBinding::new(key, OpenFind, context),
            CommandId::FindNext => KeyBinding::new(key, FindNext, context),
            CommandId::FindPrevious => KeyBinding::new(key, FindPrevious, context),
            CommandId::ZoomIn => KeyBinding::new(key, ZoomIn, context),
            CommandId::ZoomOut => KeyBinding::new(key, ZoomOut, context),
            CommandId::ResetZoom => KeyBinding::new(key, ResetZoom, context),
            CommandId::Paste => KeyBinding::new(key, Paste, context),
            CommandId::CopySelection => KeyBinding::new(key, CopySelection, context),
        }
    }
}

#[cfg(target_os = "macos")]
fn platform_keystroke(_id: CommandId, key: &str) -> Option<String> {
    Some(key.to_owned())
}

#[cfg(not(target_os = "macos"))]
fn platform_keystroke(id: CommandId, key: &str) -> Option<String> {
    if id == CommandId::HideApp {
        return None;
    }
    let key = key
        .replace("cmd-ctrl-", "ctrl-shift-")
        .replace("cmd-", "ctrl-");
    Some(key)
}

#[cfg(target_os = "macos")]
fn platform_shortcut_label(label: &str) -> String {
    label.to_owned()
}

#[cfg(not(target_os = "macos"))]
fn platform_shortcut_label(label: &str) -> String {
    let mut modifiers = vec!["Ctrl"];
    if label.contains('⌥') {
        modifiers.push("Alt");
    }
    if label.contains('⇧') || label.contains('⌃') {
        modifiers.push("Shift");
    }
    let key = label.replace(['⌘', '⌥', '⇧', '⌃'], "").replace('−', "-");
    modifiers.push(&key);
    modifiers.join("+")
}

#[cfg(target_os = "macos")]
fn shortcut_label_for_keystroke(keystroke: &Keystroke) -> String {
    let mut label = String::new();
    if keystroke.modifiers.function {
        label.push_str("fn");
    }
    if keystroke.modifiers.control {
        label.push('⌃');
    }
    if keystroke.modifiers.alt {
        label.push('⌥');
    }
    if keystroke.modifiers.shift {
        label.push('⇧');
    }
    if keystroke.modifiers.platform {
        label.push('⌘');
    }
    label.push_str(match keystroke.key.as_str() {
        "backspace" => "⌫",
        "delete" => "⌦",
        "enter" => "↩",
        "tab" => "⇥",
        "escape" => "⎋",
        "space" => "Space",
        "up" => "↑",
        "down" => "↓",
        "left" => "←",
        "right" => "→",
        key => return format!("{label}{}", key.to_ascii_uppercase()),
    });
    label
}

#[cfg(not(target_os = "macos"))]
fn shortcut_label_for_keystroke(keystroke: &Keystroke) -> String {
    let mut parts = Vec::new();
    if keystroke.modifiers.control {
        parts.push("Ctrl".to_owned());
    }
    if keystroke.modifiers.alt {
        parts.push("Alt".to_owned());
    }
    if keystroke.modifiers.shift {
        parts.push("Shift".to_owned());
    }
    if keystroke.modifiers.platform {
        parts.push("Super".to_owned());
    }
    if keystroke.modifiers.function {
        parts.push("Fn".to_owned());
    }
    let key = match keystroke.key.as_str() {
        "escape" => "Esc".to_owned(),
        "space" => "Space".to_owned(),
        key => key.to_ascii_uppercase(),
    };
    parts.push(key);
    parts.join("+")
}

pub fn primary_shortcut_label(key: &str) -> String {
    platform_shortcut_label(&format!("⌘{key}"))
}

pub fn primary_click_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Command-click"
    } else {
        "Ctrl-click"
    }
}

/// Finds another command that already owns `binding`. Contexts are
/// deliberately treated as overlapping: application shortcuts remain active
/// while terminal and navigation contexts are focused, so allowing a duplicate
/// would make the result depend on focus in ways the settings row cannot show.
pub fn shortcut_conflict(
    id: CommandId,
    binding: &str,
    overrides: &ShortcutOverrides,
) -> Option<&'static CommandSpec> {
    let candidate = Keystroke::parse(binding).ok()?;
    COMMANDS.iter().find(|command| {
        command.id != id
            && command
                .effective_keystrokes(overrides)
                .iter()
                .any(|current| {
                    Keystroke::parse(current).is_ok_and(|current| {
                        current.modifiers == candidate.modifiers && current.key == candidate.key
                    })
                })
    })
}

impl CommandId {
    pub const fn shortcut_metadata(self) -> ShortcutMetadata {
        use ShortcutCategory::{Application, Navigation, Sessions, Terminal, Workspace};
        match self {
            Self::CloseSession => ShortcutMetadata {
                title: "Close session",
                description: "Close the selected session",
                category: Sessions,
            },
            Self::ReopenSession => ShortcutMetadata {
                title: "Reopen closed session",
                description: "Restore the most recently closed session",
                category: Sessions,
            },
            Self::OpenLauncher => ShortcutMetadata {
                title: "New session",
                description: "Open the new session picker",
                category: Sessions,
            },
            Self::NewDefaultSession => ShortcutMetadata {
                title: "New default session",
                description: "Start a session with the default agent",
                category: Sessions,
            },
            Self::NewTerminal => ShortcutMetadata {
                title: "New terminal",
                description: "Start a standalone shell session",
                category: Sessions,
            },
            Self::NewCodexSession => ShortcutMetadata {
                title: "New Codex session",
                description: "Start a new Codex session",
                category: Sessions,
            },
            Self::ArchiveSelectedSession => ShortcutMetadata {
                title: "Archive session",
                description: "Archive the selected session",
                category: Sessions,
            },
            Self::RenameSelectedSession => ShortcutMetadata {
                title: "Rename session",
                description: "Rename the selected session",
                category: Sessions,
            },
            Self::DelegateSelectedSession => ShortcutMetadata {
                title: "Delegate session",
                description: "Hand off work from the selected session",
                category: Sessions,
            },
            Self::ToggleNotifications => ShortcutMetadata {
                title: "Notifications",
                description: "Open the unread notification inbox",
                category: Navigation,
            },
            Self::SelectNextAttentionSession => ShortcutMetadata {
                title: "Next session needing attention",
                description: "Jump to the next session waiting for you",
                category: Sessions,
            },
            Self::QuoteSelection => ShortcutMetadata {
                title: "Quote selection",
                description: "Add the terminal selection to this session's composer",
                category: Sessions,
            },
            Self::QuoteSelectionToSession => ShortcutMetadata {
                title: "Quote selection to session",
                description: "Send the terminal selection to another session",
                category: Sessions,
            },
            Self::ToggleHistory => ShortcutMetadata {
                title: "Conversation history",
                description: "Open or close conversation history",
                category: Navigation,
            },
            Self::ToggleOverview => ShortcutMetadata {
                title: "Session overview",
                description: "Open or close the session overview",
                category: Navigation,
            },
            Self::OpenWorktrees => ShortcutMetadata {
                title: "Worktrees overview",
                description: "Open the Git worktrees overview",
                category: Navigation,
            },
            Self::ToggleCommandPalette => ShortcutMetadata {
                title: "Command palette",
                description: "Search all available commands",
                category: Navigation,
            },
            Self::ToggleQuickOpen => ShortcutMetadata {
                title: "Quick Open",
                description: "Find and open a project folder",
                category: Navigation,
            },
            Self::SelectPreviousSession => ShortcutMetadata {
                title: "Previous session",
                description: "Select the previous session in the sidebar",
                category: Navigation,
            },
            Self::SelectNextSession => ShortcutMetadata {
                title: "Next session",
                description: "Select the next session in the sidebar",
                category: Navigation,
            },
            Self::MoveSelectedSessionUp => ShortcutMetadata {
                title: "Move session up",
                description: "Move the selected session up in the sidebar",
                category: Navigation,
            },
            Self::MoveSelectedSessionDown => ShortcutMetadata {
                title: "Move session down",
                description: "Move the selected session down in the sidebar",
                category: Navigation,
            },
            Self::SelectSession1 => {
                session_slot_metadata("Select session 1", "Select the first session")
            }
            Self::SelectSession2 => {
                session_slot_metadata("Select session 2", "Select the second session")
            }
            Self::SelectSession3 => {
                session_slot_metadata("Select session 3", "Select the third session")
            }
            Self::SelectSession4 => {
                session_slot_metadata("Select session 4", "Select the fourth session")
            }
            Self::SelectSession5 => {
                session_slot_metadata("Select session 5", "Select the fifth session")
            }
            Self::SelectSession6 => {
                session_slot_metadata("Select session 6", "Select the sixth session")
            }
            Self::SelectSession7 => {
                session_slot_metadata("Select session 7", "Select the seventh session")
            }
            Self::SelectSession8 => {
                session_slot_metadata("Select session 8", "Select the eighth session")
            }
            Self::SelectLastSession => {
                session_slot_metadata("Select last session", "Select the last session")
            }
            Self::ToggleSidebar => ShortcutMetadata {
                title: "Toggle sidebar",
                description: "Show or hide the sessions sidebar",
                category: Workspace,
            },
            Self::FocusSidebar => ShortcutMetadata {
                title: "Focus sidebar",
                description: "Move keyboard focus to the sessions sidebar",
                category: Workspace,
            },
            Self::ToggleInspector => ShortcutMetadata {
                title: "Toggle inspector",
                description: "Show or hide the session inspector",
                category: Workspace,
            },
            Self::ToggleAuxiliaryTerminal => ShortcutMetadata {
                title: "Toggle auxiliary terminal",
                description: "Show or hide the lower terminal pane",
                category: Workspace,
            },
            Self::OpenFind => ShortcutMetadata {
                title: "Find in terminal",
                description: "Search the active terminal output",
                category: Terminal,
            },
            Self::FindNext => ShortcutMetadata {
                title: "Find next",
                description: "Move to the next terminal search result",
                category: Terminal,
            },
            Self::FindPrevious => ShortcutMetadata {
                title: "Find previous",
                description: "Move to the previous terminal search result",
                category: Terminal,
            },
            Self::ZoomIn => ShortcutMetadata {
                title: "Increase text size",
                description: "Make terminal text larger",
                category: Terminal,
            },
            Self::ZoomOut => ShortcutMetadata {
                title: "Decrease text size",
                description: "Make terminal text smaller",
                category: Terminal,
            },
            Self::ResetZoom => ShortcutMetadata {
                title: "Reset text size",
                description: "Restore the default terminal text size",
                category: Terminal,
            },
            Self::Paste => ShortcutMetadata {
                title: "Paste",
                description: "Paste clipboard contents into the terminal",
                category: Terminal,
            },
            Self::CopySelection => ShortcutMetadata {
                title: "Copy selection",
                description: "Copy the terminal selection",
                category: Terminal,
            },
            Self::OpenSettings => ShortcutMetadata {
                title: "Open settings",
                description: "Open or close Diri settings",
                category: Application,
            },
            Self::CheckForUpdates => ShortcutMetadata {
                title: "Check for updates",
                description: "Look for a newer version of Diri",
                category: Application,
            },
            Self::CloseWindow => ShortcutMetadata {
                title: "Close window",
                description: "Close the current Diri window",
                category: Application,
            },
            Self::HideApp => ShortcutMetadata {
                title: "Hide Diri",
                description: "Hide all Diri windows",
                category: Application,
            },
            Self::Quit => ShortcutMetadata {
                title: "Quit Diri",
                description: "Close Diri and leave no windows open",
                category: Application,
            },
        }
    }
}

const fn session_slot_metadata(title: &'static str, description: &'static str) -> ShortcutMetadata {
    ShortcutMetadata {
        title,
        description,
        category: ShortcutCategory::Navigation,
    }
}

impl CommandId {
    pub fn action(self) -> Box<dyn Action> {
        match self {
            Self::Quit => Box::new(Quit),
            Self::HideApp => Box::new(HideApp),
            Self::CloseWindow => Box::new(CloseWindow),
            Self::CloseSession => Box::new(CloseSession),
            Self::ReopenSession => Box::new(ReopenSession),
            Self::OpenLauncher => Box::new(OpenLauncher),
            Self::NewDefaultSession => Box::new(NewDefaultSession),
            Self::NewTerminal => Box::new(NewTerminal),
            Self::NewCodexSession => Box::new(NewCodexSession),
            Self::ToggleCommandPalette => Box::new(ToggleCommandPalette),
            Self::ToggleQuickOpen => Box::new(ToggleQuickOpen),
            Self::ToggleHistory => Box::new(ToggleHistory),
            Self::ToggleNotifications => Box::new(ToggleNotifications),
            Self::ToggleOverview => Box::new(ToggleOverview),
            Self::OpenWorktrees => Box::new(OpenWorktrees),
            Self::OpenSettings => Box::new(OpenSettings),
            Self::ToggleSidebar => Box::new(ToggleSidebar),
            Self::FocusSidebar => Box::new(FocusSidebar),
            Self::ToggleInspector => Box::new(ToggleInspector),
            Self::ToggleAuxiliaryTerminal => Box::new(ToggleAuxiliaryTerminal),
            Self::QuoteSelection => Box::new(QuoteSelection),
            Self::QuoteSelectionToSession => Box::new(QuoteSelectionToSession),
            Self::ArchiveSelectedSession => Box::new(ArchiveSelectedSession),
            Self::RenameSelectedSession => Box::new(RenameSelectedSession),
            Self::DelegateSelectedSession => Box::new(DelegateSelectedSession),
            Self::SelectNextAttentionSession => Box::new(SelectNextAttentionSession),
            Self::CheckForUpdates => Box::new(CheckForUpdates),
            Self::SelectPreviousSession => Box::new(SelectPreviousSession),
            Self::SelectNextSession => Box::new(SelectNextSession),
            Self::MoveSelectedSessionUp => Box::new(MoveSelectedSessionUp),
            Self::MoveSelectedSessionDown => Box::new(MoveSelectedSessionDown),
            Self::SelectSession1 => Box::new(SelectSession1),
            Self::SelectSession2 => Box::new(SelectSession2),
            Self::SelectSession3 => Box::new(SelectSession3),
            Self::SelectSession4 => Box::new(SelectSession4),
            Self::SelectSession5 => Box::new(SelectSession5),
            Self::SelectSession6 => Box::new(SelectSession6),
            Self::SelectSession7 => Box::new(SelectSession7),
            Self::SelectSession8 => Box::new(SelectSession8),
            Self::SelectLastSession => Box::new(SelectLastSession),
            Self::OpenFind => Box::new(OpenFind),
            Self::FindNext => Box::new(FindNext),
            Self::FindPrevious => Box::new(FindPrevious),
            Self::ZoomIn => Box::new(ZoomIn),
            Self::ZoomOut => Box::new(ZoomOut),
            Self::ResetZoom => Box::new(ResetZoom),
            Self::Paste => Box::new(Paste),
            Self::CopySelection => Box::new(CopySelection),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn command_ids_and_stable_ids_are_unique() {
        let mut ids = HashSet::new();
        let mut stable_ids = HashSet::new();
        let mut bindings = HashSet::new();
        for command in COMMANDS {
            assert!(
                ids.insert(command.id),
                "duplicate command: {:?}",
                command.id
            );
            assert!(
                stable_ids.insert(command.stable_id),
                "duplicate stable id: {}",
                command.stable_id
            );
            for keystroke in command
                .keystroke
                .into_iter()
                .chain(command.alternate_keystrokes.iter().copied())
            {
                assert!(
                    bindings.insert((command.context, keystroke)),
                    "duplicate binding in {:?}: {}",
                    command.context,
                    keystroke
                );
            }
        }
    }

    #[test]
    fn shortcut_labels_come_from_the_bound_command() {
        let terminal = command(CommandId::NewTerminal);
        #[cfg(target_os = "macos")]
        assert_eq!(terminal.shortcut_label().as_deref(), Some("⌥⌘T"));
        #[cfg(not(target_os = "macos"))]
        assert_eq!(terminal.shortcut_label().as_deref(), Some("Ctrl+Alt+T"));

        let settings = command(CommandId::OpenSettings);
        #[cfg(target_os = "macos")]
        assert_eq!(settings.shortcut_label().as_deref(), Some("⌘,"));
        #[cfg(not(target_os = "macos"))]
        assert_eq!(settings.shortcut_label().as_deref(), Some("Ctrl+,"));

        let quote = command(CommandId::QuoteSelection);
        assert_eq!(quote.keystroke, Some("cmd-shift-c"));
        #[cfg(target_os = "macos")]
        assert_eq!(quote.shortcut_label().as_deref(), Some("⇧⌘C"));
        #[cfg(not(target_os = "macos"))]
        assert_eq!(quote.shortcut_label().as_deref(), Some("Ctrl+Shift+C"));
        assert_eq!(
            quote.palette.map(|metadata| metadata.title),
            Some("Quote Selection")
        );

        let picker = command(CommandId::QuoteSelectionToSession);
        assert_eq!(picker.keystroke, Some("cmd-alt-shift-c"));
        assert!(picker.palette.is_some());
    }

    #[test]
    fn session_navigation_is_context_gated() {
        for id in [
            CommandId::SelectPreviousSession,
            CommandId::SelectNextSession,
            CommandId::MoveSelectedSessionUp,
            CommandId::MoveSelectedSessionDown,
        ] {
            assert_eq!(command(id).context, Some(SESSION_NAVIGATION_CONTEXT));
        }
        assert_eq!(
            command(CommandId::SelectPreviousSession).alternate_keystrokes,
            ["cmd-alt-up", "cmd-["]
        );
        assert_eq!(
            command(CommandId::SelectNextSession).alternate_keystrokes,
            ["cmd-alt-down", "cmd-]"]
        );
    }

    #[test]
    fn sidebar_focus_has_a_global_explicit_shortcut() {
        let focus = command(CommandId::FocusSidebar);
        assert_eq!(focus.keystroke, Some("cmd-shift-b"));
        assert_eq!(focus.context, Some(APP_CONTEXT));
        assert_eq!(focus.shortcut, Some("⇧⌘B"));
    }

    #[test]
    fn every_registered_keystroke_parses() {
        let binding_count: usize = COMMANDS
            .iter()
            .map(|command| command.key_bindings(&ShortcutOverrides::new()).len())
            .sum();
        assert!(binding_count > COMMANDS.len());
    }

    #[test]
    fn overrides_replace_all_shipped_bindings_and_can_unassign_a_command() {
        let previous = command(CommandId::SelectPreviousSession);
        let mut overrides = ShortcutOverrides::new();
        overrides.insert(
            previous.stable_id.to_owned(),
            Some("cmd-shift-y".to_owned()),
        );
        assert_eq!(previous.effective_keystrokes(&overrides), ["cmd-shift-y"]);
        #[cfg(target_os = "macos")]
        assert_eq!(
            previous.shortcut_label_for(&overrides).as_deref(),
            Some("⇧⌘Y")
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            previous.shortcut_label_for(&overrides).as_deref(),
            Some("Shift+Super+Y")
        );

        overrides.insert(previous.stable_id.to_owned(), None);
        assert!(previous.effective_keystrokes(&overrides).is_empty());
        assert_eq!(previous.shortcut_label_for(&overrides), None);
    }

    #[test]
    fn conflicts_include_alternate_bindings() {
        let overrides = ShortcutOverrides::new();
        let conflict = shortcut_conflict(CommandId::OpenLauncher, "cmd-[", &overrides)
            .expect("navigation alternate should be reserved");
        assert_eq!(conflict.id, CommandId::SelectPreviousSession);
    }

    #[test]
    fn every_command_has_shortcut_page_copy() {
        for command in COMMANDS {
            let metadata = command.id.shortcut_metadata();
            assert!(!metadata.title.is_empty());
            assert!(!metadata.description.is_empty());
        }
    }

    #[test]
    fn modal_passthrough_matching_uses_registry_bindings() {
        #[cfg(target_os = "macos")]
        let (launcher, previous, other) = ("cmd-n", "cmd-[", "cmd-t");
        #[cfg(not(target_os = "macos"))]
        let (launcher, previous, other) = ("ctrl-n", "ctrl-[", "ctrl-t");
        assert!(matches_keystroke(
            CommandId::OpenLauncher,
            &Keystroke::parse(launcher).unwrap()
        ));
        assert!(matches_keystroke(
            CommandId::SelectPreviousSession,
            &Keystroke::parse(previous).unwrap()
        ));
        assert!(!matches_keystroke(
            CommandId::OpenLauncher,
            &Keystroke::parse(other).unwrap()
        ));
    }
}
