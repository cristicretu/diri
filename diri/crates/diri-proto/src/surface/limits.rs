//! Bounds on every surface, checked before anything is stored or drawn.
//!
//! A buggy or hostile agent can at worst have its write refused: the app
//! never lays out more than [`MAX_NODES`] components, nests deeper than
//! [`MAX_DEPTH`], or holds more than [`MAX_FILE_BYTES`] of one surface.

use std::collections::HashSet;

use super::action;
use super::link;
use super::node::{Node, NodeKind, walk};
use super::{SurfaceError, SurfaceState};

/// Nesting levels below the surface itself (top-level nodes are depth 1).
pub const MAX_DEPTH: usize = 12;
/// Components in one surface, tabs and checklist items included.
pub const MAX_NODES: usize = 1500;
/// Children of one container, tabs of one `tabs`, items of one list.
pub const MAX_CHILDREN: usize = 200;
/// Rows of one table.
pub const MAX_TABLE_ROWS: usize = 500;
pub const MAX_TABLE_COLUMNS: usize = 12;
pub const MAX_GRID_COLUMNS: u8 = 6;
/// One text, markdown or mermaid string.
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
/// Labels, titles, keys and option values.
pub const MAX_LABEL_BYTES: usize = 512;
pub const MAX_URL_BYTES: usize = 4096;
pub const MAX_PROMPT_BYTES: usize = 8 * 1024;
pub const MAX_PAYLOAD_BYTES: usize = 4 * 1024;
/// A whole surface file, state included.
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_ID_BYTES: usize = 64;
/// Operations in one patch.
pub const MAX_PATCH_OPS: usize = 200;
/// Agents a surface may prompt besides its owner.
pub const MAX_AGENTS: usize = 32;
/// Remembered interactions (ticks, toggles, choices).
pub const MAX_STATE_ENTRIES: usize = 4000;
pub const MAX_GAP: f32 = 48.0;
pub const MAX_IMAGE_HEIGHT: f32 = 800.0;

/// Node and item ids: `[A-Za-z0-9_.-]`, 1-64 bytes, not starting with `.`.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// Ids of other Diri objects (sessions, notes, surfaces) a surface names:
/// the same alphabet, up to 128 bytes, never a path.
pub fn is_valid_ref(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && !id.contains("..")
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

struct Checker {
    nodes: usize,
    ids: HashSet<String>,
}

impl Checker {
    fn count(&mut self, path: &str, n: usize) -> Result<(), SurfaceError> {
        self.nodes += n;
        if self.nodes > MAX_NODES {
            return Err(SurfaceError::new(
                "too_many_nodes",
                format!("a surface holds at most {MAX_NODES} components"),
                path,
            ));
        }
        Ok(())
    }

    fn id(&mut self, path: &str, id: &str) -> Result<(), SurfaceError> {
        if !is_valid_id(id) {
            return Err(SurfaceError::new(
                "invalid_id",
                format!("id {id:?} must be 1-{MAX_ID_BYTES} of A-Z a-z 0-9 _ - ."),
                path,
            ));
        }
        if !self.ids.insert(id.to_owned()) {
            return Err(SurfaceError::new(
                "duplicate_id",
                format!("id {id:?} is used twice; ids are unique across a surface"),
                path,
            ));
        }
        Ok(())
    }
}

fn text(path: &str, field: &str, value: &str, max: usize) -> Result<(), SurfaceError> {
    if value.len() > max {
        return Err(SurfaceError::new(
            "text_too_long",
            format!("{field} is {} bytes; the limit is {max}", value.len()),
            path,
        ));
    }
    Ok(())
}

fn label(path: &str, field: &str, value: &str) -> Result<(), SurfaceError> {
    text(path, field, value, MAX_LABEL_BYTES)
}

fn children(path: &str, len: usize) -> Result<(), SurfaceError> {
    if len > MAX_CHILDREN {
        return Err(SurfaceError::new(
            "too_many_children",
            format!("{len} children; a container holds at most {MAX_CHILDREN}"),
            path,
        ));
    }
    Ok(())
}

fn url(path: &str, value: &str) -> Result<(), SurfaceError> {
    if link::is_allowed(value) {
        Ok(())
    } else {
        Err(SurfaceError::new(
            "invalid_url",
            format!(
                "{value:?} is not an allowed link (schemes: {})",
                link::ALLOWED_SCHEMES.join(", ")
            ),
            path,
        ))
    }
}

fn reference(path: &str, field: &str, value: &str) -> Result<(), SurfaceError> {
    if is_valid_ref(value) {
        Ok(())
    } else {
        Err(SurfaceError::new(
            "invalid_ref",
            format!("{field} {value:?} is not a valid id"),
            path,
        ))
    }
}

fn require_id<'a>(path: &str, node: &'a Node) -> Result<&'a str, SurfaceError> {
    node.id.as_deref().ok_or_else(|| {
        SurfaceError::new(
            "missing_id",
            format!(
                "a {} needs an id so its state can be kept",
                node.type_name()
            ),
            path,
        )
    })
}

fn node_path(node: &Node, index: usize, parent: &str) -> String {
    match &node.id {
        Some(id) => format!("{parent}/{id}"),
        None => format!("{parent}/{}[{index}]", node.type_name()),
    }
}

/// Checks a whole tree against every bound. Returns the number of
/// components counted.
pub fn validate_tree(root: &[Node]) -> Result<usize, SurfaceError> {
    children("", root.len())?;
    let mut checker = Checker {
        nodes: 0,
        ids: HashSet::new(),
    };
    check_list(&mut checker, root, 1, "")?;
    Ok(checker.nodes)
}

fn check_list(
    checker: &mut Checker,
    nodes: &[Node],
    depth: usize,
    parent: &str,
) -> Result<(), SurfaceError> {
    for (index, node) in nodes.iter().enumerate() {
        let path = node_path(node, index, parent);
        if depth > MAX_DEPTH {
            return Err(SurfaceError::new(
                "too_deep",
                format!("components nest at most {MAX_DEPTH} levels"),
                &path,
            ));
        }
        checker.count(&path, 1)?;
        if let Some(id) = &node.id {
            checker.id(&path, id)?;
        }
        check_node(checker, node, &path)?;
        match &node.kind {
            NodeKind::Tabs { tabs } => {
                children(&path, tabs.len())?;
                for tab in tabs {
                    let tab_path = format!("{path}/{}", tab.id);
                    checker.count(&tab_path, 1)?;
                    checker.id(&tab_path, &tab.id)?;
                    label(&tab_path, "tab label", &tab.label)?;
                    children(&tab_path, tab.children.len())?;
                    // A tab's children sit one level below the tabs node.
                    check_list(checker, &tab.children, depth + 1, &tab_path)?;
                }
            }
            _ => {
                for list in node.child_lists() {
                    children(&path, list.len())?;
                    check_list(checker, list, depth + 1, &path)?;
                }
            }
        }
    }
    Ok(())
}

fn check_node(checker: &mut Checker, node: &Node, path: &str) -> Result<(), SurfaceError> {
    match &node.kind {
        NodeKind::Stack { gap, .. } | NodeKind::Row { gap, .. } => {
            if gap.is_some_and(|gap| !(0.0..=MAX_GAP).contains(&gap)) {
                return Err(SurfaceError::new(
                    "invalid_value",
                    format!("gap must be 0-{MAX_GAP}"),
                    path,
                ));
            }
        }
        NodeKind::Grid { columns, .. } => {
            if !(1..=MAX_GRID_COLUMNS).contains(columns) {
                return Err(SurfaceError::new(
                    "invalid_value",
                    format!("a grid has 1-{MAX_GRID_COLUMNS} columns"),
                    path,
                ));
            }
        }
        NodeKind::Card {
            title, subtitle, ..
        } => {
            if let Some(title) = title {
                label(path, "title", title)?;
            }
            if let Some(subtitle) = subtitle {
                label(path, "subtitle", subtitle)?;
            }
        }
        NodeKind::Section { title, detail, .. } => {
            label(path, "title", title)?;
            if let Some(detail) = detail {
                label(path, "detail", detail)?;
            }
        }
        NodeKind::Divider | NodeKind::Unsupported | NodeKind::Tabs { .. } => {}
        NodeKind::Heading { text: t, level } => {
            label(path, "text", t)?;
            if !(1..=3).contains(level) {
                return Err(SurfaceError::new(
                    "invalid_value",
                    "heading level is 1, 2 or 3",
                    path,
                ));
            }
        }
        NodeKind::Text { text: t, bind, .. } => {
            text(path, "text", t, MAX_TEXT_BYTES)?;
            check_binding(path, bind.as_ref())?;
        }
        NodeKind::Markdown { text: t } => text(path, "text", t, MAX_TEXT_BYTES)?,
        NodeKind::Mermaid { source } => text(path, "source", source, MAX_TEXT_BYTES)?,
        NodeKind::Badge { label: l, bind, .. } => {
            label(path, "label", l)?;
            check_binding(path, bind.as_ref())?;
        }
        NodeKind::Metric {
            label: l,
            value,
            delta,
            caption,
            bind,
            ..
        } => {
            label(path, "label", l)?;
            label(path, "value", value)?;
            if let Some(delta) = delta {
                label(path, "delta", delta)?;
            }
            if let Some(caption) = caption {
                label(path, "caption", caption)?;
            }
            check_binding(path, bind.as_ref())?;
        }
        NodeKind::Progress {
            value,
            max,
            label: l,
            ..
        } => {
            if !value.is_finite() || !max.is_finite() || *max <= 0.0 || *value < 0.0 {
                return Err(SurfaceError::new(
                    "invalid_value",
                    "progress needs 0 <= value and max > 0",
                    path,
                ));
            }
            if let Some(l) = l {
                label(path, "label", l)?;
            }
        }
        NodeKind::Table { columns, rows } => {
            if columns.is_empty() || columns.len() > MAX_TABLE_COLUMNS {
                return Err(SurfaceError::new(
                    "invalid_value",
                    format!("a table has 1-{MAX_TABLE_COLUMNS} columns"),
                    path,
                ));
            }
            if rows.len() > MAX_TABLE_ROWS {
                return Err(SurfaceError::new(
                    "too_many_children",
                    format!("a table holds at most {MAX_TABLE_ROWS} rows"),
                    path,
                ));
            }
            for column in columns {
                label(path, "column", column)?;
            }
            for row in rows {
                if row.len() > columns.len() {
                    return Err(SurfaceError::new(
                        "invalid_value",
                        "a row has more cells than the table has columns",
                        path,
                    ));
                }
                for cell in row {
                    text(path, "cell", cell.text(), MAX_LABEL_BYTES * 2)?;
                    if let Some(link) = cell.link() {
                        url(path, link)?;
                    }
                }
            }
            // Cells cost layout like small components do.
            checker.count(path, rows.len() * columns.len() / 4)?;
        }
        NodeKind::KvList { items } => {
            children(path, items.len())?;
            checker.count(path, items.len())?;
            for item in items {
                label(path, "key", &item.key)?;
                label(path, "value", &item.value)?;
                if let Some(link) = &item.link {
                    url(path, link)?;
                }
            }
        }
        NodeKind::Image { src, alt, height } => {
            image_src(path, src)?;
            if let Some(alt) = alt {
                label(path, "alt", alt)?;
            }
            if height.is_some_and(|h| !(16.0..=MAX_IMAGE_HEIGHT).contains(&h)) {
                return Err(SurfaceError::new(
                    "invalid_value",
                    format!("image height is 16-{MAX_IMAGE_HEIGHT}"),
                    path,
                ));
            }
        }
        NodeKind::Checklist { title, items, .. } => {
            require_id(path, node)?;
            if let Some(title) = title {
                label(path, "title", title)?;
            }
            children(path, items.len())?;
            checker.count(path, items.len())?;
            let mut seen = HashSet::new();
            for item in items {
                if !is_valid_id(&item.id) {
                    return Err(SurfaceError::new(
                        "invalid_id",
                        format!("item id {:?} is not valid", item.id),
                        path,
                    ));
                }
                if !seen.insert(item.id.as_str()) {
                    return Err(SurfaceError::new(
                        "duplicate_id",
                        format!("item id {:?} is used twice in this checklist", item.id),
                        path,
                    ));
                }
                text(path, "item text", &item.text, MAX_LABEL_BYTES * 2)?;
                if let Some(detail) = &item.detail {
                    label(path, "item detail", detail)?;
                }
                if let Some(link) = &item.link {
                    url(path, link)?;
                }
            }
        }
        NodeKind::Link {
            url: u,
            label: l,
            detail,
        } => {
            url(path, u)?;
            if let Some(l) = l {
                label(path, "label", l)?;
            }
            if let Some(detail) = detail {
                label(path, "detail", detail)?;
            }
        }
        NodeKind::Button {
            label: l, action, ..
        } => {
            require_id(path, node)?;
            label(path, "label", l)?;
            action::check_static(action)
                .map_err(|denied| SurfaceError::new("invalid_action", denied.to_string(), path))?;
        }
        NodeKind::Toggle { label: l, .. } => {
            require_id(path, node)?;
            label(path, "label", l)?;
        }
        NodeKind::Select {
            label: l,
            options,
            default,
            ..
        } => {
            require_id(path, node)?;
            if let Some(l) = l {
                label(path, "label", l)?;
            }
            if options.is_empty() {
                return Err(SurfaceError::new(
                    "invalid_value",
                    "a select needs at least one option",
                    path,
                ));
            }
            children(path, options.len())?;
            for option in options {
                label(path, "option value", &option.value)?;
                label(path, "option label", &option.label)?;
            }
            if let Some(default) = default
                && !options.iter().any(|option| &option.value == default)
            {
                return Err(SurfaceError::new(
                    "invalid_value",
                    format!("default {default:?} is not one of the options"),
                    path,
                ));
            }
        }
        NodeKind::Session { session, label: l } => {
            reference(path, "session", session)?;
            if let Some(l) = l {
                label(path, "label", l)?;
            }
        }
    }
    Ok(())
}

fn check_binding(path: &str, bind: Option<&super::node::Binding>) -> Result<(), SurfaceError> {
    use super::node::Binding;
    match bind {
        Some(Binding::NoteTodos { note }) => reference(path, "note", note),
        Some(binding) => match binding.session() {
            Some(session) => reference(path, "session", session),
            None => Ok(()),
        },
        None => Ok(()),
    }
}

/// Images are web pictures or files kept beside the surfaces
/// (`assets/<name>`), never an arbitrary path on disk.
fn image_src(path: &str, src: &str) -> Result<(), SurfaceError> {
    let web =
        matches!(link::scheme(src).as_deref(), Some("https" | "http")) && link::is_allowed(src);
    let asset = src
        .strip_prefix("assets/")
        .is_some_and(|name| is_valid_ref(name) && !name.contains('/'));
    if web || asset {
        Ok(())
    } else {
        Err(SurfaceError::new(
            "invalid_url",
            "an image is an http(s) URL or assets/<file> beside the surfaces",
            path,
        ))
    }
}

/// Checks state against the tree: bounded, and naming only stateful
/// components that exist.
pub fn validate_state(state: &SurfaceState) -> Result<(), SurfaceError> {
    if state.len() > MAX_STATE_ENTRIES {
        return Err(SurfaceError::new(
            "too_much_state",
            format!("a surface remembers at most {MAX_STATE_ENTRIES} values"),
            "state",
        ));
    }
    for value in state.selects.values() {
        label("state", "choice", value)?;
    }
    Ok(())
}

/// Every key the tree's stateful components may hold in [`SurfaceState`]:
/// `checklist/item` for ticks, the node id for toggles and selects.
pub fn state_keys(root: &[Node]) -> (HashSet<String>, HashSet<String>, HashSet<String>) {
    let mut checks = HashSet::new();
    let mut toggles = HashSet::new();
    let mut selects = HashSet::new();
    walk(root, &mut |node, _| {
        let Some(id) = &node.id else { return };
        match &node.kind {
            NodeKind::Checklist { items, .. } => {
                for item in items {
                    checks.insert(super::check_key(id, &item.id));
                }
            }
            NodeKind::Toggle { .. } => {
                toggles.insert(id.clone());
            }
            NodeKind::Select { .. } => {
                selects.insert(id.clone());
            }
            _ => {}
        }
    });
    (checks, toggles, selects)
}
