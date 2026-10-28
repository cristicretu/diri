//! Insert Path: a file picker that types a path at the terminal cursor.
//!
//! The picker belongs to the terminal, not to the shell, so it behaves the same
//! in fish, zsh, bash, nvim's `:terminal`, or an agent's prompt: nothing is
//! installed into the session, and the only thing the session ever sees is
//! the chosen path arriving as a paste. Paths are relative to the session
//! child's live working directory and escaped the way a Finder drop is, so a
//! plain shell reads them as one word.
//!
//! This module is the model — scanning, ranking and the text to insert — and
//! is free of GPUI so it can be tested directly.

use std::collections::BTreeSet;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::external_drop::escape_path_for_terminal;
use crate::fuzzy::{FuzzyMatcher, FuzzyQuery, PreparedText};
use crate::query_editor::QueryEditor;

/// Entries indexed for one picker. Larger trees are truncated, not refused.
pub const INDEX_CAP: usize = 20_000;
/// Directory depth the non-git walk descends to below the root.
pub const WALK_DEPTH: usize = 8;
/// Rows the picker ranks and can show.
pub const RESULT_LIMIT: usize = 50;

/// Directories never worth walking into when there is no `.gitignore` to ask:
/// dependency and build output, VCS internals, and macOS's per-user caches.
const SKIPPED_DIRECTORIES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    ".build",
    "DerivedData",
    ".cache",
    ".Trash",
    "Library",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathEntry {
    /// Relative to the index root, `/`-separated, without a trailing slash.
    pub relative: String,
    pub is_dir: bool,
    prepared: PreparedText,
}

impl PathEntry {
    fn new(relative: String, is_dir: bool) -> Self {
        let prepared = PreparedText::new(&relative);
        Self {
            relative,
            is_dir,
            prepared,
        }
    }

    #[cfg(test)]
    fn is_top_level(&self) -> bool {
        !self.relative.contains('/')
    }

    fn is_hidden(&self) -> bool {
        self.relative
            .rsplit('/')
            .next()
            .is_some_and(|name| name.starts_with('.'))
    }
}

/// Every path under one root, scanned once when the picker opens.
#[derive(Debug, Default)]
pub struct PathIndex {
    pub root: PathBuf,
    /// Files and directories under the root, gitignore-filtered in a repo.
    entries: Vec<PathEntry>,
    /// The root's own children, including ignored ones such as
    /// `node_modules`: the empty-query listing is a directory listing.
    listing: Vec<PathEntry>,
    pub truncated: bool,
}

impl PathIndex {
    #[cfg(test)]
    pub fn from_entries(root: &Path, entries: &[(&str, bool)]) -> Self {
        let entries: Vec<PathEntry> = entries
            .iter()
            .map(|(relative, is_dir)| PathEntry::new((*relative).to_owned(), *is_dir))
            .collect();
        let listing = entries
            .iter()
            .filter(|entry| entry.is_top_level())
            .cloned()
            .collect();
        let mut index = Self {
            root: root.to_path_buf(),
            entries,
            listing,
            truncated: false,
        };
        index.sort_listing();
        index
    }

    fn sort_listing(&mut self) {
        self.listing.retain(|entry| !entry.is_hidden());
        self.listing.sort_by(|a, b| {
            a.relative
                .to_lowercase()
                .cmp(&b.relative.to_lowercase())
                .then_with(|| a.relative.cmp(&b.relative))
        });
    }
}

/// Scan `root`. Inside a git work tree the index is whatever git would show
/// (tracked plus untracked-but-not-ignored); elsewhere a bounded walk that
/// skips dependency and build directories. Blocking; call off the UI thread.
pub fn scan(root: &Path) -> PathIndex {
    let mut index = PathIndex {
        root: root.to_path_buf(),
        ..PathIndex::default()
    };
    index.listing = list_children(root);
    let (entries, truncated) = match git_files(root) {
        Some(files) => entries_from_files(files),
        None => walk(root),
    };
    index.entries = entries;
    index.truncated = truncated;
    index.sort_listing();
    index
}

fn list_children(root: &Path) -> Vec<PathEntry> {
    let Ok(read) = fs::read_dir(root) else {
        return Vec::new();
    };
    read.filter_map(Result::ok)
        .take(INDEX_CAP)
        .filter_map(|child| {
            let name = child.file_name().into_string().ok()?;
            Some(PathEntry::new(name, is_dir(&child.path())))
        })
        .collect()
}

/// Whether `path` is a directory, following a symlink for the answer only.
/// A symlinked directory reads as a directory, but the walk never descends
/// through one.
fn is_dir(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

fn git_files(root: &Path) -> Option<Vec<String>> {
    let output = diri_platform::hide_console_window(&mut Command::new("git"))
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .filter_map(|path| std::str::from_utf8(path).ok().map(str::to_owned))
            .collect(),
    )
}

/// Files from git, plus every directory that contains one. Git lists no
/// directories of its own, but `cd`-ing into one is a common reason to ask.
fn entries_from_files(mut files: Vec<String>) -> (Vec<PathEntry>, bool) {
    files.sort();
    files.dedup();
    let mut directories = BTreeSet::new();
    for file in &files {
        let mut end = 0;
        while let Some(offset) = file[end..].find('/') {
            end += offset;
            directories.insert(file[..end].to_owned());
            end += 1;
        }
    }
    let total = directories.len() + files.len();
    let mut entries = Vec::with_capacity(total.min(INDEX_CAP));
    entries.extend(
        directories
            .into_iter()
            .map(|directory| PathEntry::new(directory, true)),
    );
    entries.extend(files.into_iter().map(|file| PathEntry::new(file, false)));
    entries.truncate(INDEX_CAP);
    (entries, total > INDEX_CAP)
}

/// Breadth-first, so a cap cuts the deepest paths rather than whole siblings.
fn walk(root: &Path) -> (Vec<PathEntry>, bool) {
    let mut entries = Vec::new();
    let mut queue = std::collections::VecDeque::from([(root.to_path_buf(), String::new(), 0)]);
    while let Some((directory, prefix, depth)) = queue.pop_front() {
        let Ok(read) = fs::read_dir(&directory) else {
            continue;
        };
        let mut children: Vec<_> = read.filter_map(Result::ok).collect();
        children.sort_by_key(fs::DirEntry::file_name);
        for child in children {
            if entries.len() >= INDEX_CAP {
                return (entries, true);
            }
            let Ok(name) = child.file_name().into_string() else {
                continue;
            };
            if name == ".DS_Store" {
                continue;
            }
            let Ok(file_type) = child.file_type() else {
                continue;
            };
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let directory_like =
                file_type.is_dir() || (file_type.is_symlink() && is_dir(&child.path()));
            if file_type.is_dir()
                && depth + 1 < WALK_DEPTH
                && !SKIPPED_DIRECTORIES.contains(&name.as_str())
            {
                queue.push_back((child.path(), relative.clone(), depth + 1));
            }
            entries.push(PathEntry::new(relative, directory_like));
        }
    }
    (entries, false)
}

/// One visible row: an index entry and the byte ranges the query matched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerRow {
    /// The text shown, which `highlights` index into: relative to the root
    /// for a scanned entry, the home-collapsed absolute path for a recent.
    pub relative: String,
    pub is_dir: bool,
    pub highlights: Vec<Range<usize>>,
    /// A recently chosen directory, by its absolute path.
    pub recent: Option<PathBuf>,
}

impl PickerRow {
    fn plain(entry: &PathEntry) -> Self {
        Self {
            relative: entry.relative.clone(),
            is_dir: entry.is_dir,
            highlights: Vec::new(),
            recent: None,
        }
    }

    /// The absolute directory this row stands for: itself for a directory,
    /// its parent for a file.
    pub fn directory(&self, root: &Path) -> PathBuf {
        let path = self
            .recent
            .clone()
            .unwrap_or_else(|| root.join(&self.relative));
        if self.is_dir {
            path
        } else {
            path.parent().map_or(path.clone(), Path::to_path_buf)
        }
    }
}

/// What choosing a row does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PickerMode {
    /// Type the path at the cursor (⌘E).
    #[default]
    InsertPath,
    /// Directories only; choosing one `cd`s the shell there.
    GoToDirectory,
}

/// Recents the empty query lists above the root's own children.
pub const RECENTS_SHOWN_EMPTY: usize = 5;

/// A recently chosen directory, prepared for matching by its shown text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentDirectory {
    pub path: PathBuf,
    shown: String,
    prepared: PreparedText,
}

impl RecentDirectory {
    pub fn new(path: PathBuf, home: &Path) -> Self {
        let shown = crate::quick_open::collapse_home(&path, home);
        let prepared = PreparedText::new(&shown);
        Self {
            path,
            shown,
            prepared,
        }
    }
}

/// Rank the index for `query`. An empty query lists the root like `ls`;
/// otherwise the whole tree is fuzzy-matched with `/` as the word boundary,
/// ties going to the shorter (shallower) path.
#[cfg(test)]
pub fn rank(index: &PathIndex, query: &str) -> Vec<PickerRow> {
    rank_with(index, query, &[], PickerMode::InsertPath)
}

/// [`rank`] with recent directories first and, for Go to Directory, only
/// directories. Recents are matched by their shown path; a scanned entry
/// that is also a shown recent is listed once, as the recent.
pub fn rank_with(
    index: &PathIndex,
    query: &str,
    recents: &[RecentDirectory],
    mode: PickerMode,
) -> Vec<PickerRow> {
    let parsed = FuzzyQuery::new(query);
    let mut matcher = FuzzyMatcher::paths();
    let recents = recents
        .iter()
        .filter(|recent| recent.path != index.root);
    let mut rows: Vec<PickerRow> = if parsed.is_empty() {
        recents
            .take(RECENTS_SHOWN_EMPTY)
            .map(|recent| PickerRow {
                relative: recent.shown.clone(),
                is_dir: true,
                highlights: Vec::new(),
                recent: Some(recent.path.clone()),
            })
            .collect()
    } else {
        let mut scored: Vec<(u32, &RecentDirectory)> = recents
            .filter_map(|recent| Some((parsed.score(&recent.prepared, &mut matcher)?, recent)))
            .collect();
        // Recency breaks ties; the list is already most recent first.
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored
            .into_iter()
            .map(|(_, recent)| PickerRow {
                relative: recent.shown.clone(),
                is_dir: true,
                highlights: parsed
                    .highlights(&recent.prepared, &recent.shown, &mut matcher)
                    .map(|(_, ranges)| ranges)
                    .unwrap_or_default(),
                recent: Some(recent.path.clone()),
            })
            .collect()
    };
    rows.truncate(RESULT_LIMIT);
    let shown_recent = |entry: &PathEntry| {
        let path = index.root.join(&entry.relative);
        rows.iter()
            .any(|row| row.recent.as_deref() == Some(path.as_path()))
    };
    let wanted = |entry: &&PathEntry| mode == PickerMode::InsertPath || entry.is_dir;
    let room = RESULT_LIMIT - rows.len();
    let scanned = rank_entries(index, &parsed, &mut matcher, room, wanted, shown_recent);
    rows.extend(scanned);
    rows
}

fn rank_entries(
    index: &PathIndex,
    parsed: &FuzzyQuery,
    matcher: &mut FuzzyMatcher,
    room: usize,
    wanted: impl Fn(&&PathEntry) -> bool,
    shown_recent: impl Fn(&PathEntry) -> bool,
) -> Vec<PickerRow> {
    if parsed.is_empty() {
        return index
            .listing
            .iter()
            .filter(&wanted)
            .filter(|entry| !shown_recent(entry))
            .take(room)
            .map(PickerRow::plain)
            .collect();
    }
    let mut scored: Vec<(u32, &PathEntry)> = index
        .entries
        .iter()
        .filter(&wanted)
        .filter_map(|entry| Some((parsed.score(&entry.prepared, matcher)?, entry)))
        .filter(|(_, entry)| !shown_recent(entry))
        .collect();
    scored.sort_by(|(a_score, a), (b_score, b)| {
        b_score
            .cmp(a_score)
            .then_with(|| a.relative.len().cmp(&b.relative.len()))
            .then_with(|| a.relative.cmp(&b.relative))
    });
    scored.truncate(room);
    scored
        .into_iter()
        .map(|(_, entry)| {
            let highlights = parsed
                .highlights(&entry.prepared, &entry.relative, matcher)
                .map(|(_, ranges)| ranges)
                .unwrap_or_default();
            PickerRow {
                relative: entry.relative.clone(),
                is_dir: entry.is_dir,
                highlights,
                recent: None,
            }
        })
        .collect()
}

/// What choosing `row` types into the session. Files end in a space, like a
/// Finder drop, so the next word does not fuse with the path; directories end
/// in `/` so the user can keep typing into them.
pub fn insertion_text(row: &PickerRow) -> String {
    let mut text = match &row.recent {
        // Absolute: a recent is usually somewhere else, and `~` would not
        // expand once escaped.
        Some(path) => escape_path_for_terminal(&path.to_string_lossy()),
        None => escape_path_for_terminal(&row.relative),
    };
    text.push(if row.is_dir { '/' } else { ' ' });
    text
}

/// The command line Go to Directory types: `cd -- '<path>'` and Return.
/// Single quotes read the same in fish, zsh and bash, with `'` written as
/// `'\''`. `None` for a path no quoting carries safely into all three: one
/// that is not UTF-8, holds a control character, or a backslash (fish reads
/// `\\` inside single quotes as one backslash).
pub fn cd_command(path: &Path) -> Option<String> {
    let path = path.to_str()?;
    if !path.starts_with('/') || path.chars().any(|c| c.is_control() || c == '\\') {
        return None;
    }
    Some(format!("cd -- '{}'\r", path.replace('\'', "'\\''")))
}

/// Shorten `text` to at most `max_chars` characters by eliding its middle,
/// keeping the file name — the part people scan for — whole where possible.
/// Highlight ranges are carried across: ranges inside the elided span drop
/// out, ranges after it shift left.
pub fn elide_middle(
    text: &str,
    highlights: &[Range<usize>],
    max_chars: usize,
) -> (String, Vec<Range<usize>>) {
    let chars = text.chars().count();
    if chars <= max_chars || max_chars < 5 {
        return (text.to_owned(), highlights.to_vec());
    }
    let keep = max_chars - 1;
    let tail_chars = (keep * 3 / 5).max(1);
    let head_chars = keep - tail_chars;
    let byte_at = |char_index: usize| {
        text.char_indices()
            .nth(char_index)
            .map_or(text.len(), |(offset, _)| offset)
    };
    let head_end = byte_at(head_chars);
    let tail_start = byte_at(chars - tail_chars);
    let mut out = String::with_capacity(head_end + 3 + text.len() - tail_start);
    out.push_str(&text[..head_end]);
    out.push('…');
    let tail_at = out.len();
    out.push_str(&text[tail_start..]);
    let mut ranges = Vec::new();
    for range in highlights {
        if range.start < head_end {
            ranges.push(range.start..range.end.min(head_end));
        }
        if range.end > tail_start {
            let start = range.start.max(tail_start) - tail_start + tail_at;
            ranges.push(start..range.end - tail_start + tail_at);
        }
    }
    (out, ranges)
}

/// The picker's lifecycle. The index is scanned off the UI thread after the
/// session's live working directory is known, so an open picker starts in
/// `Loading` and renders its query field immediately.
#[derive(Debug)]
pub enum PickerIndex {
    Loading,
    Ready(PathIndex),
    Failed(String),
}

/// State for one open picker on one session.
#[derive(Debug)]
pub struct PathPicker {
    /// Distinguishes this opening from a later one, so a slow scan for a
    /// picker the user already closed never populates its replacement.
    pub generation: u64,
    pub mode: PickerMode,
    pub recents: Vec<RecentDirectory>,
    pub query: QueryEditor,
    pub index: PickerIndex,
    pub rows: Vec<PickerRow>,
    pub selected: usize,
}

impl PathPicker {
    pub fn new(generation: u64, mode: PickerMode) -> Self {
        Self {
            generation,
            mode,
            recents: Vec::new(),
            query: QueryEditor::default(),
            index: PickerIndex::Loading,
            rows: Vec::new(),
            selected: 0,
        }
    }

    pub fn adopt_index(&mut self, index: PathIndex) {
        self.index = PickerIndex::Ready(index);
        self.refresh();
    }

    pub fn adopt(&mut self, index: PathIndex, recents: Vec<RecentDirectory>) {
        self.recents = recents;
        self.adopt_index(index);
    }

    /// Drops a recent from this picker's list (⌘⌫). Returns its path.
    pub fn forget_selected_recent(&mut self) -> Option<PathBuf> {
        let path = self.selected_row()?.recent.clone()?;
        self.recents.retain(|recent| recent.path != path);
        let selected = self.selected;
        self.refresh();
        self.selected = selected.min(self.rows.len().saturating_sub(1));
        Some(path)
    }

    pub fn fail(&mut self, message: String) {
        self.index = PickerIndex::Failed(message);
        self.rows.clear();
        self.selected = 0;
    }

    /// Re-rank after the query changed. Selection returns to the best match.
    pub fn refresh(&mut self) {
        if let PickerIndex::Ready(index) = &self.index {
            self.rows = rank_with(index, self.query.text(), &self.recents, self.mode);
        }
        self.selected = 0;
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
    }

    pub fn selected_row(&self) -> Option<&PickerRow> {
        self.rows.get(self.selected)
    }

    /// Tab on a directory narrows the query to its contents instead of
    /// inserting it. Returns whether the query changed.
    pub fn descend(&mut self) -> bool {
        let Some(row) = self
            .selected_row()
            .filter(|row| row.is_dir && row.recent.is_none())
        else {
            return false;
        };
        let narrowed = format!("{}/", row.relative);
        if narrowed == self.query.text() {
            return false;
        }
        self.query.clear();
        self.query.insert(&narrowed);
        self.refresh();
        true
    }

    pub fn root(&self) -> Option<&Path> {
        match &self.index {
            PickerIndex::Ready(index) => Some(&index.root),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> PathIndex {
        PathIndex::from_entries(
            Path::new("/work/replay-web"),
            &[
                ("apps", true),
                ("apps/license-lookup-app", true),
                ("apps/license-lookup-app/README.md", false),
                ("apps/license-lookup-app/src", true),
                ("apps/license-lookup-app/src/app.d.ts", false),
                ("apps/license-lookup-app/src/types", true),
                ("apps/license-lookup-app/src/types/License.ts", false),
                ("design_assets", true),
                ("Dockerfile", false),
                ("LICENSE", false),
                (".envrc", false),
                ("flake.nix", false),
            ],
        )
    }

    fn relatives(rows: &[PickerRow]) -> Vec<&str> {
        rows.iter().map(|row| row.relative.as_str()).collect()
    }

    #[test]
    fn an_empty_query_lists_the_root_like_ls_without_dotfiles() {
        let rows = rank(&index(), "");
        assert_eq!(
            relatives(&rows),
            [
                "apps",
                "design_assets",
                "Dockerfile",
                "flake.nix",
                "LICENSE"
            ]
        );
        assert!(rows[0].is_dir);
    }

    #[test]
    fn a_query_reaches_deep_paths_and_prefers_the_file_it_names() {
        let rows = rank(&index(), "licens.ts");
        assert_eq!(
            rows[0].relative,
            "apps/license-lookup-app/src/types/License.ts"
        );
        assert!(!rows[0].highlights.is_empty());
        let highlighted: String = rows[0]
            .highlights
            .iter()
            .map(|range| &rows[0].relative[range.clone()])
            .collect();
        assert!(highlighted.to_lowercase().contains("ts"));
    }

    #[test]
    fn inserted_paths_are_shell_safe_and_terminated_by_kind() {
        let file = PickerRow {
            relative: "notes/Screen Shot (1).png".into(),
            is_dir: false,
            highlights: Vec::new(),
            recent: None,
        };
        assert_eq!(insertion_text(&file), "notes/Screen\\ Shot\\ \\(1\\).png ");
        let directory = PickerRow {
            relative: "apps/src".into(),
            is_dir: true,
            highlights: Vec::new(),
            recent: None,
        };
        assert_eq!(insertion_text(&directory), "apps/src/");
    }

    #[test]
    fn tab_on_a_directory_narrows_the_query_to_its_contents() {
        let mut picker = PathPicker::new(1, PickerMode::InsertPath);
        picker.adopt_index(index());
        assert_eq!(picker.selected_row().unwrap().relative, "apps");
        assert!(picker.descend());
        assert_eq!(picker.query.text(), "apps/");
        assert!(
            picker
                .rows
                .iter()
                .all(|row| row.relative.starts_with("apps"))
        );
        picker.query.clear();
        picker.query.insert("dockerfile");
        picker.refresh();
        assert!(
            !picker.descend(),
            "a file is inserted, never descended into"
        );
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut picker = PathPicker::new(1, PickerMode::InsertPath);
        picker.adopt_index(index());
        let last = picker.rows.len() - 1;
        picker.move_selection(-1);
        assert_eq!(picker.selected, last);
        picker.move_selection(1);
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn eliding_keeps_the_file_name_and_remaps_highlights() {
        let text = "apps/license-lookup-app/src/types/License.ts";
        let name = text.find("License.ts").unwrap();
        let highlights = [0..4, name..name + 7];
        let (short, ranges) = elide_middle(text, &highlights, 30);
        assert_eq!(short.chars().count(), 30);
        assert!(short.ends_with("types/License.ts"), "{short}");
        assert!(short.contains('…'));
        let picked: Vec<&str> = ranges.iter().map(|range| &short[range.clone()]).collect();
        assert_eq!(picked, ["apps", "License"]);
        // One character over: the ellipsis is wider in bytes than the text
        // it replaces.
        let (short, ranges) = elide_middle("abcdefghijk", std::slice::from_ref(&(9..11)), 10);
        assert_eq!(short.chars().count(), 10);
        assert_eq!(&short[ranges[0].clone()], "jk");
        let unchanged = std::slice::from_ref(&(0..3));
        let (same, kept) = elide_middle("src/lib.rs", unchanged, 30);
        assert_eq!(same, "src/lib.rs");
        assert_eq!(kept, unchanged);
    }

    #[test]
    fn git_files_become_files_plus_their_directories() {
        let (entries, truncated) = entries_from_files(vec![
            "src/main.rs".into(),
            "src/bin/tool.rs".into(),
            "README.md".into(),
        ]);
        assert!(!truncated);
        let dirs: Vec<_> = entries
            .iter()
            .filter(|entry| entry.is_dir)
            .map(|entry| entry.relative.as_str())
            .collect();
        assert_eq!(dirs, ["src", "src/bin"]);
        assert_eq!(entries.len(), 5);
    }

    #[test]
    fn scanning_a_git_repo_respects_gitignore_but_lists_ignored_top_level_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap()
        };
        if !git(&["init", "-q"]).status.success() {
            return; // no git on this machine; the walk test covers the rest
        }
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "").unwrap();
        fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        let index = scan(root);
        let all = rank(&index, "index.js");
        assert!(all.is_empty(), "ignored files are not ranked: {all:?}");
        assert_eq!(rank(&index, "lib")[0].relative, "src/lib.rs");
        let listing = rank(&index, "");
        assert_eq!(relatives(&listing), ["node_modules", "src"]);
    }

    #[cfg(unix)]
    #[test]
    fn the_walk_skips_dependency_dirs_and_never_follows_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("plain");
        fs::create_dir_all(root.join("docs/deep")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("docs/deep/guide.md"), "").unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "").unwrap();
        std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();
        let (entries, truncated) = walk(&root);
        assert!(!truncated);
        let paths: Vec<_> = entries
            .iter()
            .map(|entry| entry.relative.as_str())
            .collect();
        assert!(paths.contains(&"docs/deep/guide.md"));
        assert!(paths.contains(&"node_modules"));
        assert!(!paths.iter().any(|path| path.starts_with("node_modules/")));
        assert!(paths.contains(&"loop"));
        assert!(!paths.iter().any(|path| path.starts_with("loop/")));
    }

    fn recents(paths: &[&str]) -> Vec<RecentDirectory> {
        paths
            .iter()
            .map(|path| RecentDirectory::new(PathBuf::from(path), Path::new("/Users/me")))
            .collect()
    }

    #[test]
    fn recents_come_first_most_recent_first_and_skip_the_root() {
        let recents = recents(&[
            "/Users/me/fun/dirijor",
            "/work/replay-web",
            "/work/replay-web/apps",
            "/tmp/scratch",
        ]);
        let rows = rank_with(&index(), "", &recents, PickerMode::InsertPath);
        // The root is where the picker already is; the scanned `apps` is
        // listed once, as the recent.
        assert_eq!(
            relatives(&rows[..3]),
            ["~/fun/dirijor", "/work/replay-web/apps", "/tmp/scratch"]
        );
        assert!(rows[..3].iter().all(|row| row.recent.is_some()));
        assert_eq!(rows.iter().filter(|row| row.relative == "apps").count(), 0);
        assert_eq!(rows[3].relative, "design_assets");
        // A query ranks matching recents above the tree.
        let rows = rank_with(&index(), "dirijor", &recents, PickerMode::InsertPath);
        assert_eq!(rows[0].relative, "~/fun/dirijor");
        assert!(!rows[0].highlights.is_empty());
    }

    #[test]
    fn a_recent_inserts_its_absolute_path() {
        let recents = recents(&["/Users/me/My Repos/web"]);
        let rows = rank_with(&index(), "", &recents, PickerMode::InsertPath);
        assert_eq!(rows[0].relative, "~/My Repos/web");
        assert_eq!(insertion_text(&rows[0]), "/Users/me/My\\ Repos/web/");
        assert_eq!(
            rows[0].directory(Path::new("/work/replay-web")),
            Path::new("/Users/me/My Repos/web")
        );
    }

    #[test]
    fn go_to_directory_lists_only_directories() {
        let rows = rank_with(&index(), "", &[], PickerMode::GoToDirectory);
        assert_eq!(relatives(&rows), ["apps", "design_assets"]);
        let rows = rank_with(&index(), "lic", &[], PickerMode::GoToDirectory);
        assert!(rows.iter().all(|row| row.is_dir));
        assert_eq!(rows[0].relative, "apps/license-lookup-app");
        // A file's directory is its parent.
        let file = rank(&index(), "License.ts")[0].clone();
        assert_eq!(
            file.directory(Path::new("/w")),
            Path::new("/w/apps/license-lookup-app/src/types")
        );
    }

    #[test]
    fn forgetting_a_recent_keeps_the_selection_in_place() {
        let mut picker = PathPicker::new(1, PickerMode::GoToDirectory);
        picker.adopt(index(), recents(&["/a", "/b", "/c"]));
        picker.move_selection(1);
        assert_eq!(picker.forget_selected_recent(), Some(PathBuf::from("/b")));
        assert_eq!(picker.selected_row().unwrap().relative, "/c");
        // A scanned row is not a recent: nothing to forget.
        picker.move_selection(1);
        assert_eq!(picker.forget_selected_recent(), None);
    }

    #[test]
    fn cd_commands_quote_for_every_shell_and_refuse_what_they_cannot() {
        assert_eq!(
            cd_command(Path::new("/Users/me/it's here")).as_deref(),
            Some("cd -- '/Users/me/it'\\''s here'\r")
        );
        assert_eq!(
            cd_command(Path::new("/w/-rf $(x) `y`")).as_deref(),
            Some("cd -- '/w/-rf $(x) `y`'\r")
        );
        assert_eq!(cd_command(Path::new("/w/a\nb")), None);
        assert_eq!(cd_command(Path::new("/w/a\\b")), None);
        assert_eq!(cd_command(Path::new("relative")), None);
    }
}
