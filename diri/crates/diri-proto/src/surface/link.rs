//! Rich links: what a URL points at, so a surface can badge it (a Slack
//! thread, a GitHub pull request, a Linear issue, a Diri session) instead of
//! printing the raw address.

use serde::Serialize;

/// Schemes a surface may link to or open. Anything else (`file:`,
/// `javascript:`, `data:`, an arbitrary app scheme) is refused when the
/// surface is written and again when the person clicks.
pub const ALLOWED_SCHEMES: &[&str] = &[
    "https", "http", "diri", "slack", "linear", "notion", "figma", "mailto",
];

/// What a link points at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LinkKind {
    Slack,
    GithubPull { repo: String, number: u64 },
    GithubIssue { repo: String, number: u64 },
    Github { path: String },
    Linear { key: Option<String> },
    Notion,
    Figma,
    DiriSession { session: String },
    DiriNote { note: String },
    DiriSurface { surface: String },
    Mail,
    Web { host: String },
}

impl LinkKind {
    /// The badge text a link carries before its label.
    pub fn badge(&self) -> &'static str {
        match self {
            Self::Slack => "Slack",
            Self::GithubPull { .. } => "PR",
            Self::GithubIssue { .. } => "Issue",
            Self::Github { .. } => "GitHub",
            Self::Linear { .. } => "Linear",
            Self::Notion => "Notion",
            Self::Figma => "Figma",
            Self::DiriSession { .. } => "Session",
            Self::DiriNote { .. } => "Note",
            Self::DiriSurface { .. } => "Surface",
            Self::Mail => "Mail",
            Self::Web { .. } => "Link",
        }
    }

    /// A label for a link the agent left unlabelled.
    pub fn default_label(&self) -> String {
        match self {
            Self::GithubPull { repo, number } | Self::GithubIssue { repo, number } => {
                format!("{repo}#{number}")
            }
            Self::Github { path } => path.clone(),
            Self::Linear { key: Some(key) } => key.clone(),
            Self::Linear { key: None } => "Linear".into(),
            Self::Slack => "Slack thread".into(),
            Self::Notion => "Notion page".into(),
            Self::Figma => "Figma file".into(),
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
        && scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
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
    // Drop credentials; a link never carries them into a badge.
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
        "note" => Some(LinkKind::DiriNote {
            note: id.to_owned(),
        }),
        "surface" => Some(LinkKind::DiriSurface {
            surface: id.to_owned(),
        }),
        _ => None,
    }
}

/// What `url` points at. Call [`is_allowed`] first; a refused URL reads as
/// a plain web link here.
pub fn classify(url: &str) -> LinkKind {
    let scheme = scheme(url).unwrap_or_default();
    match scheme.as_str() {
        "diri" => {
            return diri_target(url).unwrap_or(LinkKind::Web {
                host: "diri".into(),
            });
        }
        "slack" => return LinkKind::Slack,
        "linear" => {
            return LinkKind::Linear {
                key: linear_key(url),
            };
        }
        "notion" => return LinkKind::Notion,
        "figma" => return LinkKind::Figma,
        "mailto" => return LinkKind::Mail,
        _ => {}
    }
    let host = host(url).unwrap_or_default().to_ascii_lowercase();
    let path: Vec<&str> = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or_default()
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .split('/')
        .skip(1)
        .filter(|segment| !segment.is_empty())
        .collect();
    if host == "slack.com" || host.ends_with(".slack.com") {
        return LinkKind::Slack;
    }
    if host == "linear.app" {
        return LinkKind::Linear {
            key: linear_key(url),
        };
    }
    if host == "notion.so" || host.ends_with(".notion.so") || host.ends_with(".notion.site") {
        return LinkKind::Notion;
    }
    if host == "figma.com" || host.ends_with(".figma.com") {
        return LinkKind::Figma;
    }
    if host == "github.com" || host == "www.github.com" {
        if let [owner, repo, kind, number, ..] = path.as_slice()
            && let Ok(number) = number.parse::<u64>()
        {
            let repo = format!("{owner}/{repo}");
            match *kind {
                "pull" => return LinkKind::GithubPull { repo, number },
                "issues" => return LinkKind::GithubIssue { repo, number },
                _ => {}
            }
        }
        return LinkKind::Github {
            path: path.iter().take(2).copied().collect::<Vec<_>>().join("/"),
        };
    }
    LinkKind::Web {
        host: host.trim_start_matches("www.").to_owned(),
    }
}

/// `ENG-123` from a Linear URL's `/issue/ENG-123/...` path.
fn linear_key(url: &str) -> Option<String> {
    let mut segments = url.split(['/', '?', '#']);
    segments.find(|segment| *segment == "issue")?;
    let key = segments.next()?;
    let (team, number) = key.split_once('-')?;
    let valid = !team.is_empty()
        && team.chars().all(|c| c.is_ascii_alphanumeric())
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit());
    valid.then(|| key.to_ascii_uppercase())
}
