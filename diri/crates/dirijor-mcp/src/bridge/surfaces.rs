//! `surface`: Live Surfaces for agents, one action with an `op`.
//!
//! Surfaces are files in Diri's surfaces directory (`diri-surfaces`); the app
//! watches it and redraws, so nothing here needs the Engine except checking
//! which sessions a surface may prompt. One action instead of one tool per
//! verb keeps the catalog small; the component reference is served on demand
//! (`op: "schema"`), never in a prompt.

use diri_proto::surface::patch::Patch;
use diri_proto::surface::{AppSpec, Icon, Node, Scope, Style, SurfaceDoc, identity};
use diri_surfaces::{Author, StoreError, SurfaceStore};
use serde_json::{Value, json};

use super::{Bridge, McpPolicy, WriteAction, optional_string};

const DEFAULT_EVENTS: usize = 50;
const MAX_EVENTS: usize = 200;

/// The reference `op: "schema"` returns.
pub const GUIDE: &str = include_str!("surfaces_guide.md");

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
            "write_file" => self.surface_write_file(args),
            "read_file" => self.surface_read_file(args),
            "files" => self.surface_files(args),
            "data" => self.surface_data(args),
            "check" => self.surface_check(args),
            "delete" => self.surface_delete(args),
            other => Err(format!(
                "unknown op {other:?}; use schema, create, update, write_file, read_file, files, data, check, get, list, events, history, restore, promote or delete"
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
        let title = optional_string(args, "title").ok_or("missing required argument: title")?;
        let mut doc = SurfaceDoc::new("", title, scope.clone());
        apply_identity(&mut doc, args)?;
        let lua = optional_string(args, "lua");
        if lua.is_some() && doc.app.is_none() {
            doc.app = Some(AppSpec::default());
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
        if let Some(data) = args.get("data").filter(|data| !data.is_null()) {
            store.write_data(&doc.id, data).map_err(render)?;
        }
        let check = match &lua {
            Some(lua) => {
                store
                    .write_file(&doc.id, identity::APP_ENTRY, lua.as_bytes())
                    .map_err(render)?;
                Some(check_app(&store, &doc.id))
            }
            None => None,
        };
        Ok(json!({
            "surface": doc.id,
            "revision": doc.revision,
            "title": doc.title,
            "scope": doc.scope,
            "url": format!("diri://surface/{}", doc.id),
            "check": check,
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
        let identity_change = ["icon", "accent", "style", "app"]
            .iter()
            .any(|key| args.get(*key).is_some());
        let mut identity_doc = SurfaceDoc::new("x", "x", Scope::Global);
        apply_identity(&mut identity_doc, args)?;
        let lua = optional_string(args, "lua");
        if let Some(data) = args.get("data").filter(|data| !data.is_null()) {
            store.write_data(&id, data).map_err(render)?;
        }
        let check = match &lua {
            Some(lua) => {
                store
                    .write_file(&id, identity::APP_ENTRY, lua.as_bytes())
                    .map_err(render)?;
                Some(check_app(&store, &id))
            }
            None => None,
        };
        let meta_change = pinned.is_some()
            || subtitle.is_some()
            || agents.is_some()
            || title.is_some()
            || identity_change
            || lua.is_some();
        let doc = if meta_change {
            store
                .update_meta(&id, &author, |doc| {
                    if args.get("icon").is_some() {
                        doc.icon = identity_doc.icon.clone();
                    }
                    if args.get("accent").is_some() {
                        doc.accent = identity_doc.accent.clone();
                    }
                    if args.get("style").is_some() {
                        doc.style = identity_doc.style;
                    }
                    if args.get("app").is_some() {
                        doc.app = identity_doc.app.clone();
                    }
                    if lua.is_some() && doc.app.is_none() {
                        doc.app = Some(AppSpec::default());
                    }
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
        } else if args.get("data").is_some() {
            store.load(&id).map_err(render)?.doc
        } else {
            return Err(
                "nothing to update: pass lua, data, ops, root, title, subtitle, icon, accent, style, app, pinned or agents".into(),
            );
        };
        Ok(json!({
            "surface": doc.id,
            "revision": doc.revision,
            "nodes": result.as_ref().map(|(_, applied)| applied.nodes),
            "ops": result.as_ref().map(|(_, applied)| applied.ops),
            "check": check,
        }))
    }

    fn surface_write_file(&self, args: &Value) -> Result<Value, String> {
        let store = self.surface_store()?;
        let id = self.surface_id(&store, args)?;
        let path = optional_string(args, "path").ok_or("missing required argument: path")?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or("missing required argument: content (text)")?;
        store
            .write_file(&id, &path, content.as_bytes())
            .map_err(render)?;
        let check =
            (path == identity::APP_ENTRY || path.ends_with(".lua")).then(|| check_app(&store, &id));
        Ok(json!({"surface": id, "path": path, "bytes": content.len(), "check": check}))
    }

    fn surface_read_file(&self, args: &Value) -> Result<Value, String> {
        let store = self.surface_store()?;
        let id = self.surface_id(&store, args)?;
        let path = optional_string(args, "path").unwrap_or_else(|| identity::APP_ENTRY.into());
        let bytes = store.read_file(&id, &path).map_err(render)?;
        Ok(json!({"surface": id, "path": path, "content": String::from_utf8_lossy(&bytes)}))
    }

    fn surface_files(&self, args: &Value) -> Result<Value, String> {
        let store = self.surface_store()?;
        let id = self.surface_id(&store, args)?;
        Ok(json!({"surface": id, "files": store.list_files(&id).map_err(render)?}))
    }

    fn surface_data(&self, args: &Value) -> Result<Value, String> {
        let store = self.surface_store()?;
        let id = self.surface_id(&store, args)?;
        match args.get("data") {
            Some(data) if !data.is_null() => {
                store.write_data(&id, data).map_err(render)?;
                Ok(json!({"surface": id, "written": true}))
            }
            _ => Ok(json!({"surface": id, "data": store.read_data(&id).map_err(render)?})),
        }
    }

    fn surface_check(&self, args: &Value) -> Result<Value, String> {
        let store = self.surface_store()?;
        let id = self.surface_id(&store, args)?;
        Ok(json!({"surface": id, "check": check_app(&store, &id)}))
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

/// Sets icon, accent, style and app from the arguments that carry them.
fn apply_identity(doc: &mut SurfaceDoc, args: &Value) -> Result<(), String> {
    if let Some(icon) = args.get("icon") {
        doc.icon = match icon {
            Value::Null => None,
            Value::String(text) => Some(icon_shorthand(text)),
            other => Some(
                serde_json::from_value::<Icon>(other.clone())
                    .map_err(|e| format!("invalid icon: {e}"))?,
            ),
        };
    }
    if let Some(accent) = args.get("accent") {
        doc.accent = accent.as_str().map(str::to_owned).filter(|a| !a.is_empty());
    }
    if let Some(style) = args.get("style").filter(|style| !style.is_null()) {
        doc.style = serde_json::from_value::<Style>(style.clone())
            .map_err(|e| format!("invalid style: {e}"))?;
    }
    if let Some(app) = args.get("app") {
        doc.app = match app {
            Value::Null => None,
            other => Some(
                serde_json::from_value::<AppSpec>(other.clone())
                    .map_err(|e| format!("invalid app: {e}"))?,
            ),
        };
    }
    Ok(())
}

/// `"🎧"` is an emoji, `"chart.bar"` a built-in icon, `"icon.svg"` an asset.
fn icon_shorthand(text: &str) -> Icon {
    let lower = text.to_ascii_lowercase();
    if identity::IMAGE_EXTENSIONS
        .iter()
        .any(|ext| lower.ends_with(&format!(".{ext}")))
    {
        Icon::Asset(text.to_owned())
    } else if !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.')
    {
        Icon::Symbol(text.to_owned())
    } else {
        Icon::Emoji(text.to_owned())
    }
}

/// Dry-runs the app's `render` with its saved state and data, and checks
/// the result against the surface bounds, so the agent sees a Lua or shape
/// error at once instead of a blank app.
fn check_app(store: &SurfaceStore, id: &str) -> Value {
    let source = match store.app_source(id) {
        Ok(Some(source)) => source,
        Ok(None) => return json!({"ok": false, "error": "no app.lua"}),
        Err(error) => return json!({"ok": false, "error": error.to_string()}),
    };
    let doc = match store.load(id) {
        Ok(loaded) => loaded.doc,
        Err(error) => return json!({"ok": false, "error": error.to_string()}),
    };
    let data = store.read_data(id).unwrap_or(Value::Null);
    let state = doc.state.app.clone().unwrap_or(Value::Null);
    let folder = match store.folder_for(id) {
        Ok(folder) => folder,
        Err(error) => return json!({"ok": false, "error": error.to_string()}),
    };
    let app = diri_script::app::App {
        source: &source,
        dir: &folder,
    };
    let started = std::time::Instant::now();
    let rendered = match diri_script::app::render(
        app,
        &state,
        &data,
        &diri_script::app::plain_context(diri_surfaces::now_ms()),
        diri_script::app::INTERACTIVE,
    ) {
        Ok(rendered) => rendered,
        Err(error) => return json!({"ok": false, "error": error.to_string()}),
    };
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let root: Vec<Node> = match serde_json::from_value(rendered.root) {
        Ok(root) => root,
        Err(error) => return json!({"ok": false, "error": format!("render output: {error}")}),
    };
    let mut probe = doc;
    probe.root = root;
    match probe.validate() {
        Ok(nodes) => {
            json!({"ok": true, "nodes": nodes, "render_ms": (elapsed_ms * 100.0).round() / 100.0, "digest": diri_script::app::digest(&source)})
        }
        Err(error) => json!({"ok": false, "error": error.to_string()}),
    }
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
                &json!({"op": "create", "title": "Today", "scope": "global",
                        "root": serde_json::to_value(&diri_proto::surface::fixtures::whats_for_today("", Scope::Global, &[]).root).unwrap()}),
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
                &json!({"op": "get", "surface": "Today", "format": "outline"}),
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

    #[test]
    fn lua_apps_are_checked_as_they_are_written() {
        let dir = tempfile::tempdir().unwrap();
        let agent = bridge(dir.path(), Some("s-agent"));
        let created = agent
            .call(
                "surface",
                &json!({"op": "create", "title": "Queue", "icon": "🎧", "accent": "#f97316",
                        "data": {"n": 3},
                        "lua": "function render(state, data) return ui.text('items ' .. data.n, {size = 20}) end"}),
            )
            .unwrap();
        assert_eq!(created["check"]["ok"], true, "{created}");
        let id = created["surface"].as_str().unwrap();
        let got = agent
            .call("surface", &json!({"op": "get", "surface": id}))
            .unwrap();
        assert_eq!(got["surface"]["icon"], json!({"emoji": "🎧"}));
        assert_eq!(got["surface"]["app"], json!({}));
        // A broken edit is stored (the agent fixes it next) but reported now.
        let broken = agent
            .call("surface", &json!({"op": "update", "surface": id, "lua": "function render() return nil + 1 end"}))
            .unwrap();
        assert_eq!(broken["check"]["ok"], false);
        assert!(
            broken["check"]["error"]
                .as_str()
                .unwrap()
                .contains("app.lua:1"),
            "{broken}"
        );
        let looping = agent
            .call("surface", &json!({"op": "write_file", "surface": id, "path": "app.lua", "content": "function render() while true do end end"}))
            .unwrap();
        assert!(
            looping["check"]["error"]
                .as_str()
                .unwrap()
                .starts_with("budget"),
            "{looping}"
        );
        let files = agent
            .call("surface", &json!({"op": "files", "surface": id}))
            .unwrap();
        assert_eq!(files["files"][0]["path"], "app.lua");
        let escape = agent
            .call(
                "surface",
                &json!({"op": "write_file", "surface": id, "path": "../x.lua", "content": "x"}),
            )
            .unwrap_err();
        assert!(escape.starts_with("invalid_path"), "{escape}");
        // Agents have no op that approves exec: only the person, in the app.
        assert!(
            agent
                .call("surface", &json!({"op": "approve", "surface": id}))
                .is_err()
        );
        let store = agent.surface_store().unwrap();
        let source = store.app_source(id).unwrap().unwrap();
        assert!(!store.is_approved(id, &diri_script::app::digest(&source)));
    }
}
