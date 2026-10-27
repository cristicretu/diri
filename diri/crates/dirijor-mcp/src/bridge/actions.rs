//! `list_actions` / `perform_action`: the Diri app's window actions (the
//! commands its menus, keys and command palette run) through two tools,
//! instead of one tool per action. The catalog comes from the connected app
//! at call time, so it always matches the running build.
//!
//! Gates: every perform needs a live calling session, like any MCP write.
//! A destructive action (close, archive, quit) needs a `session_id` the
//! caller may release under the same policy as `release_agent`; one that
//! cannot be aimed at a session is reserved for the person.

use std::time::Duration;

use diri_proto::Method;
use diri_proto::app_actions::{ActionRisk, AppActionSpec, AppActionsListResult};
use serde_json::{Value, json};

use super::{Bridge, DEFAULT_TIMEOUT, McpPolicy, WriteAction};

/// Longer than the Engine's own default wait for the app, so the caller
/// receives the Engine's structured timeout rather than a client one.
const PERFORM_TIMEOUT: Duration =
    Duration::from_millis(diri_proto::app_actions::DEFAULT_TIMEOUT_MS + 5_000);

impl Bridge {
    fn app_actions(&self) -> Result<AppActionsListResult, String> {
        self.request_typed(Method::APP_ACTIONS_LIST, json!({}), DEFAULT_TIMEOUT)
    }

    pub(super) fn list_actions(&self, arguments: &Value) -> Result<Value, String> {
        let catalog = self.app_actions()?;
        if !catalog.connected {
            return Ok(json!({
                "connected": false,
                "actions": [],
                "note": "No Diri window is connected, so no app actions can run now."
            }));
        }
        let query = arguments
            .get("query")
            .and_then(Value::as_str)
            .map(str::to_lowercase);
        // An exact name returns that action with its full schema, long
        // value lists included; a listing abbreviates them.
        if let Some(action) = query
            .as_deref()
            .and_then(|query| catalog.actions.iter().find(|action| action.name == query))
        {
            return Ok(json!({"connected": true, "actions": [compact(action, false)]}));
        }
        let actions: Vec<Value> = catalog
            .actions
            .iter()
            .filter(|action| {
                query.as_deref().is_none_or(|query| {
                    action.name.contains(query)
                        || action.title.to_lowercase().contains(query)
                        || action.category.to_lowercase().contains(query)
                })
            })
            .map(|action| compact(action, true))
            .collect();
        Ok(json!({"connected": true, "actions": actions}))
    }

    pub(super) fn perform_action(&self, arguments: &Value) -> Result<Value, String> {
        let name = super::required_string(arguments, "action")?;
        let args = arguments.get("args").cloned().unwrap_or_else(|| json!({}));
        let snapshot = self.snapshot()?;
        let policy = McpPolicy::new(
            &snapshot.sessions,
            &snapshot.projects,
            self.caller.as_deref(),
        )?;
        let catalog = self.app_actions()?;
        if !catalog.connected {
            return Err(
                "app_not_connected: no Diri window is connected, so app actions cannot run now"
                    .into(),
            );
        }
        let spec = catalog
            .actions
            .iter()
            .find(|action| action.name == name)
            .ok_or_else(|| {
                format!("unknown_action: the app has no action {name:?}; call list_actions")
            })?;
        authorize(&policy, spec, &args)?;
        let result = self.request(
            Method::APP_PERFORM,
            json!({"action": name, "args": args}),
            PERFORM_TIMEOUT,
        )?;
        Ok(
            json!({"ok": true, "action": name, "result": result.get("result").cloned().unwrap_or(Value::Null)}),
        )
    }
}

fn authorize(policy: &McpPolicy<'_>, spec: &AppActionSpec, args: &Value) -> Result<(), String> {
    if spec.risk != ActionRisk::Destructive {
        policy.authorize(WriteAction::AppAction)?;
        return Ok(());
    }
    if !spec.takes_session() {
        return Err(format!(
            "{} closes or removes something for the whole window, so it is reserved for the person; ask them to do it",
            spec.name
        ));
    }
    let target = args
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|target| !target.is_empty())
        .ok_or_else(|| {
            format!(
                "{} is destructive: pass args.session_id, a session you may manage (the same rule as release_agent)",
                spec.name
            )
        })?;
    policy.authorize(WriteAction::Release { target })?;
    Ok(())
}

/// Value lists longer than this are abbreviated in a listing.
const LISTED_ENUM: usize = 12;

/// What an Agent needs to pick and call an action, and nothing else.
fn compact(action: &AppActionSpec, abbreviate: bool) -> Value {
    let mut entry = json!({"name": action.name, "title": action.title});
    if !action.description.is_empty() && action.description != action.title {
        entry["description"] = json!(action.description);
    }
    if action
        .args
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| !properties.is_empty())
    {
        let mut args = action.args.clone();
        if abbreviate
            && let Some(properties) = args.get_mut("properties").and_then(Value::as_object_mut)
        {
            for property in properties.values_mut() {
                let Some(values) = property.get("enum").and_then(Value::as_array).cloned() else {
                    continue;
                };
                if values.len() > LISTED_ENUM {
                    let object = property.as_object_mut().expect("a property schema");
                    object.remove("enum");
                    object.insert("examples".into(), json!(values[..LISTED_ENUM / 2]));
                    object.insert(
                        "description".into(),
                        json!(format!(
                            "one of {} values; list_actions {{\"query\":\"{}\"}} lists them all",
                            values.len(),
                            action.name
                        )),
                    );
                }
            }
        }
        entry["args"] = args;
    }
    if action.risk == ActionRisk::Destructive {
        entry["destructive"] = json!(true);
    }
    entry
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use diri_proto::{Method, SessionStatus};
    use serde_json::{Value, json};

    use super::super::audit_tests::Peer;

    fn catalog(connected: bool) -> Value {
        json!({
            "connected": connected,
            "actions": [
                {"name": "toggle-sidebar", "title": "Toggle Sidebar", "category": "Workspace", "args": {"type":"object","properties":{},"additionalProperties":false}},
                {"name": "close-session", "title": "Close Tab", "risk": "destructive", "args": {"type":"object","properties":{"session_id":{"type":"string","minLength":1}},"additionalProperties":false}},
                {"name": "quit", "title": "Quit Diri", "risk": "destructive", "args": {"type":"object","properties":{},"additionalProperties":false}},
                {"name": "set-theme", "title": "Set Color Theme", "args": {"type":"object","properties":{"theme":{"type":"string","enum": (0..40).map(|n| format!("theme-{n}")).collect::<Vec<_>>()}},"required":["theme"],"additionalProperties":false}}
            ]
        })
    }

    /// A fake Engine: the caller "parent" with child "kid" and an unrelated
    /// root "other" in another project. Records every method it answers.
    fn engine(connected: bool) -> (Peer, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&calls);
        let peer = Peer::new(move |method| {
            seen.lock().unwrap().push(method.to_owned());
            match method {
                Method::SESSION_LIST => {
                    let parent = super::super::tests::record("parent", None);
                    let mut kid = super::super::tests::record("kid", Some("parent"));
                    kid.status = SessionStatus::Working;
                    let mut other = super::super::tests::record("other", None);
                    other.project_id = diri_proto::ProjectId::new("elsewhere");
                    json!({"sessions": [parent, kid, other], "projects": []})
                }
                Method::APP_ACTIONS_LIST => catalog(connected),
                Method::APP_PERFORM => json!({"action": "x", "result": {"done": true}}),
                other => panic!("unexpected {other}"),
            }
        });
        (peer, calls)
    }

    fn performed(calls: &Arc<Mutex<Vec<String>>>) -> bool {
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == Method::APP_PERFORM)
    }

    #[test]
    fn listing_is_compact_and_filters_by_query() {
        let (peer, _) = engine(true);
        let listed = peer
            .bridge()
            .call("list_actions", &json!({"query": "side"}))
            .unwrap();
        assert_eq!(
            listed,
            json!({"connected": true, "actions": [{"name": "toggle-sidebar", "title": "Toggle Sidebar"}]})
        );
        let all = peer.bridge().call("list_actions", &json!({})).unwrap();
        assert_eq!(all["actions"][1]["destructive"], true);
        assert!(all["actions"][1]["args"]["properties"]["session_id"].is_object());
        assert!(
            all["actions"][0].get("args").is_none(),
            "no-arg actions carry no schema"
        );
        // A long value list is abbreviated in a listing, whole by exact name.
        let theme = &all["actions"][3]["args"]["properties"]["theme"];
        assert!(theme.get("enum").is_none(), "{theme}");
        assert_eq!(theme["examples"].as_array().unwrap().len(), 6);
        let exact = peer
            .bridge()
            .call("list_actions", &json!({"query": "set-theme"}))
            .unwrap();
        let values = &exact["actions"][0]["args"]["properties"]["theme"]["enum"];
        assert_eq!(values.as_array().unwrap().len(), 40);
    }

    #[test]
    fn a_safe_action_runs_for_a_live_agent() {
        let (peer, calls) = engine(true);
        let result = peer
            .bridge()
            .call("perform_action", &json!({"action": "toggle-sidebar"}))
            .unwrap();
        assert_eq!(result["result"], json!({"done": true}));
        assert!(performed(&calls));
    }

    #[test]
    fn destructive_actions_need_a_session_the_agent_may_release() {
        let (peer, calls) = engine(true);
        let quit = peer
            .bridge()
            .call("perform_action", &json!({"action": "quit"}))
            .unwrap_err();
        assert!(quit.contains("reserved for the person"), "{quit}");
        let unaimed = peer
            .bridge()
            .call("perform_action", &json!({"action": "close-session"}))
            .unwrap_err();
        assert!(unaimed.contains("args.session_id"), "{unaimed}");
        let foreign = peer
            .bridge()
            .call(
                "perform_action",
                &json!({"action": "close-session", "args": {"session_id": "other"}}),
            )
            .unwrap_err();
        assert!(foreign.contains("outside its direct children"), "{foreign}");
        assert!(!performed(&calls), "no refused action reached the app");

        peer.bridge()
            .call(
                "perform_action",
                &json!({"action": "close-session", "args": {"session_id": "kid"}}),
            )
            .unwrap();
        assert!(performed(&calls));
    }

    #[test]
    fn no_app_or_no_caller_fails_closed() {
        let (peer, calls) = engine(false);
        let listed = peer.bridge().call("list_actions", &json!({})).unwrap();
        assert_eq!(listed["connected"], false);
        let offline = peer
            .bridge()
            .call("perform_action", &json!({"action": "toggle-sidebar"}))
            .unwrap_err();
        assert!(offline.starts_with("app_not_connected"), "{offline}");
        let anonymous = super::Bridge::new(peer.path.clone(), None)
            .call("perform_action", &json!({"action": "toggle-sidebar"}))
            .unwrap_err();
        assert!(anonymous.contains("DIRIJOR_SESSION_ID"), "{anonymous}");
        assert!(!performed(&calls));
    }
}
