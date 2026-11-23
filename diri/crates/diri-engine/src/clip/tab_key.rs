//! Canonical page keys for per-tab Session memory.
//!
//! The key answers "is this the same page/conversation the person gave
//! feedback from last time?" It must be deterministic and idempotent:
//! normalizing a key's URL again yields the same key (`formal/ContextRing.lean`
//! models the query and path rules). Credentials in a URL never reach a key.
//!
//! Rules, in order:
//! * only `http`/`https` URLs have URL keys; scheme and host are lowercased,
//!   `www.` and a trailing dot are dropped, default ports are dropped;
//! * any loopback host is one dev server: `http://localhost:<port><path>`,
//!   without query or fragment (they change on every click);
//! * known sites collapse sub-views onto the thing being discussed: a Slack
//!   channel or thread, a GitHub PR or issue (not its `/files` tab), a Linear
//!   issue (not its slug), a Figma file, a Google doc;
//! * elsewhere tracking parameters are dropped and the rest sorted; a
//!   fragment survives only as a hash route (`#/…`).
//!
//! A native app without a URL is keyed by bundle id and window title.

use diri_proto::clip::ClipSource;

/// Query parameters that never identify a page.
const TRACKING: &[&str] = &[
    "fbclid", "gclid", "dclid", "msclkid", "yclid", "mc_cid", "mc_eid", "igshid", "si", "ref",
    "ref_src", "_hsenc", "_hsmi", "mkt_tok", "_ga", "_gl",
];

fn is_tracking(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("utm_") || TRACKING.contains(&lower.as_str())
}

/// The page key a clip from `source` is filed under, if any.
pub fn tab_key(source: &ClipSource) -> Option<String> {
    if let Some(url) = source.url.as_deref()
        && let Some(key) = canonical_url(url)
    {
        return Some(key);
    }
    let bundle = source
        .bundle_id
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())?;
    let title = source
        .window_title
        .as_deref()
        .map(collapse_whitespace)
        .filter(|t| !t.is_empty())?;
    Some(format!("app:{}/{}", bundle.to_ascii_lowercase(), title))
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The keys to look a page up under, most specific first: the key itself,
/// then the pages it belongs to (a Slack thread's channel, a dev server's
/// origin). Recording an affinity writes every key in the chain; lookups
/// take the first that resolves, so an exact page always wins over its
/// parent.
pub fn lookup_chain(key: &str) -> Vec<String> {
    let mut chain = vec![key.to_owned()];
    if let Some(rest) = key.strip_prefix("http://localhost:") {
        let port_end = rest.find('/').unwrap_or(rest.len());
        if port_end < rest.len() {
            chain.push(format!("http://localhost:{}", &rest[..port_end]));
        }
    } else if let Some(index) = key.find("/thread/")
        && key.starts_with("https://app.slack.com/client/")
    {
        chain.push(key[..index].to_owned());
    } else if let Some((channel, _)) = key.split_once("?thread_ts=")
        && key.contains(".slack.com/archives/")
    {
        chain.push(channel.to_owned());
    }
    chain
}

struct Parts<'a> {
    scheme: String,
    host: String,
    port: Option<&'a str>,
    path: &'a str,
    query: &'a str,
    fragment: &'a str,
}

fn split(url: &str) -> Option<Parts<'_>> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let (rest, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    // Credentials never become part of a key.
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let (host, after) = stripped.split_once(']')?;
        (
            format!("[{}]", host.to_ascii_lowercase()),
            after.strip_prefix(':'),
        )
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_ascii_lowercase(), Some(port)),
            None => (authority.to_ascii_lowercase(), None),
        }
    };
    let host = host.trim_end_matches('.').to_owned();
    if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let port = port.filter(|p| !p.is_empty());
    if port.is_some_and(|p| !p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    Some(Parts {
        scheme,
        host,
        port,
        path,
        query,
        fragment,
    })
}

fn is_loopback(host: &str) -> bool {
    matches!(
        host,
        "localhost" | "127.0.0.1" | "0.0.0.0" | "[::1]" | "[::]"
    ) || host.ends_with(".localhost")
}

/// `//a///b/` → `/a/b`; `/` → ``.
pub fn normalize_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        out.push_str(segment);
    }
    out
}

/// Drops tracking and empty parameters, then sorts by name and value so the
/// same page reached through two links has one key.
pub fn normalize_query(query: &str) -> String {
    let mut pairs: Vec<(&str, &str)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
        .filter(|(name, _)| !name.is_empty() && !is_tracking(name))
        .collect();
    pairs.sort_unstable();
    pairs.dedup();
    pairs
        .into_iter()
        .map(|(name, value)| {
            if value.is_empty() {
                name.to_owned()
            } else {
                format!("{name}={value}")
            }
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// A known site's canonical path, when it has one; `None` falls through
/// to the generic rules.
fn site_path(host: &str, path: &str, query: &str) -> Option<(String, String)> {
    let parts = segments(path);
    match host {
        "app.slack.com" if parts.first() == Some(&"client") && parts.len() >= 3 => {
            let mut keep = parts[..3].join("/");
            if let Some(index) = parts.iter().position(|p| *p == "thread")
                && let Some(thread) = parts.get(index + 1)
            {
                keep.push_str("/thread/");
                keep.push_str(thread);
            }
            Some((format!("/{keep}"), String::new()))
        }
        _ if host.ends_with(".slack.com") && parts.first() == Some(&"archives") => {
            let keep = parts.iter().take(3).copied().collect::<Vec<_>>().join("/");
            let thread = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("thread_ts="))
                .map(|ts| format!("thread_ts={ts}"))
                .unwrap_or_default();
            Some((format!("/{keep}"), thread))
        }
        "github.com"
            if parts.len() >= 4 && matches!(parts[2], "pull" | "issues" | "discussions") =>
        {
            Some((format!("/{}", parts[..4].join("/")), String::new()))
        }
        "linear.app" if parts.len() >= 3 && parts[1] == "issue" => {
            Some((format!("/{}", parts[..3].join("/")), String::new()))
        }
        "figma.com"
            if parts.len() >= 2
                && matches!(parts[0], "file" | "design" | "proto" | "board" | "slides") =>
        {
            Some((format!("/design/{}", parts[1]), String::new()))
        }
        "docs.google.com" if parts.len() >= 3 && parts[1] == "d" => {
            Some((format!("/{}", parts[..3].join("/")), String::new()))
        }
        _ => None,
    }
}

/// The canonical form of an `http(s)` URL, or `None` for anything else.
pub fn canonical_url(url: &str) -> Option<String> {
    let parts = split(url)?;
    if is_loopback(&parts.host) {
        let port = parts.port.map(|p| p.trim_start_matches('0')).unwrap_or("");
        let port = if port.is_empty() {
            if parts.scheme == "https" { "443" } else { "80" }
        } else {
            port
        };
        return Some(format!(
            "http://localhost:{port}{}",
            normalize_path(parts.path)
        ));
    }
    let host = parts.host.strip_prefix("www.").unwrap_or(&parts.host);
    let default_port = if parts.scheme == "https" { "443" } else { "80" };
    let port = parts
        .port
        .map(|p| p.trim_start_matches('0'))
        .filter(|p| !p.is_empty() && *p != default_port);
    let authority = match port {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    let (path, query, fragment) = match site_path(host, parts.path, parts.query) {
        Some((path, query)) => (path, query, String::new()),
        None => {
            let fragment = if parts.fragment.starts_with('/') || parts.fragment.starts_with("!/") {
                format!("#{}", parts.fragment)
            } else {
                String::new()
            };
            (
                normalize_path(parts.path),
                normalize_query(parts.query),
                fragment,
            )
        }
    };
    let mut key = format!("{}://{authority}{path}", parts.scheme);
    if !query.is_empty() {
        key.push('?');
        key.push_str(&query);
    }
    key.push_str(&fragment);
    Some(key)
}

/// The `localhost` port a key points at, for dev-server correlation.
pub fn localhost_port(key: &str) -> Option<u16> {
    let rest = key.strip_prefix("http://localhost:")?;
    rest.split('/').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(url: &str) -> String {
        canonical_url(url).unwrap_or_else(|| panic!("{url} has no key"))
    }

    #[test]
    fn generic_urls_drop_noise_and_sort_queries() {
        assert_eq!(
            key("HTTPS://WWW.Example.com:443//docs///intro/?utm_source=x&b=2&a=1#section"),
            "https://example.com/docs/intro?a=1&b=2"
        );
        assert_eq!(key("http://example.com:8080/"), "http://example.com:8080");
        assert_eq!(key("https://example.com/?fbclid=1"), "https://example.com");
        assert_eq!(
            key("https://a.io/app#/settings"),
            "https://a.io/app#/settings"
        );
    }

    #[test]
    fn credentials_never_reach_a_key() {
        assert_eq!(
            key("https://user:secret@example.com/x"),
            "https://example.com/x"
        );
    }

    #[test]
    fn every_loopback_host_is_one_dev_server() {
        for url in [
            "http://localhost:3000/settings?tab=2#x",
            "http://127.0.0.1:3000/settings/",
            "https://0.0.0.0:3000//settings",
            "http://[::1]:3000/settings",
            "http://app.localhost:3000/settings",
        ] {
            assert_eq!(key(url), "http://localhost:3000/settings", "{url}");
        }
        assert_eq!(key("http://localhost/"), "http://localhost:80");
        assert_eq!(localhost_port("http://localhost:3000/settings"), Some(3000));
        assert_eq!(localhost_port("https://example.com"), None);
    }

    #[test]
    fn slack_channels_and_threads() {
        assert_eq!(
            key("https://app.slack.com/client/T01/C02/thread/C02-1700000000.123456?cdn_fallback=1"),
            "https://app.slack.com/client/T01/C02/thread/C02-1700000000.123456"
        );
        assert_eq!(
            key("https://app.slack.com/client/T01/C02/details/top"),
            "https://app.slack.com/client/T01/C02"
        );
        assert_eq!(
            key("https://acme.slack.com/archives/C02/p1700000000123456?thread_ts=1700.1&cid=C02"),
            "https://acme.slack.com/archives/C02/p1700000000123456?thread_ts=1700.1"
        );
    }

    #[test]
    fn known_sites_collapse_sub_views() {
        assert_eq!(
            key("https://github.com/o/r/pull/12/files?w=1"),
            "https://github.com/o/r/pull/12"
        );
        assert_eq!(
            key("https://github.com/o/r/issues/3#issuecomment-1"),
            "https://github.com/o/r/issues/3"
        );
        assert_eq!(
            key("https://linear.app/acme/issue/ENG-42/fix-the-padding"),
            "https://linear.app/acme/issue/ENG-42"
        );
        assert_eq!(
            key("https://www.figma.com/file/AbC123/Design?node-id=1-2"),
            "https://figma.com/design/AbC123"
        );
        assert_eq!(
            key("https://docs.google.com/document/d/XYZ/edit#heading=h.1"),
            "https://docs.google.com/document/d/XYZ"
        );
    }

    #[test]
    fn non_web_urls_have_no_url_key() {
        assert_eq!(canonical_url("file:///tmp/x.html"), None);
        assert_eq!(canonical_url("about:blank"), None);
        assert_eq!(canonical_url("chrome://settings"), None);
        assert_eq!(canonical_url("https://bad host/x"), None);
        assert_eq!(canonical_url("http://example.com:abc/"), None);
    }

    #[test]
    fn canonicalization_is_idempotent() {
        for url in [
            "HTTPS://WWW.Example.com:443//docs///intro/?utm_source=x&b=2&a=1#section",
            "http://127.0.0.1:3000/settings/?a=1",
            "https://app.slack.com/client/T01/C02/thread/C02-1.2",
            "https://acme.slack.com/archives/C02/p1?thread_ts=1.1&x=2",
            "https://github.com/o/r/pull/12/files",
            "https://www.figma.com/file/AbC123/Design?node-id=1-2",
            "https://a.io/app?z=&y=2&y=1#/route",
        ] {
            let once = key(url);
            assert_eq!(key(&once), once, "{url}");
        }
    }

    #[test]
    fn native_apps_key_by_bundle_and_window() {
        let source = ClipSource {
            bundle_id: Some("com.tinyspeck.slackmacgap".into()),
            window_title: Some("  design-review  (Acme) - Slack ".into()),
            ..Default::default()
        };
        assert_eq!(
            tab_key(&source).as_deref(),
            Some("app:com.tinyspeck.slackmacgap/design-review (Acme) - Slack")
        );
        let browser = ClipSource {
            url: Some("https://example.com/a?utm_campaign=1".into()),
            bundle_id: Some("company.thebrowser.Browser".into()),
            window_title: Some("A".into()),
            ..Default::default()
        };
        assert_eq!(tab_key(&browser).as_deref(), Some("https://example.com/a"));
        assert_eq!(tab_key(&ClipSource::default()), None);
    }

    #[test]
    fn lookup_chains_fall_back_to_the_parent_page() {
        assert_eq!(
            lookup_chain("http://localhost:3000/settings"),
            ["http://localhost:3000/settings", "http://localhost:3000"]
        );
        assert_eq!(
            lookup_chain("http://localhost:3000"),
            ["http://localhost:3000"]
        );
        assert_eq!(
            lookup_chain("https://app.slack.com/client/T/C/thread/C-1.2"),
            [
                "https://app.slack.com/client/T/C/thread/C-1.2",
                "https://app.slack.com/client/T/C"
            ]
        );
        assert_eq!(
            lookup_chain("https://acme.slack.com/archives/C/p1?thread_ts=1.1"),
            [
                "https://acme.slack.com/archives/C/p1?thread_ts=1.1",
                "https://acme.slack.com/archives/C/p1"
            ]
        );
        assert_eq!(
            lookup_chain("https://example.com/a"),
            ["https://example.com/a"]
        );
    }
}
