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
    /// Box styling any component may carry: padding, fill, border, size,
    /// alignment. The agent's own look, on top of the theme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<BoxStyle>,
    #[serde(flatten)]
    pub kind: NodeKind,
}

impl Node {
    pub fn new(kind: NodeKind) -> Self {
        Self {
            id: None,
            style: None,
            kind,
        }
    }

    pub fn with_id(id: impl Into<String>, kind: NodeKind) -> Self {
        Self {
            id: Some(id.into()),
            style: None,
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
            | NodeKind::Section { children, .. }
            | NodeKind::Box { children, .. }
            | NodeKind::Scroll { children, .. } => vec![children],
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
            | NodeKind::Section { children, .. }
            | NodeKind::Box { children, .. }
            | NodeKind::Scroll { children, .. } => vec![children],
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
                | NodeKind::Box { .. }
                | NodeKind::Scroll { .. }
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

/// A color: a theme token (`primary`, `secondary`, `tertiary`, `accent`,
/// `success`, `warning`, `danger`, `info`, `background`, `surface`, `fill`,
/// `border`) or `#rrggbb` / `#rrggbbaa`. Tokens follow light and dark.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Color(pub String);

pub const COLOR_TOKENS: &[&str] = &[
    "primary",
    "secondary",
    "tertiary",
    "accent",
    "success",
    "warning",
    "danger",
    "info",
    "background",
    "surface",
    "fill",
    "border",
];

impl Color {
    /// `(r, g, b, a)` for a hex color; `None` for a token.
    pub fn hex(&self) -> Option<(u8, u8, u8, u8)> {
        let hex = self.0.strip_prefix('#')?;
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        match hex.len() {
            6 => Some((byte(0)?, byte(2)?, byte(4)?, 255)),
            8 => Some((byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
            _ => None,
        }
    }

    pub fn is_valid(&self) -> bool {
        COLOR_TOKENS.contains(&self.0.as_str()) || self.hex().is_some()
    }
}

/// A width or height: points, or `"fill"`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Length {
    Points(f32),
    Keyword(String),
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    Start,
    Center,
    End,
    Stretch,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Justify {
    Start,
    Center,
    End,
    Between,
    Around,
}

/// Free styling for any component's box.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct BoxStyle {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub padding: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub padding_x: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub padding_y: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<Color>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border: Option<Color>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border_width: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<Length>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<Length>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_width: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<f32>,
    /// Share of the free space along the parent's axis (flex-grow).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grow: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<Align>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justify: Option<Justify>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Weight {
    #[default]
    Regular,
    Medium,
    Semibold,
    Bold,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Row,
    #[default]
    Column,
}

/// One vector shape on a `canvas`, in the canvas's own points, origin
/// top-left.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum Shape {
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<Color>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        radius: Option<f32>,
    },
    Circle {
        x: f32,
        y: f32,
        r: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<Color>,
    },
    /// A polyline through `points` (`[[x, y], ...]`).
    Line {
        points: Vec<[f32; 2]>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<Color>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<f32>,
    },
    /// A filled polygon through `points`: the area under a line, a wedge.
    Area {
        points: Vec<[f32; 2]>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<Color>,
    },
}

impl Shape {
    /// Points it costs to lay out and paint.
    pub fn cost(&self) -> usize {
        match self {
            Self::Line { points, .. } | Self::Area { points, .. } => 1 + points.len() / 8,
            _ => 1,
        }
    }

    pub fn color(&self) -> Option<&Color> {
        match self {
            Self::Rect { color, .. }
            | Self::Circle { color, .. }
            | Self::Line { color, .. }
            | Self::Area { color, .. } => color.as_ref(),
        }
    }
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
    /// The glyph on the item's link chip, as the agent picks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<super::identity::Icon>,
    /// The words on the link chip; the link's host when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_label: Option<String>,
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
        /// Point size (9-64).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        weight: Option<Weight>,
        /// Overrides `tone`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<Color>,
        /// One line, cut with an ellipsis.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncate: bool,
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
        /// A checklist id: the bar follows its ticks live and `value`/`max`
        /// are only the fallback.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        track: Option<String>,
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
        /// The agent's glyph for it; a generic link glyph otherwise.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        icon: Option<super::identity::Icon>,
        /// A short word after the label ("PR", "Thread"), as the agent says.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        badge: Option<String>,
    },
    Button {
        label: String,
        action: Action,
        #[serde(default)]
        variant: ButtonStyle,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        icon: Option<super::identity::Icon>,
    },
    Toggle {
        label: String,
        #[serde(default)]
        default: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        notify: bool,
        /// A Lua app's current value (the app owns its state); `default` and
        /// the surface's state otherwise.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<bool>,
    },
    Select {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        options: Vec<SelectOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        notify: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    /// A tick box with a label, for a Lua app's own state.
    Checkbox {
        label: String,
        #[serde(default)]
        value: bool,
    },
    /// A one-line text field; Enter sends its text to the app.
    Input {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    /// A flex box: the general layout primitive (`style` does the rest).
    Box {
        #[serde(default)]
        direction: Direction,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        wrap: bool,
        #[serde(default)]
        children: Vec<Node>,
    },
    /// A vertically scrolling region of fixed `height`.
    Scroll {
        height: f32,
        #[serde(default)]
        children: Vec<Node>,
    },
    /// Takes the free space along its parent's axis.
    Spacer,
    Code {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
    /// Vector drawing: bars, sparklines, rings, any list of shapes.
    Canvas {
        width: f32,
        height: f32,
        shapes: Vec<Shape>,
    },
    /// A media card: thumbnail, title and details, opening `url` on click.
    Media {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<String>,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subtitle: Option<String>,
        /// Corner badge on the thumbnail ("12:04", "LIVE").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        badge: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// Thumbnail width / height; 16:9 by default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        aspect: Option<f32>,
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
            Self::Checkbox { .. } => "checkbox",
            Self::Input { .. } => "input",
            Self::Box { .. } => "box",
            Self::Scroll { .. } => "scroll",
            Self::Spacer => "spacer",
            Self::Code { .. } => "code",
            Self::Canvas { .. } => "canvas",
            Self::Media { .. } => "media",
            Self::Unsupported => "unsupported",
        }
    }

    /// Components whose value the person changes and the surface keeps.
    pub fn is_stateful(&self) -> bool {
        matches!(
            self,
            Self::Checklist { .. }
                | Self::Toggle { .. }
                | Self::Select { .. }
                | Self::Checkbox { .. }
                | Self::Input { .. }
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
    "checkbox",
    "input",
    "box",
    "scroll",
    "spacer",
    "code",
    "canvas",
    "media",
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
