//! Worktree diff loading and unified-patch parsing.
//!
//! The inspector crosses this module through one interface: a session cwd in,
//! a flat render snapshot out. Git process details, untracked files, hunk line
//! accounting, and output limits stay local to the implementation.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Command;

use diri_proto::{SessionDiffBase, SessionReadDiffResult};

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

const MAX_DIFF_BYTES: usize = 16 * 1024 * 1024;
const MAX_UNTRACKED_FILES: usize = 200;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiffLayer {
    #[default]
    Branch,
    Staged,
    Working,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffRowKind {
    File,
    Hunk,
    Context,
    Addition,
    Deletion,
    Meta,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffRow {
    pub kind: DiffRowKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: String,
}

/// One changed file and the semantic hunks contained by its visible rows.
///
/// `row_range` uses the same indices as [`DiffSnapshot::rows`]. Paths are
/// repository-relative for locally loaded diffs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffFile {
    pub path: PathBuf,
    pub row_range: Range<usize>,
    pub additions: usize,
    pub deletions: usize,
    pub hunks: Vec<DiffHunk>,
}

/// A whole, independently applicable unified-diff hunk.
///
/// `patch` repeats the complete file preamble before the selected hunk so it
/// can be sent directly to `git apply`. The fingerprint is deterministic FNV-1a
/// over those exact bytes and is intended for UI identity, not trust.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffHunk {
    pub header: String,
    pub row_range: Range<usize>,
    pub old_start: Option<u32>,
    pub new_start: Option<u32>,
    pub additions: usize,
    pub deletions: usize,
    pub patch: Vec<u8>,
    pub fingerprint: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffSnapshot {
    pub repo_root: PathBuf,
    pub base_ref: Option<String>,
    pub layer: DiffLayer,
    pub rows: Vec<DiffRow>,
    pub file_diffs: Vec<DiffFile>,
    pub files: usize,
    pub additions: usize,
    pub deletions: usize,
    pub max_text_columns: usize,
    pub truncated: bool,
}

#[derive(Debug)]
pub enum DiffError {
    NotRepository,
    Git(String),
}

impl fmt::Display for DiffError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRepository => {
                formatter.write_str("This session is not inside a Git repository")
            }
            Self::Git(message) => write!(formatter, "Git could not load changes: {message}"),
        }
    }
}

impl std::error::Error for DiffError {}

pub fn load_worktree_diff_against(
    cwd: &Path,
    comparison: SessionDiffBase,
) -> Result<DiffSnapshot, DiffError> {
    if comparison == SessionDiffBase::DefaultBranch {
        return load_local_diff(cwd, DiffLayer::Branch);
    }

    let repo_root = discover_repository(cwd)?;
    load_diff_from_repository(&repo_root, DiffLayer::Branch, LocalDiffSource::Head)
}

/// Loads one semantic local review lane.
///
/// `Branch` is the combined feature-branch overview against the default branch,
/// including index, worktree, and untracked content. `Staged` is HEAD to index;
/// `Working` is index to worktree plus bounded untracked content. Keeping these
/// lanes separate is what makes the returned hunk patches safe to mutate.
pub fn load_local_diff(cwd: &Path, layer: DiffLayer) -> Result<DiffSnapshot, DiffError> {
    let repo_root = discover_repository(cwd)?;
    let source = match layer {
        DiffLayer::Branch => LocalDiffSource::DefaultBranch,
        DiffLayer::Staged => LocalDiffSource::Staged,
        DiffLayer::Working => LocalDiffSource::Working,
    };
    load_diff_from_repository(&repo_root, layer, source)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalDiffSource {
    DefaultBranch,
    Head,
    Staged,
    Working,
}

fn discover_repository(cwd: &Path) -> Result<PathBuf, DiffError> {
    let root_output = git(cwd, ["rev-parse", "--show-toplevel"])?;
    if !root_output.status.success() {
        return Err(DiffError::NotRepository);
    }
    let repo_root = PathBuf::from(String::from_utf8_lossy(&root_output.stdout).trim());
    if repo_root.as_os_str().is_empty() {
        return Err(DiffError::NotRepository);
    }
    Ok(repo_root)
}

fn load_diff_from_repository(
    repo_root: &Path,
    layer: DiffLayer,
    source: LocalDiffSource,
) -> Result<DiffSnapshot, DiffError> {
    let has_head = git(repo_root, ["rev-parse", "--verify", "HEAD"])
        .is_ok_and(|output| output.status.success());
    let mut patch = Vec::new();
    let mut base_ref = None;
    match source {
        LocalDiffSource::DefaultBranch if has_head => {
            let resolution = resolve_comparison(repo_root, SessionDiffBase::DefaultBranch)?;
            base_ref = Some(resolution.label);
            append_output(
                &mut patch,
                git(
                    repo_root,
                    [
                        "diff",
                        "--no-ext-diff",
                        "--no-color",
                        "--unified=3",
                        resolution.commit.as_str(),
                        "--",
                    ],
                )?,
            )?;
        }
        LocalDiffSource::Head if has_head => {
            base_ref = Some("HEAD".to_owned());
            append_output(
                &mut patch,
                git(
                    repo_root,
                    [
                        "diff",
                        "--no-ext-diff",
                        "--no-color",
                        "--unified=3",
                        "HEAD",
                        "--",
                    ],
                )?,
            )?;
        }
        LocalDiffSource::Staged => {
            if has_head {
                base_ref = Some("HEAD".to_owned());
                append_output(
                    &mut patch,
                    git(
                        repo_root,
                        [
                            "diff",
                            "--no-ext-diff",
                            "--no-color",
                            "--unified=3",
                            "--cached",
                            "HEAD",
                            "--",
                        ],
                    )?,
                )?;
            } else {
                append_cached_diff(repo_root, &mut patch)?;
            }
        }
        LocalDiffSource::Working => append_working_diff(repo_root, &mut patch)?,
        LocalDiffSource::DefaultBranch | LocalDiffSource::Head => {
            // An unborn branch has no comparison commit. Preserve the existing
            // combined overview by concatenating index and worktree lanes.
            append_cached_diff(repo_root, &mut patch)?;
            append_working_diff(repo_root, &mut patch)?;
        }
    }

    if matches!(
        source,
        LocalDiffSource::DefaultBranch | LocalDiffSource::Head | LocalDiffSource::Working
    ) {
        append_untracked_diffs(repo_root, &mut patch)?;
    }

    let truncated = patch.len() > MAX_DIFF_BYTES;
    patch.truncate(MAX_DIFF_BYTES);
    let mut snapshot = parse_unified_diff_bytes(&patch);
    snapshot.repo_root = repo_root.to_path_buf();
    snapshot.base_ref = base_ref;
    snapshot.layer = layer;
    snapshot.truncated = truncated;
    if truncated {
        snapshot.rows.push(DiffRow {
            kind: DiffRowKind::Meta,
            old_line: None,
            new_line: None,
            text: "Diff truncated at 16 MB".to_owned(),
        });
    }
    Ok(snapshot)
}

fn append_cached_diff(repo_root: &Path, patch: &mut Vec<u8>) -> Result<(), DiffError> {
    append_output(
        patch,
        git(
            repo_root,
            [
                "diff",
                "--no-ext-diff",
                "--no-color",
                "--unified=3",
                "--cached",
                "--",
            ],
        )?,
    )
}

fn append_working_diff(repo_root: &Path, patch: &mut Vec<u8>) -> Result<(), DiffError> {
    append_output(
        patch,
        git(
            repo_root,
            ["diff", "--no-ext-diff", "--no-color", "--unified=3", "--"],
        )?,
    )
}

fn append_untracked_diffs(repo_root: &Path, patch: &mut Vec<u8>) -> Result<(), DiffError> {
    let untracked = git(
        repo_root,
        ["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    if !untracked.status.success() {
        return Err(git_failure(&untracked));
    }
    for path in untracked
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .take(MAX_UNTRACKED_FILES)
    {
        if patch.len() >= MAX_DIFF_BYTES {
            break;
        }
        #[cfg(unix)]
        let path = OsString::from_vec(path.to_vec());
        #[cfg(not(unix))]
        let path = OsString::from(String::from_utf8_lossy(path).into_owned());
        let output = Command::new("git")
            .current_dir(repo_root)
            .args([
                "diff",
                "--no-index",
                "--no-color",
                "--unified=3",
                "--",
                "/dev/null",
            ])
            .arg(path)
            .output()
            .map_err(|error| DiffError::Git(error.to_string()))?;
        // `git diff --no-index` returns 1 when it found a difference.
        if !output.status.success() && output.status.code() != Some(1) {
            return Err(git_failure(&output));
        }
        append_bytes(patch, &output.stdout);
    }
    Ok(())
}

pub fn parse_unified_diff(patch: &str) -> DiffSnapshot {
    parse_unified_diff_bytes(patch.as_bytes())
}

fn parse_unified_diff_bytes(patch: &[u8]) -> DiffSnapshot {
    let mut snapshot = DiffSnapshot::default();
    let mut old_line = None;
    let mut new_line = None;
    let mut current_file = None;
    let mut current_hunk = None;
    let mut file_preamble = Vec::new();

    for raw_line in patch.split_inclusive(|byte| *byte == b'\n') {
        let line_bytes = trim_patch_line(raw_line);
        let line = String::from_utf8_lossy(line_bytes);

        if let Some(header) = line.strip_prefix("diff --git ") {
            finish_hunk(&mut snapshot, &mut current_hunk);
            finish_file(&mut snapshot, &mut current_file);

            let path = PathBuf::from(diff_path(header));
            let row_start = snapshot.rows.len();
            snapshot.files += 1;
            snapshot.file_diffs.push(DiffFile {
                path: path.clone(),
                row_range: row_start..row_start,
                additions: 0,
                deletions: 0,
                hunks: Vec::new(),
            });
            current_file = Some(snapshot.file_diffs.len() - 1);
            current_hunk = None;
            file_preamble.clear();
            file_preamble.extend_from_slice(raw_line);
            old_line = None;
            new_line = None;

            push_row(
                &mut snapshot,
                DiffRow {
                    kind: DiffRowKind::File,
                    old_line: None,
                    new_line: None,
                    text: path.to_string_lossy().into_owned(),
                },
            );
            continue;
        }

        if line.starts_with("@@") {
            finish_hunk(&mut snapshot, &mut current_hunk);
            let (old, new) = parse_hunk_start(&line);
            let header = line.into_owned();
            old_line = old;
            new_line = new;
            let row_start = snapshot.rows.len();
            if let Some(file_index) = current_file {
                let mut hunk_patch = file_preamble.clone();
                hunk_patch.extend_from_slice(raw_line);
                snapshot.file_diffs[file_index].hunks.push(DiffHunk {
                    header: header.clone(),
                    row_range: row_start..row_start,
                    old_start: old,
                    new_start: new,
                    additions: 0,
                    deletions: 0,
                    patch: hunk_patch,
                    fingerprint: 0,
                });
                current_hunk = Some((file_index, snapshot.file_diffs[file_index].hunks.len() - 1));
            }
            push_row(
                &mut snapshot,
                DiffRow {
                    kind: DiffRowKind::Hunk,
                    old_line: None,
                    new_line: None,
                    text: header,
                },
            );
            continue;
        }

        if let Some((file_index, hunk_index)) = current_hunk {
            snapshot.file_diffs[file_index].hunks[hunk_index]
                .patch
                .extend_from_slice(raw_line);
        } else if current_file.is_some() {
            // Everything before the first hunk is part of the complete file
            // preamble repeated by every independently applicable hunk.
            file_preamble.extend_from_slice(raw_line);
        }

        let row = if line.starts_with("--- ") || line.starts_with("+++ ") {
            continue;
        } else if let Some(text) = line.strip_prefix('+') {
            let current = new_line;
            new_line = new_line.map(|line| line.saturating_add(1));
            snapshot.additions += 1;
            if let Some(file_index) = current_file {
                snapshot.file_diffs[file_index].additions += 1;
            }
            if let Some((file_index, hunk_index)) = current_hunk {
                snapshot.file_diffs[file_index].hunks[hunk_index].additions += 1;
            }
            DiffRow {
                kind: DiffRowKind::Addition,
                old_line: None,
                new_line: current,
                text: text.to_owned(),
            }
        } else if let Some(text) = line.strip_prefix('-') {
            let current = old_line;
            old_line = old_line.map(|line| line.saturating_add(1));
            snapshot.deletions += 1;
            if let Some(file_index) = current_file {
                snapshot.file_diffs[file_index].deletions += 1;
            }
            if let Some((file_index, hunk_index)) = current_hunk {
                snapshot.file_diffs[file_index].hunks[hunk_index].deletions += 1;
            }
            DiffRow {
                kind: DiffRowKind::Deletion,
                old_line: current,
                new_line: None,
                text: text.to_owned(),
            }
        } else if let Some(text) = line.strip_prefix(' ') {
            let old = old_line;
            let new = new_line;
            old_line = old_line.map(|line| line.saturating_add(1));
            new_line = new_line.map(|line| line.saturating_add(1));
            DiffRow {
                kind: DiffRowKind::Context,
                old_line: old,
                new_line: new,
                text: text.to_owned(),
            }
        } else {
            DiffRow {
                kind: DiffRowKind::Meta,
                old_line: None,
                new_line: None,
                text: line.into_owned(),
            }
        };
        push_row(&mut snapshot, row);
    }

    finish_hunk(&mut snapshot, &mut current_hunk);
    finish_file(&mut snapshot, &mut current_file);
    snapshot
}

fn trim_patch_line(mut line: &[u8]) -> &[u8] {
    if let Some(without_newline) = line.strip_suffix(b"\n") {
        line = without_newline;
    }
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn push_row(snapshot: &mut DiffSnapshot, row: DiffRow) {
    snapshot.max_text_columns = snapshot
        .max_text_columns
        .max(row.text.chars().count().min(500));
    snapshot.rows.push(row);
}

fn finish_hunk(snapshot: &mut DiffSnapshot, current: &mut Option<(usize, usize)>) {
    let Some((file_index, hunk_index)) = current.take() else {
        return;
    };
    let hunk = &mut snapshot.file_diffs[file_index].hunks[hunk_index];
    hunk.row_range.end = snapshot.rows.len();
    hunk.fingerprint = fnv1a64(&hunk.patch);
}

fn finish_file(snapshot: &mut DiffSnapshot, current: &mut Option<usize>) {
    let Some(file_index) = current.take() else {
        return;
    };
    snapshot.file_diffs[file_index].row_range.end = snapshot.rows.len();
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

/// Converts the daemon's bounded wire payload into the same render snapshot
/// used by local Git. Keeping this conversion here guarantees identical row,
/// hunk, and summary behavior for local and remote sessions.
pub fn snapshot_from_read_diff(result: SessionReadDiffResult) -> DiffSnapshot {
    let mut snapshot = parse_unified_diff_bytes(&result.patch);
    snapshot.repo_root = PathBuf::from(result.repo_root);
    snapshot.base_ref = result.base_ref;
    snapshot.layer = DiffLayer::Branch;
    snapshot.truncated = result.truncated;
    if result.truncated {
        snapshot.rows.push(DiffRow {
            kind: DiffRowKind::Meta,
            old_line: None,
            new_line: None,
            text: "Diff truncated by the daemon".to_owned(),
        });
    }
    snapshot
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ComparisonResolution {
    label: String,
    commit: String,
}

fn resolve_comparison(
    repo_root: &Path,
    comparison: SessionDiffBase,
) -> Result<ComparisonResolution, DiffError> {
    if comparison == SessionDiffBase::Head {
        return Ok(ComparisonResolution {
            label: "HEAD".to_owned(),
            commit: "HEAD".to_owned(),
        });
    }

    if let Some(base_ref) = resolve_default_branch_ref(repo_root) {
        let merge_base = git(repo_root, ["merge-base", base_ref.as_str(), "HEAD"])?;
        if merge_base.status.success() {
            let commit = String::from_utf8_lossy(&merge_base.stdout)
                .trim()
                .to_owned();
            if !commit.is_empty() {
                return Ok(ComparisonResolution {
                    label: base_ref,
                    commit,
                });
            }
        }
    }

    Ok(ComparisonResolution {
        label: "HEAD".to_owned(),
        commit: "HEAD".to_owned(),
    })
}

fn resolve_default_branch_ref(repo_root: &Path) -> Option<String> {
    let origin_head = git(
        repo_root,
        [
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .ok()
    .filter(|output| output.status.success())
    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    .filter(|candidate| !candidate.is_empty());

    origin_head
        .into_iter()
        .chain(
            ["origin/main", "main", "origin/master", "master"]
                .into_iter()
                .map(str::to_owned),
        )
        .find(|candidate| {
            let peeled = format!("{candidate}^{{commit}}");
            git(
                repo_root,
                ["rev-parse", "--verify", "--quiet", peeled.as_str()],
            )
            .is_ok_and(|output| output.status.success())
        })
}

fn git<I, S>(cwd: &Path, args: I) -> Result<std::process::Output, DiffError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|error| DiffError::Git(error.to_string()))
}

fn append_output(patch: &mut Vec<u8>, output: std::process::Output) -> Result<(), DiffError> {
    if !output.status.success() {
        return Err(git_failure(&output));
    }
    append_bytes(patch, &output.stdout);
    Ok(())
}

fn append_bytes(patch: &mut Vec<u8>, bytes: &[u8]) {
    let remaining = MAX_DIFF_BYTES.saturating_add(1).saturating_sub(patch.len());
    patch.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
}

fn git_failure(output: &std::process::Output) -> DiffError {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    DiffError::Git(if stderr.is_empty() {
        format!("git exited with {}", output.status)
    } else {
        stderr
    })
}

fn diff_path(header: &str) -> String {
    header
        .rsplit_once(" b/")
        .map(|(_, path)| path)
        .or_else(|| header.rsplit_once(" \"b/").map(|(_, path)| path))
        .unwrap_or(header)
        .trim_matches('"')
        .to_owned()
}

fn parse_hunk_start(header: &str) -> (Option<u32>, Option<u32>) {
    let mut fields = header.split_whitespace();
    let _at = fields.next();
    let old = fields.next().and_then(|field| range_start(field, '-'));
    let new = fields.next().and_then(|field| range_start(field, '+'));
    (old, new)
}

fn range_start(field: &str, prefix: char) -> Option<u32> {
    field.strip_prefix(prefix)?.split(',').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_files_hunks_counts_and_line_numbers() {
        let patch = "diff --git a/src/main.rs b/src/main.rs\nindex 111..222 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10,3 +10,4 @@ fn main() {\n same\n-old\n+new\n+extra\n";
        let snapshot = parse_unified_diff(patch);

        assert_eq!(snapshot.files, 1);
        assert_eq!(snapshot.additions, 2);
        assert_eq!(snapshot.deletions, 1);
        assert_eq!(snapshot.rows[0].kind, DiffRowKind::File);
        assert_eq!(snapshot.rows[0].text, "src/main.rs");
        assert_eq!(snapshot.rows[4].old_line, Some(11));
        assert_eq!(snapshot.rows[4].new_line, None);
        assert_eq!(snapshot.rows[5].old_line, None);
        assert_eq!(snapshot.rows[5].new_line, Some(11));
        assert_eq!(snapshot.rows[6].new_line, Some(12));

        let file = &snapshot.file_diffs[0];
        assert_eq!(file.path, Path::new("src/main.rs"));
        assert_eq!(file.row_range, 0..7);
        assert_eq!(file.additions, 2);
        assert_eq!(file.deletions, 1);
        assert_eq!(file.hunks.len(), 1);
        let hunk = &file.hunks[0];
        assert_eq!(hunk.header, "@@ -10,3 +10,4 @@ fn main() {");
        assert_eq!(hunk.row_range, 2..7);
        assert_eq!(hunk.old_start, Some(10));
        assert_eq!(hunk.new_start, Some(10));
        assert_eq!(hunk.additions, 2);
        assert_eq!(hunk.deletions, 1);
        assert_eq!(hunk.patch, patch.as_bytes());
        assert_eq!(hunk.fingerprint, fnv1a64(patch.as_bytes()));
        assert_ne!(hunk.fingerprint, 0);
    }

    #[test]
    fn parses_single_line_hunk_ranges() {
        assert_eq!(parse_hunk_start("@@ -4 +8 @@"), (Some(4), Some(8)));
    }

    #[test]
    fn empty_patch_is_an_empty_snapshot() {
        assert_eq!(parse_unified_diff(""), DiffSnapshot::default());
    }

    #[test]
    fn daemon_diff_uses_the_local_parser_and_marks_truncation() {
        let snapshot = snapshot_from_read_diff(SessionReadDiffResult {
            patch: b"diff --git a/a.txt b/a.txt\n@@ -1 +1 @@\n-old\n+new\n".to_vec(),
            repo_root: "/srv/app".to_owned(),
            truncated: true,
            base_ref: Some("origin/main".to_owned()),
        });

        assert_eq!(snapshot.repo_root, PathBuf::from("/srv/app"));
        assert_eq!(snapshot.files, 1);
        assert_eq!(snapshot.additions, 1);
        assert_eq!(snapshot.deletions, 1);
        assert!(snapshot.truncated);
        assert_eq!(snapshot.base_ref.as_deref(), Some("origin/main"));
        assert_eq!(
            snapshot.rows.last().unwrap().text,
            "Diff truncated by the daemon"
        );
    }

    #[test]
    fn loads_tracked_and_untracked_worktree_changes() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let root = directory.path();
        run(root, &["init", "--quiet"]);
        fs::write(root.join("tracked.txt"), "before\n").unwrap();
        run(root, &["add", "tracked.txt"]);
        run(
            root,
            &[
                "-c",
                "user.name=diri tests",
                "-c",
                "user.email=diri@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        fs::write(root.join("tracked.txt"), "after\n").unwrap();
        fs::write(root.join("untracked.txt"), "new\n").unwrap();

        let snapshot = load_worktree_diff_against(root, SessionDiffBase::DefaultBranch)
            .expect("worktree diff");

        assert_eq!(snapshot.repo_root, root.canonicalize().unwrap());
        assert_eq!(snapshot.files, 2);
        assert_eq!(snapshot.additions, 2);
        assert_eq!(snapshot.deletions, 1);
        assert!(
            snapshot
                .rows
                .iter()
                .any(|row| row.kind == DiffRowKind::File && row.text == "untracked.txt")
        );
    }

    #[test]
    fn local_layers_separate_index_from_worktree_and_untracked_content() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let root = directory.path();
        run(root, &["init", "--quiet"]);
        fs::write(root.join("staged.txt"), "staged base\n").unwrap();
        fs::write(root.join("working.txt"), "working base\n").unwrap();
        run(root, &["add", "--all"]);
        run(
            root,
            &[
                "-c",
                "user.name=diri tests",
                "-c",
                "user.email=diri@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );

        fs::write(root.join("staged.txt"), "staged change\n").unwrap();
        run(root, &["add", "staged.txt"]);
        fs::write(root.join("working.txt"), "working change\n").unwrap();
        fs::write(root.join("untracked.txt"), "new\n").unwrap();

        let staged = load_local_diff(root, DiffLayer::Staged).expect("staged lane");
        assert_eq!(staged.layer, DiffLayer::Staged);
        assert_eq!(staged.files, 1);
        assert_eq!(staged.file_diffs[0].path, Path::new("staged.txt"));

        let working = load_local_diff(root, DiffLayer::Working).expect("working lane");
        assert_eq!(working.layer, DiffLayer::Working);
        assert_eq!(working.files, 2);
        assert!(
            working
                .file_diffs
                .iter()
                .any(|file| file.path == Path::new("working.txt"))
        );
        assert!(
            working
                .file_diffs
                .iter()
                .any(|file| file.path == Path::new("untracked.txt"))
        );

        let branch = load_local_diff(root, DiffLayer::Branch).expect("branch lane");
        assert_eq!(branch.layer, DiffLayer::Branch);
        assert_eq!(branch.files, 3);
    }

    #[test]
    fn loads_committed_branch_changes_against_main() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let root = directory.path();
        run(root, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(root.join("tracked.txt"), "on main\n").unwrap();
        run(root, &["add", "tracked.txt"]);
        run(
            root,
            &[
                "-c",
                "user.name=diri tests",
                "-c",
                "user.email=diri@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "main fixture",
            ],
        );
        run(root, &["checkout", "--quiet", "-b", "feature"]);
        fs::write(root.join("tracked.txt"), "on feature\n").unwrap();
        run(root, &["add", "tracked.txt"]);
        run(
            root,
            &[
                "-c",
                "user.name=diri tests",
                "-c",
                "user.email=diri@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "feature change",
            ],
        );

        let snapshot =
            load_worktree_diff_against(root, SessionDiffBase::DefaultBranch).expect("branch diff");

        assert_eq!(snapshot.files, 1);
        assert_eq!(snapshot.additions, 1);
        assert_eq!(snapshot.deletions, 1);
        assert_eq!(snapshot.base_ref.as_deref(), Some("main"));
    }

    #[test]
    fn head_comparison_excludes_committed_branch_changes() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let root = directory.path();
        run(root, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(root.join("tracked.txt"), "on main\n").unwrap();
        run(root, &["add", "tracked.txt"]);
        run(
            root,
            &[
                "-c",
                "user.name=diri tests",
                "-c",
                "user.email=diri@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "main fixture",
            ],
        );
        run(root, &["checkout", "--quiet", "-b", "feature"]);
        fs::write(root.join("tracked.txt"), "committed feature\n").unwrap();
        run(root, &["add", "tracked.txt"]);
        run(
            root,
            &[
                "-c",
                "user.name=diri tests",
                "-c",
                "user.email=diri@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "feature change",
            ],
        );

        let clean = load_worktree_diff_against(root, SessionDiffBase::Head).expect("head diff");
        assert_eq!(clean.files, 0);
        assert_eq!(clean.base_ref.as_deref(), Some("HEAD"));

        fs::write(root.join("tracked.txt"), "working change\n").unwrap();
        let dirty = load_worktree_diff_against(root, SessionDiffBase::Head).expect("head diff");
        assert_eq!(dirty.files, 1);
        assert_eq!(dirty.additions, 1);
        assert_eq!(dirty.deletions, 1);
    }

    fn run(cwd: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(arguments)
            .output()
            .expect("git command");
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
