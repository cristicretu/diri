//! The component tree a Live Surface draws.
//!
//! A surface is data, never code: every node is one of a closed set of
//! components the app draws natively, and every string, list and nesting
//! level is bounded ([`super::limits`]). A `type` this build does not know
//! reads as [`NodeKind::Unsupported`] and draws as a quiet placeholder, so a
//! surface written by a newer Diri still opens.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::action::Action;

/// One component. `id` is optional for static content and required for
/// anything interactive or addressed by a patch; ids are unique across the
/// whole surface (tab ids included).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Node {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(flatten)]
    pub kind: NodeKind,
}

impl Node {
    pub fn new(kind: NodeKind) -> Self {
        Self { id: None, kind }
    }

    pub fn with_id(id: impl Into<String>, kind: NodeKind) -> Self {
        Self {
            id: Some(id.into()),
            kind,
        }
    }

    /// The child lists this node owns, in draw order. Tabs own one list per
    /// tab.
    pub fn child_lists(&self) -> Vec<&Vec<Node>> {
        match &self.kind {
            NodeKind::Stack { children, .. }
            | NodeKind::Row { children, .. }
            | NodeKind::Grid { children, .. }
            | NodeKind::Card { children, .. }
            | NodeKind::Section { children, .. } => vec![children],
            NodeKind::Tabs { tabs } => tabs.iter().map(|tab| &tab.children).collect(),
            _ => Vec::new(),
        }
    }

    pub fn child_lists_mut(&mut self) -> Vec<&mut Vec<Node>> {
        match &mut self.kind {
            NodeKind::Stack { children, .. }
            | NodeKind::Row { children, .. }
            | NodeKind::Grid { children, .. }
            | NodeKind::Card { children, .. }
            | NodeKind::Section { children, .. } => vec![children],
            NodeKind::Tabs { tabs } => tabs.iter_mut().map(|tab| &mut tab.children).collect(),
            _ => Vec::new(),
        }
    }

    /// Whether `insert` may put children directly inside this node.
    pub fn is_container(&self) -> bool {
        matches!(
            self.kind,
            NodeKind::Stack { .. }
                | NodeKind::Row { .. }
                | NodeKind::Grid { .. }
                | NodeKind::Card { .. }
                | NodeKind::Section { .. }
        )
    }

    /// The `type` tag, for messages.
    pub fn type_name(&self) -> &'static str {
        self.kind.type_name()
    }
}

/// How strongly a piece of text or a badge reads.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    #[default]
    Neutral,
    Muted,
    Info,
    Success,
    Warning,
    Danger,
    Accent,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trend {
    Up,
    Down,
    #[default]
    Flat,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ButtonStyle {
    Primary,
    #[default]
    Secondary,
    Danger,
}

/// A tab of a `tabs` node: a labelled list of children.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Tab {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub children: Vec<Node>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ChecklistItem {
    pub id: String,
    pub text: String,
    /// The tick a fresh surface starts with. Once the person ticks it, the
    /// surface's state ([`super::SurfaceState::checks`]) wins.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub checked: bool,
    /// A rich link shown after the text (a PR, a Linear issue, a session).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct KvItem {
    pub key: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<Tone>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
}

/// A table cell: plain text, or text with a link or a tone.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Cell {
    Text(String),
    Rich {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        link: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
    },
}

impl Cell {
    pub fn text(&self) -> &str {
        match self {
            Self::Text(text) | Self::Rich { text, .. } => text,
        }
    }

    pub fn link(&self) -> Option<&str> {
        match self {
            Self::Text(_) => None,
            Self::Rich { link, .. } => link.as_deref(),
        }
    }

    pub fn tone(&self) -> Option<Tone> {
        match self {
            Self::Text(_) => None,
            Self::Rich { tone, .. } => *tone,
        }
    }
}

/// Live Diri state a component reads at draw time instead of a value the
/// agent wrote, so the surface stays current with no agent running.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum Binding {
    /// A session's status: working, needs input, idle, exited.
    SessionStatus { session: String },
    /// A session's current tab title.
    SessionTitle { session: String },
    /// How many sessions are working right now.
    WorkingCount,
    /// How many sessions wait on the person.
    NeedsInputCount,
    /// Sessions with unseen finished turns.
    UnreadCount,
    /// A note's open/total to-dos, by note id.
    NoteTodos { note: String },
    /// The checks of the first pull request a session opened.
    PrChecks { session: String },
}

impl Binding {
    pub fn session(&self) -> Option<&str> {
        match self {
            Self::SessionStatus { session }
            | Self::SessionTitle { session }
            | Self::PrChecks { session } => Some(session),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeKind {
    // Layout.
    Stack {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gap: Option<f32>,
        #[serde(default)]
        children: Vec<Node>,
    },
    Row {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gap: Option<f32>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        wrap: bool,
        #[serde(default)]
        children: Vec<Node>,
    },
    Grid {
        columns: u8,
        #[serde(default)]
        children: Vec<Node>,
    },
    Card {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subtitle: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default)]
        children: Vec<Node>,
    },
    Section {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default)]
        children: Vec<Node>,
    },
    Divider,
    Tabs {
        tabs: Vec<Tab>,
    },
    // Content.
    Heading {
        text: String,
        #[serde(default = "default_level")]
        level: u8,
    },
    Text {
        #[serde(default)]
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        mono: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bind: Option<Binding>,
    },
    Markdown {
        text: String,
    },
    Badge {
        #[serde(default)]
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bind: Option<Binding>,
    },
    Metric {
        label: String,
        #[serde(default)]
        value: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delta: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trend: Option<Trend>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bind: Option<Binding>,
    },
    Progress {
        value: f64,
        #[serde(default = "default_max")]
        max: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
    },
    Table {
        columns: Vec<String>,
        #[serde(default)]
        rows: Vec<Vec<Cell>>,
    },
    KvList {
        items: Vec<KvItem>,
    },
    Mermaid {
        source: String,
    },
    Image {
        src: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<f32>,
    },
    // Interactive.
    Checklist {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        items: Vec<ChecklistItem>,
        /// Tell the owning agent when the person ticks an item.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        notify: bool,
    },
    Link {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Button {
        label: String,
        action: Action,
        #[serde(default)]
        style: ButtonStyle,
    },
    Toggle {
        label: String,
        #[serde(default)]
        default: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        notify: bool,
    },
    Select {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        options: Vec<SelectOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        notify: bool,
    },
    /// A live row for one Diri session: icon, title and status, kept current
    /// by the app; clicking it opens the session.
    Session {
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// A component this build does not know; drawn as a placeholder.
    #[serde(other)]
    Unsupported,
}

fn default_level() -> u8 {
    2
}

fn default_max() -> f64 {
    1.0
}

impl NodeKind {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Stack { .. } => "stack",
            Self::Row { .. } => "row",
            Self::Grid { .. } => "grid",
            Self::Card { .. } => "card",
            Self::Section { .. } => "section",
            Self::Divider => "divider",
            Self::Tabs { .. } => "tabs",
            Self::Heading { .. } => "heading",
            Self::Text { .. } => "text",
            Self::Markdown { .. } => "markdown",
            Self::Badge { .. } => "badge",
            Self::Metric { .. } => "metric",
            Self::Progress { .. } => "progress",
            Self::Table { .. } => "table",
            Self::KvList { .. } => "kv_list",
            Self::Mermaid { .. } => "mermaid",
            Self::Image { .. } => "image",
            Self::Checklist { .. } => "checklist",
            Self::Link { .. } => "link",
            Self::Button { .. } => "button",
            Self::Toggle { .. } => "toggle",
            Self::Select { .. } => "select",
            Self::Session { .. } => "session",
            Self::Unsupported => "unsupported",
        }
    }

    /// Components whose value the person changes and the surface keeps.
    pub fn is_stateful(&self) -> bool {
        matches!(
            self,
            Self::Checklist { .. } | Self::Toggle { .. } | Self::Select { .. }
        )
    }

    pub fn binding(&self) -> Option<&Binding> {
        match self {
            Self::Text { bind, .. } | Self::Badge { bind, .. } | Self::Metric { bind, .. } => {
                bind.as_ref()
            }
            _ => None,
        }
    }
}

/// Every type tag, for schemas and help.
pub const NODE_TYPES: &[&str] = &[
    "stack",
    "row",
    "grid",
    "card",
    "section",
    "divider",
    "tabs",
    "heading",
    "text",
    "markdown",
    "badge",
    "metric",
    "progress",
    "table",
    "kv_list",
    "mermaid",
    "image",
    "checklist",
    "link",
    "button",
    "toggle",
    "select",
    "session",
];

/// Depth-first walk over `nodes` and everything under them, with each
/// node's depth (top level = 1).
pub fn walk<'a>(nodes: &'a [Node], visit: &mut impl FnMut(&'a Node, usize)) {
    fn go<'a>(nodes: &'a [Node], depth: usize, visit: &mut impl FnMut(&'a Node, usize)) {
        for node in nodes {
            visit(node, depth);
            for list in node.child_lists() {
                go(list, depth + 1, visit);
            }
        }
    }
    go(nodes, 1, visit);
}

/// Finds the node with `id`.
pub fn find<'a>(nodes: &'a [Node], id: &str) -> Option<&'a Node> {
    for node in nodes {
        if node.id.as_deref() == Some(id) {
            return Some(node);
        }
        for list in node.child_lists() {
            if let Some(found) = find(list, id) {
                return Some(found);
            }
        }
    }
    None
}

/// A node as JSON, for field-level patches.
pub(crate) fn to_value(node: &Node) -> Value {
    serde_json::to_value(node).unwrap_or(Value::Null)
}
