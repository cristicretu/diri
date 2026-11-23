//! Path portability: `~/` forms for export, prefix rules for import.

use std::path::Path;

/// `path` with a leading `home` replaced by `~` (whole components only).
pub fn to_portable(home: &str, path: &str) -> String {
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return path.to_owned();
    }
    if path == home {
        return "~".to_owned();
    }
    match path.strip_prefix(home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_owned(),
    }
}

/// Expands a leading `~` against `home`.
pub fn expand(home: &str, path: &str) -> String {
    let home = home.trim_end_matches('/');
    if path == "~" {
        return home.to_owned();
    }
    match path.strip_prefix("~/") {
        Some(rest) => format!("{home}/{rest}"),
        None => path.to_owned(),
    }
}

/// Replaces a whole-component prefix `from` of `path` with `to`.
pub fn replace_prefix(path: &str, from: &str, to: &str) -> Option<String> {
    let from = from.trim_end_matches('/');
    let to = to.trim_end_matches('/');
    if from.is_empty() {
        return None;
    }
    if path == from {
        return Some(to.to_owned());
    }
    let rest = path.strip_prefix(from)?;
    rest.starts_with('/').then(|| format!("{to}{rest}"))
}

/// Ordered prefix rules applied to portable (`~/`) or absolute paths. The
/// longest matching `from` wins, so a mapped project beats the home rule.
#[derive(Clone, Debug, Default)]
pub struct PathMap {
    /// The home on this Mac; `~/` expands to it.
    home: String,
    /// The exporting Mac's home: absolute paths under it are treated as `~/`.
    source_home: String,
    /// `(from, to)` in expanded or `~/` form, normalized to `~/` when under
    /// the source home.
    rules: Vec<(String, String)>,
}

impl PathMap {
    pub fn new(source_home: &str, home: &str) -> Self {
        Self {
            home: home.trim_end_matches('/').to_owned(),
            source_home: source_home.trim_end_matches('/').to_owned(),
            rules: Vec::new(),
        }
    }

    pub fn home(&self) -> &str {
        &self.home
    }

    /// Adds `from -> to`. `from` is in the exporting Mac's terms (`~/`,
    /// or absolute); `to` is in this Mac's terms (`~/` or absolute).
    pub fn add(&mut self, from: &str, to: &str) {
        let from = self.source_portable(from);
        let to = expand(&self.home, to);
        self.rules.retain(|(existing, _)| existing != &from);
        self.rules.push((from, to));
        self.rules
            .sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    }

    pub fn rules(&self) -> &[(String, String)] {
        &self.rules
    }

    fn source_portable(&self, path: &str) -> String {
        to_portable(&self.source_home, path.trim_end_matches('/'))
    }

    /// The rule that applies to `path`, if any (not counting the home rule).
    pub fn rule_for(&self, path: &str) -> Option<&(String, String)> {
        let portable = self.source_portable(path);
        self.rules
            .iter()
            .find(|(from, _)| replace_prefix(&portable, from, "").is_some())
    }

    /// Where `path` (exported, `~/` or absolute) lives on this Mac.
    pub fn remap(&self, path: &str) -> String {
        let portable = self.source_portable(path);
        for (from, to) in &self.rules {
            if let Some(mapped) = replace_prefix(&portable, from, to) {
                return mapped;
            }
        }
        expand(&self.home, &portable)
    }
}

/// Claude Code's per-project transcript directory name for `cwd`. Mirrors
/// `diri_engine::inject::claude_project_slug` (an Engine test keeps the two
/// in step).
pub fn claude_project_slug(cwd: &str) -> String {
    #[cfg(windows)]
    {
        cwd.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect()
    }
    #[cfg(not(windows))]
    cwd.replace(['/', '.'], "-")
}

/// `~/.claude/projects/<slug>/<id>.jsonl` under `home`.
pub fn claude_transcript_path(home: &Path, cwd: &str, id: &str) -> std::path::PathBuf {
    home.join(".claude/projects")
        .join(claude_project_slug(cwd))
        .join(format!("{id}.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_round_trip_respects_components() {
        assert_eq!(to_portable("/Users/al", "/Users/alice/x"), "/Users/alice/x");
        assert_eq!(to_portable("/Users/alice", "/Users/alice/x"), "~/x");
        assert_eq!(to_portable("/Users/alice/", "/Users/alice"), "~");
        assert_eq!(expand("/Users/bob", "~/x"), "/Users/bob/x");
        assert_eq!(expand("/Users/bob", "/opt/x"), "/opt/x");
    }

    #[test]
    fn longest_rule_wins_then_home() {
        let mut map = PathMap::new("/Users/alice", "/Users/bob");
        map.add("/Users/alice/work/repo", "/Users/bob/code/repo");
        map.add("~/work", "~/w");
        assert_eq!(map.remap("~/work/repo/src"), "/Users/bob/code/repo/src");
        assert_eq!(map.remap("/Users/alice/work/repo"), "/Users/bob/code/repo");
        assert_eq!(map.remap("~/work/site"), "/Users/bob/w/site");
        assert_eq!(map.remap("~/notes"), "/Users/bob/notes");
        assert_eq!(map.remap("/opt/x"), "/opt/x");
        assert_eq!(map.remap("~/work/repository"), "/Users/bob/w/repository");
    }

    #[test]
    fn claude_slug_matches_claude_code() {
        assert_eq!(
            claude_project_slug("/Users/bob/code/my.repo"),
            "-Users-bob-code-my-repo"
        );
    }
}
