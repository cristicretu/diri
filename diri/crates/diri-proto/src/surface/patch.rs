//! Patches: change parts of a surface by node id, all or nothing.
//!
//! [`apply`] is a pure function of the surface and the patch. It applies the
//! operations in order to a copy, checks the result against every bound, and
//! only then returns it, so a refused patch leaves the surface exactly as it
//! was. Components the patch keeps keep their state (ticks, toggles,
//! choices), which is what lets an agent update a live checklist without the
//! person losing what they ticked. `diri/formal/SurfaceModel.lean` models
//! these guarantees.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::limits;
use super::node::{self, Node};
use super::{SurfaceDoc, SurfaceError};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PatchOp {
    /// Swap the component `id` for `node`.
    Replace { id: String, node: Node },
    /// Remove the component `id` and everything inside it.
    Remove { id: String },
    /// Put `node` into container `parent` (or a tab), or at the top level
    /// when `parent` is absent; at `index`, or at the end.
    Insert {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
        node: Node,
    },
    /// Change some fields of component `id`, keeping the rest. `type` and
    /// `id` cannot change.
    Set {
        id: String,
        fields: Map<String, Value>,
    },
    /// Rename the surface.
    Title {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subtitle: Option<String>,
    },
    /// Replace every top-level component.
    Root { root: Vec<Node> },
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Patch {
    pub ops: Vec<PatchOp>,
    /// Refuse the patch unless the surface is still at this revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// What a successful patch touched, for the reply and the app's redraw.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Applied {
    pub ops: usize,
    pub nodes: usize,
    pub revision: u64,
}

fn not_found(id: &str) -> SurfaceError {
    SurfaceError::new("not_found", format!("no component with id {id:?}"), id)
}

/// Applies `patch` to `doc`, returning the new surface. The revision is
/// bumped once for the whole patch; `updated_ms`/`updated_by` are the
/// caller's to set.
pub fn apply(doc: &SurfaceDoc, patch: &Patch) -> Result<(SurfaceDoc, Applied), SurfaceError> {
    if let Some(expected) = patch.expected_revision
        && expected != doc.revision
    {
        return Err(SurfaceError::new(
            "revision_conflict",
            format!(
                "the surface is at revision {}, not {expected}; read it again and retry",
                doc.revision
            ),
            "",
        ));
    }
    if patch.ops.is_empty() {
        return Err(SurfaceError::new(
            "invalid_value",
            "a patch needs at least one op",
            "ops",
        ));
    }
    if patch.ops.len() > limits::MAX_PATCH_OPS {
        return Err(SurfaceError::new(
            "too_many_ops",
            format!("a patch holds at most {} ops", limits::MAX_PATCH_OPS),
            "ops",
        ));
    }
    let mut next = doc.clone();
    for (index, op) in patch.ops.iter().enumerate() {
        apply_op(&mut next, op).map_err(|mut error| {
            error.message = format!("op {index}: {}", error.message);
            error
        })?;
    }
    next.state = next.state.pruned_to(&next.root);
    next.revision = doc.revision.saturating_add(1);
    let nodes = next.validate()?;
    let applied = Applied {
        ops: patch.ops.len(),
        nodes,
        revision: next.revision,
    };
    Ok((next, applied))
}

/// Replaces the whole tree (an agent's full `update`), keeping the state of
/// every component that survives.
pub fn replace_all(
    doc: &SurfaceDoc,
    title: Option<&str>,
    root: Vec<Node>,
    expected_revision: Option<u64>,
) -> Result<(SurfaceDoc, Applied), SurfaceError> {
    let mut ops = Vec::with_capacity(2);
    if let Some(title) = title {
        ops.push(PatchOp::Title {
            title: title.to_owned(),
            subtitle: doc.subtitle.clone(),
        });
    }
    ops.push(PatchOp::Root { root });
    apply(
        doc,
        &Patch {
            ops,
            expected_revision,
        },
    )
}

fn apply_op(doc: &mut SurfaceDoc, op: &PatchOp) -> Result<(), SurfaceError> {
    match op {
        PatchOp::Replace { id, node } => {
            let slot = find_mut(&mut doc.root, id).ok_or_else(|| not_found(id))?;
            *slot = node.clone();
            Ok(())
        }
        PatchOp::Remove { id } => {
            if remove(&mut doc.root, id) {
                Ok(())
            } else {
                Err(not_found(id))
            }
        }
        PatchOp::Insert {
            parent,
            index,
            node,
        } => {
            let list = match parent {
                None => &mut doc.root,
                Some(parent) => container_mut(&mut doc.root, parent).ok_or_else(|| {
                    SurfaceError::new(
                        "not_found",
                        format!("no container or tab with id {parent:?}"),
                        parent.as_str(),
                    )
                })?,
            };
            let at = index.unwrap_or(list.len()).min(list.len());
            // Refuse before growing: an unbounded insert never allocates.
            if list.len() >= limits::MAX_CHILDREN {
                return Err(SurfaceError::new(
                    "too_many_children",
                    format!("a container holds at most {}", limits::MAX_CHILDREN),
                    parent.clone().unwrap_or_default(),
                ));
            }
            list.insert(at, node.clone());
            Ok(())
        }
        PatchOp::Set { id, fields } => {
            if fields.contains_key("type") || fields.contains_key("id") {
                return Err(SurfaceError::new(
                    "invalid_value",
                    "set cannot change a component's type or id; use replace",
                    id.as_str(),
                ));
            }
            let slot = find_mut(&mut doc.root, id).ok_or_else(|| not_found(id))?;
            let mut value = node::to_value(slot);
            let object = value.as_object_mut().ok_or_else(|| {
                SurfaceError::new("invalid_value", "component is not an object", id.as_str())
            })?;
            for (key, field) in fields {
                if field.is_null() {
                    object.remove(key);
                } else {
                    object.insert(key.clone(), field.clone());
                }
            }
            let updated: Node = serde_json::from_value(value).map_err(|error| {
                SurfaceError::new(
                    "invalid_value",
                    format!("invalid fields: {error}"),
                    id.as_str(),
                )
            })?;
            *slot = updated;
            Ok(())
        }
        PatchOp::Title { title, subtitle } => {
            doc.title = title.clone();
            doc.subtitle = subtitle.clone();
            Ok(())
        }
        PatchOp::Root { root } => {
            doc.root = root.clone();
            Ok(())
        }
    }
}

fn find_mut<'a>(nodes: &'a mut [Node], id: &str) -> Option<&'a mut Node> {
    for node in nodes.iter_mut() {
        if node.id.as_deref() == Some(id) {
            return Some(node);
        }
        for list in node.child_lists_mut() {
            if let Some(found) = find_mut(list, id) {
                return Some(found);
            }
        }
    }
    None
}

fn remove(nodes: &mut Vec<Node>, id: &str) -> bool {
    if let Some(index) = nodes.iter().position(|node| node.id.as_deref() == Some(id)) {
        nodes.remove(index);
        return true;
    }
    for node in nodes.iter_mut() {
        if let node::NodeKind::Tabs { tabs } = &mut node.kind
            && let Some(index) = tabs.iter().position(|tab| tab.id == id)
        {
            tabs.remove(index);
            return true;
        }
        for list in node.child_lists_mut() {
            if remove(list, id) {
                return true;
            }
        }
    }
    false
}

/// The child list of container `id`, or of the tab `id`.
fn container_mut<'a>(nodes: &'a mut [Node], id: &str) -> Option<&'a mut Vec<Node>> {
    for node in nodes.iter_mut() {
        if node.id.as_deref() == Some(id) {
            if !node.is_container() {
                return None;
            }
            return node.child_lists_mut().into_iter().next();
        }
        // Decide on a shared borrow first: returning a mutable borrow from
        // one branch and reborrowing in another is beyond today's borrowck.
        let tab_index = match &node.kind {
            node::NodeKind::Tabs { tabs } => tabs.iter().position(|tab| tab.id == id),
            _ => None,
        };
        if let Some(index) = tab_index {
            return match &mut node.kind {
                node::NodeKind::Tabs { tabs } => Some(&mut tabs[index].children),
                _ => None,
            };
        }
        for list in node.child_lists_mut() {
            if let Some(found) = container_mut(list, id) {
                return Some(found);
            }
        }
    }
    None
}
