//! The profile's JSON documents: preferences, launch recipes, schedules and
//! projects, converted between this Mac's form (absolute paths, path-hashed
//! project ids, account and host bindings) and the portable form (`~/`
//! paths, project roots instead of ids, no machine bindings).

use std::collections::{BTreeMap, HashMap};

use diri_proto::portable::{PORTABLE_PREF_KEYS, SkippedItem};
use diri_proto::schedules::{ScheduleRecord, ScheduleWhen};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::classify::redact_secret_fields;
use crate::pathmap::{PathMap, expand, to_portable};

/// Preference arrays that hold project ids.
const PROJECT_ID_PREFS: [&str; 3] = [
    "sidebarPinnedProjects",
    "sidebarProjectOrder",
    "sidebarCollapsedProjects",
];
/// Portable stand-in for a project id: its root.
const ROOT_KEY_PREFIX: &str = "root:";

/// One project as it travels.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectEntry {
    /// `~/work/repo`.
    pub root: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_origin: Option<String>,
}

/// A local project: its id, absolute root, display name.
#[derive(Clone, Debug)]
pub struct LocalProject {
    pub id: String,
    pub root: String,
    pub name: String,
}

/// `projects.json` from the Engine's local (non-remote) projects.
pub fn export_projects(home: &str, projects: &[LocalProject]) -> Value {
    let mut entries: Vec<ProjectEntry> = projects
        .iter()
        .map(|project| ProjectEntry {
            root: to_portable(home, &project.root),
            name: project.name.clone(),
            git_origin: crate::origin::read_origin(std::path::Path::new(&project.root)),
        })
        .collect();
    entries.sort_by(|a, b| a.root.cmp(&b.root));
    entries.dedup_by(|a, b| a.root == b.root);
    json!({ "format": 1, "projects": entries })
}

pub fn read_projects(doc: &Value) -> Vec<ProjectEntry> {
    doc.get("projects")
        .and_then(|items| serde_json::from_value(items.clone()).ok())
        .unwrap_or_default()
}

/// Splits `prefs.json` into the portable `prefs.json` (allowlisted keys,
/// project ids as roots) and `recipes.json`.
pub fn export_prefs(
    home: &str,
    prefs: &Map<String, Value>,
    projects: &[LocalProject],
) -> (Value, Value, Vec<SkippedItem>) {
    let roots: HashMap<&str, &str> = projects
        .iter()
        .map(|p| (p.id.as_str(), p.root.as_str()))
        .collect();
    let mut out = Map::new();
    for key in PORTABLE_PREF_KEYS {
        if *key == "launchRecipes" {
            continue;
        }
        let Some(value) = prefs.get(*key) else {
            continue;
        };
        let value = if PROJECT_ID_PREFS.contains(key) {
            Value::Array(
                value
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|id| roots.get(id.as_str()?))
                    .map(|root| {
                        Value::String(format!("{ROOT_KEY_PREFIX}{}", to_portable(home, root)))
                    })
                    .collect(),
            )
        } else {
            value.clone()
        };
        out.insert((*key).to_owned(), value);
    }
    let mut prefs_doc = Value::Object(out);
    redact_secret_fields(&mut prefs_doc);
    let mut skipped = Vec::new();
    let mut items = Vec::new();
    for recipe in prefs
        .get("launchRecipes")
        .and_then(|book| book.get("items"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = recipe
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("recipe");
        if recipe.get("host").is_some_and(|h| !h.is_null()) {
            skipped.push(SkippedItem {
                item: format!("Recipe “{name}”"),
                reason: "runs on a remote host".into(),
            });
            continue;
        }
        let mut recipe = recipe.clone();
        let Some(object) = recipe.as_object_mut() else {
            continue;
        };
        object.remove("accountProfileId");
        if let Some(project) = object.get_mut("project").and_then(Value::as_object_mut) {
            let root = project
                .get("last_known_root")
                .or_else(|| project.get("path"))
                .and_then(Value::as_str)
                .map(|root| to_portable(home, root));
            project.clear();
            project.insert("kind".into(), json!("path"));
            project.insert("path".into(), json!(root.unwrap_or_default()));
        }
        redact_secret_fields(&mut recipe);
        items.push(recipe);
    }
    (prefs_doc, json!({ "format": 1, "items": items }), skipped)
}

/// The pending preferences the app applies: portable keys only, project
/// roots resolved to this Mac's ids, sidebar lists unioned with what is
/// already here, and recipes merged into the local book (content duplicates
/// dropped, colliding ids renumbered).
pub fn import_prefs(
    portable: &Value,
    recipes: Option<&Value>,
    current: &Map<String, Value>,
    map: &PathMap,
    project_id: &dyn Fn(&str) -> String,
) -> Map<String, Value> {
    let mut pending = Map::new();
    let incoming = portable.as_object().cloned().unwrap_or_default();
    for key in PORTABLE_PREF_KEYS {
        if *key == "launchRecipes" {
            continue;
        }
        let Some(value) = incoming.get(*key) else {
            continue;
        };
        if PROJECT_ID_PREFS.contains(key) {
            let mut ids: Vec<Value> = current
                .get(*key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for root in value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                let Some(root) = root.strip_prefix(ROOT_KEY_PREFIX) else {
                    continue;
                };
                let id = Value::String(project_id(&map.remap(root)));
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            pending.insert((*key).to_owned(), Value::Array(ids));
        } else if *key == "conversationTitles" {
            // Names given on either Mac survive; this Mac's win on a clash.
            let mut titles = value.as_object().cloned().unwrap_or_default();
            if let Some(local) = current.get(*key).and_then(Value::as_object) {
                titles.extend(local.clone());
            }
            pending.insert((*key).to_owned(), Value::Object(titles));
        } else {
            pending.insert((*key).to_owned(), value.clone());
        }
    }
    if let Some(recipes) = recipes {
        let book = merge_recipe_book(current.get("launchRecipes"), recipes, map, project_id);
        pending.insert("launchRecipes".into(), book);
    }
    pending
}

fn recipe_fingerprint(recipe: &Value) -> Value {
    let mut recipe = recipe.clone();
    if let Some(object) = recipe.as_object_mut() {
        object.remove("id");
        if let Some(project) = object.get_mut("project").and_then(Value::as_object_mut) {
            project.remove("id");
        }
    }
    recipe
}

fn merge_recipe_book(
    current: Option<&Value>,
    incoming: &Value,
    map: &PathMap,
    project_id: &dyn Fn(&str) -> String,
) -> Value {
    let mut book = current
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(|| {
            let mut book = Map::new();
            book.insert("version".into(), json!(1));
            book.insert("nextId".into(), json!(1));
            book
        });
    let mut items: Vec<Value> = book
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut next_id = book.get("nextId").and_then(Value::as_u64).unwrap_or(1);
    for recipe in incoming
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mut recipe = recipe.clone();
        let Some(object) = recipe.as_object_mut() else {
            continue;
        };
        if let Some(project) = object.get_mut("project").and_then(Value::as_object_mut) {
            let root = project
                .get("path")
                .and_then(Value::as_str)
                .map(|root| map.remap(root))
                .unwrap_or_default();
            project.clear();
            project.insert("kind".into(), json!("tracked"));
            project.insert("id".into(), json!(project_id(&root)));
            project.insert("last_known_root".into(), json!(root));
        }
        let fingerprint = recipe_fingerprint(&recipe);
        if items
            .iter()
            .any(|item| recipe_fingerprint(item) == fingerprint)
        {
            continue;
        }
        let id = recipe
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if id.is_empty()
            || items
                .iter()
                .any(|item| item.get("id").and_then(Value::as_str) == Some(&id))
        {
            while items.iter().any(|item| {
                item.get("id").and_then(Value::as_str) == Some(&format!("recipe-{next_id}"))
            }) {
                next_id += 1;
            }
            recipe["id"] = json!(format!("recipe-{next_id}"));
            next_id += 1;
        }
        items.push(recipe);
    }
    book.insert("nextId".into(), json!(next_id));
    book.insert("items".into(), Value::Array(items));
    Value::Object(book)
}

/// `schedules.json`: local-only schedules, `~/` paths, no account binding.
pub fn export_schedules(home: &str, schedules: &[ScheduleRecord]) -> (Value, Vec<SkippedItem>) {
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    for record in schedules {
        if record.spec.spawn.host.is_some() {
            skipped.push(SkippedItem {
                item: format!("Schedule “{}”", record.spec.title),
                reason: "runs on a remote host".into(),
            });
            continue;
        }
        let mut spec = record.spec.clone();
        spec.spawn.account_profile_id = None;
        spec.spawn.parent = None;
        spec.spawn.same_repo_as = None;
        spec.spawn.cwd = to_portable(home, &spec.spawn.cwd);
        spec.spawn.start_directory = spec.spawn.start_directory.map(|d| to_portable(home, &d));
        if let ScheduleWhen::Files { folder, .. } = &mut spec.when {
            *folder = to_portable(home, folder);
        }
        let mut value = json!({
            "id": record.id,
            "spec": spec,
            "createdAt": record.created_at,
        });
        redact_secret_fields(&mut value);
        items.push(value);
    }
    (json!({ "format": 1, "schedules": items }), skipped)
}

/// One schedule to create on this Mac.
#[derive(Clone, Debug)]
pub struct ImportedSchedule {
    pub id: String,
    pub spec: diri_proto::schedules::ScheduleSpec,
}

pub fn import_schedules(doc: &Value, map: &PathMap) -> Vec<ImportedSchedule> {
    doc.get("schedules")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.to_owned();
            let mut spec: diri_proto::schedules::ScheduleSpec =
                serde_json::from_value(item.get("spec")?.clone()).ok()?;
            spec.spawn.cwd = map.remap(&spec.spawn.cwd);
            spec.spawn.start_directory = spec.spawn.start_directory.map(|d| map.remap(&d));
            if let ScheduleWhen::Files { folder, .. } = &mut spec.when {
                *folder = map.remap(folder);
            }
            Some(ImportedSchedule { id, spec })
        })
        .collect()
}

/// Expanded project roots a portable `projects.json` names, for remap.
pub fn project_roots(doc: &Value, home: &str) -> BTreeMap<String, ProjectEntry> {
    read_projects(doc)
        .into_iter()
        .map(|entry| (expand(home, &entry.root), entry))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projects() -> Vec<LocalProject> {
        vec![LocalProject {
            id: "p_old".into(),
            root: "/Users/alice/work/repo".into(),
            name: "repo".into(),
        }]
    }

    #[test]
    fn prefs_travel_without_machine_state_and_with_roots_for_ids() {
        let prefs = json!({
            "terminalTheme": "Dusk",
            "windowPlacement": {"x": 1},
            "talkVoiceCommand": "curl -H 'Authorization: Bearer x'",
            "sidebarPinnedProjects": ["p_old", "p_gone"],
            "launchRecipes": {"version": 1, "nextId": 2, "items": [{
                "id": "recipe-1", "name": "Review", "agent": {"claudeCode": {}},
                "accountProfileId": "work",
                "project": {"kind": "tracked", "id": "p_old", "last_known_root": "/Users/alice/work/repo"},
                "worktree": {"kind": "currentCheckout"}, "initialPrompt": "review"
            }]}
        });
        let (doc, recipes, skipped) =
            export_prefs("/Users/alice", prefs.as_object().unwrap(), &projects());
        assert!(skipped.is_empty());
        assert_eq!(doc["terminalTheme"], "Dusk");
        assert!(doc.get("windowPlacement").is_none());
        assert!(doc.get("talkVoiceCommand").is_none());
        assert_eq!(doc["sidebarPinnedProjects"], json!(["root:~/work/repo"]));
        let recipe = &recipes["items"][0];
        assert!(recipe.get("accountProfileId").is_none());
        assert_eq!(
            recipe["project"],
            json!({"kind": "path", "path": "~/work/repo"})
        );

        let mut map = PathMap::new("/Users/alice", "/Users/bob");
        map.add("~/work/repo", "/Users/bob/code/repo");
        let current = json!({
            "sidebarPinnedProjects": ["p_bob"],
            "launchRecipes": {"version": 1, "nextId": 2, "items": [{"id": "recipe-1", "name": "Mine"}]}
        });
        let id = |root: &str| format!("id:{root}");
        let pending = import_prefs(
            &doc,
            Some(&recipes),
            current.as_object().unwrap(),
            &map,
            &id,
        );
        assert_eq!(
            pending["sidebarPinnedProjects"],
            json!(["p_bob", "id:/Users/bob/code/repo"])
        );
        let items = pending["launchRecipes"]["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1]["id"], "recipe-2");
        assert_eq!(
            items[1]["project"]["last_known_root"],
            "/Users/bob/code/repo"
        );
        // Importing again adds nothing.
        let current = Value::Object(pending.clone());
        let again = import_prefs(
            &doc,
            Some(&recipes),
            current.as_object().unwrap(),
            &map,
            &id,
        );
        assert_eq!(again["launchRecipes"]["items"].as_array().unwrap().len(), 2);
        assert_eq!(
            again["sidebarPinnedProjects"],
            pending["sidebarPinnedProjects"]
        );
    }
}
