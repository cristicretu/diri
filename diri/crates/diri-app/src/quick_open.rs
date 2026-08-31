//! Folder indexing and ranking for the ⌘P Quick Open surface.
//!
//! Ported from `DirectoryIndex.swift` and `QuickOpenView.swift`. Filesystem
//! walking and rank work are plain blocking functions by design so GPUI can run
//! them on its background executor; no daemon method is involved.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_proto::paths::DirijorPaths;
use serde::{Deserialize, Serialize};

use crate::fuzzy::{FuzzyMatcher, FuzzyQuery, PreparedText, Score};

pub const MAX_DEPTH: usize = 4;
pub const INDEX_CAP: usize = 20_000;
pub const RECENT_LIMIT: usize = 8;
pub const RESULT_LIMIT: usize = 50;
pub const RANK_DEBOUNCE: Duration = Duration::from_millis(25);
/// Bump when `DirectoryEntry`'s shape or the traversal's meaning changes, so a
/// new build never ranks against an index built under different rules.
pub const INDEX_CACHE_VERSION: u32 = 1;
/// How long a scan stays fresh enough to skip re-walking the filesystem.
pub const RESCAN_AFTER: Duration = Duration::from_secs(30);

const SKIP_NAMES: &[&str] = &[
    "node_modules",
    ".build",
    "dist",
    "build",
    "DerivedData",
    "Library",
    ".Trash",
    "vendor",
    "target",
    "Pods",
];

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct DirectoryEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_git_repo: bool,
    pub depth: usize,
}

/// The stale-while-revalidate cache displayed by Quick Open.
#[derive(Clone, Debug, Default)]
pub struct DirectoryIndex {
    entries: Vec<DirectoryEntry>,
    is_scanning: bool,
    scanned_at: Option<Instant>,
}

impl DirectoryIndex {
    pub fn entries(&self) -> &[DirectoryEntry] {
        &self.entries
    }

    pub const fn is_scanning(&self) -> bool {
        self.is_scanning
    }

    pub fn begin_scan(&mut self) -> bool {
        if self.is_scanning {
            return false;
        }
        self.is_scanning = true;
        true
    }

    /// A scan is worth running when nothing is indexed yet or the last one has
    /// aged out. Opening Quick Open five times in a minute should walk the
    /// filesystem once, not five times.
    pub fn needs_scan(&self, now: Instant) -> bool {
        !self.is_scanning
            && self
                .scanned_at
                .is_none_or(|at| now.duration_since(at) >= RESCAN_AFTER)
    }

    /// Adopt a disk-cached index without claiming it is freshly scanned, so the
    /// next open still revalidates.
    pub fn adopt_cached(&mut self, entries: Vec<DirectoryEntry>) {
        if self.entries.is_empty() {
            self.entries = entries;
        }
    }

    pub fn finish_scan(&mut self, entries: Vec<DirectoryEntry>, now: Instant) {
        self.entries = entries;
        self.is_scanning = false;
        self.scanned_at = Some(now);
    }
}

/// The on-disk index. Quick Open is useless until the first scan lands, and a
/// cold scan of a checkout-heavy home directory is seconds of `read_dir`; this
/// makes the *next* launch instant and revalidates behind the results.
#[derive(Debug, Deserialize, Serialize)]
pub struct IndexCache {
    pub version: u32,
    /// The roots this index was built from. Different roots, different index —
    /// a stale cache from an old `quick_open_roots` setting is worse than none.
    pub roots: Vec<PathBuf>,
    pub entries: Vec<DirectoryEntry>,
}

pub fn cache_file(home: &Path) -> PathBuf {
    DirijorPaths::quick_open_cache_file(home)
}

pub fn load_cache(path: &Path, roots: &[PathBuf]) -> Option<Vec<DirectoryEntry>> {
    let bytes = fs::read(path).ok()?;
    let cache: IndexCache = serde_json::from_slice(&bytes).ok()?;
    (cache.version == INDEX_CACHE_VERSION && cache.roots == roots).then_some(cache.entries)
}

pub fn store_cache(path: &Path, roots: &[PathBuf], entries: &[DirectoryEntry]) {
    let cache = IndexCache {
        version: INDEX_CACHE_VERSION,
        roots: roots.to_vec(),
        entries: entries.to_vec(),
    };
    let Ok(bytes) = serde_json::to_vec(&cache) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Write-then-rename: a half-written index would be parsed as no index at
    // all on the next launch, silently costing a cold scan.
    let temporary = path.with_extension("json.tmp");
    if fs::write(&temporary, bytes).is_ok() && fs::rename(&temporary, path).is_err() {
        let _ = fs::remove_file(&temporary);
    }
}

/// Parse newline/comma-separated roots, expand `~`, standardize, and dedupe in
/// source order. An empty setting uses `fallback` exactly like Swift.
pub fn resolve_roots(roots_setting: &str, fallback: &[PathBuf], home: &Path) -> Vec<PathBuf> {
    let parsed: Vec<_> = roots_setting
        .split(['\n', ','])
        .map(str::trim)
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .collect();
    let source = if parsed.is_empty() {
        fallback.to_vec()
    } else {
        parsed
    };

    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for raw in source {
        let expanded = expand_tilde(&raw, home);
        let standardized = lexical_standardize(&expanded);
        if seen.insert(standardized.clone()) {
            result.push(standardized);
        }
    }
    result
}

/// Blocking scan intended to be dispatched to GPUI's background executor.
///
/// Traversal is breadth-first *by design*. A depth-first walk spends the whole
/// `INDEX_CAP` inside whichever subtree sorts first — on a checkout-heavy home
/// directory that meant 20 000 entries burned inside `anara-*` and every folder
/// alphabetically after it invisible to Quick Open. Breadth-first guarantees
/// every top-level folder is indexed before any of their children, so the cap
/// truncates the deepest, least useful level instead of the second half of the
/// alphabet.
pub fn scan(roots: &[PathBuf], standalone_roots: &[PathBuf]) -> Vec<DirectoryEntry> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();

    for root in standalone_roots {
        if result.len() >= INDEX_CAP {
            break;
        }
        if !root.is_dir() || !seen.insert(root.clone()) {
            continue;
        }
        result.push(entry(root, 0));
    }

    let mut frontier: Vec<PathBuf> = roots.iter().filter(|root| root.is_dir()).cloned().collect();
    for depth in 0..=MAX_DEPTH {
        if frontier.is_empty() || result.len() >= INDEX_CAP {
            break;
        }
        let mut next = Vec::new();
        for path in frontier {
            if result.len() >= INDEX_CAP {
                break;
            }
            if seen.insert(path.clone()) {
                result.push(entry(&path, depth));
            }
            if depth < MAX_DEPTH {
                children(&path, &mut next);
            }
        }
        frontier = next;
    }
    result
}

/// Append the indexable subdirectories of `path`, skipping hidden entries, the
/// build/dependency noise in `SKIP_NAMES`, and symlinks (which would otherwise
/// let a loop re-enter the tree).
fn children(path: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for child in entries.flatten() {
        let name = child.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIP_NAMES.contains(&name.as_ref()) {
            continue;
        }
        let Ok(file_type) = child.file_type() else {
            continue;
        };
        if file_type.is_dir() && !file_type.is_symlink() {
            out.push(child.path());
        }
    }
}

fn entry(path: &Path, depth: usize) -> DirectoryEntry {
    DirectoryEntry {
        path: path.to_path_buf(),
        name: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        is_git_repo: path.join(".git").exists(),
        depth,
    }
}

fn expand_tilde(path: &Path, home: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return home.to_path_buf();
    }
    text.strip_prefix("~/")
        .map_or_else(|| path.to_path_buf(), |suffix| home.join(suffix))
}

fn lexical_standardize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

#[derive(Clone, Debug)]
pub struct RankCandidate {
    pub path: PathBuf,
    pub name: String,
    pub is_git_repo: bool,
    pub depth: usize,
    prepared_name: PreparedText,
    prepared_path: PreparedText,
}

impl RankCandidate {
    pub fn new(path: PathBuf, name: String, is_git_repo: bool, depth: usize) -> Self {
        // The home prefix is noise every candidate shares: scoring the raw
        // absolute path lets the account name ("giga") match half the pool.
        let path_text = home_relative(&path);
        Self {
            prepared_name: PreparedText::new(&name),
            prepared_path: PreparedText::new(&path_text),
            path,
            name,
            is_git_repo,
            depth,
        }
    }
}

fn home_relative(path: &Path) -> String {
    let text = path.to_string_lossy();
    let Some(home) = std::env::var_os("HOME") else {
        return text.into_owned();
    };
    let home = home.to_string_lossy();
    if home.is_empty() {
        return text.into_owned();
    }
    text.strip_prefix(home.as_ref())
        .map_or_else(|| text.clone().into_owned(), |rest| format!("~{rest}"))
}

impl From<&DirectoryEntry> for RankCandidate {
    fn from(entry: &DirectoryEntry) -> Self {
        Self::new(
            entry.path.clone(),
            entry.name.clone(),
            entry.is_git_repo,
            entry.depth,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickOpenItem {
    pub path: PathBuf,
    pub name: String,
    pub is_git_repo: bool,
}

impl From<&RankCandidate> for QuickOpenItem {
    fn from(candidate: &RankCandidate) -> Self {
        Self {
            path: candidate.path.clone(),
            name: candidate.name.clone(),
            is_git_repo: candidate.is_git_repo,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct QuickOpenSnapshot {
    /// Shared, not cloned: every keystroke hands this to a background ranking
    /// task, and the index holds up to `INDEX_CAP` candidates.
    pub pool: Arc<Vec<RankCandidate>>,
    pub recent: Vec<QuickOpenItem>,
    pub folders: Vec<QuickOpenItem>,
}

/// Merge the index with configured projects and session working directories,
/// then derive the unfiltered Recent/Folders sections exactly as Swift does.
pub fn build_snapshot(
    entries: &[DirectoryEntry],
    projects: &[(PathBuf, String)],
    session_cwds: &[PathBuf],
) -> QuickOpenSnapshot {
    let mut pool = Vec::with_capacity(entries.len() + 16);
    let mut indices = HashMap::with_capacity(entries.len() + 16);
    for entry in entries {
        insert_candidate(&mut pool, &mut indices, RankCandidate::from(entry));
    }
    for (root, name) in projects {
        if !indices.contains_key(root) {
            insert_candidate(
                &mut pool,
                &mut indices,
                RankCandidate::new(root.clone(), name.clone(), is_git(root), 0),
            );
        }
    }
    for cwd in session_cwds {
        if !indices.contains_key(cwd) {
            insert_candidate(
                &mut pool,
                &mut indices,
                RankCandidate::new(cwd.clone(), file_name(cwd), is_git(cwd), 0),
            );
        }
    }

    let mut recent = Vec::new();
    let mut seen = HashSet::new();
    for (root, name) in projects {
        if seen.insert(root.clone()) {
            recent.push(QuickOpenItem {
                path: root.clone(),
                name: name.clone(),
                is_git_repo: is_git(root),
            });
        }
    }
    for cwd in session_cwds {
        if seen.insert(cwd.clone()) {
            recent.push(QuickOpenItem {
                path: cwd.clone(),
                name: file_name(cwd),
                is_git_repo: is_git(cwd),
            });
        }
    }
    recent.truncate(RECENT_LIMIT);

    let recent_paths: HashSet<_> = recent.iter().map(|item| &item.path).collect();
    let mut folders: Vec<_> = entries
        .iter()
        .filter(|entry| !recent_paths.contains(&entry.path))
        .cloned()
        .collect();
    folders.sort_by(|left, right| {
        right
            .is_git_repo
            .cmp(&left.is_git_repo)
            .then_with(|| left.depth.cmp(&right.depth))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    let folders = folders
        .iter()
        .take(RESULT_LIMIT)
        .map(|entry| QuickOpenItem {
            path: entry.path.clone(),
            name: entry.name.clone(),
            is_git_repo: entry.is_git_repo,
        })
        .collect();

    QuickOpenSnapshot {
        pool: Arc::new(pool),
        recent,
        folders,
    }
}

fn insert_candidate(
    pool: &mut Vec<RankCandidate>,
    indices: &mut HashMap<PathBuf, usize>,
    candidate: RankCandidate,
) {
    let index = pool.len();
    indices.insert(candidate.path.clone(), index);
    pool.push(candidate);
}

fn is_git(path: &Path) -> bool {
    path.join(".git").exists()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Matching the folder's own name beats matching somewhere in its path; worth
/// four matched characters so a deep path hit cannot outrank a name hit.
const NAME_PRIORITY: Score = 64;
/// Per level of remaining depth — shallower folders are likelier targets.
const DEPTH_BONUS: Score = 8;
/// Git checkouts are what people actually open agents in.
const GIT_BONUS: Score = 24;

/// A folder that matched the query, with the byte ranges of its name to
/// highlight (empty when only the path matched).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RankedFolder {
    pub item: QuickOpenItem,
    pub name_matches: Vec<Range<usize>>,
    pub score: Score,
}

/// Folder-aware score: the better of name-vs-path, then the structural
/// bonuses. Both matchers are supplied by the caller so one ranking pass
/// allocates two matchers rather than two per candidate.
pub fn rank_candidate(
    query: &FuzzyQuery,
    candidate: &RankCandidate,
    names: &mut FuzzyMatcher,
    paths: &mut FuzzyMatcher,
) -> Option<(Score, Vec<Range<usize>>)> {
    let name = query.highlights(&candidate.prepared_name, &candidate.name, names);
    let path = query.score(&candidate.prepared_path, paths);

    let (base, matches) = match (name, path) {
        (Some((name, ranges)), Some(path)) => ((name + NAME_PRIORITY).max(path), ranges),
        (Some((name, ranges)), None) => (name + NAME_PRIORITY, ranges),
        (None, Some(path)) => (path, Vec::new()),
        (None, None) => return None,
    };

    let depth = MAX_DEPTH.saturating_sub(candidate.depth) as Score;
    let git = if candidate.is_git_repo { GIT_BONUS } else { 0 };
    Some((base + depth * DEPTH_BONUS + git, matches))
}

pub fn rank(query: &str, pool: &[RankCandidate], limit: usize) -> Vec<RankedFolder> {
    let parsed = FuzzyQuery::new(query);
    if parsed.is_empty() {
        return Vec::new();
    }
    let mut names = FuzzyMatcher::text();
    let mut paths = FuzzyMatcher::paths();
    let mut matches: Vec<_> = pool
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            rank_candidate(&parsed, candidate, &mut names, &mut paths).map(
                |(score, name_matches)| {
                    (
                        index,
                        RankedFolder {
                            item: QuickOpenItem::from(candidate),
                            name_matches,
                            score,
                        },
                    )
                },
            )
        })
        .collect();
    matches.sort_by(|left, right| {
        right
            .1
            .score
            .cmp(&left.1.score)
            .then_with(|| left.0.cmp(&right.0))
    });
    matches
        .into_iter()
        .take(limit)
        .map(|(_, folder)| folder)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn ranked_names(query: &str, pool: &[RankCandidate]) -> Vec<String> {
        rank(query, pool, RESULT_LIMIT)
            .into_iter()
            .map(|folder| folder.item.name)
            .collect()
    }

    fn fixture_pool() -> [RankCandidate; 6] {
        [
            RankCandidate::new("/work/dotfiles".into(), "dotfiles".into(), true, 0),
            RankCandidate::new("/work/docs".into(), "docs".into(), false, 3),
            RankCandidate::new("/work/deep-docs".into(), "deep-docs".into(), false, 4),
            RankCandidate::new("/work/docs/frontend".into(), "frontend".into(), true, 1),
            RankCandidate::new("/work/ClaudeCode".into(), "ClaudeCode".into(), false, 2),
            RankCandidate::new("/work/cachecontrol".into(), "cachecontrol".into(), false, 2),
        ]
    }

    #[test]
    fn ranking_puts_name_matches_first_and_breaks_ties_on_git_and_depth() {
        let pool = fixture_pool();

        // Both start with "do"; the shallow git checkout wins the tie. The
        // rest match through their path, so they rank below.
        let names = ranked_names("do", &pool);
        assert_eq!(&names[..2], ["dotfiles", "docs"]);
        assert!(names.contains(&"frontend".to_owned()));
        assert!(!names.contains(&"cachecontrol".to_owned()));

        // Optimal alignment: the second `c` reaches ClaudeCode's capital.
        assert_eq!(ranked_names("cc", &pool), ["ClaudeCode", "cachecontrol"]);
        assert_eq!(ranked_names("front", &pool), ["frontend"]);
        assert!(ranked_names("zzq", &pool).is_empty());
        assert!(ranked_names("   ", &pool).is_empty());
    }

    #[test]
    fn ranking_matches_paths_by_segment_and_reports_name_highlights() {
        let pool = fixture_pool();

        // A segmented query only the full path can satisfy.
        assert_eq!(ranked_names("work/front", &pool), ["frontend"]);

        let ranked = rank("front", &pool, RESULT_LIMIT);
        assert_eq!(ranked[0].name_matches.len(), 1);
        assert_eq!(ranked[0].name_matches[0], 0..5);

        // Path-only matches leave the name unhighlighted.
        let ranked = rank("work", &pool, RESULT_LIMIT);
        assert!(ranked.iter().all(|folder| folder.name_matches.is_empty()));
    }

    #[test]
    fn scan_stops_at_four_levels_and_skips_noise_and_symlinks() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(root.join("one/two/three/four/five")).unwrap();
        fs::create_dir_all(root.join("node_modules/ignored")).unwrap();
        fs::create_dir_all(root.join("repo/.git")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("one"), root.join("linked")).unwrap();

        let entries = scan(std::slice::from_ref(&root), &[]);
        let relative: HashSet<_> = entries
            .iter()
            .map(|entry| entry.path.strip_prefix(&root).unwrap().to_path_buf())
            .collect();
        assert!(relative.contains(Path::new("")));
        assert!(relative.contains(Path::new("one/two/three/four")));
        assert!(!relative.contains(Path::new("one/two/three/four/five")));
        assert!(!relative.contains(Path::new("node_modules")));
        assert!(!relative.contains(Path::new("linked")));
        assert!(
            entries
                .iter()
                .any(|entry| entry.name == "repo" && entry.is_git_repo)
        );
        // Breadth-first: depths appear in non-decreasing order.
        assert!(
            entries
                .windows(2)
                .all(|pair| pair[0].depth <= pair[1].depth)
        );
    }

    #[test]
    fn a_deep_first_subtree_cannot_starve_later_top_level_folders() {
        // The real failure this guards: `~/fun` held 260 checkouts, the walk
        // was depth-first, and the index cap was spent inside the `anara-*`
        // block — so `kairoskraft` was never indexed and ⌘P said "No matches".
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        let deep = root.join("aaa-hog");
        for branch in 0..40 {
            fs::create_dir_all(deep.join(format!("b{branch}/c/d"))).unwrap();
        }
        fs::create_dir_all(root.join("zzz-target")).unwrap();

        let entries = scan(std::slice::from_ref(&root), &[]);
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert!(
            names.contains(&"zzz-target"),
            "top-level folder was starved"
        );

        // The cap is far larger than this fixture, so the guarantee under test
        // is the ordering one: every top-level folder is indexed before any
        // grandchild, which is what makes a truncated index still useful.
        let target = names.iter().position(|name| *name == "zzz-target").unwrap();
        let first_deep = entries.iter().position(|entry| entry.depth >= 2).unwrap();
        assert!(target < first_deep, "depth-first order starves the tail");
    }

    fn fixture_entry(name: &str) -> DirectoryEntry {
        DirectoryEntry {
            path: PathBuf::from(format!("/work/{name}")),
            name: name.to_owned(),
            is_git_repo: true,
            depth: 1,
        }
    }

    #[test]
    fn the_index_cache_round_trips_and_refuses_stale_shapes() {
        let temp = tempdir().unwrap();
        // Nested, to prove the writer creates its Application Support folder.
        let path = temp
            .path()
            .join("Application Support/diri/quick-open-index.json");
        let roots = vec![PathBuf::from("/work")];
        let entries = vec![fixture_entry("diri"), fixture_entry("anara")];

        store_cache(&path, &roots, &entries);
        assert_eq!(load_cache(&path, &roots), Some(entries.clone()));

        // A different roots setting indexed a different world.
        assert!(load_cache(&path, &[PathBuf::from("/elsewhere")]).is_none());

        // A newer build's traversal rules invalidate the old index.
        let stale = format!(
            r#"{{"version":{},"roots":["/work"],"entries":[]}}"#,
            INDEX_CACHE_VERSION + 1
        );
        fs::write(&path, stale).unwrap();
        assert!(load_cache(&path, &roots).is_none());

        // Truncated JSON reads as "no cache", never as a panic.
        fs::write(&path, "{\"version\":1,\"roo").unwrap();
        assert!(load_cache(&path, &roots).is_none());
        assert!(load_cache(&temp.path().join("missing.json"), &roots).is_none());
    }

    #[test]
    fn scans_are_throttled_and_a_cache_never_overwrites_a_fresh_scan() {
        let mut index = DirectoryIndex::default();
        let now = Instant::now();
        assert!(index.needs_scan(now), "an empty index must scan");

        assert!(index.begin_scan());
        assert!(!index.begin_scan(), "no concurrent scans");
        assert!(!index.needs_scan(now), "a scan is already in flight");

        index.finish_scan(vec![fixture_entry("scanned")], now);
        assert!(!index.needs_scan(now), "just scanned");
        assert!(index.needs_scan(now + RESCAN_AFTER), "aged out");

        // The disk cache lands asynchronously; if the scan won the race it
        // holds the truth and the cache must not roll it back.
        index.adopt_cached(vec![fixture_entry("cached")]);
        assert_eq!(index.entries()[0].name, "scanned");
    }

    #[test]
    fn root_resolution_matches_swift_rules() {
        let home = Path::new("/Users/tester");
        let roots = resolve_roots(" ~/fun,~/src\n~/fun ", &[], home);
        assert_eq!(roots, [home.join("fun"), home.join("src")]);

        let fallback = [PathBuf::from("~/fallback")];
        assert_eq!(
            resolve_roots("  ", &fallback, home),
            [home.join("fallback")]
        );
    }

    #[test]
    fn snapshot_keeps_project_then_session_recency_and_git_first_browse_order() {
        let temp = tempdir().unwrap();
        let project = temp.path().join("project");
        let session = temp.path().join("session");
        let git_folder = temp.path().join("z-git");
        let plain_folder = temp.path().join("a-plain");
        for path in [&project, &session, &git_folder, &plain_folder] {
            fs::create_dir_all(path).unwrap();
        }
        fs::create_dir(git_folder.join(".git")).unwrap();
        let entries = vec![entry(&plain_folder, 0), entry(&git_folder, 2)];

        let snapshot = build_snapshot(
            &entries,
            &[(project.clone(), "Named Project".into())],
            &[session.clone(), project.clone()],
        );
        let recents: Vec<_> = snapshot.recent.iter().map(|item| &item.path).collect();
        assert_eq!(recents, [&project, &session]);
        assert_eq!(snapshot.folders[0].path, git_folder);
        assert_eq!(snapshot.folders[1].path, plain_folder);
        assert_eq!(snapshot.pool.len(), 4);
    }
}
