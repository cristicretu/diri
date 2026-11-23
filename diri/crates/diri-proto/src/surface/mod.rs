//! Live Surfaces: native dashboards and mini-apps agents build inside Diri.
//!
//! A surface is a bounded tree of native components ([`node`]) plus the
//! state the person gave it (ticks, toggles, choices). Agents create and
//! patch surfaces through the `surface` MCP action and `dirijor surface`; the
//! app draws them with GPUI and keeps them live from Engine state
//! ([`node::Binding`]). Everything in this module is pure data and pure
//! functions: storage is `diri-surfaces`, drawing is `diri-app`.
//!
//! Two scopes share one format: a saved surface (global or for one project)
//! lives in the sidebar; a session surface belongs to one session and shows
//! in its right panel until it is saved ([`SurfaceDoc::promoted`]).

pub mod action;
pub mod limits;
pub mod link;
pub mod node;
pub mod patch;
pub mod template;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use action::{Action, Authority, Denied, Effect, Gesture, authorize};
pub use node::{Binding, Node, NodeKind, Tone};
pub use patch::{Patch, PatchOp};

/// The schema this build reads and writes. A file with a higher major is
/// shown read-only and its tree is never rewritten by this build.
pub const SCHEMA_VERSION: u32 = 1;

/// Where a surface belongs.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    /// Saved, shown for every project.
    #[default]
    Global,
    /// Saved, shown with one project (its folder).
    Project { project: String },
    /// Attached to one live session; shown in its right panel.
    Session { session: String },
}

impl Scope {
    pub fn is_saved(&self) -> bool {
        !matches!(self, Self::Session { .. })
    }

    pub fn session(&self) -> Option<&str> {
        match self {
            Self::Session { session } => Some(session),
            _ => None,
        }
    }
}

/// What the person has done on a surface, keyed by stable ids so patches
/// that keep a component keep its state.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct SurfaceState {
    /// `checklist/item` → ticked.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checks: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub toggles: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub selects: BTreeMap<String, String>,
}

impl SurfaceState {
    pub fn len(&self) -> usize {
        self.checks.len() + self.toggles.len() + self.selects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Keeps only entries the tree still has components for.
    pub fn pruned_to(&self, root: &[Node]) -> Self {
        let (checks, toggles, selects) = limits::state_keys(root);
        Self {
            checks: self
                .checks
                .iter()
                .filter(|(key, _)| checks.contains(*key))
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            toggles: self
                .toggles
                .iter()
                .filter(|(key, _)| toggles.contains(*key))
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            selects: self
                .selects
                .iter()
                .filter(|(key, _)| selects.contains(*key))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }
}

/// The state key of a checklist item.
pub fn check_key(checklist: &str, item: &str) -> String {
    format!("{checklist}/{item}")
}

/// One surface, as stored and as an agent reads it back.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SurfaceDoc {
    pub schema: u32,
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub scope: Scope,
    /// Shown as a sidebar row (saved surfaces only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    /// The session that receives this surface's events and refresh requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Other sessions its buttons may prompt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    /// Bumped on every write; `update` may require the one it read.
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub created_ms: u64,
    #[serde(default)]
    pub updated_ms: u64,
    /// Who wrote the current revision ("agent <session>", "you", "schedule").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    #[serde(default)]
    pub root: Vec<Node>,
    #[serde(default, skip_serializing_if = "SurfaceState::is_empty")]
    pub state: SurfaceState,
}

impl SurfaceDoc {
    pub fn new(id: impl Into<String>, title: impl Into<String>, scope: Scope) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            id: id.into(),
            title: title.into(),
            subtitle: None,
            scope,
            pinned: false,
            owner: None,
            agents: Vec::new(),
            revision: 0,
            created_ms: 0,
            updated_ms: 0,
            updated_by: None,
            root: Vec::new(),
            state: SurfaceState::default(),
        }
    }

    /// Everything [`limits`] bounds, and the metadata. A doc that passes is
    /// safe to store and to draw.
    pub fn validate(&self) -> Result<usize, SurfaceError> {
        if self.schema > SCHEMA_VERSION {
            return Err(SurfaceError::new(
                "unsupported_schema",
                format!(
                    "surface schema {} is newer than this Diri reads ({SCHEMA_VERSION})",
                    self.schema
                ),
                "schema",
            ));
        }
        if !limits::is_valid_ref(&self.id) {
            return Err(SurfaceError::new("invalid_id", "invalid surface id", "id"));
        }
        if self.title.trim().is_empty() || self.title.len() > limits::MAX_LABEL_BYTES {
            return Err(SurfaceError::new(
                "invalid_value",
                "a surface needs a title of at most 512 bytes",
                "title",
            ));
        }
        if self
            .subtitle
            .as_ref()
            .is_some_and(|s| s.len() > limits::MAX_LABEL_BYTES)
        {
            return Err(SurfaceError::new(
                "text_too_long",
                "subtitle is too long",
                "subtitle",
            ));
        }
        match &self.scope {
            Scope::Global => {}
            Scope::Project { project } => {
                if project.is_empty() || project.len() > 4096 || project.contains('\0') {
                    return Err(SurfaceError::new(
                        "invalid_value",
                        "invalid project",
                        "scope",
                    ));
                }
            }
            Scope::Session { session } => {
                if !limits::is_valid_ref(session) {
                    return Err(SurfaceError::new(
                        "invalid_ref",
                        "invalid session id",
                        "scope",
                    ));
                }
            }
        }
        if let Some(owner) = &self.owner
            && !limits::is_valid_ref(owner)
        {
            return Err(SurfaceError::new(
                "invalid_ref",
                "invalid owner session",
                "owner",
            ));
        }
        if self.agents.len() > limits::MAX_AGENTS
            || self.agents.iter().any(|agent| !limits::is_valid_ref(agent))
        {
            return Err(SurfaceError::new(
                "invalid_ref",
                format!(
                    "agents lists at most {} valid session ids",
                    limits::MAX_AGENTS
                ),
                "agents",
            ));
        }
        let count = limits::validate_tree(&self.root)?;
        limits::validate_state(&self.state)?;
        let size = serde_json::to_vec(self)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX);
        if size > limits::MAX_FILE_BYTES {
            return Err(SurfaceError::new(
                "too_large",
                format!(
                    "the surface is {size} bytes; the limit is {}",
                    limits::MAX_FILE_BYTES
                ),
                "",
            ));
        }
        Ok(count)
    }

    pub fn authority(&self) -> Authority<'_> {
        Authority {
            owner: self.owner.as_deref(),
            agents: &self.agents,
        }
    }

    /// Whether checklist item `item` of `checklist` shows ticked.
    pub fn is_checked(&self, checklist: &str, item: &node::ChecklistItem) -> bool {
        self.state
            .checks
            .get(&check_key(checklist, &item.id))
            .copied()
            .unwrap_or(item.checked)
    }

    pub fn toggle_value(&self, id: &str, default: bool) -> bool {
        self.state.toggles.get(id).copied().unwrap_or(default)
    }

    pub fn select_value<'a>(&'a self, id: &str, default: Option<&'a str>) -> Option<&'a str> {
        self.state.selects.get(id).map(String::as_str).or(default)
    }

    /// `(done, total)` of one checklist.
    pub fn checklist_counts(&self, checklist: &str) -> Option<(usize, usize)> {
        match node::find(&self.root, checklist).map(|node| &node.kind) {
            Some(NodeKind::Checklist { items, .. }) => Some((
                items.iter().filter(|item| self.is_checked(checklist, item)).count(),
                items.len(),
            )),
            _ => None,
        }
    }

    /// `(done, total)` across every checklist.
    pub fn checklist_progress(&self) -> (usize, usize) {
        let mut done = 0;
        let mut total = 0;
        node::walk(&self.root, &mut |node, _| {
            if let (Some(id), NodeKind::Checklist { items, .. }) = (&node.id, &node.kind) {
                for item in items {
                    total += 1;
                    if self.is_checked(id, item) {
                        done += 1;
                    }
                }
            }
        });
        (done, total)
    }

    /// A saved copy of a session surface: same tree and state, new id,
    /// scoped to `scope`, pinned, revision restarted. The session surface
    /// itself is left as it was.
    pub fn promoted(&self, new_id: impl Into<String>, scope: Scope, now_ms: u64) -> Self {
        let mut saved = self.clone();
        saved.id = new_id.into();
        saved.scope = scope;
        saved.pinned = true;
        saved.revision = 1;
        saved.created_ms = now_ms;
        saved.updated_ms = now_ms;
        saved.updated_by = Some("you".into());
        saved
    }

    /// Plain-text outline an agent reads back (`surface get --text`).
    pub fn outline(&self) -> String {
        let mut out = format!("# {}\n", self.title);
        node::walk(&self.root, &mut |node, depth| {
            let indent = "  ".repeat(depth - 1);
            let id = node
                .id
                .as_deref()
                .map(|id| format!(" #{id}"))
                .unwrap_or_default();
            let detail = match &node.kind {
                NodeKind::Heading { text, .. } | NodeKind::Text { text, .. } => {
                    format!(" {}", first_line(text))
                }
                NodeKind::Card { title: Some(t), .. } | NodeKind::Section { title: t, .. } => {
                    format!(" {t}")
                }
                NodeKind::Metric { label, value, .. } => format!(" {label}: {value}"),
                NodeKind::Checklist { items, .. } => {
                    let list = node.id.as_deref().unwrap_or_default();
                    let lines: Vec<String> = items
                        .iter()
                        .map(|item| {
                            let mark = if self.is_checked(list, item) {
                                "x"
                            } else {
                                " "
                            };
                            format!("\n{indent}  - [{mark}] {} ({})", item.text, item.id)
                        })
                        .collect();
                    lines.concat()
                }
                NodeKind::Link { url, label, .. } => {
                    format!(" {} <{url}>", label.as_deref().unwrap_or(""))
                }
                NodeKind::Button { label, action, .. } => {
                    format!(" [{label}] → {}", action.kind_name())
                }
                NodeKind::Toggle { label, default, .. } => {
                    let id = node.id.as_deref().unwrap_or_default();
                    format!(" {label} = {}", self.toggle_value(id, *default))
                }
                NodeKind::Select { default, .. } => {
                    let id = node.id.as_deref().unwrap_or_default();
                    format!(
                        " = {}",
                        self.select_value(id, default.as_deref()).unwrap_or("-")
                    )
                }
                NodeKind::Session { session, .. } => format!(" {session}"),
                _ => String::new(),
            };
            out.push_str(&format!("{indent}- {}{id}{detail}\n", node.type_name()));
        });
        out
    }
}

fn first_line(text: &str) -> &str {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(80) {
        Some((index, _)) => &line[..index],
        None => line,
    }
}

/// A person's change to a stateful component.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Interaction {
    Check {
        checklist: String,
        item: String,
        checked: bool,
    },
    Toggle {
        id: String,
        on: bool,
    },
    Select {
        id: String,
        value: String,
    },
}

impl SurfaceDoc {
    /// Applies the person's change to the state. Returns whether the owning
    /// agent asked to hear about it (`notify` on the component).
    pub fn interact(&mut self, interaction: &Interaction) -> Result<bool, SurfaceError> {
        let not_found =
            |id: &str| SurfaceError::new("not_found", format!("no component {id:?} to change"), id);
        match interaction {
            Interaction::Check {
                checklist,
                item,
                checked,
            } => {
                let node = node::find(&self.root, checklist).ok_or_else(|| not_found(checklist))?;
                let NodeKind::Checklist { items, notify, .. } = &node.kind else {
                    return Err(not_found(checklist));
                };
                if !items.iter().any(|candidate| &candidate.id == item) {
                    return Err(not_found(item));
                }
                let notify = *notify;
                self.state
                    .checks
                    .insert(check_key(checklist, item), *checked);
                Ok(notify)
            }
            Interaction::Toggle { id, on } => {
                let node = node::find(&self.root, id).ok_or_else(|| not_found(id))?;
                let NodeKind::Toggle { notify, .. } = &node.kind else {
                    return Err(not_found(id));
                };
                let notify = *notify;
                self.state.toggles.insert(id.clone(), *on);
                Ok(notify)
            }
            Interaction::Select { id, value } => {
                let node = node::find(&self.root, id).ok_or_else(|| not_found(id))?;
                let NodeKind::Select {
                    options, notify, ..
                } = &node.kind
                else {
                    return Err(not_found(id));
                };
                if !options.iter().any(|option| &option.value == value) {
                    return Err(SurfaceError::new(
                        "invalid_value",
                        format!("{value:?} is not one of the options"),
                        id,
                    ));
                }
                let notify = *notify;
                self.state.selects.insert(id.clone(), value.clone());
                Ok(notify)
            }
        }
    }
}

/// Something the person did on a surface, as the event log keeps it and
/// agents read it (`surface events`).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SurfaceEvent {
    pub seq: u64,
    pub at_ms: u64,
    #[serde(flatten)]
    pub what: EventKind,
    /// The session told about it, when one was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_to: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventKind {
    Interaction(Interaction),
    Action {
        node: String,
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
    },
}

/// A structured refusal: `code` is stable for agents, `path` names the
/// component (`/card-id/checklist-id`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SurfaceError {
    pub code: &'static str,
    pub message: String,
    pub path: String,
}

impl SurfaceError {
    pub fn new(code: &'static str, message: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            path: path.into(),
        }
    }
}

impl std::fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}: {}", self.code, self.message)
        } else {
            write!(f, "{}: {} (at {})", self.code, self.message, self.path)
        }
    }
}

impl std::error::Error for SurfaceError {}

/// The message an agent receives when the person acts on its surface. One
/// line of context, then JSON, so agents parse it without guessing.
pub fn event_prompt(doc: &SurfaceDoc, event: &SurfaceEvent) -> String {
    let body = serde_json::json!({
        "surface": doc.id,
        "title": doc.title,
        "revision": doc.revision,
        "event": event,
    });
    format!(
        "[Diri Live Surface event] The person acted on your surface \"{}\". Update it with the surface action (op update) if this changes what it should show.\n{}",
        doc.title, body
    )
}
