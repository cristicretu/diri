//! Three-way merge of profile JSON documents, per key or per record, so two
//! Macs editing different recipes or preferences never clobber each other.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

/// How a profile document merges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocKind {
    /// Top-level members merge one by one (`prefs.json`).
    Object,
    /// `{ <field>: [records] }`, records keyed by `key`.
    Collection {
        field: &'static str,
        key: &'static str,
    },
}

/// The merge rule for a profile path, `None` for plain files.
pub fn doc_kind(rel: &str) -> Option<DocKind> {
    Some(match rel {
        "prefs.json" => DocKind::Object,
        "recipes.json" => DocKind::Collection {
            field: "items",
            key: "id",
        },
        "schedules.json" => DocKind::Collection {
            field: "schedules",
            key: "id",
        },
        "projects.json" => DocKind::Collection {
            field: "projects",
            key: "root",
        },
        _ => return None,
    })
}

/// Three-way pick for one member. `None` means absent (deleted).
fn pick(
    base: Option<&Value>,
    local: Option<&Value>,
    remote: Option<&Value>,
) -> (Option<Value>, bool) {
    if local == remote {
        return (local.cloned(), false);
    }
    if local == base {
        return (remote.cloned(), false);
    }
    if remote == base {
        return (local.cloned(), false);
    }
    // Both changed differently: this Mac's edit wins; a deletion on one side
    // never beats an edit on the other.
    (local.or(remote).cloned(), true)
}

/// Merges `local` and `remote` against `base`. Returns the merged document
/// and the members both sides changed differently (local kept).
pub fn merge3(
    kind: DocKind,
    base: Option<&Value>,
    local: Option<&Value>,
    remote: Option<&Value>,
) -> (Value, Vec<String>) {
    match kind {
        DocKind::Object => merge_object(base, local, remote),
        DocKind::Collection { field, key } => merge_collection(field, key, base, local, remote),
    }
}

fn as_object(value: Option<&Value>) -> Map<String, Value> {
    value
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn merge_object(
    base: Option<&Value>,
    local: Option<&Value>,
    remote: Option<&Value>,
) -> (Value, Vec<String>) {
    let (base, local, remote) = (as_object(base), as_object(local), as_object(remote));
    let keys: BTreeSet<&String> = local.keys().chain(remote.keys()).collect();
    let mut merged = Map::new();
    let mut conflicts = Vec::new();
    for key in keys {
        let (value, conflict) = pick(base.get(key), local.get(key), remote.get(key));
        if conflict {
            conflicts.push(key.clone());
        }
        if let Some(value) = value {
            merged.insert(key.clone(), value);
        }
    }
    (Value::Object(merged), conflicts)
}

fn records<'a>(doc: &'a Map<String, Value>, field: &str, key: &str) -> Vec<(String, &'a Value)> {
    doc.get(field)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| Some((item.get(key)?.as_str()?.to_owned(), item)))
                .collect()
        })
        .unwrap_or_default()
}

fn merge_collection(
    field: &'static str,
    key: &'static str,
    base: Option<&Value>,
    local: Option<&Value>,
    remote: Option<&Value>,
) -> (Value, Vec<String>) {
    let (base_doc, local_doc, remote_doc) = (as_object(base), as_object(local), as_object(remote));
    let base_items = records(&base_doc, field, key);
    let local_items = records(&local_doc, field, key);
    let remote_items = records(&remote_doc, field, key);
    let find = |items: &[(String, &Value)], id: &str| {
        items
            .iter()
            .find(|(candidate, _)| candidate == id)
            .map(|(_, v)| (*v).clone())
    };
    // Local order first, then records only the other side has.
    let mut order: Vec<String> = local_items.iter().map(|(id, _)| id.clone()).collect();
    for (id, _) in &remote_items {
        if !order.contains(id) {
            order.push(id.clone());
        }
    }
    for (id, _) in &base_items {
        if !order.contains(id) {
            order.push(id.clone());
        }
    }
    let mut merged = Vec::new();
    let mut conflicts = Vec::new();
    for id in order {
        let b = find(&base_items, &id);
        let l = find(&local_items, &id);
        let r = find(&remote_items, &id);
        if b.is_none()
            && let (Some(l), Some(r)) = (&l, &r)
            && l != r
        {
            // Both Macs added a record under the same key (recipe ids are
            // per-Mac counters): keep both, the other Mac's under a new key.
            merged.push(l.clone());
            let mut other = r.clone();
            if let Some(object) = other.as_object_mut() {
                let suffix = &crate::files::sha256_hex(r.to_string().as_bytes())[..8];
                object.insert(key.to_owned(), Value::String(format!("{id}~{suffix}")));
            }
            merged.push(other);
            continue;
        }
        let (value, conflict) = pick(b.as_ref(), l.as_ref(), r.as_ref());
        if conflict {
            conflicts.push(format!("{field}/{id}"));
        }
        if let Some(value) = value {
            merged.push(value);
        }
    }
    // Other members of the document (a format number) merge like an object.
    let strip = |doc: &Map<String, Value>| {
        let mut doc = doc.clone();
        doc.remove(field);
        Value::Object(doc)
    };
    let (Value::Object(mut rest), more) = merge_object(
        Some(&strip(&base_doc)),
        Some(&strip(&local_doc)),
        Some(&strip(&remote_doc)),
    ) else {
        unreachable!("merge_object returns an object");
    };
    conflicts.extend(more);
    rest.insert(field.to_owned(), Value::Array(merged));
    (Value::Object(rest), conflicts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn object_members_merge_independently() {
        let base = json!({"a": 1, "b": 1, "c": 1});
        let local = json!({"a": 2, "b": 1, "c": 3});
        let remote = json!({"a": 1, "b": 2, "c": 4, "d": 5});
        let (merged, conflicts) = merge3(DocKind::Object, Some(&base), Some(&local), Some(&remote));
        assert_eq!(merged, json!({"a": 2, "b": 2, "c": 3, "d": 5}));
        assert_eq!(conflicts, vec!["c".to_owned()]);
    }

    #[test]
    fn deletions_propagate_only_from_an_unchanged_side() {
        let base = json!({"a": 1, "b": 1});
        let local = json!({"b": 1});
        let remote = json!({"a": 1, "b": 2});
        let (merged, _) = merge3(DocKind::Object, Some(&base), Some(&local), Some(&remote));
        assert_eq!(merged, json!({"b": 2}));
        // Deleted here, edited there: the edit survives.
        let remote = json!({"a": 9, "b": 1});
        let (merged, conflicts) = merge3(DocKind::Object, Some(&base), Some(&local), Some(&remote));
        assert_eq!(merged, json!({"a": 9, "b": 1}));
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn colliding_new_records_are_both_kept() {
        let kind = doc_kind("recipes.json").unwrap();
        let local = json!({"items": [{"id": "recipe-1", "name": "Review"}]});
        let remote = json!({"items": [{"id": "recipe-1", "name": "Deploy"}, {"id": "recipe-2", "name": "x"}]});
        let (merged, conflicts) = merge3(kind, None, Some(&local), Some(&remote));
        let items = merged["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["name"], "Review");
        assert_eq!(items[1]["name"], "Deploy");
        assert!(items[1]["id"].as_str().unwrap().starts_with("recipe-1~"));
        assert!(conflicts.is_empty());
        // Merging the result with itself is stable.
        let (again, _) = merge3(kind, Some(&merged), Some(&merged), Some(&merged));
        assert_eq!(again, merged);
    }
}
