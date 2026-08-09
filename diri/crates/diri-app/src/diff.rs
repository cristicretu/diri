//! Worktree diff loading and unified-patch parsing.
//!
//! The inspector crosses this module through one interface: a session cwd in,
//! a flat render snapshot out. Git process details, untracked files, hunk line
//! accounting, and output limits stay local to the implementation.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use diri_proto::{SessionDiffBase, SessionReadDiffResult};

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

const MAX_DIFF_BYTES: usize = 16 * 1024 * 1024;
const MAX_UNTRACKED_FILES: usize = 200;

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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffSnapshot {
    pub repo_root: PathBuf,
    pub base_ref: Option<String>,
    pub rows: Vec<DiffRow>,
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
    let root_output = git(cwd, ["rev-parse", "--show-toplevel"])?;
    if !root_output.status.success() {
        return Err(DiffError::NotRepository);
    }
    let repo_root = PathBuf::from(String::from_utf8_lossy(&root_output.stdout).trim());
    if repo_root.as_os_str().is_empty() {
        return Err(DiffError::NotRepository);
    }

    let has_head = git(&repo_root, ["rev-parse", "--verify", "HEAD"])
        .is_ok_and(|output| output.status.success());
    let mut patch = Vec::new();
    let mut base_ref = None;
    if has_head {
        let resolution = resolve_comparison(&repo_root, comparison)?;
        base_ref = Some(resolution.label);
        append_output(
            &mut patch,
            git(
                &repo_root,
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
    } else {
        append_output(
            &mut patch,
            git(
                &repo_root,
                [
                    "diff",
                    "--no-ext-diff",
                    "--no-color",
                    "--unified=3",
                    "--cached",
                    "--",
                ],
            )?,
        )?;
        append_output(
            &mut patch,
            git(
                &repo_root,
                ["diff", "--no-ext-diff", "--no-color", "--unified=3", "--"],
            )?,
        )?;
    }

    let untracked = git(
        &repo_root,
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
        let output = git_command(&repo_root)
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
        append_bytes(&mut patch, &output.stdout);
    }

    let truncated = patch.len() > MAX_DIFF_BYTES;
    patch.truncate(MAX_DIFF_BYTES);
    let mut snapshot = parse_unified_diff(&String::from_utf8_lossy(&patch));
    snapshot.repo_root = repo_root;
    snapshot.base_ref = base_ref;
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

pub fn parse_unified_diff(patch: &str) -> DiffSnapshot {
    let mut snapshot = DiffSnapshot::default();
    let mut old_line = None;
    let mut new_line = None;

    for line in patch.lines() {
        let row = if let Some(header) = line.strip_prefix("diff --git ") {
            snapshot.files += 1;
            old_line = None;
            new_line = None;
            DiffRow {
                kind: DiffRowKind::File,
                old_line: None,
                new_line: None,
                text: diff_path(header),
            }
        } else if line.starts_with("--- ") || line.starts_with("+++ ") {
            continue;
        } else if line.starts_with("@@") {
            let (old, new) = parse_hunk_start(line);
            old_line = old;
            new_line = new;
            DiffRow {
                kind: DiffRowKind::Hunk,
                old_line: None,
                new_line: None,
                text: line.to_owned(),
            }
        } else if let Some(text) = line.strip_prefix('+') {
            let current = new_line;
            new_line = new_line.map(|line| line.saturating_add(1));
            snapshot.additions += 1;
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
                text: line.to_owned(),
            }
        };
        snapshot.max_text_columns = snapshot
            .max_text_columns
            .max(row.text.chars().count().min(500));
        snapshot.rows.push(row);
    }
    snapshot
}

/// Converts the daemon's bounded wire payload into the same render snapshot
/// used by local Git. Keeping this conversion here guarantees identical row,
/// hunk, and summary behavior for local and remote sessions.
pub fn snapshot_from_read_diff(result: SessionReadDiffResult) -> DiffSnapshot {
    let mut snapshot = parse_unified_diff(&String::from_utf8_lossy(&result.patch));
    snapshot.repo_root = PathBuf::from(result.repo_root);
    snapshot.base_ref = result.base_ref;
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
    git_command(cwd)
        .args(args)
        .output()
        .map_err(|error| DiffError::Git(error.to_string()))
}

fn git_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("LANGUAGE", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    command
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
    fn internal_git_commands_pin_the_machine_readable_locale() {
        let command = git_command(Path::new("/tmp"));
        let environment = command
            .get_envs()
            .filter_map(|(key, value)| Some((key.to_str()?, value?.to_str()?)))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(environment.get("LC_ALL"), Some(&"C"));
        assert_eq!(environment.get("LANG"), Some(&"C"));
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
