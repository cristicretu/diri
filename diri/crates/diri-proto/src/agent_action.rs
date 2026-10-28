//! Who changed the window: an Agent (through Diri's MCP tools) asked the
//! Engine to open, close, split, focus or show something the person is
//! looking at. The request carries the asking session as [`ACTOR_FIELD`];
//! after the Engine commits it, it publishes [`EventName::AGENT_ACTION`] so
//! the app can name the actor and the action. The app's own requests never
//! carry an actor, so the person's input is never announced back to them.
//!
//! Additive: an Engine that predates the field ignores it, and an app that
//! predates the event never subscribes to it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Method, SessionId};

/// The request field naming the session that asked for a change.
pub const ACTOR_FIELD: &str = "actor";

/// What an Agent did to the window, in the words the app shows.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAction {
    NewTab,
    NewNote,
    CloseTab,
    ShowTab,
    RenameTab,
    MoveTab,
    SplitPane,
    MovePane,
    ClosePane,
    FocusPane,
    ZoomPane,
    UnzoomPane,
    EqualizePanes,
    SwapPanes,
    ResizePane,
    ChangeWorkspaces,
    OpenApiRequest,
    StopAgent,
    SleepAgent,
    WakeAgent,
    ResumeAgent,
}

/// `agent.action`: `session_id` (the actor) did `action`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentActionEvent {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub action: AgentAction,
}

/// Whether requests for `method` are stamped with an actor. Only methods
/// that change what the window shows; reads never are. A tracked spawn is
/// not stamped: its strict params already name the actor as `senderID`.
pub fn announces(method: &str) -> bool {
    matches!(
        method,
        Method::WORKSPACE_MUTATE
            | Method::SESSION_SPAWN
            | Method::SESSION_FORK
            | Method::SESSION_KILL
            | Method::SESSION_REVEAL
            | Method::SESSION_OPEN_API_REQUEST
            | Method::SESSION_HIBERNATE
            | Method::SESSION_WAKE
            | Method::SESSION_RESUME
    )
}

/// The actor and action a request announces once it succeeds. `None` when
/// the request names no actor or changes nothing the window shows.
pub fn announcement(method: &str, params: Option<&Value>) -> Option<AgentActionEvent> {
    let params = params?;
    // A tracked spawn always names its sender; the app drops a note sender,
    // which is the person starting work from a note.
    let field = if method == Method::SESSION_SPAWN_TRACKED {
        "senderID"
    } else {
        ACTOR_FIELD
    };
    let actor = params
        .get(field)
        .and_then(Value::as_str)
        .filter(|actor| !actor.is_empty())?;
    let action = action(method, params)?;
    Some(AgentActionEvent {
        session_id: SessionId::new(actor),
        action,
    })
}

fn action(method: &str, params: &Value) -> Option<AgentAction> {
    let spawned = |spawn: &Value| {
        if spawn.get("kind").and_then(Value::as_str) == Some(crate::AgentKind::NOTE_ID) {
            AgentAction::NewNote
        } else {
            AgentAction::NewTab
        }
    };
    Some(match method {
        Method::WORKSPACE_MUTATE => return mutation(params.get("mutation")?),
        Method::SESSION_SPAWN => spawned(params),
        Method::SESSION_SPAWN_TRACKED => spawned(params.get("spawn").unwrap_or(params)),
        Method::SESSION_FORK => AgentAction::NewTab,
        Method::SESSION_KILL => AgentAction::StopAgent,
        Method::SESSION_REVEAL => AgentAction::ShowTab,
        Method::SESSION_OPEN_API_REQUEST => AgentAction::OpenApiRequest,
        Method::SESSION_HIBERNATE => AgentAction::SleepAgent,
        Method::SESSION_WAKE => AgentAction::WakeAgent,
        Method::SESSION_RESUME => AgentAction::ResumeAgent,
        _ => return None,
    })
}

fn mutation(mutation: &Value) -> Option<AgentAction> {
    Some(match mutation.get("type")?.as_str()? {
        "createTab" | "openProjectAgent" => AgentAction::NewTab,
        "removeTab" => AgentAction::CloseTab,
        "selectTab" => AgentAction::ShowTab,
        "renameTab" => AgentAction::RenameTab,
        "moveTab" => AgentAction::MoveTab,
        "splitPane" => AgentAction::SplitPane,
        "moveNode" => AgentAction::MovePane,
        "removePane" => AgentAction::ClosePane,
        "focusPane" => AgentAction::FocusPane,
        "zoomPane" => {
            if mutation.get("paneId").is_some_and(|pane| !pane.is_null()) {
                AgentAction::ZoomPane
            } else {
                AgentAction::UnzoomPane
            }
        }
        "equalizeTab" => AgentAction::EqualizePanes,
        "swapPanes" => AgentAction::SwapPanes,
        "resizeSplit" => AgentAction::ResizePane,
        "createWorkspace" | "renameWorkspace" | "removeWorkspace" | "moveWorkspace" => {
            AgentAction::ChangeWorkspaces
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{DockEdge, PaneId, TabId, WorkspaceMutation, WorkspaceMutationParams};
    use serde_json::json;

    fn stamped(mutation: WorkspaceMutation) -> Value {
        let mut params = serde_json::to_value(WorkspaceMutationParams {
            expected_revision: 3,
            mutation,
        })
        .unwrap();
        params[ACTOR_FIELD] = json!("agent-1");
        params
    }

    #[test]
    fn requests_without_an_actor_are_the_persons_own_and_never_announced() {
        let params = serde_json::to_value(WorkspaceMutationParams {
            expected_revision: 3,
            mutation: WorkspaceMutation::ZoomPane {
                tab_id: TabId("t".into()),
                pane_id: Some(PaneId("p".into())),
            },
        })
        .unwrap();
        assert_eq!(announcement(Method::WORKSPACE_MUTATE, Some(&params)), None);
        assert_eq!(announcement(Method::SESSION_REVEAL, None), None);
        let empty = json!({"sessionID": "s", ACTOR_FIELD: ""});
        assert_eq!(announcement(Method::SESSION_REVEAL, Some(&empty)), None);
    }

    #[test]
    fn layout_mutations_name_the_actor_and_the_typed_action() {
        let zoom = stamped(WorkspaceMutation::ZoomPane {
            tab_id: TabId("t".into()),
            pane_id: Some(PaneId("p".into())),
        });
        assert_eq!(
            announcement(Method::WORKSPACE_MUTATE, Some(&zoom)),
            Some(AgentActionEvent {
                session_id: SessionId::new("agent-1"),
                action: AgentAction::ZoomPane,
            })
        );
        let cases = [
            (
                WorkspaceMutation::ZoomPane {
                    tab_id: TabId("t".into()),
                    pane_id: None,
                },
                AgentAction::UnzoomPane,
            ),
            (
                WorkspaceMutation::SplitPane {
                    tab_id: TabId("t".into()),
                    target: PaneId("p".into()),
                    session_id: SessionId::new("s"),
                    edge: DockEdge::Right,
                },
                AgentAction::SplitPane,
            ),
            (
                WorkspaceMutation::FocusPane {
                    tab_id: TabId("t".into()),
                    pane_id: PaneId("p".into()),
                },
                AgentAction::FocusPane,
            ),
            (
                WorkspaceMutation::EqualizeTab {
                    tab_id: TabId("t".into()),
                },
                AgentAction::EqualizePanes,
            ),
            (
                WorkspaceMutation::RemovePane {
                    tab_id: TabId("t".into()),
                    pane_id: PaneId("p".into()),
                },
                AgentAction::ClosePane,
            ),
        ];
        for (mutation, expected) in cases {
            let params = stamped(mutation);
            assert_eq!(
                announcement(Method::WORKSPACE_MUTATE, Some(&params)).map(|event| event.action),
                Some(expected)
            );
        }
    }

    #[test]
    fn session_requests_map_to_plain_actions() {
        let actor = |extra: Value| {
            let mut params = extra;
            params[ACTOR_FIELD] = json!("agent-1");
            params
        };
        let action = |method: &str, params: Value| {
            announcement(method, Some(&actor(params))).map(|event| event.action)
        };
        assert_eq!(
            action(Method::SESSION_SPAWN, json!({"kind": "claude", "cwd": "/"})),
            Some(AgentAction::NewTab)
        );
        assert_eq!(
            action(Method::SESSION_SPAWN, json!({"kind": "note", "cwd": "/"})),
            Some(AgentAction::NewNote)
        );
        let tracked = json!({"senderID": "agent-2", "operationID": "o", "spawn": {"kind": "codex", "cwd": "/"}});
        assert_eq!(
            announcement(Method::SESSION_SPAWN_TRACKED, Some(&tracked)),
            Some(AgentActionEvent {
                session_id: SessionId::new("agent-2"),
                action: AgentAction::NewTab,
            })
        );
        // Its strict params must never be stamped.
        assert!(!announces(Method::SESSION_SPAWN_TRACKED));
        assert_eq!(
            action(Method::SESSION_REVEAL, json!({"sessionID": "s"})),
            Some(AgentAction::ShowTab)
        );
        assert_eq!(
            action(Method::SESSION_KILL, json!({"sessionID": "s"})),
            Some(AgentAction::StopAgent)
        );
        assert_eq!(action(Method::SESSION_LIST, json!({})), None);
        assert!(!announces(Method::SESSION_LIST));
        assert!(!announces(Method::SESSION_READ_SCREEN));
        assert!(announces(Method::WORKSPACE_MUTATE));
    }

    #[test]
    fn the_event_wire_shape_is_stable() {
        let event = AgentActionEvent {
            session_id: SessionId::new("a"),
            action: AgentAction::EqualizePanes,
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({"sessionID": "a", "action": "equalize_panes"})
        );
    }
}
