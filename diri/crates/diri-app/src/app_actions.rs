//! The window's exported actions: what its menus, keys and command palette
//! run, published to the Engine so an Agent (`perform_action`), a script or
//! the `dirijor do` CLI can run them by name.
//!
//! The command registry (`commands::COMMANDS`) is the source: every command
//! is exported under its stable id with its English title and description,
//! unless [`exposure`] keeps it in. A few palette pages that take a value
//! (a tab number, a theme, a session) are added as actions with typed
//! arguments. Running one dispatches the same GPUI action a key press or
//! palette row does, so behaviour cannot drift between the two paths.

use diri_proto::SessionId;
use diri_proto::app_actions::{
    ACTION_UNAVAILABLE, ActionRisk, AppActionSpec, AppActionsPublishParams, AppPerformRequest,
    INVALID_ARGS, UNKNOWN_ACTION,
};
use diri_proto::control::ControlError;
use serde_json::{Value, json};

use crate::commands::{self, CommandId};

/// Names of the actions that take arguments, beside the command registry's.
pub const SELECT_TAB: &str = "select-tab";
pub const SHOW_SESSION: &str = "show-session";
pub const SET_THEME: &str = "set-theme";

/// How a command is offered to callers outside the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exposure {
    /// Not exported: it reads or writes the clipboard or the person's
    /// selection, opens a system file dialog, or brings Diri to the front.
    Hidden,
    Safe,
    /// Closes, quits, archives, removes, or installs. An Agent needs a
    /// `session_id` it may manage, or cannot run it at all.
    Destructive,
}

/// Every command's exposure. Exhaustive on purpose: a new command must be
/// decided here before it builds.
pub fn exposure(id: CommandId) -> Exposure {
    use CommandId::*;
    match id {
        // Clipboard and selection: what is there may be a secret.
        Paste | CopySelection | CopyLastCommandOutput | QuoteSelection
        | QuoteSelectionToSession => Exposure::Hidden,
        // A save panel the person must answer.
        ExportScrollback => Exposure::Hidden,
        // Reads other terminals' settings files from disk.
        ImportTerminalSetup => Exposure::Hidden,
        // Bringing Diri forward or hiding it moves the person's focus.
        HideApp | ShowApp => Exposure::Hidden,
        Quit | CloseWindow | CloseSession | ArchiveSelectedSession | RemoveFocusedPane
        | CheckForUpdates | ClearScrollback | ResetTerminal => Exposure::Destructive,
        NewWindow | ReopenSession | OpenLauncher | NewDefaultSession | NewTerminal
        | NewCodexSession | ToggleCommandPalette | ToggleQuickOpen | ToggleHistory
        | ToggleNotifications | ToggleOverview | ToggleTabPeek
        | FocusPaneLeft | FocusPaneRight | FocusPaneUp | FocusPaneDown | SplitPaneRight
        | SplitPaneBelow | FocusPreviousPane | FocusNextPane | EqualizePanes
        | TogglePaneZoom | PaneGrowWidth | PaneShrinkWidth | PaneGrowHeight
        | PaneShrinkHeight | SwapPaneLeft | SwapPaneRight | SwapPaneUp | SwapPaneDown
        | MovePaneLeft | MovePaneRight | MovePaneUp | MovePaneDown | OpenWorktrees | NewNote
        | ShowTodos | SearchNotes | NoteVersionHistory | NoteGraph | OpenSettings
        | ToggleSidebar | ToggleTabOrientation | HorizontalTabs | VerticalTabs
        | ComfortableDensity | CompactDensity | FocusSidebar | ToggleInspector
        | ToggleAuxiliaryTerminal | RenameSelectedSession | DelegateSelectedSession
        | SelectNextAttentionSession | ShowWhatsNew | ToggleReadAloud | ToggleReadThisTab
        | ReadOnlyThisTab | ReadAllExceptThisTab | ReadAllTabs | TogglePerfOverlay
        | ToggleRenderCounters | SelectPreviousSession | SelectNextSession
        | MoveSelectedSessionUp | MoveSelectedSessionDown | SelectSession1 | SelectSession2
        | SelectSession3 | SelectSession4 | SelectSession5 | SelectSession6 | SelectSession7
        | SelectSession8 | SelectLastSession | OpenFind | FindNext | FindPrevious | ZoomIn
        | ZoomOut | ResetZoom | EnterCopyMode | FindSelection | InsertPath | PreviousPrompt
        | NextPrompt | ToggleActivity | SplitPaneLeft | SplitPaneAbove | SplitPaneBestFit
        | MovePaneToNewTab | MovePaneToNextTab | MovePaneToPreviousTab | MoveTabToNewWindow
        | MoveTabToNextWindow | GoToDirectory | ReloadScripts => Exposure::Safe,
    }
}

/// Commands an Agent may aim at one session with `session_id` instead of
/// the window's selected one.
fn takes_session(id: CommandId) -> bool {
    matches!(id, CommandId::CloseSession | CommandId::ArchiveSelectedSession)
}

fn session_arg() -> Value {
    json!({"type": "string", "minLength": 1,
        "description": "The session to act on; defaults to the window's selected tab."})
}

/// The catalog the app publishes. Built from the command registry each time
/// the app connects, so it always matches this build.
pub fn catalog() -> Vec<AppActionSpec> {
    let mut actions: Vec<AppActionSpec> = commands::COMMANDS
        .iter()
        .filter(|command| exposure(command.id) != Exposure::Hidden)
        .map(|command| {
            let metadata = command.id.english_shortcut_metadata();
            let args = if takes_session(command.id) {
                json!({"type": "object", "properties": {"session_id": session_arg()}, "additionalProperties": false})
            } else {
                AppActionSpec::no_args()
            };
            AppActionSpec {
                name: command.stable_id.into(),
                title: command_title(command, true).into(),
                description: metadata.description.into(),
                category: metadata.category.english_label().into(),
                args,
                risk: if exposure(command.id) == Exposure::Destructive {
                    ActionRisk::Destructive
                } else {
                    ActionRisk::Safe
                },
                shortcut: command.shortcut.map(Into::into),
            }
        })
        .collect();
    let themes: Vec<Value> = diri_term::themes::all()
        .iter()
        .map(|theme| json!(theme.id))
        .collect();
    actions.extend([
        AppActionSpec {
            name: SELECT_TAB.into(),
            title: "Select Tab".into(),
            description: "Show the tab at this position in the sidebar: 1 is the first, -1 the last.".into(),
            category: commands::ShortcutCategory::Navigation.english_label().into(),
            args: json!({"type": "object", "properties": {"index": {"type": "integer", "minimum": -1, "maximum": 8}}, "required": ["index"], "additionalProperties": false}),
            risk: ActionRisk::Safe,
            shortcut: None,
        },
        AppActionSpec {
            name: SHOW_SESSION.into(),
            title: "Show Session".into(),
            description: "Select a session's tab in the window, without bringing Diri to the front.".into(),
            category: commands::ShortcutCategory::Navigation.english_label().into(),
            args: json!({"type": "object", "properties": {"session_id": {"type": "string", "minLength": 1}}, "required": ["session_id"], "additionalProperties": false}),
            risk: ActionRisk::Safe,
            shortcut: None,
        },
        AppActionSpec {
            name: SET_THEME.into(),
            title: "Set Color Theme".into(),
            description: "Use this terminal color theme and stop following the system appearance.".into(),
            category: commands::ShortcutCategory::Application.english_label().into(),
            args: json!({"type": "object", "properties": {"theme": {"type": "string", "enum": themes}}, "required": ["theme"], "additionalProperties": false}),
            risk: ActionRisk::Safe,
            shortcut: None,
        },
    ]);
    actions
}

pub fn publish_params() -> AppActionsPublishParams {
    AppActionsPublishParams {
        build: env!("CARGO_PKG_VERSION").into(),
        actions: catalog(),
    }
}

/// The palette's title when the command has one (what people search for),
/// else the Shortcuts title; in English for callers, or in the interface
/// language for the capsule.
fn command_title(command: &commands::CommandSpec, english: bool) -> &'static str {
    match (command.palette, english) {
        (Some(palette), true) => crate::i18n::english(palette.title_id),
        (Some(palette), false) => palette.title(),
        (None, true) => command.id.english_shortcut_metadata().title,
        (None, false) => command.id.shortcut_metadata().title,
    }
}

/// The title the capsule shows for an Agent's action, in the interface
/// language. `None` for a name this build does not export.
pub fn title(name: &str) -> Option<String> {
    if let Some(command) = commands::COMMANDS
        .iter()
        .find(|command| command.stable_id == name && exposure(command.id) != Exposure::Hidden)
    {
        return Some(command_title(command, false).to_owned());
    }
    match name {
        SELECT_TAB | SHOW_SESSION => Some(crate::i18n::t("agent_notice.show_tab").to_owned()),
        SET_THEME => Some(crate::i18n::t("palette.color_theme").to_owned()),
        _ => None,
    }
}

/// What a request asks the window to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// Dispatch this command's GPUI action, as its key binding would.
    Command(CommandId),
    /// Close (stop) this session's tab.
    CloseSession(SessionId),
    /// Archive this session.
    ArchiveSession(SessionId),
    ShowSession(SessionId),
    SetTheme(String),
}

/// Checks a request against this build's catalog. The Engine already did,
/// against what was published; this guards a request that raced an upgrade.
pub fn resolve(request: &AppPerformRequest) -> Result<Resolved, ControlError> {
    let spec = catalog()
        .into_iter()
        .find(|spec| spec.name == request.action)
        .ok_or_else(|| {
            ControlError::new(
                UNKNOWN_ACTION,
                format!("this app has no action {:?}", request.action),
            )
        })?;
    spec.validate(&request.args)
        .map_err(|error| ControlError::new(INVALID_ARGS, error))?;
    let session = request
        .args
        .get("session_id")
        .and_then(Value::as_str)
        .map(SessionId::new);
    Ok(match request.action.as_str() {
        SELECT_TAB => {
            let index = request.args["index"].as_i64().unwrap_or(0);
            Resolved::Command(match index {
                -1 => CommandId::SelectLastSession,
                1 => CommandId::SelectSession1,
                2 => CommandId::SelectSession2,
                3 => CommandId::SelectSession3,
                4 => CommandId::SelectSession4,
                5 => CommandId::SelectSession5,
                6 => CommandId::SelectSession6,
                7 => CommandId::SelectSession7,
                8 => CommandId::SelectSession8,
                _ => {
                    return Err(ControlError::new(
                        INVALID_ARGS,
                        "index is 1 to 8, or -1 for the last tab",
                    ));
                }
            })
        }
        SHOW_SESSION => Resolved::ShowSession(session.expect("required by the schema")),
        SET_THEME => Resolved::SetTheme(request.args["theme"].as_str().unwrap_or("").into()),
        name => {
            let id = commands::COMMANDS
                .iter()
                .find(|command| command.stable_id == name)
                .map(|command| command.id)
                .expect("the catalog is built from the registry");
            match (id, session) {
                (CommandId::CloseSession, Some(session)) => Resolved::CloseSession(session),
                (CommandId::ArchiveSelectedSession, Some(session)) => {
                    Resolved::ArchiveSession(session)
                }
                (id, _) => Resolved::Command(id),
            }
        }
    })
}

/// Commands main.rs answers with an application-wide `cx.on_action`, which
/// the window's dispatch tree does not list but a dispatch still reaches.
pub fn handled_globally(id: CommandId) -> bool {
    matches!(
        id,
        CommandId::Quit
            | CommandId::CloseWindow
            | CommandId::CloseSession
            | CommandId::NewWindow
            | CommandId::ReloadScripts
    )
}

/// The window could not run it now (for example a terminal command while no
/// terminal has focus).
pub fn unavailable(name: &str) -> ControlError {
    ControlError::new(
        ACTION_UNAVAILABLE,
        format!("{name} is not available in the window right now"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(action: &str, args: Value) -> AppPerformRequest {
        AppPerformRequest {
            request_id: 1,
            action: action.into(),
            args,
            actor: None,
        }
    }

    #[test]
    fn every_exported_command_comes_from_the_registry_once_with_a_title() {
        let actions = catalog();
        let mut names: Vec<&str> = actions.iter().map(|action| action.name.as_str()).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "action names are unique");
        let exported = commands::COMMANDS
            .iter()
            .filter(|command| exposure(command.id) != Exposure::Hidden)
            .count();
        // The registry's commands plus the three value-taking actions.
        assert_eq!(total, exported + 3);
        assert!(total >= 90, "{total} actions");
        for action in &actions {
            assert!(!action.title.is_empty(), "{}", action.name);
            assert_eq!(action.args["type"], "object", "{}", action.name);
            assert!(title(&action.name).is_some(), "{}", action.name);
        }
    }

    #[test]
    fn nothing_that_touches_the_clipboard_or_selection_is_exported() {
        let names: Vec<String> = catalog().into_iter().map(|action| action.name).collect();
        for hidden in [
            "terminal-paste",
            "terminal-copy",
            "terminal-copy-last-command-output",
            "quote-selection",
            "quote-selection-to-session",
            "terminal-export",
            "show-app",
            "hide-app",
        ] {
            assert!(!names.iter().any(|name| name == hidden), "{hidden}");
            assert_eq!(title(hidden), None, "{hidden}");
        }
    }

    #[test]
    fn closing_quitting_and_archiving_are_destructive() {
        let actions = catalog();
        let risk = |name: &str| actions.iter().find(|a| a.name == name).unwrap().risk;
        for name in [
            "quit",
            "close-window",
            "close-session",
            "archive-selected-session",
            "remove-focused-pane",
            "check-for-updates",
        ] {
            assert_eq!(risk(name), ActionRisk::Destructive, "{name}");
        }
        assert_eq!(risk("toggle-sidebar"), ActionRisk::Safe);
        let close = actions.iter().find(|a| a.name == "close-session").unwrap();
        assert!(close.takes_session());
        let quit = actions.iter().find(|a| a.name == "quit").unwrap();
        assert!(!quit.takes_session(), "quit cannot be aimed at a session");
    }

    #[test]
    fn requests_resolve_to_the_same_commands_keys_run() {
        assert_eq!(
            resolve(&request("split-pane-right", json!({}))),
            Ok(Resolved::Command(CommandId::SplitPaneRight))
        );
        assert_eq!(
            resolve(&request(SELECT_TAB, json!({"index": 3}))),
            Ok(Resolved::Command(CommandId::SelectSession3))
        );
        assert_eq!(
            resolve(&request(SELECT_TAB, json!({"index": -1}))),
            Ok(Resolved::Command(CommandId::SelectLastSession))
        );
        assert_eq!(
            resolve(&request(SELECT_TAB, json!({"index": 0})))
                .unwrap_err()
                .code,
            INVALID_ARGS
        );
        assert_eq!(
            resolve(&request("close-session", json!({"session_id": "s-2"}))),
            Ok(Resolved::CloseSession(SessionId::new("s-2")))
        );
        assert_eq!(
            resolve(&request("close-session", json!({}))),
            Ok(Resolved::Command(CommandId::CloseSession))
        );
        assert_eq!(
            resolve(&request("terminal-paste", json!({})))
                .unwrap_err()
                .code,
            UNKNOWN_ACTION
        );
        assert_eq!(
            resolve(&request("toggle-sidebar", json!({"x": 1})))
                .unwrap_err()
                .code,
            INVALID_ARGS
        );
        let theme = diri_term::themes::all()[0].id;
        assert_eq!(
            resolve(&request(SET_THEME, json!({"theme": theme}))),
            Ok(Resolved::SetTheme(theme.into()))
        );
        assert_eq!(
            resolve(&request(SET_THEME, json!({"theme": "no-such-theme"})))
                .unwrap_err()
                .code,
            INVALID_ARGS
        );
    }
}
