//! Native Git review operations.
//!
//! The review cockpit crosses one deep module: discover a repository, read a
//! structured snapshot, then perform path- or patch-scoped mutations. Git's
//! porcelain formats, literal pathspec rules, subprocess hardening, timeouts,
//! and output limits stay local to this implementation.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

const GIT_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_STDOUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 128 * 1024;
const MAX_COMMIT_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_PATCH_BYTES: usize = 4 * 1024 * 1024;
const MAX_STATUS_ENTRIES: usize = 20_000;

/// A discovered, non-bare Git worktree.
///
/// Holding the root makes subsequent operations insensitive to changes in the
/// process working directory and gives every mutation the same path-safety
/// rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitRepository {
    root: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReviewStatus {
    pub repo_root: PathBuf,
    pub branch: BranchInfo,
    pub staged: Vec<FileChange>,
    pub unstaged: Vec<FileChange>,
    pub untracked: Vec<FileChange>,
    pub conflicted: Vec<FileChange>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BranchInfo {
    /// `None` means detached HEAD. An unborn branch still has a name.
    pub name: Option<String>,
    /// The full HEAD object id, or `None` for an unborn branch.
    pub oid: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u64,
    pub behind: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    /// Repository-relative, lossless on Unix, and safe to pass back to a
    /// mutation method as a literal path.
    pub path: PathBuf,
    pub original_path: Option<PathBuf>,
    pub kind: ChangeKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Unmerged,
    Unknown(char),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchMutation {
    Stage,
    Unstage,
    Discard,
}

impl PatchMutation {
    const fn label(self) -> &'static str {
        match self {
            Self::Stage => "staging",
            Self::Unstage => "unstaging",
            Self::Discard => "discarding",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitResult {
    pub oid: String,
    pub summary: String,
}

#[derive(Debug)]
pub enum GitReviewError {
    NotRepository(PathBuf),
    CouldNotRunGit {
        operation: &'static str,
        source: io::Error,
    },
    GitFailed {
        operation: &'static str,
        exit_code: Option<i32>,
        message: String,
    },
    TimedOut {
        operation: &'static str,
        timeout: Duration,
    },
    OutputTooLarge {
        operation: &'static str,
        limit: usize,
    },
    InvalidPath {
        path: PathBuf,
        reason: &'static str,
    },
    EmptySelection,
    EmptyPatch,
    InvalidPatch {
        reason: &'static str,
    },
    PatchTooLarge {
        size: usize,
        limit: usize,
    },
    PatchDoesNotApply {
        mutation: PatchMutation,
        message: String,
    },
    EmptyCommitMessage,
    CommitMessageTooLarge {
        size: usize,
        limit: usize,
    },
    MalformedStatus(String),
}

impl fmt::Display for GitReviewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRepository(path) => write!(
                formatter,
                "{} is not inside a non-bare Git repository",
                path.display()
            ),
            Self::CouldNotRunGit { operation, source } => {
                write!(formatter, "could not run Git while {operation}: {source}")
            }
            Self::GitFailed {
                operation,
                exit_code,
                message,
            } => {
                let exit = exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_owned());
                if message.is_empty() {
                    write!(formatter, "Git failed while {operation} (exit {exit})")
                } else {
                    write!(
                        formatter,
                        "Git failed while {operation} (exit {exit}): {message}"
                    )
                }
            }
            Self::TimedOut { operation, timeout } => write!(
                formatter,
                "Git timed out after {:.0}s while {operation}",
                timeout.as_secs_f32()
            ),
            Self::OutputTooLarge { operation, limit } => write!(
                formatter,
                "Git produced more than {limit} bytes while {operation}"
            ),
            Self::InvalidPath { path, reason } => {
                write!(
                    formatter,
                    "unsafe repository path {}: {reason}",
                    path.display()
                )
            }
            Self::EmptySelection => formatter.write_str("select at least one changed path"),
            Self::EmptyPatch => formatter.write_str("review patch cannot be empty"),
            Self::InvalidPatch { reason } => write!(formatter, "invalid review patch: {reason}"),
            Self::PatchTooLarge { size, limit } => {
                write!(formatter, "review patch is {size} bytes (limit {limit})")
            }
            Self::PatchDoesNotApply { mutation, message } => {
                write!(
                    formatter,
                    "review patch is stale or no longer applies while {}: {message}",
                    mutation.label()
                )
            }
            Self::EmptyCommitMessage => formatter.write_str("commit message cannot be empty"),
            Self::CommitMessageTooLarge { size, limit } => {
                write!(formatter, "commit message is {size} bytes (limit {limit})")
            }
            Self::MalformedStatus(message) => {
                write!(formatter, "Git returned malformed status data: {message}")
            }
        }
    }
}

impl std::error::Error for GitReviewError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CouldNotRunGit { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl GitRepository {
    /// Resolves the worktree root containing `cwd`.
    pub fn discover(cwd: &Path) -> Result<Self, GitReviewError> {
        let output = run_git(
            cwd,
            ["rev-parse", "--show-toplevel"],
            None,
            "finding repository",
        )?;
        if !output.status.success() {
            let message = output.stderr_message();
            if message.contains("not a git repository") || message.contains("not a git work tree") {
                return Err(GitReviewError::NotRepository(cwd.to_path_buf()));
            }
            return Err(output.failure("finding repository"));
        }

        let root = path_from_output_line(&output.stdout);
        if root.as_os_str().is_empty() {
            return Err(GitReviewError::NotRepository(cwd.to_path_buf()));
        }
        let bare = run_git(
            &root,
            ["rev-parse", "--is-bare-repository"],
            None,
            "checking repository",
        )?;
        let bare = ensure_success(bare, "checking repository")?;
        if String::from_utf8_lossy(&bare.stdout).trim() == "true" {
            return Err(GitReviewError::NotRepository(cwd.to_path_buf()));
        }

        Ok(Self { root })
    }

    /// Reads porcelain-v2 status. Conflicts are reported only in
    /// `conflicted`; an ordinary path can appear in both `staged` and
    /// `unstaged` when its index and worktree versions both changed.
    pub fn status(&self) -> Result<ReviewStatus, GitReviewError> {
        let output = run_git(
            &self.root,
            [
                "status",
                "--porcelain=v2",
                "--branch",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ],
            None,
            "reading status",
        )?;
        ensure_success(output, "reading status")
            .and_then(|output| parse_status(&self.root, &output.stdout))
    }

    /// Every requested path, plus the source of any staged rename whose
    /// destination was requested. Order is preserved and sources are appended
    /// without duplicating a path the caller already named.
    fn with_rename_sources(&self, paths: &[PathBuf]) -> Result<Vec<PathBuf>, GitReviewError> {
        let mut resolved = paths.to_vec();
        let renames: Vec<_> = self
            .status()?
            .staged
            .into_iter()
            .filter(|change| matches!(change.kind, ChangeKind::Renamed | ChangeKind::Copied))
            .filter_map(|change| change.original_path.map(|source| (change.path, source)))
            .collect();
        for (destination, source) in renames {
            if paths.iter().any(|path| path == &destination) && !resolved.contains(&source) {
                resolved.push(source);
            }
        }
        Ok(resolved)
    }

    /// Stages the complete current contents of each path, including deletions.
    pub fn stage_paths(&self, paths: &[PathBuf]) -> Result<(), GitReviewError> {
        let paths = validate_paths(paths)?;
        let mut args = literal_path_command(["add"]);
        args.push(OsString::from("--"));
        args.extend(paths);
        self.run_mutation(args, "staging paths")
    }

    /// Restores paths in the index from HEAD. On an unborn branch, removes
    /// them from the index while preserving their worktree contents.
    ///
    /// A staged rename occupies two index entries, and callers only know the
    /// destination — the diff row and the file list both name the new path.
    /// Resetting that path alone leaves the source staged as a deletion, so
    /// the next commit removes the file's content entirely. Rename sources are
    /// resolved here, where the porcelain format is already understood, rather
    /// than asking every caller to carry them.
    pub fn unstage_paths(&self, paths: &[PathBuf]) -> Result<(), GitReviewError> {
        let paths = self.with_rename_sources(paths)?;
        let paths = validate_paths(&paths)?;
        let head = run_git(
            &self.root,
            ["rev-parse", "--verify", "--quiet", "HEAD"],
            None,
            "checking HEAD",
        )?;
        let mut args = if head.status.success() {
            literal_path_command(["reset", "--quiet", "HEAD"])
        } else if head.status.code() == Some(1) {
            literal_path_command(["rm", "--quiet", "-r", "-f", "--cached", "--ignore-unmatch"])
        } else {
            return Err(head.failure("checking HEAD"));
        };
        args.push(OsString::from("--"));
        args.extend(paths);
        self.run_mutation(args, "unstaging paths")
    }

    /// Discards only unstaged tracked changes, restoring from the index.
    /// Untracked files are never deleted by this method.
    pub fn discard_unstaged(&self, paths: &[PathBuf]) -> Result<(), GitReviewError> {
        let paths = validate_paths(paths)?;
        let mut args = literal_path_command(["restore", "--worktree"]);
        args.push(OsString::from("--"));
        args.extend(paths);
        self.run_mutation(args, "discarding unstaged changes")
    }

    /// Applies one or more complete unified-diff hunks as a single mutation.
    ///
    /// The patch is preflighted against the current repository state. A hunk
    /// loaded before an overlapping external edit therefore returns
    /// [`GitReviewError::PatchDoesNotApply`] without changing the index or
    /// worktree. `Discard` never receives `--cached`, so callers must only pass
    /// patches from the tracked working-tree lane.
    pub fn apply_patch(&self, patch: &[u8], mutation: PatchMutation) -> Result<(), GitReviewError> {
        if patch.iter().all(u8::is_ascii_whitespace) {
            return Err(GitReviewError::EmptyPatch);
        }
        if patch.len() > MAX_PATCH_BYTES {
            return Err(GitReviewError::PatchTooLarge {
                size: patch.len(),
                limit: MAX_PATCH_BYTES,
            });
        }
        if patch.contains(&0) {
            return Err(GitReviewError::InvalidPatch {
                reason: "patch contains a NUL byte",
            });
        }
        if mutation == PatchMutation::Discard && patch_creates_file(patch) {
            return Err(GitReviewError::InvalidPatch {
                reason: "discard cannot delete an untracked or newly added file",
            });
        }

        let mut options = vec!["apply", "--recount", "--whitespace=nowarn"];
        match mutation {
            PatchMutation::Stage => options.push("--cached"),
            PatchMutation::Unstage => {
                options.push("--cached");
                options.push("--reverse");
            }
            PatchMutation::Discard => options.push("--reverse"),
        }

        if mutation == PatchMutation::Stage {
            // `--cached --check` only validates the patch's preimage against
            // the index. Also reverse-check its postimage against the live
            // worktree so a snapshot cannot stage content that has since been
            // edited again. Unrelated hunks remain valid and can still stage.
            let worktree_check = run_git(
                &self.root,
                [
                    "apply",
                    "--recount",
                    "--whitespace=nowarn",
                    "--reverse",
                    "--check",
                ],
                Some(patch),
                "checking review patch against worktree",
            )?;
            if !worktree_check.status.success() {
                return Err(patch_rejected(worktree_check, mutation));
            }
        }

        let mut check_options = options.clone();
        check_options.push("--check");
        let preflight = run_git(
            &self.root,
            check_options,
            Some(patch),
            "checking review patch",
        )?;
        if !preflight.status.success() {
            return Err(patch_rejected(preflight, mutation));
        }

        let applied = run_git(&self.root, options, Some(patch), "applying review patch")?;
        if !applied.status.success() {
            return Err(patch_rejected(applied, mutation));
        }
        Ok(())
    }

    /// Creates a commit from the current index with a validated message.
    /// Editor, signing, and hooks are disabled so this operation cannot prompt
    /// outside the cockpit; the repository's index is otherwise respected.
    pub fn commit(&self, message: &str) -> Result<CommitResult, GitReviewError> {
        if message.trim().is_empty() {
            return Err(GitReviewError::EmptyCommitMessage);
        }
        if message.len() > MAX_COMMIT_MESSAGE_BYTES {
            return Err(GitReviewError::CommitMessageTooLarge {
                size: message.len(),
                limit: MAX_COMMIT_MESSAGE_BYTES,
            });
        }
        if message.as_bytes().contains(&0) {
            return Err(GitReviewError::GitFailed {
                operation: "validating commit message",
                exit_code: None,
                message: "commit message contains a NUL byte".to_owned(),
            });
        }

        let output = run_git(
            &self.root,
            [
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "--quiet",
                "--file=-",
                "--cleanup=strip",
            ],
            Some(message.as_bytes()),
            "creating commit",
        )?;
        ensure_success(output, "creating commit")?;

        let identity = run_git(
            &self.root,
            ["show", "-s", "--format=%H%x00%s", "HEAD"],
            None,
            "reading new commit",
        )?;
        let identity = ensure_success(identity, "reading new commit")?;
        let identity = trim_line_ending(&identity.stdout);
        let separator = identity.iter().position(|byte| *byte == 0).ok_or_else(|| {
            GitReviewError::MalformedStatus("new commit identity has no separator".to_owned())
        })?;
        let (oid, summary) = (&identity[..separator], &identity[separator + 1..]);
        let oid = String::from_utf8_lossy(oid).into_owned();
        if oid.is_empty() {
            return Err(GitReviewError::MalformedStatus(
                "new commit object id is empty".to_owned(),
            ));
        }

        Ok(CommitResult {
            oid,
            summary: String::from_utf8_lossy(summary).into_owned(),
        })
    }

    fn run_mutation(
        &self,
        args: Vec<OsString>,
        operation: &'static str,
    ) -> Result<(), GitReviewError> {
        let output = run_git(&self.root, args, None, operation)?;
        ensure_success(output, operation).map(|_| ())
    }
}

fn patch_creates_file(patch: &[u8]) -> bool {
    patch.split(|byte| *byte == b'\n').any(|line| {
        line.strip_suffix(b"\r").unwrap_or(line) == b"--- /dev/null"
            || line.starts_with(b"new file mode ")
    })
}

fn patch_rejected(output: GitOutput, mutation: PatchMutation) -> GitReviewError {
    let message = output.stderr_message();
    GitReviewError::PatchDoesNotApply {
        mutation,
        message: if message.is_empty() {
            "Git rejected the selected hunk; refresh the review and try again".to_owned()
        } else {
            message
        },
    }
}

fn literal_path_command<const N: usize>(command: [&str; N]) -> Vec<OsString> {
    let mut args = vec![OsString::from("--literal-pathspecs")];
    args.extend(command.into_iter().map(OsString::from));
    args
}

fn validate_paths(paths: &[PathBuf]) -> Result<Vec<OsString>, GitReviewError> {
    if paths.is_empty() {
        return Err(GitReviewError::EmptySelection);
    }

    paths
        .iter()
        .map(|path| {
            if path.as_os_str().is_empty() {
                return Err(invalid_path(path, "path is empty"));
            }
            if path.is_absolute() {
                return Err(invalid_path(path, "absolute paths are not accepted"));
            }

            let mut components = path.components();
            let Some(first) = components.next() else {
                return Err(invalid_path(path, "path is empty"));
            };
            let first = match first {
                Component::Normal(first) => first,
                Component::ParentDir => {
                    return Err(invalid_path(path, "parent traversal is not accepted"));
                }
                Component::CurDir => {
                    return Err(invalid_path(path, "current-directory paths are too broad"));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(invalid_path(path, "absolute paths are not accepted"));
                }
            };
            if os_str_eq_ignore_ascii_case(first, OsStr::new(".git")) {
                return Err(invalid_path(path, "Git metadata cannot be mutated"));
            }
            for component in components {
                if !matches!(component, Component::Normal(_)) {
                    return Err(invalid_path(
                        path,
                        "only normalized repository-relative paths are accepted",
                    ));
                }
            }
            if os_str_contains_nul(path.as_os_str()) {
                return Err(invalid_path(path, "path contains a NUL byte"));
            }
            Ok(path.as_os_str().to_owned())
        })
        .collect()
}

fn invalid_path(path: &Path, reason: &'static str) -> GitReviewError {
    GitReviewError::InvalidPath {
        path: path.to_path_buf(),
        reason,
    }
}

fn os_str_eq_ignore_ascii_case(left: &OsStr, right: &OsStr) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(unix)]
fn os_str_contains_nul(value: &OsStr) -> bool {
    value.as_bytes().contains(&0)
}

#[cfg(not(unix))]
fn os_str_contains_nul(value: &OsStr) -> bool {
    value.to_string_lossy().contains('\0')
}

fn parse_status(root: &Path, bytes: &[u8]) -> Result<ReviewStatus, GitReviewError> {
    let records: Vec<&[u8]> = bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .collect();
    let mut status = ReviewStatus {
        repo_root: root.to_path_buf(),
        ..ReviewStatus::default()
    };
    let mut index = 0;
    let mut entries = 0;

    while index < records.len() {
        let record = records[index];
        index += 1;

        if let Some(header) = record.strip_prefix(b"# ") {
            parse_branch_header(&mut status.branch, header)?;
            continue;
        }

        entries += 1;
        if entries > MAX_STATUS_ENTRIES {
            return Err(GitReviewError::OutputTooLarge {
                operation: "reading status",
                limit: MAX_STATUS_ENTRIES,
            });
        }

        match record.first().copied() {
            Some(b'1') => {
                let fields = split_fields(record, 9);
                require_fields(&fields, 9, "ordinary entry")?;
                add_tracked_change(&mut status, fields[1], fields[8], None, false)?;
            }
            Some(b'2') => {
                let fields = split_fields(record, 10);
                require_fields(&fields, 10, "rename/copy entry")?;
                let original = records.get(index).copied().ok_or_else(|| {
                    GitReviewError::MalformedStatus(
                        "rename/copy entry has no original path".to_owned(),
                    )
                })?;
                index += 1;
                add_tracked_change(
                    &mut status,
                    fields[1],
                    fields[9],
                    Some(path_from_bytes(original)),
                    false,
                )?;
            }
            Some(b'u') => {
                let fields = split_fields(record, 11);
                require_fields(&fields, 11, "unmerged entry")?;
                add_tracked_change(&mut status, fields[1], fields[10], None, true)?;
            }
            Some(b'?') if record.get(1) == Some(&b' ') => {
                status.untracked.push(FileChange {
                    path: path_from_bytes(&record[2..]),
                    original_path: None,
                    kind: ChangeKind::Added,
                });
            }
            Some(other) => {
                return Err(GitReviewError::MalformedStatus(format!(
                    "unknown entry kind {:?}",
                    char::from(other)
                )));
            }
            None => {}
        }
    }

    Ok(status)
}

fn parse_branch_header(branch: &mut BranchInfo, header: &[u8]) -> Result<(), GitReviewError> {
    if let Some(value) = header.strip_prefix(b"branch.oid ") {
        if value == b"(initial)" {
            branch.oid = None;
        } else {
            branch.oid = Some(String::from_utf8_lossy(value).into_owned());
        }
    } else if let Some(value) = header.strip_prefix(b"branch.head ") {
        if value == b"(detached)" {
            branch.name = None;
        } else {
            branch.name = Some(String::from_utf8_lossy(value).into_owned());
        }
    } else if let Some(value) = header.strip_prefix(b"branch.upstream ") {
        branch.upstream = Some(String::from_utf8_lossy(value).into_owned());
    } else if let Some(value) = header.strip_prefix(b"branch.ab ") {
        let fields = split_fields(value, 2);
        require_fields(&fields, 2, "branch ahead/behind header")?;
        branch.ahead = parse_prefixed_count(fields[0], b'+')?;
        branch.behind = parse_prefixed_count(fields[1], b'-')?;
    }
    Ok(())
}

fn parse_prefixed_count(value: &[u8], prefix: u8) -> Result<u64, GitReviewError> {
    let Some(number) = value.strip_prefix(&[prefix]) else {
        return Err(GitReviewError::MalformedStatus(format!(
            "branch count {:?} has the wrong prefix",
            String::from_utf8_lossy(value)
        )));
    };
    String::from_utf8_lossy(number).parse().map_err(|_| {
        GitReviewError::MalformedStatus(format!(
            "branch count {:?} is not a number",
            String::from_utf8_lossy(value)
        ))
    })
}

fn add_tracked_change(
    status: &mut ReviewStatus,
    xy: &[u8],
    path: &[u8],
    original_path: Option<PathBuf>,
    explicitly_unmerged: bool,
) -> Result<(), GitReviewError> {
    if xy.len() != 2 {
        return Err(GitReviewError::MalformedStatus(format!(
            "XY status {:?} is not two bytes",
            String::from_utf8_lossy(xy)
        )));
    }
    let index_kind = xy[0];
    let worktree_kind = xy[1];
    let path = path_from_bytes(path);

    if explicitly_unmerged || is_unmerged(index_kind, worktree_kind) {
        status.conflicted.push(FileChange {
            path,
            original_path,
            kind: ChangeKind::Unmerged,
        });
        return Ok(());
    }

    if index_kind != b'.' {
        status.staged.push(FileChange {
            path: path.clone(),
            original_path: original_path.clone(),
            kind: change_kind(index_kind),
        });
    }
    if worktree_kind != b'.' {
        status.unstaged.push(FileChange {
            path,
            original_path,
            kind: change_kind(worktree_kind),
        });
    }
    Ok(())
}

fn is_unmerged(index: u8, worktree: u8) -> bool {
    index == b'U' || worktree == b'U' || matches!((index, worktree), (b'D', b'D') | (b'A', b'A'))
}

fn change_kind(value: u8) -> ChangeKind {
    match value {
        b'A' => ChangeKind::Added,
        b'M' => ChangeKind::Modified,
        b'D' => ChangeKind::Deleted,
        b'R' => ChangeKind::Renamed,
        b'C' => ChangeKind::Copied,
        b'T' => ChangeKind::TypeChanged,
        b'U' => ChangeKind::Unmerged,
        other => ChangeKind::Unknown(char::from(other)),
    }
}

fn split_fields(bytes: &[u8], count: usize) -> Vec<&[u8]> {
    bytes.splitn(count, |byte| *byte == b' ').collect()
}

fn require_fields(
    fields: &[&[u8]],
    expected: usize,
    description: &str,
) -> Result<(), GitReviewError> {
    if fields.len() == expected {
        Ok(())
    } else {
        Err(GitReviewError::MalformedStatus(format!(
            "{description} has {} fields, expected {expected}",
            fields.len()
        )))
    }
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

fn path_from_output_line(bytes: &[u8]) -> PathBuf {
    path_from_bytes(trim_line_ending(bytes))
}

fn trim_line_ending(mut bytes: &[u8]) -> &[u8] {
    if bytes.ends_with(b"\n") {
        bytes = &bytes[..bytes.len() - 1];
    }
    if bytes.ends_with(b"\r") {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

struct GitOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl GitOutput {
    fn stderr_message(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_owned()
    }

    fn failure(&self, operation: &'static str) -> GitReviewError {
        GitReviewError::GitFailed {
            operation,
            exit_code: self.status.code(),
            message: self.stderr_message(),
        }
    }
}

fn ensure_success(output: GitOutput, operation: &'static str) -> Result<GitOutput, GitReviewError> {
    if output.status.success() {
        Ok(output)
    } else {
        Err(output.failure(operation))
    }
}

fn run_git<I, S>(
    cwd: &Path,
    args: I,
    input: Option<&[u8]>,
    operation: &'static str,
) -> Result<GitOutput, GitReviewError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .arg("--no-pager")
        .arg("-c")
        .arg("color.ui=false")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true")
        .env("SSH_ASKPASS", "true")
        .env("GCM_INTERACTIVE", "never")
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .env("LC_ALL", "C")
        .env("GIT_OPTIONAL_LOCKS", "0");

    let mut child = command
        .spawn()
        .map_err(|source| GitReviewError::CouldNotRunGit { operation, source })?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || read_bounded(stdout, MAX_STDOUT_BYTES));
    let stderr_reader = thread::spawn(move || read_bounded(stderr, MAX_STDERR_BYTES));

    let stdin_writer = input.map(|input| {
        let input = input.to_vec();
        let mut stdin = child.stdin.take().expect("piped stdin");
        thread::spawn(move || {
            let result = stdin.write_all(&input);
            drop(stdin);
            result
        })
    });

    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitReviewError::TimedOut {
                    operation,
                    timeout: GIT_TIMEOUT,
                });
            }
            Err(source) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitReviewError::CouldNotRunGit { operation, source });
            }
        }
    };

    if let Some(writer) = stdin_writer {
        match writer.join() {
            Ok(Ok(())) => {}
            // Git may reject input before consuming the complete patch. Its
            // stderr is more useful than the resulting broken pipe.
            Ok(Err(error)) if error.kind() == io::ErrorKind::BrokenPipe => {}
            Ok(Err(source)) => {
                return Err(GitReviewError::CouldNotRunGit { operation, source });
            }
            Err(_) => {
                return Err(GitReviewError::CouldNotRunGit {
                    operation,
                    source: io::Error::other("Git stdin writer panicked"),
                });
            }
        }
    }

    let (stdout, stdout_truncated) = join_reader(stdout_reader, operation)?;
    let (stderr, stderr_truncated) = join_reader(stderr_reader, operation)?;
    if stdout_truncated {
        return Err(GitReviewError::OutputTooLarge {
            operation,
            limit: MAX_STDOUT_BYTES,
        });
    }
    let stderr = if stderr_truncated {
        let mut stderr = stderr;
        stderr.extend_from_slice(b"\n[stderr truncated]");
        stderr
    } else {
        stderr
    };

    Ok(GitOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_bounded<R: Read>(mut reader: R, limit: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut truncated = false;
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..count.min(remaining)]);
        truncated |= count > remaining;
    }
    Ok((bytes, truncated))
}

fn join_reader(
    reader: thread::JoinHandle<io::Result<(Vec<u8>, bool)>>,
    operation: &'static str,
) -> Result<(Vec<u8>, bool), GitReviewError> {
    match reader.join() {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(source)) => Err(GitReviewError::CouldNotRunGit { operation, source }),
        Err(_) => Err(GitReviewError::CouldNotRunGit {
            operation,
            source: io::Error::other("Git output reader panicked"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffHunk, DiffLayer, load_local_diff};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_REPO: AtomicU64 = AtomicU64::new(0);

    struct TestRepo {
        path: PathBuf,
    }

    impl TestRepo {
        /// `None` is the intentional, clean skip path when Git is unavailable.
        fn new() -> Option<Self> {
            if !Command::new("git")
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
            {
                eprintln!("skipping Git review test: git is unavailable");
                return None;
            }
            let ordinal = NEXT_REPO.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("diri-git-review-{}-{ordinal}", std::process::id()));
            fs::create_dir(&path).expect("create test repository directory");
            let repo = Self { path };
            repo.git(["init", "--quiet"]);
            repo.git(["symbolic-ref", "HEAD", "refs/heads/main"]);
            repo.git(["config", "user.name", "Diri Test"]);
            repo.git(["config", "user.email", "diri@example.invalid"]);
            Some(repo)
        }

        fn git<const N: usize>(&self, args: [&str; N]) -> Vec<u8> {
            let output = Command::new("git")
                .current_dir(&self.path)
                .args(args)
                .stdin(Stdio::null())
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .expect("run test git");
            assert!(
                output.status.success(),
                "git failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }

        fn git_expect_failure<const N: usize>(&self, args: [&str; N]) {
            let output = Command::new("git")
                .current_dir(&self.path)
                .args(args)
                .stdin(Stdio::null())
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .expect("run test git");
            assert!(!output.status.success());
        }

        fn write(&self, path: &str, contents: &str) {
            let path = self.path.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }

        fn review(&self) -> GitRepository {
            GitRepository::discover(&self.path).unwrap()
        }

        fn commit_all(&self, message: &str) {
            self.git(["add", "--all"]);
            self.git(["commit", "--quiet", "-m", message]);
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Unstaging a staged rename by its destination alone used to leave the
    /// source staged as a deletion, so the next commit dropped the file's
    /// content. Both index entries must come back together.
    #[test]
    fn unstaging_a_rename_by_its_destination_restores_the_source() {
        let Some(repo) = TestRepo::new() else { return };
        repo.write("old.txt", "content worth keeping\n");
        repo.commit_all("base");
        repo.git(["mv", "old.txt", "new.txt"]);

        let review = repo.review();
        let staged = review.status().unwrap().staged;
        assert_eq!(staged.len(), 1, "expected one staged rename: {staged:?}");
        assert_eq!(staged[0].kind, ChangeKind::Renamed);

        review
            .unstage_paths(&[PathBuf::from("new.txt")])
            .expect("unstage the rename destination");

        let status = review.status().unwrap();
        assert!(
            status.staged.is_empty(),
            "nothing should remain staged, found {:?}",
            status.staged
        );
        assert!(
            repo.path.join("new.txt").exists(),
            "the renamed file must survive in the worktree"
        );
        // The original content is still reachable from HEAD, which is what a
        // staged deletion would have destroyed on the next commit.
        let head = repo.git(["show", "HEAD:old.txt"]);
        assert_eq!(String::from_utf8_lossy(&head), "content worth keeping\n");
    }

    #[test]
    fn discovers_nested_repository_and_reads_grouped_status() {
        let Some(repo) = TestRepo::new() else { return };
        repo.write("tracked.txt", "base\n");
        repo.commit_all("base");
        repo.write("tracked.txt", "worktree\n");
        repo.write("both.txt", "staged\n");
        repo.git(["add", "both.txt"]);
        repo.write("both.txt", "staged and worktree\n");
        repo.write(":(glob) literal name.txt", "untracked\n");
        fs::create_dir(repo.path.join("nested")).unwrap();

        let review = GitRepository::discover(&repo.path.join("nested")).unwrap();
        assert_eq!(review.root, repo.path.canonicalize().unwrap());
        let status = review.status().unwrap();

        assert_eq!(status.branch.name.as_deref(), Some("main"));
        assert!(status.branch.oid.is_some());
        assert_eq!(status.staged.len(), 1);
        assert_eq!(status.staged[0].path, Path::new("both.txt"));
        assert_eq!(status.staged[0].kind, ChangeKind::Added);
        assert_eq!(status.unstaged.len(), 2);
        assert!(status.unstaged.iter().any(|change| {
            change.path == Path::new("tracked.txt") && change.kind == ChangeKind::Modified
        }));
        assert!(status.unstaged.iter().any(|change| {
            change.path == Path::new("both.txt") && change.kind == ChangeKind::Modified
        }));
        assert_eq!(status.untracked.len(), 1);
        assert_eq!(
            status.untracked[0].path,
            Path::new(":(glob) literal name.txt")
        );
        assert!(status.conflicted.is_empty());
    }

    #[test]
    fn stage_unstage_and_discard_use_literal_paths() {
        let Some(repo) = TestRepo::new() else { return };
        repo.write("base.txt", "base\n");
        repo.commit_all("base");
        let review = repo.review();
        let magic = PathBuf::from(":(glob) [literal].txt");
        repo.write(magic.to_str().unwrap(), "one\n");

        review.stage_paths(std::slice::from_ref(&magic)).unwrap();
        assert!(
            review
                .status()
                .unwrap()
                .staged
                .iter()
                .any(|c| c.path == magic)
        );

        review.unstage_paths(std::slice::from_ref(&magic)).unwrap();
        let status = review.status().unwrap();
        assert!(status.staged.is_empty());
        assert!(status.untracked.iter().any(|c| c.path == magic));

        repo.write("base.txt", "changed\n");
        review
            .discard_unstaged(&[PathBuf::from("base.txt")])
            .unwrap();
        assert_eq!(
            fs::read_to_string(repo.path.join("base.txt")).unwrap(),
            "base\n"
        );
    }

    #[test]
    fn whole_hunk_patches_stage_unstage_and_discard_only_the_selected_hunk() {
        let Some(repo) = TestRepo::new() else { return };
        let base = numbered_lines(30);
        repo.write("review.txt", &base);
        repo.commit_all("base");

        let mut changed = base.lines().map(str::to_owned).collect::<Vec<_>>();
        changed[1] = "changed early".to_owned();
        changed[20] = "changed late".to_owned();
        repo.write("review.txt", &(changed.join("\n") + "\n"));

        let working = load_local_diff(&repo.path, DiffLayer::Working).unwrap();
        let file = working
            .file_diffs
            .iter()
            .find(|file| file.path == Path::new("review.txt"))
            .unwrap();
        assert_eq!(file.hunks.len(), 2);
        let early = hunk_containing(&file.hunks, "changed early");
        repo.review()
            .apply_patch(&early.patch, PatchMutation::Stage)
            .unwrap();

        let cached = String::from_utf8(repo.git(["diff", "--cached", "--", "review.txt"]))
            .expect("utf-8 cached diff");
        let unstaged =
            String::from_utf8(repo.git(["diff", "--", "review.txt"])).expect("utf-8 worktree diff");
        assert!(cached.contains("changed early"));
        assert!(!cached.contains("changed late"));
        assert!(!unstaged.contains("changed early"));
        assert!(unstaged.contains("changed late"));

        let staged = load_local_diff(&repo.path, DiffLayer::Staged).unwrap();
        let staged_hunk = &staged.file_diffs[0].hunks[0];
        repo.review()
            .apply_patch(&staged_hunk.patch, PatchMutation::Unstage)
            .unwrap();
        assert!(repo.git(["diff", "--cached"]).is_empty());

        let working = load_local_diff(&repo.path, DiffLayer::Working).unwrap();
        let file = working
            .file_diffs
            .iter()
            .find(|file| file.path == Path::new("review.txt"))
            .unwrap();
        let early = hunk_containing(&file.hunks, "changed early");
        repo.review()
            .apply_patch(&early.patch, PatchMutation::Discard)
            .unwrap();

        let contents = fs::read_to_string(repo.path.join("review.txt")).unwrap();
        assert!(contents.contains("line 02"));
        assert!(!contents.contains("changed early"));
        assert!(contents.contains("changed late"));
    }

    #[test]
    fn untracked_whole_hunk_patch_can_be_staged() {
        let Some(repo) = TestRepo::new() else { return };
        repo.write("base.txt", "base\n");
        repo.commit_all("base");
        repo.write("new.txt", "first\nsecond\n");

        let working = load_local_diff(&repo.path, DiffLayer::Working).unwrap();
        let file = working
            .file_diffs
            .iter()
            .find(|file| file.path == Path::new("new.txt"))
            .unwrap();
        assert_eq!(file.hunks.len(), 1);
        let review = repo.review();
        assert!(matches!(
            review.apply_patch(&file.hunks[0].patch, PatchMutation::Discard),
            Err(GitReviewError::InvalidPatch { .. })
        ));
        assert_eq!(
            fs::read_to_string(repo.path.join("new.txt")).unwrap(),
            "first\nsecond\n"
        );
        review
            .apply_patch(&file.hunks[0].patch, PatchMutation::Stage)
            .unwrap();

        assert_eq!(
            String::from_utf8(repo.git(["show", ":new.txt"])).unwrap(),
            "first\nsecond\n"
        );
        assert!(
            repo.review()
                .status()
                .unwrap()
                .staged
                .iter()
                .any(|change| change.path == Path::new("new.txt"))
        );
    }

    #[test]
    fn stale_hunk_patch_is_rejected_without_mutating_the_index() {
        let Some(repo) = TestRepo::new() else { return };
        repo.write("stale.txt", "before\n");
        repo.commit_all("base");
        repo.write("stale.txt", "first edit\n");
        let snapshot = load_local_diff(&repo.path, DiffLayer::Working).unwrap();
        let patch = snapshot.file_diffs[0].hunks[0].patch.clone();

        repo.write("stale.txt", "overlapping newer edit\n");
        assert!(matches!(
            repo.review().apply_patch(&patch, PatchMutation::Stage),
            Err(GitReviewError::PatchDoesNotApply {
                mutation: PatchMutation::Stage,
                ..
            })
        ));
        assert!(repo.git(["diff", "--cached"]).is_empty());
    }

    #[test]
    fn patch_input_is_bounded_and_rejects_empty_or_nul_data() {
        let Some(repo) = TestRepo::new() else { return };
        let review = repo.review();
        assert!(matches!(
            review.apply_patch(b" \n\t", PatchMutation::Stage),
            Err(GitReviewError::EmptyPatch)
        ));
        assert!(matches!(
            review.apply_patch(b"diff --git a/a b/a\0", PatchMutation::Stage),
            Err(GitReviewError::InvalidPatch { .. })
        ));
        assert!(matches!(
            review.apply_patch(&vec![b'x'; MAX_PATCH_BYTES + 1], PatchMutation::Stage),
            Err(GitReviewError::PatchTooLarge { .. })
        ));
    }

    #[test]
    fn unstages_on_an_unborn_branch_without_deleting_the_file() {
        let Some(repo) = TestRepo::new() else { return };
        let review = repo.review();
        repo.write("first.txt", "first\n");
        review.stage_paths(&[PathBuf::from("first.txt")]).unwrap();
        review.unstage_paths(&[PathBuf::from("first.txt")]).unwrap();

        assert_eq!(
            fs::read_to_string(repo.path.join("first.txt")).unwrap(),
            "first\n"
        );
        let status = review.status().unwrap();
        assert!(status.staged.is_empty());
        assert_eq!(status.untracked[0].path, Path::new("first.txt"));
    }

    #[test]
    fn commit_validates_message_and_returns_identity() {
        let Some(repo) = TestRepo::new() else { return };
        let review = repo.review();
        repo.write("first.txt", "hello\n");
        review.stage_paths(&[PathBuf::from("first.txt")]).unwrap();

        assert!(matches!(
            review.commit(" \n\t"),
            Err(GitReviewError::EmptyCommitMessage)
        ));
        let commit = review.commit("Review cockpit foundation\n").unwrap();
        assert_eq!(commit.oid.len(), 40);
        assert_eq!(commit.summary, "Review cockpit foundation");
        assert!(review.status().unwrap().staged.is_empty());
    }

    #[test]
    fn conflict_entries_are_kept_out_of_other_groups() {
        let Some(repo) = TestRepo::new() else { return };
        repo.write("conflict.txt", "base\n");
        repo.commit_all("base");
        repo.git(["checkout", "--quiet", "-b", "side"]);
        repo.write("conflict.txt", "side\n");
        repo.commit_all("side");
        repo.git(["checkout", "--quiet", "main"]);
        repo.write("conflict.txt", "main\n");
        repo.commit_all("main");
        repo.git_expect_failure(["merge", "--no-edit", "side"]);

        let status = repo.review().status().unwrap();
        assert!(status.staged.is_empty());
        assert!(status.unstaged.is_empty());
        assert_eq!(status.conflicted.len(), 1);
        assert_eq!(status.conflicted[0].path, Path::new("conflict.txt"));
        assert_eq!(status.conflicted[0].kind, ChangeKind::Unmerged);
    }

    #[test]
    fn rejects_paths_that_can_escape_or_broaden_the_mutation() {
        let Some(repo) = TestRepo::new() else { return };
        let review = repo.review();

        for path in ["../outside", "/absolute", ".", ".git/config", "a/../b"] {
            assert!(matches!(
                review.stage_paths(&[PathBuf::from(path)]),
                Err(GitReviewError::InvalidPath { .. })
            ));
        }
        assert!(matches!(
            review.stage_paths(&[]),
            Err(GitReviewError::EmptySelection)
        ));
    }

    #[test]
    fn parses_branch_tracking_and_rename_records() {
        let bytes = b"# branch.oid abcdef\0# branch.head feature\0# branch.upstream origin/feature\0# branch.ab +3 -2\x002 R. N... 100644 100644 100644 aaaaaaa bbbbbbb R100 new name.txt\0old name.txt\0";
        let status = parse_status(Path::new("/repo"), bytes).unwrap();

        assert_eq!(status.branch.name.as_deref(), Some("feature"));
        assert_eq!(status.branch.upstream.as_deref(), Some("origin/feature"));
        assert_eq!(status.branch.ahead, 3);
        assert_eq!(status.branch.behind, 2);
        assert_eq!(status.staged.len(), 1);
        assert_eq!(status.staged[0].kind, ChangeKind::Renamed);
        assert_eq!(status.staged[0].path, Path::new("new name.txt"));
        assert_eq!(
            status.staged[0].original_path.as_deref(),
            Some(Path::new("old name.txt"))
        );
    }

    fn numbered_lines(count: usize) -> String {
        (1..=count)
            .map(|line| format!("line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    fn hunk_containing<'a>(hunks: &'a [DiffHunk], needle: &str) -> &'a DiffHunk {
        hunks
            .iter()
            .find(|hunk| String::from_utf8_lossy(&hunk.patch).contains(needle))
            .unwrap_or_else(|| panic!("missing hunk containing {needle:?}"))
    }
}
