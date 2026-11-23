//! Links, generically: a scheme gate, Diri's own `diri://` targets, and the
//! host of a web link. Nothing here knows any vendor; how a link looks
//! (its icon, its badge word) is the agent's to say on the component.

use serde::Serialize;

/// Schemes a surface may link to or open. Anything else (`file:`,
/// `javascript:`, `data:`, an app's private scheme) is refused when the
/// surface is written and again when the person clicks. Web links to any
/// service work: macOS hands them to its app if one claims them.
pub const ALLOWED_SCHEMES: &[&str] = &["https", "http", "mailto", "diri"];

/// What a link points at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LinkKind {
    DiriSession { session: String },
    DiriNote { note: String },
    DiriSurface { surface: String },
    Mail,
    Web { host: String },
}

impl LinkKind {
    /// A label for a link the agent left unlabelled.
    pub fn default_label(&self) -> String {
        match self {
            Self::DiriSession { session } => format!("Session {session}"),
            Self::DiriNote { note } => format!("Note {note}"),
            Self::DiriSurface { surface } => format!("Surface {surface}"),
            Self::Mail => "Email".into(),
            Self::Web { host } => host.clone(),
        }
    }

    /// Whether clicking stays inside Diri.
    pub fn is_internal(&self) -> bool {
        matches!(
            self,
            Self::DiriSession { .. } | Self::DiriNote { .. } | Self::DiriSurface { .. }
        )
    }
}

/// The scheme of `url`, lowercased, when it has a well-formed one.
pub fn scheme(url: &str) -> Option<String> {
    let (scheme, _) = url.split_once(':')?;
    let valid = !scheme.is_empty()
        && scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then(|| scheme.to_ascii_lowercase())
}

/// Whether a surface may link to `url`: an allowed scheme, no control
/// characters or whitespace, and for web links a host.
pub fn is_allowed(url: &str) -> bool {
    if url.is_empty() || url.len() > super::limits::MAX_URL_BYTES {
        return false;
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return false;
    }
    let Some(scheme) = scheme(url) else {
        return false;
    };
    if !ALLOWED_SCHEMES.contains(&scheme.as_str()) {
        return false;
    }
    match scheme.as_str() {
        "http" | "https" => host(url).is_some_and(|host| !host.is_empty()),
        "diri" => diri_target(url).is_some(),
        _ => true,
    }
}

fn host(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    // Drop credentials; a link never carries them into a label.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    Some(host.split(':').next().unwrap_or(host))
}

/// `diri://session/<id>`, `diri://note/<id>` and `diri://surface/<id>`.
fn diri_target(url: &str) -> Option<LinkKind> {
    let rest = url.strip_prefix("diri://")?;
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (kind, id) = rest.split_once('/')?;
    let id = id.trim_end_matches('/');
    if !super::limits::is_valid_ref(id) {
        return None;
    }
    match kind {
        "session" => Some(LinkKind::DiriSession {
            session: id.to_owned(),
        }),
        "note" => Some(LinkKind::DiriNote { note: id.to_owned() }),
        "surface" => Some(LinkKind::DiriSurface {
            surface: id.to_owned(),
        }),
        _ => None,
    }
}

/// What `url` points at. Call [`is_allowed`] first; a refused URL reads as
/// a plain web link here.
pub fn classify(url: &str) -> LinkKind {
    match scheme(url).as_deref() {
        Some("diri") => diri_target(url).unwrap_or(LinkKind::Web {
            host: "diri".into(),
        }),
        Some("mailto") => LinkKind::Mail,
        _ => LinkKind::Web {
            host: host(url)
                .unwrap_or_default()
                .to_ascii_lowercase()
                .trim_start_matches("www.")
                .to_owned(),
        },
    }
}
