//! Extracts and classifies URLs (PRs, Linear issues, previews, plain links)
//! from a session's screen text.
//!
//! Ported from `ArtifactScanner`. Input is the ANSI-free joined visible-lines
//! output, so escape sequences never split a URL. Ordered specific → generic;
//! the first classifying match per text range wins, and a URL already claimed
//! by a specific rule never re-matches as a generic link.

use std::sync::LazyLock;

use diri_proto::{ArtifactKind, DateMillis, SessionArtifact};
use regex::Regex;

pub const MAX_ARTIFACTS: usize = 50;

static RULES: LazyLock<Vec<(ArtifactKind, Regex)>> = LazyLock::new(|| {
    let rule = |kind: ArtifactKind, pattern: &str| {
        (
            kind,
            Regex::new(&format!("(?i){pattern}")).expect("artifact pattern"),
        )
    };
    vec![
        rule(
            ArtifactKind::PullRequest,
            r"(https?://)?github\.com/[\w.-]+/[\w.-]+/pull/\d+",
        ),
        rule(
            ArtifactKind::LinearIssue,
            r"(https?://)?linear\.app/[\w-]+/issue/[A-Za-z][A-Za-z0-9]*-\d+(/[\w-]+)?",
        ),
        rule(
            ArtifactKind::Preview,
            r#"https?://(localhost|127\.0\.0\.1):\d+[^\s"'\)\]]*"#,
        ),
        rule(
            ArtifactKind::Preview,
            r#"https?://[^\s"'\)\]]+\.(vercel\.app|ngrok[-.][^\s"'\)\]]+)[^\s"'\)\]]*"#,
        ),
        rule(ArtifactKind::Link, r#"https?://[^\s"'\)\]]+"#),
    ]
});

/// Scans `text`, merges with `existing` (preserving each URL's original
/// `firstSeenAt`), dedupes by URL, and caps at [`MAX_ARTIFACTS`] — the oldest
/// entries drop first when over the cap.
pub fn scan(text: &str, existing: &[SessionArtifact], now: DateMillis) -> Vec<SessionArtifact> {
    let mut order: Vec<String> = Vec::new();
    let mut by_url: std::collections::HashMap<String, SessionArtifact> =
        std::collections::HashMap::new();
    for artifact in existing {
        if !by_url.contains_key(&artifact.url) {
            order.push(artifact.url.clone());
            by_url.insert(artifact.url.clone(), artifact.clone());
        }
    }

    let mut claimed: Vec<std::ops::Range<usize>> = Vec::new();
    for (kind, regex) in RULES.iter() {
        for found in regex.find_iter(text) {
            let range = found.range();
            if claimed
                .iter()
                .any(|prior| prior.start < range.end && range.start < prior.end)
            {
                continue;
            }
            claimed.push(range);
            let Some(url) = normalize(found.as_str()) else {
                continue;
            };
            if !by_url.contains_key(&url) {
                order.push(url.clone());
                by_url.insert(
                    url.clone(),
                    SessionArtifact {
                        kind: *kind,
                        url,
                        first_seen_at: now,
                    },
                );
            }
        }
    }

    let mut result: Vec<SessionArtifact> = order
        .into_iter()
        .filter_map(|url| by_url.remove(&url))
        .collect();
    if result.len() > MAX_ARTIFACTS {
        // Keep the newest by first-seen time, stable for ties via order.
        let mut indexed: Vec<(usize, SessionArtifact)> = result.into_iter().enumerate().collect();
        indexed.sort_by(|a, b| {
            a.1.first_seen_at
                .0
                .partial_cmp(&b.1.first_seen_at.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        let mut kept: Vec<(usize, SessionArtifact)> =
            indexed.split_off(indexed.len() - MAX_ARTIFACTS);
        kept.sort_by_key(|(index, _)| *index);
        result = kept.into_iter().map(|(_, artifact)| artifact).collect();
    }
    result
}

/// Strips the trailing punctuation terminals love to append and guarantees a
/// scheme (bare `github.com/…` matches get `https://`).
pub fn normalize(raw: &str) -> Option<String> {
    let mut url = raw;
    while let Some(last) = url.chars().last() {
        if ".,;:!?)]}>'\"`".contains(last) {
            url = &url[..url.len() - last.len_utf8()];
        } else {
            break;
        }
    }
    if url.is_empty() {
        return None;
    }
    let lower = url.to_lowercase();
    Some(
        if lower.starts_with("http://") || lower.starts_with("https://") {
            url.to_string()
        } else {
            format!("https://{url}")
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_fresh(text: &str) -> Vec<SessionArtifact> {
        scan(text, &[], DateMillis(1000.0))
    }

    #[test]
    fn urls_classify_specific_over_generic() {
        let text = "PR at https://github.com/o/r/pull/42 and docs https://example.com/page \
                    with preview http://localhost:3000/app and linear.app/team/issue/ABC-7";
        let found = scan_fresh(text);
        let kind_of = |needle: &str| {
            found
                .iter()
                .find(|artifact| artifact.url.contains(needle))
                .map(|artifact| artifact.kind)
        };
        assert_eq!(kind_of("pull/42"), Some(ArtifactKind::PullRequest));
        assert_eq!(kind_of("example.com"), Some(ArtifactKind::Link));
        assert_eq!(kind_of("localhost:3000"), Some(ArtifactKind::Preview));
        assert_eq!(kind_of("ABC-7"), Some(ArtifactKind::LinearIssue));
        assert_eq!(found.len(), 4, "{found:?}");
    }

    #[test]
    fn trailing_punctuation_is_stripped_and_schemes_added() {
        let found = scan_fresh("see (github.com/o/r/pull/9).");
        assert_eq!(found[0].url, "https://github.com/o/r/pull/9");
    }

    #[test]
    fn existing_artifacts_keep_their_first_seen_time() {
        let first = scan("https://github.com/o/r/pull/1", &[], DateMillis(500.0));
        let merged = scan(
            "https://github.com/o/r/pull/1 again",
            &first,
            DateMillis(900.0),
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].first_seen_at, DateMillis(500.0));
    }

    #[test]
    fn the_cap_drops_oldest_first() {
        let mut existing = Vec::new();
        for n in 0..MAX_ARTIFACTS {
            existing.push(SessionArtifact {
                kind: ArtifactKind::Link,
                url: format!("https://example.com/{n}"),
                first_seen_at: DateMillis(n as f64),
            });
        }
        let result = scan("https://example.com/newest", &existing, DateMillis(9999.0));
        assert_eq!(result.len(), MAX_ARTIFACTS);
        assert!(
            result
                .iter()
                .any(|artifact| artifact.url.ends_with("newest"))
        );
        assert!(
            !result.iter().any(|artifact| artifact.url.ends_with("/0")),
            "the oldest entry is the one dropped"
        );
    }
}
