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
    /// A remote session's index is built one directory level at a time from
    /// its Helper (`session.list_dir`): the directories already listed, the
    /// root being `""`. `None` for a local index, which is scanned whole.
    remote_listed: Option<BTreeSet<String>>,
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
            remote_listed: None,
        };
        index.sort_listing();
        index
    }

    /// A remote session's root level, as its Helper listed it relative to the
    /// remote child's live working directory.
    pub fn from_remote(listing: &diri_proto::remote_pty::ListDirResult) -> Self {
        let entries: Vec<PathEntry> = remote_entries("", &listing.entries).collect();
        let mut index = Self {
            root: PathBuf::from(&listing.cwd),
            listing: entries.clone(),
            entries,
            truncated: listing.truncated,
            remote_listed: Some(BTreeSet::from([String::new()])),
        };
        index.sort_listing();
        index
    }

    /// Add one more remote level under `directory` (relative to the root).
    /// Listing a level twice, or past the index cap, adds nothing.
    pub fn merge_remote(
        &mut self,
        directory: &str,
        listing: &diri_proto::remote_pty::ListDirResult,
    ) {
        let Some(listed) = self.remote_listed.as_mut() else {
            return;
        };
        if !listed.insert(directory.to_owned()) {
            return;
        }
        let room = INDEX_CAP.saturating_sub(self.entries.len());
        let before = self.entries.len();
        self.entries
            .extend(remote_entries(directory, &listing.entries).take(room));
        self.truncated |=
            listing.truncated || self.entries.len() - before < listing.entries.len();
    }

    /// The directory a remote query is typing into (`src/ma` → `src`) when
    /// that directory is known and not listed yet, so the picker can fetch
    /// it. Local indexes are complete and never ask.
    pub fn unlisted_directory(&self, query: &str) -> Option<String> {
        let listed = self.remote_listed.as_ref()?;
        let (directory, _) = query.rsplit_once('/')?;
        let directory = directory.trim_end_matches('/');
        if directory.is_empty() || listed.contains(directory) {
            return None;
        }
        self.entries
            .iter()
            .any(|entry| entry.is_dir && entry.relative == directory)
            .then(|| directory.to_owned())
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

fn remote_entries<'a>(
    directory: &'a str,
    entries: &'a [diri_proto::remote_pty::ListDirEntry],
) -> impl Iterator<Item = PathEntry> + 'a {
    entries
        .iter()
        // A name the shell would split on `/` cannot be one entry.
        .filter(|entry| !entry.name.is_empty() && !entry.name.contains('/'))
        .map(move |entry| {
            let relative = if directory.is_empty() {
                entry.name.clone()
            } else {
                format!("{directory}/{}", entry.name)
            };
            PathEntry::new(
                relative,
                entry.kind == diri_proto::remote_pty::ListDirEntryKind::Directory,
            )
        })
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
    pub relative: String,
    pub is_dir: bool,
    pub highlights: Vec<Range<usize>>,
}

impl PickerRow {
    fn plain(entry: &PathEntry) -> Self {
        Self {
            relative: entry.relative.clone(),
            is_dir: entry.is_dir,
            highlights: Vec::new(),
        }
    }
}

/// Rank the index for `query`. An empty query lists the root like `ls`;
/// otherwise the whole tree is fuzzy-matched with `/` as the word boundary,
/// ties going to the shorter (shallower) path.
pub fn rank(index: &PathIndex, query: &str) -> Vec<PickerRow> {
    let parsed = FuzzyQuery::new(query);
    if parsed.is_empty() {
        return index
            .listing
            .iter()
            .take(RESULT_LIMIT)
            .map(PickerRow::plain)
            .collect();
    }
    let mut matcher = FuzzyMatcher::paths();
    let mut scored: Vec<(u32, &PathEntry)> = index
        .entries
        .iter()
        .filter_map(|entry| Some((parsed.score(&entry.prepared, &mut matcher)?, entry)))
        .collect();
    scored.sort_by(|(a_score, a), (b_score, b)| {
        b_score
            .cmp(a_score)
            .then_with(|| a.relative.len().cmp(&b.relative.len()))
            .then_with(|| a.relative.cmp(&b.relative))
    });
    scored.truncate(RESULT_LIMIT);
    scored
        .into_iter()
        .map(|(_, entry)| {
            let highlights = parsed
                .highlights(&entry.prepared, &entry.relative, &mut matcher)
                .map(|(_, ranges)| ranges)
                .unwrap_or_default();
            PickerRow {
                relative: entry.relative.clone(),
                is_dir: entry.is_dir,
                highlights,
            }
        })
        .collect()
}

/// What choosing `row` types into the session. Files end in a space, like a
/// Finder drop, so the next word does not fuse with the path; directories end
/// in `/` so the user can keep typing into them.
pub fn insertion_text(row: &PickerRow) -> String {
    let mut text = escape_path_for_terminal(&row.relative);
    text.push(if row.is_dir { '/' } else { ' ' });
    text
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

fn empty_listing() -> diri_proto::remote_pty::ListDirResult {
    diri_proto::remote_pty::ListDirResult {
        cwd: String::new(),
        path: String::new(),
        entries: Vec::new(),
        truncated: false,
        failure: None,
    }
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
    pub query: QueryEditor,
    pub index: PickerIndex,
    pub rows: Vec<PickerRow>,
    pub selected: usize,
    /// The remote level being fetched, so one keystroke asks only once.
    pending_directory: Option<String>,
}

impl PathPicker {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            query: QueryEditor::default(),
            index: PickerIndex::Loading,
            rows: Vec::new(),
            selected: 0,
            pending_directory: None,
        }
    }

    /// The remote directory the current query needs and nobody has asked
    /// for yet. Marks it pending; the caller lists it and hands the answer
    /// to [`Self::adopt_directory`].
    pub fn take_wanted_directory(&mut self) -> Option<String> {
        let PickerIndex::Ready(index) = &self.index else {
            return None;
        };
        let wanted = index.unlisted_directory(self.query.text())?;
        if self.pending_directory.as_deref() == Some(wanted.as_str()) {
            return None;
        }
        self.pending_directory = Some(wanted.clone());
        Some(wanted)
    }

    /// Merge one fetched remote level and re-rank, keeping the query. A
    /// refused level is simply not merged; the query still ranks what is known.
    pub fn adopt_directory(
        &mut self,
        directory: &str,
        listing: Option<&diri_proto::remote_pty::ListDirResult>,
    ) {
        if self.pending_directory.as_deref() == Some(directory) {
            self.pending_directory = None;
        }
        let PickerIndex::Ready(index) = &mut self.index else {
            return;
        };
        match listing {
            Some(listing) if listing.failure.is_none() => index.merge_remote(directory, listing),
            // Remember the refusal so the same keystrokes do not ask again.
            _ => index.merge_remote(directory, &empty_listing()),
        }
        self.refresh();
    }

    pub fn adopt_index(&mut self, index: PathIndex) {
        self.index = PickerIndex::Ready(index);
        self.refresh();
    }

    pub fn fail(&mut self, message: String) {
        self.index = PickerIndex::Failed(message);
        self.rows.clear();
        self.selected = 0;
    }

    /// Re-rank after the query changed. Selection returns to the best match.
    pub fn refresh(&mut self) {
        if let PickerIndex::Ready(index) = &self.index {
            self.rows = rank(index, self.query.text());
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
        let Some(row) = self.selected_row().filter(|row| row.is_dir) else {
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
        };
        assert_eq!(insertion_text(&file), "notes/Screen\\ Shot\\ \\(1\\).png ");
        let directory = PickerRow {
            relative: "apps/src".into(),
            is_dir: true,
            highlights: Vec::new(),
        };
        assert_eq!(insertion_text(&directory), "apps/src/");
    }

    #[test]
    fn tab_on_a_directory_narrows_the_query_to_its_contents() {
        let mut picker = PathPicker::new(1);
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
        let mut picker = PathPicker::new(1);
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

    fn remote_level(names: &[(&str, bool)]) -> diri_proto::remote_pty::ListDirResult {
        use diri_proto::remote_pty::{ListDirEntry, ListDirEntryKind};
        diri_proto::remote_pty::ListDirResult {
            cwd: "/srv/app".into(),
            path: "/srv/app".into(),
            entries: names
                .iter()
                .map(|(name, is_dir)| ListDirEntry {
                    name: (*name).into(),
                    kind: if *is_dir {
                        ListDirEntryKind::Directory
                    } else {
                        ListDirEntryKind::File
                    },
                    symlink: false,
                })
                .collect(),
            truncated: false,
            failure: None,
        }
    }

    #[test]
    fn a_remote_index_lists_its_root_then_fetches_levels_as_the_query_enters_them() {
        let mut picker = PathPicker::new(1);
        picker.adopt_index(PathIndex::from_remote(&remote_level(&[
            ("src", true),
            (".git", true),
            ("README.md", false),
        ])));
        assert_eq!(picker.root(), Some(Path::new("/srv/app")));
        assert_eq!(relatives(&picker.rows), ["README.md", "src"]);
        assert_eq!(picker.take_wanted_directory(), None, "the root is listed");

        // Tab into `src`: the query names an unlisted level, asked for once.
        picker.selected = 1;
        assert!(picker.descend());
        assert_eq!(picker.take_wanted_directory().as_deref(), Some("src"));
        assert_eq!(picker.take_wanted_directory(), None, "already pending");
        picker.adopt_directory("src", Some(&remote_level(&[("main.rs", false), ("ui", true)])));
        assert_eq!(relatives(&picker.rows)[..2], ["src/ui", "src/main.rs"]);
        assert_eq!(picker.take_wanted_directory(), None, "src is listed now");

        picker.query.insert("ui/");
        picker.refresh();
        assert_eq!(picker.take_wanted_directory().as_deref(), Some("src/ui"));
        // A refused level is remembered and never retried by the same query.
        picker.adopt_directory("src/ui", None);
        assert_eq!(picker.take_wanted_directory(), None);

        // A name that is not a known directory is never asked for.
        picker.query.clear();
        picker.query.insert("README.md/x");
        picker.refresh();
        assert_eq!(picker.take_wanted_directory(), None);
        assert_eq!(insertion_text(&PickerRow {
            relative: "src/main.rs".into(),
            is_dir: false,
            highlights: Vec::new(),
        }), "src/main.rs ");
    }

    #[test]
    fn local_indexes_never_ask_for_remote_levels() {
        let mut picker = PathPicker::new(1);
        picker.adopt_index(index());
        picker.query.insert("apps/");
        picker.refresh();
        assert_eq!(picker.take_wanted_directory(), None);
    }
}
