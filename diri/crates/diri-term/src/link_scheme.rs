//! Which link schemes a terminal click may hand to the system.
//!
//! Web links open directly. Every other scheme (`ssh://`, `mailto:`,
//! `vscode://`, …) is an *app link*: it starts another program, so the host
//! asks before opening one. Schemes that run code in a browser or read local
//! data (`javascript:`, `data:`, …) are never links at all.

/// Schemes never offered as links, whatever the program printed. `file` is
/// absent because file references have their own path (and their own
/// executable check in the host).
const BLOCKED: &[&str] = &[
    "javascript",
    "vbscript",
    "livescript",
    "data",
    "blob",
    "about",
    "view-source",
    "jar",
    "filesystem",
    "mhtml",
];

/// The kind of a link a click would open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    /// `http`/`https`: opens in the browser without asking.
    Web,
    /// Any other allowed scheme: opens another app, so it asks first.
    App,
}

/// The URI's scheme, lowercased: a letter, then letters, digits, `+`, `-`
/// or `.`, two to 32 characters, before a `:`. A Windows drive letter
/// (`C:`) is too short to be one.
#[must_use]
pub fn scheme(uri: &str) -> Option<String> {
    let (scheme, _) = uri.split_once(':')?;
    let mut chars = scheme.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        && (2..=32).contains(&scheme.len());
    valid.then(|| scheme.to_ascii_lowercase())
}

#[must_use]
pub fn is_blocked(scheme: &str) -> bool {
    BLOCKED.contains(&scheme)
}

/// What clicking `uri` would open, or `None` for a URI that is not a link.
#[must_use]
pub fn kind(uri: &str) -> Option<LinkKind> {
    let scheme = scheme(uri)?;
    if is_blocked(&scheme) || scheme == "file" {
        return None;
    }
    Some(if scheme == "http" || scheme == "https" {
        LinkKind::Web
    } else {
        LinkKind::App
    })
}

/// An app link in `uri`, or `None`. Plain output text needs `scheme://`
/// (or `mailto:` with an address) so that prose like `note: fix` or
/// `localhost:3000` never becomes a link; a program's explicit OSC 8 link
/// only needs a scheme and something after it.
#[must_use]
pub fn app_link(uri: &str, explicit: bool) -> Option<&str> {
    if kind(uri)? != LinkKind::App {
        return None;
    }
    let scheme = scheme(uri)?;
    let rest = &uri[scheme.len() + 1..];
    if rest.is_empty() || uri.chars().any(char::is_control) {
        return None;
    }
    let plain_ok =
        rest.starts_with("//") && rest.len() > 2 || scheme == "mailto" && rest.contains('@');
    (explicit || plain_ok).then_some(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_web_app_and_refused_schemes() {
        assert_eq!(kind("https://example.com"), Some(LinkKind::Web));
        assert_eq!(kind("HTTP://example.com"), Some(LinkKind::Web));
        for app in [
            "ssh://forge",
            "mailto:me@example.com",
            "vscode://file/a.rs:1",
            "cursor://file/x",
            "x-github-client://openRepo/https://github.com/a/b",
            "zed://file/x",
        ] {
            assert_eq!(kind(app), Some(LinkKind::App), "{app}");
        }
        for refused in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "data:text/html,<script>",
            "vbscript:msgbox",
            "blob:https://x/1",
            "file:///Applications/Calculator.app",
            "C:\\Windows",
            "no scheme",
        ] {
            assert_eq!(kind(refused), None, "{refused}");
        }
    }

    #[test]
    fn plain_text_needs_a_double_slash_or_a_mail_address() {
        assert_eq!(app_link("ssh://forge", false), Some("ssh://forge"));
        assert_eq!(
            app_link("mailto:me@example.com", false),
            Some("mailto:me@example.com")
        );
        for prose in ["note:fix", "localhost:3000", "mailto:", "ssh://", "tel:123"] {
            assert_eq!(app_link(prose, false), None, "{prose}");
        }
        // A program's own OSC 8 link is explicit.
        assert_eq!(app_link("tel:123", true), Some("tel:123"));
        assert_eq!(app_link("javascript:alert(1)", true), None);
        assert_eq!(app_link("ssh://a\u{7}b", true), None);
    }
}
