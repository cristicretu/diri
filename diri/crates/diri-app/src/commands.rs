//! The application's command registry.
//!
//! A command's typed GPUI action, default key binding, context, and palette
//! presentation live together here. Views execute actions; they do not decode
//! keystrokes or maintain their own copies of shortcut labels.

use std::borrow::Cow;

use gpui::{Action, App, KeyBinding, Keystroke, actions};

pub const APP_CONTEXT: &str = "Diri";
pub const SESSION_NAVIGATION_CONTEXT: &str = "DiriSessionNavigation";
pub const TERMINAL_CONTEXT: &str = "DiriTerminal";
pub const NAVIGATION_CONTEXT: &str = "DiriNavigation";
pub const UTILITY_CONTEXT: &str = "DiriUtility";

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
        ToggleOverview,
        OpenWorktrees,
        OpenSettings,
        ToggleSidebar,
        ToggleInspector,
        ToggleAuxiliaryTerminal,
        ArchiveSelectedSession,
        RenameSelectedSession,
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
    ToggleOverview,
    OpenWorktrees,
    OpenSettings,
    ToggleSidebar,
    ToggleInspector,
    ToggleAuxiliaryTerminal,
    ArchiveSelectedSession,
    RenameSelectedSession,
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
        "Quick Open…",
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
        SelectNextAttentionSession,
        "select-next-attention-session",
        Some("cmd-shift-j"),
        Some("⇧⌘J"),
        Some(APP_CONTEXT)
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
    command(id)
        .keystroke
        .into_iter()
        .chain(command(id).alternate_keystrokes.iter().copied())
        .filter_map(|binding| platform_keystroke(id, binding))
        .filter_map(|binding| Keystroke::parse(&binding).ok())
        .any(|binding| {
            binding.modifiers == keystroke.modifiers
                && (binding.key == keystroke.key
                    || keystroke.key_char.as_ref() == Some(&binding.key))
        })
}

pub fn bind_default_keys(cx: &mut App) {
    cx.bind_keys(COMMANDS.iter().flat_map(CommandSpec::key_bindings));
}

impl CommandSpec {
    fn key_bindings(&self) -> Vec<KeyBinding> {
        self.keystroke
            .into_iter()
            .chain(self.alternate_keystrokes.iter().copied())
            .filter_map(|key| platform_keystroke(self.id, key))
            .map(|key| self.key_binding(&key))
            .collect()
    }

    pub fn shortcut_label(&self) -> Option<String> {
        self.shortcut.map(platform_shortcut_label)
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
            CommandId::ToggleOverview => KeyBinding::new(key, ToggleOverview, context),
            CommandId::OpenWorktrees => KeyBinding::new(key, OpenWorktrees, context),
            CommandId::OpenSettings => KeyBinding::new(key, OpenSettings, context),
            CommandId::ToggleSidebar => KeyBinding::new(key, ToggleSidebar, context),
            CommandId::ToggleInspector => KeyBinding::new(key, ToggleInspector, context),
            CommandId::ToggleAuxiliaryTerminal => {
                KeyBinding::new(key, ToggleAuxiliaryTerminal, context)
            }
            CommandId::ArchiveSelectedSession => {
                KeyBinding::new(key, ArchiveSelectedSession, context)
            }
            CommandId::RenameSelectedSession => {
                KeyBinding::new(key, RenameSelectedSession, context)
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
fn platform_keystroke(_id: CommandId, key: &'static str) -> Option<Cow<'static, str>> {
    Some(Cow::Borrowed(key))
}

#[cfg(not(target_os = "macos"))]
fn platform_keystroke(id: CommandId, key: &'static str) -> Option<Cow<'static, str>> {
    if id == CommandId::HideApp {
        return None;
    }
    let key = key
        .replace("cmd-ctrl-", "ctrl-shift-")
        .replace("cmd-", "ctrl-");
    Some(Cow::Owned(key))
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
            Self::ToggleOverview => Box::new(ToggleOverview),
            Self::OpenWorktrees => Box::new(OpenWorktrees),
            Self::OpenSettings => Box::new(OpenSettings),
            Self::ToggleSidebar => Box::new(ToggleSidebar),
            Self::ToggleInspector => Box::new(ToggleInspector),
            Self::ToggleAuxiliaryTerminal => Box::new(ToggleAuxiliaryTerminal),
            Self::ArchiveSelectedSession => Box::new(ArchiveSelectedSession),
            Self::RenameSelectedSession => Box::new(RenameSelectedSession),
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
    fn every_registered_keystroke_parses() {
        let binding_count: usize = COMMANDS
            .iter()
            .map(|command| command.key_bindings().len())
            .sum();
        assert!(binding_count > COMMANDS.len());
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
