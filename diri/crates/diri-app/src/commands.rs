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

actions!(
    diri_app,
    [
        Quit,
        HideApp,
        ShowApp,
        NewWindow,
        CloseWindow,
        ReportProblem
    ]
);

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
        ToggleTabPeek,
        ReviewLaunches,
        FocusPaneLeft,
        FocusPaneRight,
        FocusPaneUp,
        FocusPaneDown,
        SplitPaneRight,
        SplitPaneBelow,
        TogglePaneZoom,
        RemoveFocusedPane,
        PaneGrowWidth,
        PaneShrinkWidth,
        PaneGrowHeight,
        PaneShrinkHeight,
        SwapPaneLeft,
        SwapPaneRight,
        SwapPaneUp,
        SwapPaneDown,
        MovePaneLeft,
        MovePaneRight,
        MovePaneUp,
        MovePaneDown,
        OpenWorktrees,
        NewNote,
        ShowTodos,
        SearchNotes,
        NoteVersionHistory,
        NoteGraph,
        OpenSettings,
        // Palette destination: open Settings even when it is already visible.
        // OpenSettings retains the Cmd+, toggle behavior.
        ShowSettings,
        // Setup surfaces: open Settings on the Agents page for this Mac.
        ShowAgentSettings,
        ToggleSidebar,
        ToggleTabOrientation,
        HorizontalTabs,
        VerticalTabs,
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
        ShowWhatsNew,
        TogglePerfOverlay,
        ToggleRenderCounters,
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
        EnterCopyMode,
        FindSelection,
        InsertPath,
        ExportScrollback,
        PreviousPrompt,
        NextPrompt,
    ]
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommandId {
    Quit,
    HideApp,
    ShowApp,
    CloseWindow,
    NewWindow,
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
    ToggleTabPeek,
    ReviewLaunches,
    FocusPaneLeft,
    FocusPaneRight,
    FocusPaneUp,
    FocusPaneDown,
    SplitPaneRight,
    SplitPaneBelow,
    TogglePaneZoom,
    RemoveFocusedPane,
    PaneGrowWidth,
    PaneShrinkWidth,
    PaneGrowHeight,
    PaneShrinkHeight,
    SwapPaneLeft,
    SwapPaneRight,
    SwapPaneUp,
    SwapPaneDown,
    MovePaneLeft,
    MovePaneRight,
    MovePaneUp,
    MovePaneDown,
    OpenWorktrees,
    NewNote,
    ShowTodos,
    SearchNotes,
    NoteVersionHistory,
    NoteGraph,
    OpenSettings,
    ToggleSidebar,
    ToggleTabOrientation,
    HorizontalTabs,
    VerticalTabs,
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
    ShowWhatsNew,
    TogglePerfOverlay,
    ToggleRenderCounters,
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
    EnterCopyMode,
    FindSelection,
    InsertPath,
    ExportScrollback,
    PreviousPrompt,
    NextPrompt,
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

    pub fn label(self) -> &'static str {
        self.label_in(crate::i18n::t)
    }

    pub fn english_label(self) -> &'static str {
        self.label_in(crate::i18n::english)
    }

    fn label_in(self, t: fn(&'static str) -> &'static str) -> &'static str {
        t(match self {
            Self::Sessions => "command.category.sessions",
            Self::Navigation => "command.category.navigation",
            Self::Workspace => "command.category.workspace",
            Self::Terminal => "command.category.terminal",
            Self::Application => "command.category.application",
        })
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
    /// Catalog id of the palette title; `title()` shows it.
    pub title_id: &'static str,
    pub system_image: &'static str,
    pub keywords: &'static str,
}

impl PaletteMetadata {
    /// The palette title in the interface language.
    pub fn title(&self) -> &'static str {
        crate::i18n::t(self.title_id)
    }
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
                title_id: $title,
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
    #[cfg(target_os = "macos")]
    spec!(ShowApp, "show-app", None, None, None),
    spec!(
        NewWindow,
        "new-window",
        Some("cmd-shift-n"),
        Some("⇧⌘N"),
        None,
        "command.new_window.palette_title",
        "macwindow.badge.plus",
        "window open independent view"
    ),
    spec!(
        CloseWindow,
        "close-window",
        Some("cmd-shift-w"),
        Some("⇧⌘W"),
        None,
        "command.close_window.palette_title",
        "macwindow",
        "window close keep sessions running"
    ),
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
        "command.new_terminal.palette_title",
        "terminal",
        "shell console zsh bash tty"
    ),
    spec!(
        NewCodexSession,
        "new-codex",
        Some("cmd-alt-shift-n"),
        Some("⌥⇧⌘N"),
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
        "command.toggle_quick_open.palette_title",
        "folder",
        "folder project directory jump goto find"
    ),
    spec!(
        ToggleHistory,
        "history",
        Some("cmd-shift-h"),
        Some("⇧⌘H"),
        Some(APP_CONTEXT),
        "command.toggle_history.palette_title",
        "clock.fill",
        "history conversations past resume"
    ),
    spec!(
        ToggleOverview,
        "session-overview",
        Some("cmd-shift-o"),
        Some("⇧⌘O"),
        Some(APP_CONTEXT),
        "command.toggle_overview.palette_title",
        "square.grid.2x2",
        "board grid switcher all sessions pinch zoom"
    ),
    spec!(
        ToggleTabPeek,
        "tab-peek",
        Some("ctrl-shift-space"),
        Some("⌃⇧Space"),
        Some(APP_CONTEXT),
        "command.toggle_tab_peek.palette_title",
        "rectangle.stack",
        "preview strip tabs glance"
    ),
    spec!(
        ReviewLaunches,
        "review-launches",
        None,
        None,
        Some(APP_CONTEXT),
        "command.review_launches.palette_title",
        "tray",
        "workspace create pending retry placement receipts"
    ),
    spec!(
        FocusPaneLeft,
        "focus-pane-left",
        Some("ctrl-alt-left"),
        Some("⌃⌥←"),
        Some(APP_CONTEXT),
        "command.focus_pane_left.palette_title",
        "rectangle.split.2x1",
        "workspace focus pane left"
    ),
    spec!(
        FocusPaneRight,
        "focus-pane-right",
        Some("ctrl-alt-right"),
        Some("⌃⌥→"),
        Some(APP_CONTEXT),
        "command.focus_pane_right.palette_title",
        "rectangle.split.2x1",
        "workspace focus pane right"
    ),
    spec!(
        FocusPaneUp,
        "focus-pane-up",
        Some("ctrl-alt-up"),
        Some("⌃⌥↑"),
        Some(APP_CONTEXT),
        "command.focus_pane_up.palette_title",
        "rectangle.split.2x1",
        "workspace focus pane up"
    ),
    spec!(
        FocusPaneDown,
        "focus-pane-down",
        Some("ctrl-alt-down"),
        Some("⌃⌥↓"),
        Some(APP_CONTEXT),
        "command.focus_pane_down.palette_title",
        "rectangle.split.2x1",
        "workspace focus pane down"
    ),
    spec!(
        SplitPaneRight,
        "split-pane-right",
        Some("cmd-d"),
        Some("⌘D"),
        Some(APP_CONTEXT),
        "command.split_pane_right.palette_title",
        "rectangle.split.2x1",
        "workspace split pane right add layout"
    ),
    spec!(
        SplitPaneBelow,
        "split-pane-below",
        Some("cmd-alt-shift-d"),
        Some("⇧⌥⌘D"),
        Some(APP_CONTEXT),
        "command.split_pane_below.palette_title",
        "rectangle.split.2x1",
        "workspace split pane below add layout"
    ),
    spec!(
        TogglePaneZoom,
        "toggle-pane-zoom",
        Some("cmd-shift-enter"),
        Some("⇧⌘Return"),
        Some(APP_CONTEXT),
        "command.toggle_pane_zoom.palette_title",
        "rectangle.split.2x1",
        "workspace toggle pane zoom maximize restore"
    ),
    spec!(
        RemoveFocusedPane,
        "remove-focused-pane",
        None,
        None,
        Some(APP_CONTEXT),
        "command.remove_focused_pane.palette_title",
        "rectangle.split.2x1",
        "workspace remove focused pane close"
    ),
    spec!(
        PaneGrowWidth,
        "pane-grow-width",
        Some("cmd-alt-shift-right"),
        Some("⌥⇧⌘→"),
        Some(APP_CONTEXT),
        "command.pane_grow_width.palette_title",
        "rectangle.split.2x1",
        "workspace pane grow width resize divider"
    ),
    spec!(
        PaneShrinkWidth,
        "pane-shrink-width",
        Some("cmd-alt-shift-left"),
        Some("⌥⇧⌘←"),
        Some(APP_CONTEXT),
        "command.pane_shrink_width.palette_title",
        "rectangle.split.2x1",
        "workspace pane shrink width resize divider"
    ),
    spec!(
        PaneGrowHeight,
        "pane-grow-height",
        Some("cmd-alt-shift-down"),
        Some("⌥⇧⌘↓"),
        Some(APP_CONTEXT),
        "command.pane_grow_height.palette_title",
        "rectangle.split.2x1",
        "workspace pane grow height resize divider"
    ),
    spec!(
        PaneShrinkHeight,
        "pane-shrink-height",
        Some("cmd-alt-shift-up"),
        Some("⌥⇧⌘↑"),
        Some(APP_CONTEXT),
        "command.pane_shrink_height.palette_title",
        "rectangle.split.2x1",
        "workspace pane shrink height resize divider"
    ),
    spec!(
        SwapPaneLeft,
        "swap-pane-left",
        None,
        None,
        Some(APP_CONTEXT),
        "command.swap_pane_left.palette_title",
        "rectangle.split.2x1",
        "workspace swap pane left exchange"
    ),
    spec!(
        SwapPaneRight,
        "swap-pane-right",
        None,
        None,
        Some(APP_CONTEXT),
        "command.swap_pane_right.palette_title",
        "rectangle.split.2x1",
        "workspace swap pane right exchange"
    ),
    spec!(
        SwapPaneUp,
        "swap-pane-up",
        None,
        None,
        Some(APP_CONTEXT),
        "command.swap_pane_up.palette_title",
        "rectangle.split.2x1",
        "workspace swap pane up exchange"
    ),
    spec!(
        SwapPaneDown,
        "swap-pane-down",
        None,
        None,
        Some(APP_CONTEXT),
        "command.swap_pane_down.palette_title",
        "rectangle.split.2x1",
        "workspace swap pane down exchange"
    ),
    spec!(
        MovePaneLeft,
        "move-pane-left",
        None,
        None,
        Some(APP_CONTEXT),
        "command.move_pane_left.palette_title",
        "rectangle.split.2x1",
        "workspace move pane left dock"
    ),
    spec!(
        MovePaneRight,
        "move-pane-right",
        None,
        None,
        Some(APP_CONTEXT),
        "command.move_pane_right.palette_title",
        "rectangle.split.2x1",
        "workspace move pane right dock"
    ),
    spec!(
        MovePaneUp,
        "move-pane-up",
        None,
        None,
        Some(APP_CONTEXT),
        "command.move_pane_up.palette_title",
        "rectangle.split.2x1",
        "workspace move pane up dock"
    ),
    spec!(
        MovePaneDown,
        "move-pane-down",
        None,
        None,
        Some(APP_CONTEXT),
        "command.move_pane_down.palette_title",
        "rectangle.split.2x1",
        "workspace move pane down dock"
    ),
    spec!(
        NewNote,
        "new-note",
        Some("cmd-alt-n"),
        Some("⌥⌘N"),
        Some(APP_CONTEXT),
        "command.new_note.palette_title",
        "doc.text",
        "note notes markdown todo todos write memo prd plan doc"
    ),
    spec!(
        ShowTodos,
        "todos",
        Some("cmd-ctrl-t"),
        Some("⌃⌘T"),
        Some(APP_CONTEXT),
        "command.show_todos.palette_title",
        "checklist",
        "todos to-dos tasks checklist open review notes"
    ),
    spec!(
        SearchNotes,
        "search-notes",
        Some("cmd-shift-f"),
        Some("⇧⌘F"),
        Some(APP_CONTEXT),
        "command.search_notes.palette_title",
        "magnifyingglass",
        "notes find search open archived memo doc"
    ),
    spec!(
        NoteVersionHistory,
        "note-version-history",
        None,
        None,
        Some(APP_CONTEXT),
        "command.note_version_history.palette_title",
        "arrow.counterclockwise",
        "note history versions earlier restore undo changes"
    ),
    spec!(
        NoteGraph,
        "note-graph",
        None,
        None,
        Some(APP_CONTEXT),
        "command.note_graph.palette_title",
        "point.3.filled.connected.trianglepath.dotted",
        "note graph links backlinks connections map related notes"
    ),
    spec!(
        OpenWorktrees,
        "worktrees",
        Some("cmd-alt-w"),
        Some("⌥⌘W"),
        Some(APP_CONTEXT),
        "command.open_worktrees.palette_title",
        "square.stack.3d.up",
        "git branch checkout"
    ),
    spec!(
        OpenSettings,
        "settings",
        Some("cmd-,"),
        Some("⌘,"),
        Some(APP_CONTEXT),
        "command.open_settings.palette_title",
        "gearshape",
        "preferences config options"
    ),
    spec!(
        ToggleSidebar,
        "toggle-sidebar",
        Some("cmd-b"),
        Some("⌘B"),
        Some(APP_CONTEXT),
        "command.toggle_sidebar.palette_title",
        "sidebar.left",
        "hide show panel sidebar horizontal vertical tabs top bar"
    ),
    spec!(
        ToggleTabOrientation,
        "toggle-tab-orientation",
        Some("cmd-shift-s"),
        Some("⇧⌘S"),
        Some(APP_CONTEXT),
        "command.toggle_tab_orientation.palette_title",
        "rectangle.split.2x1",
        "horizontal vertical tabs sidebar layout"
    ),
    spec!(
        HorizontalTabs,
        "horizontal-tabs",
        None,
        None,
        Some(APP_CONTEXT),
        "command.horizontal_tabs.palette_title",
        "rectangle.topthird.inset.filled",
        "tab placement orientation top"
    ),
    spec!(
        VerticalTabs,
        "vertical-tabs",
        None,
        None,
        Some(APP_CONTEXT),
        "command.vertical_tabs.palette_title",
        "sidebar.left",
        "tab placement orientation side"
    ),
    spec!(
        FocusSidebar,
        "focus-sidebar",
        Some("cmd-shift-b"),
        Some("⇧⌘B"),
        Some(APP_CONTEXT),
        "command.focus_sidebar.palette_title",
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
        "command.quote_selection.palette_title",
        "text.quote",
        "append cite composer draft context"
    ),
    spec!(
        QuoteSelectionToSession,
        "quote-selection-to-session",
        Some("cmd-alt-shift-c"),
        Some("⌥⇧⌘C"),
        Some(APP_CONTEXT),
        "command.quote_selection_to_session.palette_title",
        "sidebar.left",
        "append cite composer draft target another agent"
    ),
    spec!(
        ArchiveSelectedSession,
        "archive-selected-session",
        Some("cmd-alt-shift-w"),
        Some("⌥⇧⌘W"),
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
        "command.delegate_selected_session.palette_title",
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
        "command.toggle_notifications.palette_title",
        "bell",
        "inbox unread alerts attention"
    ),
    spec!(
        CheckForUpdates,
        "check-for-updates",
        None,
        None,
        Some(APP_CONTEXT),
        "command.check_for_updates.palette_title",
        "arrow.triangle.2.circlepath",
        "upgrade version release"
    ),
    spec!(
        ShowWhatsNew,
        "whats-new",
        None,
        None,
        Some(APP_CONTEXT),
        "command.show_whats_new.palette_title",
        "sparkles",
        "whats new release highlights features changelog demo video tour"
    ),
    spec!(
        TogglePerfOverlay,
        "toggle-perf-overlay",
        Some("cmd-alt-p"),
        Some("⌥⌘P"),
        Some(APP_CONTEXT),
        "command.toggle_perf_overlay.palette_title",
        "chart.bar",
        "developer fps frame meter perf performance overlay jank dropped frames memory debug"
    ),
    spec!(
        ToggleRenderCounters,
        "toggle-render-counters",
        Some("cmd-alt-r"),
        Some("⌥⌘R"),
        Some(APP_CONTEXT),
        "command.toggle_render_counters.palette_title",
        "square.stack.3d.up",
        "developer renders render count redraw views perf performance debug"
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
    spec!(
        EnterCopyMode,
        "terminal-copy-mode",
        Some("cmd-alt-c"),
        Some("⌘⌥C"),
        Some(TERMINAL_CONTEXT),
        "command.enter_copy_mode.palette_title",
        "doc.on.clipboard",
        "copy keyboard terminal selection"
    ),
    spec!(
        FindSelection,
        "terminal-find-selection",
        Some("cmd-alt-f"),
        Some("⌘⌥F"),
        Some(TERMINAL_CONTEXT),
        "command.find_selection.palette_title",
        "magnifyingglass",
        "find selection terminal"
    ),
    spec!(
        InsertPath,
        "terminal-insert-path",
        Some("cmd-e"),
        Some("⌘E"),
        Some(TERMINAL_CONTEXT),
        "command.insert_path.palette_title",
        "magnifyingglass",
        "insert path file picker fuzzy terminal"
    ),
    spec!(
        ExportScrollback,
        "terminal-export",
        Some("cmd-shift-e"),
        Some("⌘⇧E"),
        Some(TERMINAL_CONTEXT),
        "command.export_scrollback.palette_title",
        "doc.text",
        "terminal log export scrollback"
    ),
    spec!(
        PreviousPrompt,
        "terminal-previous-prompt",
        Some("cmd-shift-up"),
        Some("⌘⇧↑"),
        Some(TERMINAL_CONTEXT),
        "command.previous_prompt.palette_title",
        "arrow.up",
        "terminal message prompt previous conversation"
    ),
    spec!(
        NextPrompt,
        "terminal-next-prompt",
        Some("cmd-shift-down"),
        Some("⌘⇧↓"),
        Some(TERMINAL_CONTEXT),
        "command.next_prompt.palette_title",
        "arrow.down",
        "terminal message prompt next conversation"
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
    // The Notes window's editing keys live in their own key contexts, so
    // they never shadow terminal or app shortcuts in the main window.
    cx.bind_keys(crate::notes::key_bindings());
    // The Files surface's editor keys, likewise scoped to its contexts.
    cx.bind_keys(crate::code_viewer::key_bindings());
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
        // The system owns this binding, including while Diri is inactive.
        if self.id == CommandId::ShowApp {
            return Vec::new();
        }
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
        if self.window_default_is_claimed(overrides) {
            return None;
        }
        match overrides.get(self.stable_id) {
            Some(None) => None,
            Some(Some(binding)) if Keystroke::parse(binding).is_ok() => Keystroke::parse(binding)
                .ok()
                .map(|key| shortcut_label_for_keystroke(&key)),
            #[cfg(not(target_os = "macos"))]
            _ if matches!(
                self.id,
                CommandId::DelegateSelectedSession
                    | CommandId::FocusPaneLeft
                    | CommandId::FocusPaneRight
                    | CommandId::FocusPaneUp
                    | CommandId::FocusPaneDown
            ) =>
            {
                linux_keystroke(self.id, self.keystroke?)
                    .and_then(|key| Keystroke::parse(&key).ok())
                    .map(|key| shortcut_label_for_keystroke(&key))
            }
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
        if self.window_default_is_claimed(overrides) {
            return Vec::new();
        }
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

    /// These defaults are new. A binding already saved by the user wins when
    /// upgrading; the window action remains available in menus and palette.
    fn window_default_is_claimed(&self, overrides: &ShortcutOverrides) -> bool {
        if !matches!(self.id, CommandId::NewWindow | CommandId::CloseWindow)
            || overrides.contains_key(self.stable_id)
        {
            return false;
        }
        let Some(candidate) = self
            .keystroke
            .and_then(|key| platform_keystroke(self.id, key))
            .and_then(|key| Keystroke::parse(&key).ok())
        else {
            return false;
        };
        overrides.iter().any(|(id, key)| {
            id != self.stable_id
                && COMMANDS.iter().any(|command| command.stable_id == id)
                && key
                    .as_ref()
                    .and_then(|key| Keystroke::parse(key).ok())
                    .is_some_and(|key| {
                        key.key == candidate.key && key.modifiers == candidate.modifiers
                    })
        })
    }

    fn key_binding(&self, key: &str) -> KeyBinding {
        let context = self.context;
        match self.id {
            CommandId::Quit => KeyBinding::new(key, Quit, context),
            CommandId::HideApp => KeyBinding::new(key, HideApp, context),
            CommandId::ShowApp => KeyBinding::new(key, ShowApp, context),
            CommandId::CloseWindow => KeyBinding::new(key, CloseWindow, context),
            CommandId::NewWindow => KeyBinding::new(key, NewWindow, context),
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
            CommandId::ReviewLaunches => KeyBinding::new(key, ReviewLaunches, context),
            CommandId::ToggleTabPeek => KeyBinding::new(key, ToggleTabPeek, context),
            CommandId::FocusPaneLeft => KeyBinding::new(key, FocusPaneLeft, context),
            CommandId::FocusPaneRight => KeyBinding::new(key, FocusPaneRight, context),
            CommandId::FocusPaneUp => KeyBinding::new(key, FocusPaneUp, context),
            CommandId::FocusPaneDown => KeyBinding::new(key, FocusPaneDown, context),
            CommandId::SplitPaneRight => KeyBinding::new(key, SplitPaneRight, context),
            CommandId::SplitPaneBelow => KeyBinding::new(key, SplitPaneBelow, context),
            CommandId::TogglePaneZoom => KeyBinding::new(key, TogglePaneZoom, context),
            CommandId::RemoveFocusedPane => KeyBinding::new(key, RemoveFocusedPane, context),
            CommandId::PaneGrowWidth => KeyBinding::new(key, PaneGrowWidth, context),
            CommandId::PaneShrinkWidth => KeyBinding::new(key, PaneShrinkWidth, context),
            CommandId::PaneGrowHeight => KeyBinding::new(key, PaneGrowHeight, context),
            CommandId::PaneShrinkHeight => KeyBinding::new(key, PaneShrinkHeight, context),
            CommandId::SwapPaneLeft => KeyBinding::new(key, SwapPaneLeft, context),
            CommandId::SwapPaneRight => KeyBinding::new(key, SwapPaneRight, context),
            CommandId::SwapPaneUp => KeyBinding::new(key, SwapPaneUp, context),
            CommandId::SwapPaneDown => KeyBinding::new(key, SwapPaneDown, context),
            CommandId::MovePaneLeft => KeyBinding::new(key, MovePaneLeft, context),
            CommandId::MovePaneRight => KeyBinding::new(key, MovePaneRight, context),
            CommandId::MovePaneUp => KeyBinding::new(key, MovePaneUp, context),
            CommandId::MovePaneDown => KeyBinding::new(key, MovePaneDown, context),

            CommandId::OpenWorktrees => KeyBinding::new(key, OpenWorktrees, context),
            CommandId::NewNote => KeyBinding::new(key, NewNote, context),
            CommandId::ShowTodos => KeyBinding::new(key, ShowTodos, context),
            CommandId::SearchNotes => KeyBinding::new(key, SearchNotes, context),
            CommandId::NoteVersionHistory => KeyBinding::new(key, NoteVersionHistory, context),
            CommandId::NoteGraph => KeyBinding::new(key, NoteGraph, context),
            CommandId::OpenSettings => KeyBinding::new(key, OpenSettings, context),
            CommandId::ToggleSidebar => KeyBinding::new(key, ToggleSidebar, context),
            CommandId::ToggleTabOrientation => KeyBinding::new(key, ToggleTabOrientation, context),
            CommandId::HorizontalTabs => KeyBinding::new(key, HorizontalTabs, context),
            CommandId::VerticalTabs => KeyBinding::new(key, VerticalTabs, context),
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
            CommandId::ShowWhatsNew => KeyBinding::new(key, ShowWhatsNew, context),
            CommandId::TogglePerfOverlay => KeyBinding::new(key, TogglePerfOverlay, context),
            CommandId::ToggleRenderCounters => KeyBinding::new(key, ToggleRenderCounters, context),
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
            CommandId::EnterCopyMode => KeyBinding::new(key, EnterCopyMode, context),
            CommandId::FindSelection => KeyBinding::new(key, FindSelection, context),
            CommandId::InsertPath => KeyBinding::new(key, InsertPath, context),
            CommandId::ExportScrollback => KeyBinding::new(key, ExportScrollback, context),
            CommandId::PreviousPrompt => KeyBinding::new(key, PreviousPrompt, context),
            CommandId::NextPrompt => KeyBinding::new(key, NextPrompt, context),
        }
    }
}

#[cfg(target_os = "macos")]
fn platform_keystroke(_id: CommandId, key: &str) -> Option<String> {
    Some(key.to_owned())
}

#[cfg(not(target_os = "macos"))]
fn platform_keystroke(id: CommandId, key: &str) -> Option<String> {
    linux_keystroke(id, key)
}

#[cfg(any(test, not(target_os = "macos")))]
fn linux_keystroke(id: CommandId, key: &str) -> Option<String> {
    if id == CommandId::HideApp {
        return None;
    }
    // Cmd-Ctrl-D would otherwise collide with the inspector's Cmd-Shift-D
    // after translating macOS modifiers to Linux.
    if id == CommandId::DelegateSelectedSession {
        return Some("ctrl-alt-d".to_owned());
    }
    // Cmd-Ctrl-T would land on New Tab's Ctrl-Shift-T.
    if id == CommandId::ShowTodos {
        return Some("ctrl-alt-shift-t".to_owned());
    }
    let pane_focus = match id {
        CommandId::FocusPaneLeft => Some("ctrl-alt-h"),
        CommandId::FocusPaneRight => Some("ctrl-alt-l"),
        CommandId::FocusPaneUp => Some("ctrl-alt-k"),
        CommandId::FocusPaneDown => Some("ctrl-alt-j"),
        _ => None,
    };
    Some(pane_focus.map_or_else(|| linux_chord(key), str::to_owned))
}

/// Maps a macOS `cmd-` chord onto the string `simulate_keystrokes` must send
/// on this OS. GPUI treats `cmd` as Super on Linux; shipped bindings use Ctrl.
#[cfg(test)]
pub(crate) fn test_chords(spec: &str) -> String {
    spec.split_whitespace()
        .map(|token| {
            if cfg!(target_os = "macos") {
                token.to_owned()
            } else {
                linux_chord(token)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(any(test, not(target_os = "macos")))]
fn linux_chord(key: &str) -> String {
    key.replace("cmd-ctrl-", "ctrl-shift-")
        .replace("cmd-", "ctrl-")
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

/// How a native menu prints a keymap source such as `"cmd-alt-1"`: `⌥⌘1`.
pub(crate) fn keystroke_label(source: &str) -> String {
    Keystroke::parse(source)
        .map_or_else(|_| source.to_owned(), |k| shortcut_label_for_keystroke(&k))
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
    /// The title, description and group Settings › Shortcuts shows, in the
    /// interface language.
    pub fn shortcut_metadata(self) -> ShortcutMetadata {
        self.shortcut_metadata_in(crate::i18n::t)
    }

    /// The English metadata, so a search typed in English still finds a
    /// shortcut whatever language is shown.
    pub fn english_shortcut_metadata(self) -> ShortcutMetadata {
        self.shortcut_metadata_in(crate::i18n::english)
    }

    fn shortcut_metadata_in(self, t: fn(&'static str) -> &'static str) -> ShortcutMetadata {
        use ShortcutCategory::{Application, Navigation, Sessions, Terminal, Workspace};
        match self {
            Self::CloseSession => ShortcutMetadata {
                title: t("command.close_session.title"),
                description: t("command.close_session.description"),
                category: Sessions,
            },
            Self::ReopenSession => ShortcutMetadata {
                title: t("command.reopen_session.title"),
                description: t("command.reopen_session.description"),
                category: Sessions,
            },
            Self::OpenLauncher => ShortcutMetadata {
                title: t("command.open_launcher.title"),
                description: t("command.open_launcher.description"),
                category: Sessions,
            },
            Self::NewDefaultSession => ShortcutMetadata {
                title: t("command.new_default_session.title"),
                description: t("command.new_default_session.description"),
                category: Sessions,
            },
            Self::NewTerminal => ShortcutMetadata {
                title: t("command.new_terminal.title"),
                description: t("command.new_terminal.description"),
                category: Sessions,
            },
            Self::NewCodexSession => ShortcutMetadata {
                title: t("command.new_codex_session.title"),
                description: t("command.new_codex_session.description"),
                category: Sessions,
            },
            Self::ArchiveSelectedSession => ShortcutMetadata {
                title: t("command.archive_selected_session.title"),
                description: t("command.archive_selected_session.description"),
                category: Sessions,
            },
            Self::RenameSelectedSession => ShortcutMetadata {
                title: t("command.rename_selected_session.title"),
                description: t("command.rename_selected_session.description"),
                category: Sessions,
            },
            Self::DelegateSelectedSession => ShortcutMetadata {
                title: t("command.delegate_selected_session.title"),
                description: t("command.delegate_selected_session.description"),
                category: Sessions,
            },
            Self::ToggleNotifications => ShortcutMetadata {
                title: t("command.toggle_notifications.title"),
                description: t("command.toggle_notifications.description"),
                category: Navigation,
            },
            Self::SelectNextAttentionSession => ShortcutMetadata {
                title: t("command.select_next_attention_session.title"),
                description: t("command.select_next_attention_session.description"),
                category: Sessions,
            },
            Self::QuoteSelection => ShortcutMetadata {
                title: t("command.quote_selection.title"),
                description: t("command.quote_selection.description"),
                category: Sessions,
            },
            Self::QuoteSelectionToSession => ShortcutMetadata {
                title: t("command.quote_selection_to_session.title"),
                description: t("command.quote_selection_to_session.description"),
                category: Sessions,
            },
            Self::ToggleHistory => ShortcutMetadata {
                title: t("command.toggle_history.title"),
                description: t("command.toggle_history.description"),
                category: Navigation,
            },
            Self::ToggleOverview => ShortcutMetadata {
                title: t("command.toggle_overview.title"),
                description: t("command.toggle_overview.description"),
                category: Navigation,
            },
            Self::ReviewLaunches => ShortcutMetadata {
                title: t("command.review_launches.title"),
                description: t("command.review_launches.description"),
                category: Workspace,
            },
            Self::FocusPaneLeft => ShortcutMetadata {
                title: t("command.focus_pane_left.title"),
                description: t("command.focus_pane_left.description"),
                category: Workspace,
            },
            Self::FocusPaneRight => ShortcutMetadata {
                title: t("command.focus_pane_right.title"),
                description: t("command.focus_pane_right.description"),
                category: Workspace,
            },
            Self::FocusPaneUp => ShortcutMetadata {
                title: t("command.focus_pane_up.title"),
                description: t("command.focus_pane_up.description"),
                category: Workspace,
            },
            Self::FocusPaneDown => ShortcutMetadata {
                title: t("command.focus_pane_down.title"),
                description: t("command.focus_pane_down.description"),
                category: Workspace,
            },
            Self::SplitPaneRight => ShortcutMetadata {
                title: t("command.split_pane_right.title"),
                description: t("command.split_pane_right.description"),
                category: Workspace,
            },
            Self::SplitPaneBelow => ShortcutMetadata {
                title: t("command.split_pane_below.title"),
                description: t("command.split_pane_below.description"),
                category: Workspace,
            },
            Self::TogglePaneZoom => ShortcutMetadata {
                title: t("command.toggle_pane_zoom.title"),
                description: t("command.toggle_pane_zoom.description"),
                category: Workspace,
            },
            Self::RemoveFocusedPane => ShortcutMetadata {
                title: t("command.remove_focused_pane.title"),
                description: t("command.remove_focused_pane.description"),
                category: Workspace,
            },
            Self::PaneGrowWidth => ShortcutMetadata {
                title: t("command.pane_grow_width.title"),
                description: t("command.pane_grow_width.description"),
                category: Workspace,
            },
            Self::PaneShrinkWidth => ShortcutMetadata {
                title: t("command.pane_shrink_width.title"),
                description: t("command.pane_shrink_width.description"),
                category: Workspace,
            },
            Self::PaneGrowHeight => ShortcutMetadata {
                title: t("command.pane_grow_height.title"),
                description: t("command.pane_grow_height.description"),
                category: Workspace,
            },
            Self::PaneShrinkHeight => ShortcutMetadata {
                title: t("command.pane_shrink_height.title"),
                description: t("command.pane_shrink_height.description"),
                category: Workspace,
            },
            Self::SwapPaneLeft => ShortcutMetadata {
                title: t("command.swap_pane_left.title"),
                description: t("command.swap_pane_left.description"),
                category: Workspace,
            },
            Self::SwapPaneRight => ShortcutMetadata {
                title: t("command.swap_pane_right.title"),
                description: t("command.swap_pane_right.description"),
                category: Workspace,
            },
            Self::SwapPaneUp => ShortcutMetadata {
                title: t("command.swap_pane_up.title"),
                description: t("command.swap_pane_up.description"),
                category: Workspace,
            },
            Self::SwapPaneDown => ShortcutMetadata {
                title: t("command.swap_pane_down.title"),
                description: t("command.swap_pane_down.description"),
                category: Workspace,
            },
            Self::MovePaneLeft => ShortcutMetadata {
                title: t("command.move_pane_left.title"),
                description: t("command.move_pane_left.description"),
                category: Workspace,
            },
            Self::MovePaneRight => ShortcutMetadata {
                title: t("command.move_pane_right.title"),
                description: t("command.move_pane_right.description"),
                category: Workspace,
            },
            Self::MovePaneUp => ShortcutMetadata {
                title: t("command.move_pane_up.title"),
                description: t("command.move_pane_up.description"),
                category: Workspace,
            },
            Self::MovePaneDown => ShortcutMetadata {
                title: t("command.move_pane_down.title"),
                description: t("command.move_pane_down.description"),
                category: Workspace,
            },
            Self::ToggleTabPeek => ShortcutMetadata {
                title: t("command.toggle_tab_peek.title"),
                description: t("command.toggle_tab_peek.description"),
                category: Navigation,
            },
            Self::ShowTodos => ShortcutMetadata {
                title: t("command.show_todos.title"),
                description: t("command.show_todos.description"),
                category: Navigation,
            },
            Self::SearchNotes => ShortcutMetadata {
                title: t("command.search_notes.title"),
                description: t("command.search_notes.description"),
                category: Navigation,
            },
            Self::NewNote => ShortcutMetadata {
                title: t("command.new_note.title"),
                description: t("command.new_note.description"),
                category: Navigation,
            },
            Self::NoteVersionHistory => ShortcutMetadata {
                title: t("command.note_version_history.title"),
                description: t("command.note_version_history.description"),
                category: Navigation,
            },
            Self::NoteGraph => ShortcutMetadata {
                title: t("command.note_graph.title"),
                description: t("command.note_graph.description"),
                category: Navigation,
            },
            Self::OpenWorktrees => ShortcutMetadata {
                title: t("command.open_worktrees.title"),
                description: t("command.open_worktrees.description"),
                category: Navigation,
            },
            Self::ToggleCommandPalette => ShortcutMetadata {
                title: t("command.toggle_command_palette.title"),
                description: t("command.toggle_command_palette.description"),
                category: Navigation,
            },
            Self::ToggleQuickOpen => ShortcutMetadata {
                title: t("command.toggle_quick_open.title"),
                description: t("command.toggle_quick_open.description"),
                category: Navigation,
            },
            Self::SelectPreviousSession => ShortcutMetadata {
                title: t("command.select_previous_session.title"),
                description: t("command.select_previous_session.description"),
                category: Navigation,
            },
            Self::SelectNextSession => ShortcutMetadata {
                title: t("command.select_next_session.title"),
                description: t("command.select_next_session.description"),
                category: Navigation,
            },
            Self::MoveSelectedSessionUp => ShortcutMetadata {
                title: t("command.move_selected_session_up.title"),
                description: t("command.move_selected_session_up.description"),
                category: Navigation,
            },
            Self::MoveSelectedSessionDown => ShortcutMetadata {
                title: t("command.move_selected_session_down.title"),
                description: t("command.move_selected_session_down.description"),
                category: Navigation,
            },
            Self::SelectSession1 => session_slot_metadata(
                t("command.select_session_1.title"),
                t("command.select_session_1.description"),
            ),
            Self::SelectSession2 => session_slot_metadata(
                t("command.select_session_2.title"),
                t("command.select_session_2.description"),
            ),
            Self::SelectSession3 => session_slot_metadata(
                t("command.select_session_3.title"),
                t("command.select_session_3.description"),
            ),
            Self::SelectSession4 => session_slot_metadata(
                t("command.select_session_4.title"),
                t("command.select_session_4.description"),
            ),
            Self::SelectSession5 => session_slot_metadata(
                t("command.select_session_5.title"),
                t("command.select_session_5.description"),
            ),
            Self::SelectSession6 => session_slot_metadata(
                t("command.select_session_6.title"),
                t("command.select_session_6.description"),
            ),
            Self::SelectSession7 => session_slot_metadata(
                t("command.select_session_7.title"),
                t("command.select_session_7.description"),
            ),
            Self::SelectSession8 => session_slot_metadata(
                t("command.select_session_8.title"),
                t("command.select_session_8.description"),
            ),
            Self::SelectLastSession => session_slot_metadata(
                t("command.select_last_session.title"),
                t("command.select_last_session.description"),
            ),
            Self::ToggleTabOrientation | Self::HorizontalTabs | Self::VerticalTabs => {
                ShortcutMetadata {
                    title: t("command.tab_orientation.title"),
                    description: t("command.tab_orientation.description"),
                    category: Workspace,
                }
            }
            Self::ToggleSidebar => ShortcutMetadata {
                title: t("command.toggle_sidebar.title"),
                description: t("command.toggle_sidebar.description"),
                category: Workspace,
            },
            Self::FocusSidebar => ShortcutMetadata {
                title: t("command.focus_sidebar.title"),
                description: t("command.focus_sidebar.description"),
                category: Workspace,
            },
            Self::ToggleInspector => ShortcutMetadata {
                title: t("command.toggle_inspector.title"),
                description: t("command.toggle_inspector.description"),
                category: Workspace,
            },
            Self::ToggleAuxiliaryTerminal => ShortcutMetadata {
                title: t("command.toggle_auxiliary_terminal.title"),
                description: t("command.toggle_auxiliary_terminal.description"),
                category: Workspace,
            },
            Self::OpenFind => ShortcutMetadata {
                title: t("command.open_find.title"),
                description: t("command.open_find.description"),
                category: Terminal,
            },
            Self::FindNext => ShortcutMetadata {
                title: t("command.find_next.title"),
                description: t("command.find_next.description"),
                category: Terminal,
            },
            Self::FindPrevious => ShortcutMetadata {
                title: t("command.find_previous.title"),
                description: t("command.find_previous.description"),
                category: Terminal,
            },
            Self::ZoomIn => ShortcutMetadata {
                title: t("command.zoom_in.title"),
                description: t("command.zoom_in.description"),
                category: Terminal,
            },
            Self::ZoomOut => ShortcutMetadata {
                title: t("command.zoom_out.title"),
                description: t("command.zoom_out.description"),
                category: Terminal,
            },
            Self::ResetZoom => ShortcutMetadata {
                title: t("command.reset_zoom.title"),
                description: t("command.reset_zoom.description"),
                category: Terminal,
            },
            Self::Paste => ShortcutMetadata {
                title: t("command.paste.title"),
                description: t("command.paste.description"),
                category: Terminal,
            },
            Self::EnterCopyMode => ShortcutMetadata {
                title: t("command.enter_copy_mode.title"),
                description: t("command.enter_copy_mode.description"),
                category: ShortcutCategory::Terminal,
            },
            Self::FindSelection => ShortcutMetadata {
                title: t("command.find_selection.title"),
                description: t("command.find_selection.description"),
                category: ShortcutCategory::Terminal,
            },
            Self::InsertPath => ShortcutMetadata {
                title: t("command.insert_path.title"),
                description: t("command.insert_path.description"),
                category: ShortcutCategory::Terminal,
            },
            Self::ExportScrollback => ShortcutMetadata {
                title: t("command.export_scrollback.title"),
                description: t("command.export_scrollback.description"),
                category: ShortcutCategory::Terminal,
            },
            Self::PreviousPrompt => ShortcutMetadata {
                title: t("command.previous_prompt.title"),
                description: t("command.previous_prompt.description"),
                category: ShortcutCategory::Terminal,
            },
            Self::NextPrompt => ShortcutMetadata {
                title: t("command.next_prompt.title"),
                description: t("command.next_prompt.description"),
                category: ShortcutCategory::Terminal,
            },
            Self::CopySelection => ShortcutMetadata {
                title: t("command.copy_selection.title"),
                description: t("command.copy_selection.description"),
                category: Terminal,
            },
            Self::OpenSettings => ShortcutMetadata {
                title: t("command.open_settings.title"),
                description: t("command.open_settings.description"),
                category: Application,
            },
            Self::CheckForUpdates => ShortcutMetadata {
                title: t("command.check_for_updates.title"),
                description: t("command.check_for_updates.description"),
                category: Application,
            },
            Self::ShowWhatsNew => ShortcutMetadata {
                title: t("command.show_whats_new.title"),
                description: t("command.show_whats_new.description"),
                category: Application,
            },
            Self::TogglePerfOverlay => ShortcutMetadata {
                title: t("command.toggle_perf_overlay.title"),
                description: t("command.toggle_perf_overlay.description"),
                category: Application,
            },
            Self::ToggleRenderCounters => ShortcutMetadata {
                title: t("command.toggle_render_counters.title"),
                description: t("command.toggle_render_counters.description"),
                category: Application,
            },
            Self::NewWindow => ShortcutMetadata {
                title: t("command.new_window.title"),
                description: t("command.new_window.description"),
                category: Application,
            },
            Self::CloseWindow => ShortcutMetadata {
                title: t("command.close_window.title"),
                description: t("command.close_window.description"),
                category: Application,
            },
            Self::HideApp => ShortcutMetadata {
                title: t("command.hide_app.title"),
                description: t("command.hide_app.description"),
                category: Application,
            },
            Self::ShowApp => ShortcutMetadata {
                title: t("command.show_app.title"),
                description: t("command.show_app.description"),
                category: Application,
            },
            Self::Quit => ShortcutMetadata {
                title: t("command.quit.title"),
                description: t("command.quit.description"),
                category: Application,
            },
        }
    }
}

fn session_slot_metadata(title: &'static str, description: &'static str) -> ShortcutMetadata {
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
            Self::ShowApp => Box::new(ShowApp),
            Self::CloseWindow => Box::new(CloseWindow),
            Self::NewWindow => Box::new(NewWindow),
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
            Self::ToggleTabPeek => Box::new(ToggleTabPeek),
            Self::ReviewLaunches => Box::new(ReviewLaunches),
            Self::FocusPaneLeft => Box::new(FocusPaneLeft),
            Self::FocusPaneRight => Box::new(FocusPaneRight),
            Self::FocusPaneUp => Box::new(FocusPaneUp),
            Self::FocusPaneDown => Box::new(FocusPaneDown),
            Self::SplitPaneRight => Box::new(SplitPaneRight),
            Self::SplitPaneBelow => Box::new(SplitPaneBelow),
            Self::TogglePaneZoom => Box::new(TogglePaneZoom),
            Self::RemoveFocusedPane => Box::new(RemoveFocusedPane),
            Self::PaneGrowWidth => Box::new(PaneGrowWidth),
            Self::PaneShrinkWidth => Box::new(PaneShrinkWidth),
            Self::PaneGrowHeight => Box::new(PaneGrowHeight),
            Self::PaneShrinkHeight => Box::new(PaneShrinkHeight),
            Self::SwapPaneLeft => Box::new(SwapPaneLeft),
            Self::SwapPaneRight => Box::new(SwapPaneRight),
            Self::SwapPaneUp => Box::new(SwapPaneUp),
            Self::SwapPaneDown => Box::new(SwapPaneDown),
            Self::MovePaneLeft => Box::new(MovePaneLeft),
            Self::MovePaneRight => Box::new(MovePaneRight),
            Self::MovePaneUp => Box::new(MovePaneUp),
            Self::MovePaneDown => Box::new(MovePaneDown),

            Self::OpenWorktrees => Box::new(OpenWorktrees),
            Self::NewNote => Box::new(NewNote),
            Self::ShowTodos => Box::new(ShowTodos),
            Self::SearchNotes => Box::new(SearchNotes),
            Self::NoteVersionHistory => Box::new(NoteVersionHistory),
            Self::NoteGraph => Box::new(NoteGraph),
            Self::OpenSettings => Box::new(OpenSettings),
            Self::ToggleSidebar => Box::new(ToggleSidebar),
            Self::ToggleTabOrientation => Box::new(ToggleTabOrientation),
            Self::HorizontalTabs => Box::new(HorizontalTabs),
            Self::VerticalTabs => Box::new(VerticalTabs),
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
            Self::ShowWhatsNew => Box::new(ShowWhatsNew),
            Self::TogglePerfOverlay => Box::new(TogglePerfOverlay),
            Self::ToggleRenderCounters => Box::new(ToggleRenderCounters),
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
            Self::EnterCopyMode => Box::new(EnterCopyMode),
            Self::FindSelection => Box::new(FindSelection),
            Self::InsertPath => Box::new(InsertPath),
            Self::ExportScrollback => Box::new(ExportScrollback),
            Self::PreviousPrompt => Box::new(PreviousPrompt),
            Self::NextPrompt => Box::new(NextPrompt),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn pane_shortcuts_do_not_shadow_other_contexts_or_alternates() {
        let overrides = ShortcutOverrides::default();
        for spec in COMMANDS
            .iter()
            .filter(|spec| crate::workspace_workbench::PaneCommand::from_id(spec.id).is_some())
        {
            for binding in spec.effective_keystrokes(&overrides) {
                assert!(
                    shortcut_conflict(spec.id, &binding, &overrides).is_none(),
                    "{:?} conflicts on {}",
                    spec.id,
                    binding
                );
            }
            if let Some(binding) = spec.keystroke.and_then(|key| linux_keystroke(spec.id, key)) {
                let parsed = Keystroke::parse(&binding).unwrap();
                for other in COMMANDS.iter().filter(|other| other.id != spec.id) {
                    for key in other
                        .keystroke
                        .into_iter()
                        .chain(other.alternate_keystrokes.iter().copied())
                    {
                        if let Some(other_key) = linux_keystroke(other.id, key) {
                            let other_parsed = Keystroke::parse(&other_key).unwrap();
                            assert!(
                                parsed.modifiers != other_parsed.modifiers
                                    || parsed.key != other_parsed.key,
                                "{:?} conflicts with {:?} on Linux {}",
                                spec.id,
                                other.id,
                                binding
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn application_and_global_defaults_are_unique_across_product_actions() {
        let defaults = ShortcutOverrides::new();
        let commands: Vec<_> = COMMANDS
            .iter()
            .filter(|command| command.context.is_none() || command.context == Some(APP_CONTEXT))
            .collect();
        for (index, command) in commands.iter().enumerate() {
            for key in command.effective_keystrokes(&defaults) {
                let parsed = Keystroke::parse(&key).unwrap();
                for other in &commands[index + 1..] {
                    for other_key in other.effective_keystrokes(&defaults) {
                        let other_parsed = Keystroke::parse(&other_key).unwrap();
                        assert!(
                            parsed.key != other_parsed.key
                                || parsed.modifiers != other_parsed.modifiers,
                            "{:?} and {:?} share application chord {}",
                            command.id,
                            other.id,
                            key
                        );
                    }
                }
            }
        }
        // More specific editor/browser contexts may intentionally reuse keys.
        let overrides = ShortcutOverrides::from([
            ("new-codex".into(), Some(test_chords("cmd-shift-n"))),
            (
                "archive-selected-session".into(),
                Some(test_chords("cmd-shift-w")),
            ),
        ]);
        assert_eq!(
            command(CommandId::NewCodexSession).effective_keystrokes(&overrides),
            vec![test_chords("cmd-shift-n")]
        );
        assert_eq!(
            command(CommandId::ArchiveSelectedSession).effective_keystrokes(&overrides),
            vec![test_chords("cmd-shift-w")]
        );
        assert!(
            command(CommandId::NewWindow)
                .effective_keystrokes(&overrides)
                .is_empty()
        );
        assert!(
            command(CommandId::CloseWindow)
                .effective_keystrokes(&overrides)
                .is_empty()
        );
    }

    #[test]
    fn new_window_defaults_preserve_existing_custom_bindings() {
        let overrides = ShortcutOverrides::from([
            ("open-launcher".into(), Some(test_chords("cmd-shift-n"))),
            ("new-default".into(), Some(test_chords("cmd-shift-w"))),
        ]);
        for id in [CommandId::NewWindow, CommandId::CloseWindow] {
            assert!(command(id).effective_keystrokes(&overrides).is_empty());
            assert_eq!(command(id).shortcut_label_for(&overrides), None);
        }
        assert_eq!(
            command(CommandId::OpenLauncher).effective_keystrokes(&overrides),
            vec![test_chords("cmd-shift-n")]
        );
        assert_eq!(
            shortcut_conflict(
                CommandId::NewWindow,
                &test_chords("cmd-shift-n"),
                &overrides
            )
            .unwrap()
            .id,
            CommandId::OpenLauncher
        );
        assert_eq!(
            command(CommandId::OpenLauncher).effective_keystrokes(&ShortcutOverrides::new()),
            vec![test_chords("cmd-n")]
        );
        assert_eq!(
            command(CommandId::CloseSession).effective_keystrokes(&ShortcutOverrides::new()),
            vec![test_chords("cmd-w")]
        );
    }

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
    fn linux_default_bindings_do_not_collide() {
        let mut bindings = HashSet::new();
        for command in COMMANDS {
            for key in command
                .keystroke
                .into_iter()
                .chain(command.alternate_keystrokes.iter().copied())
            {
                let Some(key) = linux_keystroke(command.id, key) else {
                    continue;
                };
                assert!(
                    bindings.insert((command.context, key.clone())),
                    "duplicate Linux binding for {:?}: {key}",
                    command.id
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

        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            command(CommandId::DelegateSelectedSession)
                .shortcut_label_for(&ShortcutOverrides::new())
                .as_deref(),
            Some("Ctrl+Alt+D")
        );

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
            quote.palette.map(|metadata| metadata.title()),
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
        let expected: usize = COMMANDS
            .iter()
            .map(|command| {
                command
                    .effective_keystrokes(&ShortcutOverrides::new())
                    .len()
            })
            .sum();
        assert_eq!(binding_count, expected);
        assert!(binding_count > 0);
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
        let conflict =
            shortcut_conflict(CommandId::OpenLauncher, &test_chords("cmd-["), &overrides)
                .expect("navigation alternate should be reserved");
        assert_eq!(conflict.id, CommandId::SelectPreviousSession);
    }

    #[test]
    fn every_command_has_shortcut_page_copy() {
        for command in COMMANDS {
            let metadata = command.id.shortcut_metadata();
            assert!(!metadata.title.is_empty());
            assert!(!metadata.description.is_empty());
            // Palette titles are catalog ids the source scan cannot check.
            if let Some(palette) = command.palette {
                assert_ne!(
                    crate::i18n::english(palette.title_id),
                    palette.title_id,
                    "{} palette title id is missing from the catalog",
                    command.stable_id
                );
            }
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
