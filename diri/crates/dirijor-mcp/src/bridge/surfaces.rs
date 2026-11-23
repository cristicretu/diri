//! `surface`: Live Surfaces for agents, one action with an `op`.
//!
//! Surfaces are files in Diri's surfaces directory (`diri-surfaces`); the app
//! watches it and redraws, so nothing here needs the Engine except checking
//! which sessions a surface may prompt. One action instead of one tool per
//! verb keeps the catalog small; the component reference is served on demand
//! (`op: "schema"`), never in a prompt.

use diri_proto::surface::patch::Patch;
use diri_proto::surface::{Node, Scope, SurfaceDoc, template};
use diri_surfaces::{Author, StoreError, SurfaceStore};
use serde_json::{Value, json};

use super::{Bridge, McpPolicy, WriteAction, optional_string};

const DEFAULT_EVENTS: usize = 50;
const MAX_EVENTS: usize = 200;

/// The reference `op: "schema"` returns.
pub const GUIDE: &str = r#"# Diri Live Surfaces

A surface is a native dashboard Diri draws from JSON: no HTML, no server. Saved
surfaces (scope global/project) are sidebar rows; a session surface shows in that
session's right panel. The person's ticks, toggles and choices persist across your
updates as long as component ids stay the same.

## Ops
- create {title, scope: session|global|project, project?, session?, template?: today|blank, root?, pinned?, agents?}
  scope defaults to your session. Returns {surface, revision}.
- update {surface, ops?: [patch ops], root?, title?, subtitle?, pinned?, agents?, expected_revision?}
  Prefer ops for live changes: nothing flickers, state is kept.
- get {surface, format?: json|outline}   list {scope?: saved|session|all}
- events {surface, after?: seq}: what the person ticked/clicked since you last looked.
- history {surface, revision?}   restore {surface, revision}   promote {surface, scope?: global|project}
- delete {surface}

## Patch ops (applied all-or-nothing)
{"op":"set","id":"m1","fields":{"value":"12","delta":"+3"}}   change some fields
{"op":"replace","id":"x","node":{...}}   {"op":"remove","id":"x"}
{"op":"insert","parent":"card-id|tab-id","index":0,"node":{...}}  (no parent = top level)
{"op":"title","title":"..."}   {"op":"root","root":[...]}

## Components ("type"; "id" required for interactive ones, unique per surface)
Layout: stack{children,gap} row{children,gap,wrap} grid{columns 1-6,children}
  card{title,subtitle,tone,children} section{title,detail,children} divider
  tabs{tabs:[{id,label,children}]}
Content: heading{text,level 1-3} text{text,tone,mono} markdown{text} badge{label,tone}
  metric{label,value,delta,trend:up|down|flat,caption} progress{value,max,label,tone}
  table{columns:[..],rows:[[cell|{text,link,tone}]]} kv_list{items:[{key,value,link,tone}]}
  mermaid{source} image{src: https URL or assets/<file>, alt, height}
Interactive: checklist{id,title,items:[{id,text,checked,link,detail}],notify}
  link{url,label,detail}: Slack, GitHub PR/issue, Linear, Notion and diri://session/<id>,
    diri://note/<id> links get badges automatically.
  button{id,label,style:primary|secondary|danger,action}
  toggle{id,label,default,notify} select{id,label,options:[{value,label}],default,notify}
  session{session: id}: a live row with the session's status.
Tones: neutral muted info success warning danger accent.
Live bindings (no agent needed to stay current): add "bind" to text/badge/metric:
  {"source":"session_status","session":ID} session_title working_count needs_input_count
  unread_count {"source":"note_todos","note":ID} {"source":"pr_checks","session":ID}

## Button actions (run only when the person clicks)
{"kind":"open_url","url":...} {"kind":"open_session","session":ID} {"kind":"open_note","note":ID}
{"kind":"send_prompt","text":...,"session"?: ID} {"kind":"run_app_skill","name":...}
{"kind":"event","name":"triage","payload"?:{}} {"kind":"refresh"}
Prompts, app skills and events go to the surface's owner (its creator) or a session in
"agents". With notify:true, ticks and choices are delivered to the owner as a
"[Diri Live Surface event]" message; read them any time with op events.

Limits: 12 levels, 1500 components, 200 children, 16 KiB per text, 1 MiB per surface.
Links: https http diri slack linear notion figma mailto only.
"#;

impl Bridge {
    fn surface_store(&self) -> Result<SurfaceStore, String> {
        let dir = match &self.notes_dir {
            // Tests point notes somewhere private; surfaces sit beside them.
            Some(notes) => notes
                .parent()
                .map(|parent| parent.join("surfaces"))
                .ok_or("invalid notes directory")?,
            None => {
                SurfaceStore::resolve_dir().ok_or("cannot find a home directory for surfaces")?
            }
        };
        SurfaceStore::open(dir).map_err(|e| format!("cannot open surfaces: {e}"))
    }

    fn surface_author(&self) -> Author {
        self.caller.clone().map_or(Author::Tool, Author::Agent)
    }

    /// Sessions a surface may prompt must be ones the caller may prompt
    /// itself; the person (no caller) may name any.
    fn check_surface_reach(&self, sessions: &[String]) -> Result<(), String> {
        let Some(caller) = self.caller.as_deref() else {
            return Ok(());
        };
        let others: Vec<&String> = sessions.iter().filter(|s| s.as_str() != caller).collect();
        if others.is_empty() {
            return Ok(());
        }
        let snapshot = self.snapshot()?;
        let policy = McpPolicy::new(&snapshot.sessions, &snapshot.projects, Some(caller))?;
        for session in others {
            policy
                .authorize(WriteAction::SendPrompt { target: session })
                .map_err(|error| format!("agents: {error}"))?;
        }
        Ok(())
    }

    pub(super) fn surface(&self, args: &Value) -> Result<Value, String> {
        let op = optional_string(args, "op").unwrap_or_else(|| "list".into());
        match op.as_str() {
            "schema" | "help" => Ok(json!({"guide": GUIDE})),
            "list" => self.surface_list(args),
            "get" => self.surface_get(args),
            "create" => self.surface_create(args),
            "update" | "patch" => self.surface_update(args),
            "events" => self.surface_events(args),
            "history" => self.surface_history(args),
            "restore" => self.surface_restore(args),
            "promote" | "save" => self.surface_promote(args),
            "delete" => self.surface_delete(args),
            other => Err(format!(
                "unknown op {other:?}; use schema, list, get, create, update, events, history, restore, promote or delete"
            )),
        }
    }

    fn surface_id(&self, store: &SurfaceStore, args: &Value) -> Result<String, String> {
        let query = optional_string(args, "surface")
            .or_else(|| optional_string(args, "id"))
            .ok_or("missing required argument: surface")?;
        store.resolve(&query).map_err(render)
    }

    fn surface_list(&self, args: &Value) -> Result<Value, String> {
        let store = self.surface_store()?;
        let filter = optional_string(args, "scope").unwrap_or_else(|| "all".into());
        let mine = self.caller.as_deref();
        let surfaces: Vec<Value> = store
            .list()
            .map_err(|e| format!("cannot list surfaces: {e}"))?
            .into_iter()
            .filter(|meta| match filter.as_str() {
                "saved" => meta.scope.is_saved(),
                "session" => !meta.scope.is_saved(),
                "mine" => meta.owner.as_deref() == mine || meta.scope.session() == mine,
                _ => true,
            })
            .map(|meta| {
                let mut value = serde_json::to_value(&meta).unwrap_or(Value::Null);
                value["url"] = json!(format!("diri://surface/{}", meta.id));
                value
            })
            .collect();
        Ok(json!({"surfaces": surfaces, "directory": store.dir()}))
    }

    fn surface_get(&self, args: &Value) -> Result<Value, String> {
        let store = self.surface_store()?;
        let id = self.surface_id(&store, args)?;
        let loaded = store.load(&id).map_err(render)?;
        let latest_event = store
            .events(&id, 0, usize::MAX)
            .ok()
            .and_then(|events| events.last().map(|event| event.seq))
            .unwrap_or(0);
        if optional_string(args, "format").as_deref() == Some("outline") {
            return Ok(json!({
                "surface": id,
                "revision": loaded.doc.revision,
                "outline": loaded.doc.outline(),
                "latest_event": latest_event,
            }));
        }
        Ok(json!({
            "surface": loaded.doc,
            "read_only": loaded.read_only,
            "latest_event": latest_event,
        }))
    }

    fn surface_create(&self, args: &Value) -> Result<Value, String> {
        let store = self.surface_store()?;
        let scope = scope_from(args, self.caller.as_deref())?;
        let title = optional_string(args, "title");
        let sessions: Vec<String> = string_list(args, "sessions")?;
        let mut doc = match optional_string(args, "template") {
            Some(name) => {
                template::by_name(&name, "", scope.clone(), &sessions).ok_or_else(|| {
                    format!(
                        "unknown template {name:?}; use {}",
                        template::NAMES.join(" or ")
                    )
                })?
            }
            None => SurfaceDoc::new(
                "",
                title.clone().ok_or("missing required argument: title")?,
                scope.clone(),
            ),
        };
        if let Some(title) = title {
            doc.title = title;
        }
        if let Some(subtitle) = optional_string(args, "subtitle") {
            doc.subtitle = Some(subtitle);
        }
        if let Some(root) = args.get("root").filter(|root| !root.is_null()) {
            doc.root = nodes(root)?;
        }
        doc.pinned = args
            .get("pinned")
            .and_then(Value::as_bool)
            .unwrap_or(doc.pinned || scope.is_saved());
        if !scope.is_saved() {
            doc.pinned = false;
        }
        let agents = string_list(args, "agents")?;
        self.check_surface_reach(&agents)?;
        doc.agents = agents;
        doc.owner = self.caller.clone();
        let doc = store.create(doc, &self.surface_author()).map_err(render)?;
        Ok(json!({
            "surface": doc.id,
            "revision": doc.revision,
            "title": doc.title,
            "scope": doc.scope,
            "url": format!("diri://surface/{}", doc.id),
            "note": if doc.scope.is_saved() {
                "Saved surface: it is a row in Diri's sidebar."
            } else {
                "Session surface: it shows in your session's right panel. The person can save it."
            },
        }))
    }

    fn surface_update(&self, args: &Value) -> Result<Value, String> {
        let store = self.surface_store()?;
        let id = self.surface_id(&store, args)?;
        let author = self.surface_author();
        let expected = args.get("expected_revision").and_then(Value::as_u64);
        let mut result = None;
        if let Some(ops) = args.get("ops").filter(|ops| !ops.is_null()) {
            let patch: Patch = serde_json::from_value(json!({
                "ops": ops,
                "expected_revision": expected,
            }))
            .map_err(|e| format!("invalid ops: {e}"))?;
            result = Some(store.patch(&id, &patch, &author).map_err(render)?);
        } else if let Some(root) = args.get("root").filter(|root| !root.is_null()) {
            let title = optional_string(args, "title");
            result = Some(
                store
                    .replace(&id, title.as_deref(), nodes(root)?, expected, &author)
                    .map_err(render)?,
            );
        }
        let pinned = args.get("pinned").and_then(Value::as_bool);
        let subtitle = optional_string(args, "subtitle");
        let title = optional_string(args, "title").filter(|_| result.is_none());
        let agents = match args.get("agents") {
            Some(Value::Null) | None => None,
            Some(_) => Some(string_list(args, "agents")?),
        };
        if let Some(agents) = &agents {
            self.check_surface_reach(agents)?;
        }
        let meta_change =
            pinned.is_some() || subtitle.is_some() || agents.is_some() || title.is_some();
        let doc = if meta_change {
            store
                .update_meta(&id, &author, |doc| {
                    if let Some(pinned) = pinned {
                        doc.pinned = pinned && doc.scope.is_saved();
                    }
                    if let Some(subtitle) = subtitle {
                        doc.subtitle = Some(subtitle).filter(|s| !s.is_empty());
                    }
                    if let Some(title) = title {
                        doc.title = title;
                    }
                    if let Some(agents) = agents {
                        doc.agents = agents;
                    }
                })
                .map_err(render)?
        } else if let Some((doc, _)) = &result {
            doc.clone()
        } else {
            return Err(
                "nothing to update: pass ops, root, title, subtitle, pinned or agents".into(),
            );
        };
        Ok(json!({
            "surface": doc.id,
            "revision": doc.revision,
            "nodes": result.as_ref().map(|(_, applied)| applied.nodes),
            "ops": result.as_ref().map(|(_, applied)| applied.ops),
        }))
    }

    fn surface_events(&self, args: &Value) -> Result<Value, String> {
        let store = self.surface_store()?;
        let id = self.surface_id(&store, args)?;
        let after = args.get("after").and_then(Value::as_u64).unwrap_or(0);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_EVENTS, |l| l as usize)
            .clamp(1, MAX_EVENTS);
        let events = store.events(&id, after, limit).map_err(render)?;
        let last = events.last().map_or(after, |event| event.seq);
        Ok(json!({"surface": id, "events": events, "next_after": last}))
    }

    fn surface_history(&self, args: &Value) -> Result<Value, String> {
        let store = self.surface_store()?;
        let id = self.surface_id(&store, args)?;
        if let Some(revision) = args.get("revision").and_then(Value::as_u64) {
            let doc = store.read_version(&id, revision).map_err(render)?;
            return Ok(json!({"surface": id, "version": doc}));
        }
        let versions = store.history(&id).map_err(render)?;
        Ok(json!({"surface": id, "versions": versions}))
    }

    fn surface_restore(&self, args: &Value) -> Result<Value, String> {
        let store = self.surface_store()?;
        let id = self.surface_id(&store, args)?;
        let revision = args
            .get("revision")
            .and_then(Value::as_u64)
            .ok_or("missing required argument: revision")?;
        let doc = store
            .restore(&id, revision, &self.surface_author())
            .map_err(render)?;
        Ok(json!({"surface": id, "revision": doc.revision, "restored_from": revision}))
    }

    fn surface_promote(&self, args: &Value) -> Result<Value, String> {
        let store = self.surface_store()?;
        let id = self.surface_id(&store, args)?;
        let scope = match optional_string(args, "scope").as_deref() {
            None | Some("global") => Scope::Global,
            Some("project") => Scope::Project {
                project: optional_string(args, "project").ok_or("scope project needs project")?,
            },
            Some(other) => return Err(format!("promote to global or project, not {other:?}")),
        };
        let doc = store
            .promote(&id, scope, &self.surface_author())
            .map_err(render)?;
        Ok(json!({
            "surface": doc.id,
            "from": id,
            "revision": doc.revision,
            "url": format!("diri://surface/{}", doc.id),
        }))
    }

    fn surface_delete(&self, args: &Value) -> Result<Value, String> {
        let store = self.surface_store()?;
        let id = self.surface_id(&store, args)?;
        let doc = store.load(&id).map_err(render)?.doc;
        if let Some(caller) = self.caller.as_deref() {
            let mine = doc.owner.as_deref().is_none_or(|owner| owner == caller)
                || doc.scope.session() == Some(caller);
            if !mine {
                return Err(format!(
                    "surface {id} belongs to session {}; only it or the person can delete it",
                    doc.owner.unwrap_or_default()
                ));
            }
        }
        let trashed = store.delete(&id).map_err(render)?;
        Ok(json!({"deleted": id, "trash": trashed}))
    }
}

fn render(error: StoreError) -> String {
    error.to_string()
}

fn nodes(value: &Value) -> Result<Vec<Node>, String> {
    serde_json::from_value(value.clone()).map_err(|e| format!("invalid root: {e}"))
}

fn string_list(args: &Value, key: &str) -> Result<Vec<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{key} must be a list of session ids"))
            })
            .collect(),
        Some(_) => Err(format!("{key} must be a list of session ids")),
    }
}

fn scope_from(args: &Value, caller: Option<&str>) -> Result<Scope, String> {
    let kind = optional_string(args, "scope");
    let session = optional_string(args, "session");
    match kind.as_deref() {
        Some("global") => Ok(Scope::Global),
        Some("project") => Ok(Scope::Project {
            project: optional_string(args, "project")
                .or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .map(|d| d.display().to_string())
                })
                .ok_or("scope project needs project")?,
        }),
        Some("session") | None => match session.or_else(|| caller.map(str::to_owned)) {
            Some(session) => Ok(Scope::Session { session }),
            None if kind.is_none() => Ok(Scope::Global),
            None => Err("scope session needs session (or run inside a Diri session)".into()),
        },
        Some(other) => Err(format!(
            "scope is session, global or project, not {other:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn bridge(dir: &std::path::Path, caller: Option<&str>) -> Bridge {
        Bridge::new(PathBuf::from("/nonexistent"), caller.map(str::to_owned))
            .with_notes_dir(dir.join("notes"))
    }

    #[test]
    fn agent_roundtrip_create_patch_tick_events() {
        let dir = tempfile::tempdir().unwrap();
        let agent = bridge(dir.path(), Some("s-agent"));
        let created = agent
            .call(
                "surface",
                &json!({"op": "create", "template": "today", "scope": "global"}),
            )
            .unwrap();
        let id = created["surface"].as_str().unwrap().to_owned();
        assert_eq!(created["revision"], 1);

        let updated = agent
            .call(
                "surface",
                &json!({"op": "update", "surface": id, "expected_revision": 1,
                        "ops": [{"op": "set", "id": "m-shipped", "fields": {"value": "11"}}]}),
            )
            .unwrap();
        assert_eq!(updated["revision"], 2);

        // The person ticks in the app (same store path the app uses).
        let store = agent.surface_store().unwrap();
        store
            .interact(
                &id,
                &diri_proto::surface::Interaction::Check {
                    checklist: "today".into(),
                    item: "spec".into(),
                    checked: true,
                },
            )
            .unwrap();
        let events = agent
            .call("surface", &json!({"op": "events", "surface": id}))
            .unwrap();
        assert_eq!(events["events"][0]["kind"], "check");
        assert_eq!(events["next_after"], 1);

        let outline = agent
            .call(
                "surface",
                &json!({"op": "get", "surface": "today", "format": "outline"}),
            )
            .unwrap();
        assert!(
            outline["outline"]
                .as_str()
                .unwrap()
                .contains("[x] Read the Q4 onboarding spec")
        );

        let stale = agent
            .call(
                "surface",
                &json!({"op": "update", "surface": id, "expected_revision": 1,
                        "ops": [{"op": "remove", "id": "summary"}]}),
            )
            .unwrap_err();
        assert!(stale.starts_with("revision_conflict"), "{stale}");
    }

    #[test]
    fn session_scope_is_the_default_and_promotes() {
        let dir = tempfile::tempdir().unwrap();
        let agent = bridge(dir.path(), Some("s-agent"));
        let created = agent
            .call(
                "surface",
                &json!({"op": "create", "title": "Debug", "root": [
                    {"type": "checklist", "id": "steps", "items": [{"id": "a", "text": "Repro"}]}
                ]}),
            )
            .unwrap();
        assert_eq!(created["scope"]["kind"], "session");
        assert_eq!(created["scope"]["session"], "s-agent");
        let id = created["surface"].as_str().unwrap();
        let promoted = agent
            .call("surface", &json!({"op": "promote", "surface": id}))
            .unwrap();
        assert_ne!(promoted["surface"], created["surface"]);
        let saved = agent
            .call("surface", &json!({"op": "list", "scope": "saved"}))
            .unwrap();
        assert_eq!(saved["surfaces"].as_array().unwrap().len(), 1);
        assert_eq!(saved["surfaces"][0]["pinned"], true);
    }

    #[test]
    fn bad_surfaces_are_refused_with_codes() {
        let dir = tempfile::tempdir().unwrap();
        let agent = bridge(dir.path(), Some("s-agent"));
        let error = agent
            .call("surface", &json!({"op": "create", "title": "X", "scope": "global", "root": [
                {"type": "button", "id": "b", "label": "pwn", "action": {"kind": "open_url", "url": "file:///etc/passwd"}}
            ]}))
            .unwrap_err();
        assert!(error.starts_with("invalid_action"), "{error}");
        let mut deep = json!({"type": "divider"});
        for _ in 0..13 {
            deep = json!({"type": "stack", "children": [deep]});
        }
        let error = agent
            .call(
                "surface",
                &json!({"op": "create", "title": "X", "root": [deep]}),
            )
            .unwrap_err();
        assert!(error.starts_with("too_deep"), "{error}");
    }

    #[test]
    fn prompting_other_sessions_needs_the_engine_to_vouch() {
        let dir = tempfile::tempdir().unwrap();
        let agent = bridge(dir.path(), Some("s-agent"));
        // No Engine at /nonexistent: an agents list naming someone else is
        // refused rather than trusted.
        let error = agent
            .call(
                "surface",
                &json!({"op": "create", "title": "X", "scope": "global", "agents": ["s-other"]}),
            )
            .unwrap_err();
        assert!(!error.is_empty());
        // The person (no caller) may name any session.
        let person = bridge(dir.path(), None);
        person
            .call(
                "surface",
                &json!({"op": "create", "title": "X", "scope": "global", "agents": ["s-other"]}),
            )
            .unwrap();
    }

    #[test]
    fn only_the_owner_or_the_person_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let owner = bridge(dir.path(), Some("s-owner"));
        let created = owner
            .call(
                "surface",
                &json!({"op": "create", "title": "Mine", "scope": "global"}),
            )
            .unwrap();
        let id = created["surface"].as_str().unwrap();
        let other = bridge(dir.path(), Some("s-other"));
        let error = other
            .call("surface", &json!({"op": "delete", "surface": id}))
            .unwrap_err();
        assert!(error.contains("belongs to session s-owner"), "{error}");
        // Any agent may still update it: an 8am schedule refreshes it.
        other
            .call(
                "surface",
                &json!({"op": "update", "surface": id, "subtitle": "Refreshed"}),
            )
            .unwrap();
        owner
            .call("surface", &json!({"op": "delete", "surface": id}))
            .unwrap();
    }
}
