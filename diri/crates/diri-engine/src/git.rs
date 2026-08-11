//! Git facts a session needs: which branch it is on, and whether it is in a
//! linked worktree.
//!
//! Branch reading parses `.git/HEAD` directly rather than shelling out. It is
//! polled for a live label in the sidebar, and a `git` subprocess per session
//! per second is a cost worth not paying. Worktree operations do shell out —
//! they are rare, and reimplementing `git worktree` would be reckless.
//!
//! Ported from the Swift `DirijorGit`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use diri_proto::HostEntry;

use crate::remote::manager::RemoteManager;

/// One entry from `git worktree list --porcelain`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_detached: bool,
    pub is_prunable: bool,
}

/// The current branch for a working directory: a branch name, a short SHA when
/// HEAD is detached, or `None` outside a repository.
pub fn branch(cwd: &Path) -> Option<String> {
    let git_dir = git_dir(cwd)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let trimmed = head.trim();

    if let Some(reference) = trimmed.strip_prefix("ref: ") {
        return match reference.split_once("refs/heads/") {
            Some((_, name)) => Some(name.to_string()),
            None => reference.rsplit('/').next().map(str::to_string),
        };
    }
    // Detached HEAD: a raw object id.
    Some(trimmed.chars().take(8).collect())
}

/// True when `cwd` is inside a *linked* worktree rather than the main checkout.
///
/// The signal is what `.git` is: a directory in the main checkout, a file
/// carrying `gitdir:` indirection in a linked one. This is what distinguishes
/// an agent's own worktree from the primary tree.
pub fn is_linked_worktree(cwd: &Path) -> bool {
    let mut dir = cwd.to_path_buf();
    loop {
        let dot_git = dir.join(".git");
        if let Ok(metadata) = std::fs::metadata(&dot_git) {
            return !metadata.is_dir();
        }
        if !dir.pop() {
            return false;
        }
    }
}

/// Resolves the directory holding `HEAD`, following worktree indirection.
fn git_dir(cwd: &Path) -> Option<PathBuf> {
    let mut dir = cwd.to_path_buf();
    loop {
        let dot_git = dir.join(".git");
        if let Ok(metadata) = std::fs::metadata(&dot_git) {
            if metadata.is_dir() {
                return Some(dot_git);
            }
            // `.git` is a file: "gitdir: <path>".
            let contents = std::fs::read_to_string(&dot_git).ok()?;
            let line = contents.lines().next()?;
            let target = line.strip_prefix("gitdir: ")?.trim();
            let resolved = if Path::new(target).is_absolute() {
                PathBuf::from(target)
            } else {
                dir.join(target)
            };
            return Some(resolved);
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub fn is_repository(path: &Path) -> bool {
    git_dir(path).is_some()
}

/// The repository root for `path`.
pub fn repository_root(path: &Path) -> Option<String> {
    let output = run(&["rev-parse", "--show-toplevel"], path).ok()?;
    let trimmed = output.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub fn list_worktrees(repo: &Path) -> std::io::Result<Vec<WorktreeInfo>> {
    let porcelain = run(&["worktree", "list", "--porcelain"], repo)?;
    Ok(parse_porcelain(&porcelain))
}

/// Parses `git worktree list --porcelain`.
///
/// Blocks are separated by blank lines, but a `worktree` line also starts a new
/// block — trailing output without a final blank line still has to flush.
pub fn parse_porcelain(porcelain: &str) -> Vec<WorktreeInfo> {
    /// One block being accumulated. Replaced wholesale on flush, so flags
    /// cannot leak from one worktree into the next.
    #[derive(Default)]
    struct Block {
        path: Option<String>,
        branch: Option<String>,
        is_bare: bool,
        is_detached: bool,
        is_prunable: bool,
    }

    fn flush(block: &mut Block, results: &mut Vec<WorktreeInfo>) {
        let Some(path) = block.path.take() else {
            return;
        };
        let finished = std::mem::take(block);
        results.push(WorktreeInfo {
            path,
            branch: finished.branch,
            is_bare: finished.is_bare,
            is_detached: finished.is_detached,
            is_prunable: finished.is_prunable,
        });
    }

    let mut results = Vec::new();
    let mut block = Block::default();

    for line in porcelain.split('\n') {
        if line.is_empty() {
            flush(&mut block, &mut results);
        } else if let Some(rest) = line.strip_prefix("worktree ") {
            flush(&mut block, &mut results);
            block.path = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("branch ") {
            block.branch = Some(rest.strip_prefix("refs/heads/").unwrap_or(rest).to_string());
        } else if line == "bare" {
            block.is_bare = true;
        } else if line == "detached" {
            block.is_detached = true;
        } else if line == "prunable" || line.starts_with("prunable ") {
            block.is_prunable = true;
        }
        // `HEAD <sha>` and other keys carry nothing this type models.
    }
    flush(&mut block, &mut results);
    results
}

/// Runs a git command, bounded and without inheriting a terminal.
///
/// The hardening is the same as the test helpers': a git that can prompt is a
/// git that can hang forever, and ambient config from the host has no business
/// affecting what the daemon sees.
fn run(args: &[&str], cwd: &Path) -> std::io::Result<String> {
    let output = Command::new("/usr/bin/git")
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", cwd)
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("LANGUAGE", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Adjective/noun pairs for generated branch names. Memorable beats unique:
/// these names end up in branch lists and sidebar rows that people read.
const ADJECTIVES: &[&str] = &[
    "brisk", "calm", "deft", "eager", "fleet", "gentle", "hardy", "keen", "lively", "merry",
    "nimble", "plucky", "quiet", "rapid", "steady", "swift", "tidy", "vivid", "witty", "zesty",
];

const NOUNS: &[&str] = &[
    "otter", "heron", "maple", "cedar", "falcon", "willow", "badger", "sparrow", "cypress",
    "marten", "juniper", "raven", "birch", "lynx", "hazel", "osprey", "aspen", "finch", "poplar",
    "wren",
];

/// `dirijor/<adjective>-<noun>-<4hex>`.
pub fn generated_branch_name() -> String {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).expect("the OS random source");
    let adjective = ADJECTIVES[bytes[0] as usize % ADJECTIVES.len()];
    let noun = NOUNS[bytes[1] as usize % NOUNS.len()];
    let hex = u16::from_be_bytes([bytes[2], bytes[3]]);
    format!("dirijor/{adjective}-{noun}-{hex:04x}")
}

/// Lowercases a branch name and collapses every run of characters outside
/// `[a-z0-9]` into a single dash, so it can be a directory name.
pub fn branch_to_path_slug(branch: &str) -> String {
    let mut slug = String::with_capacity(branch.len());
    let mut last_was_dash = false;
    for character in branch.to_lowercase().chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            slug.push(character);
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

/// Creates a worktree beside the repository, on a new branch.
///
/// The path is `<parent>/<repo-name>-<branch-slug>`, which keeps sibling
/// worktrees visibly related to their repository in a file browser and in
/// shell completion.
pub fn create_worktree(
    repo: &Path,
    branch: Option<&str>,
    base: Option<&str>,
) -> std::io::Result<WorktreeInfo> {
    let branch_name = branch
        .map(str::to_string)
        .unwrap_or_else(generated_branch_name);
    let slug = branch_to_path_slug(&branch_name);
    let parent = repo.parent().unwrap_or(Path::new("."));
    let repo_name = repo
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".into());
    let worktree_path = parent.join(format!("{repo_name}-{slug}"));
    let worktree_str = worktree_path.to_string_lossy().to_string();

    let mut args = vec!["worktree", "add", "-b", &branch_name, &worktree_str];
    if let Some(base) = base {
        args.push(base);
    }
    run(&args, repo)?;

    // Report the path git will report. It records the *resolved* path, and on
    // macOS the common roots are symlinks — `/tmp` is `/private/tmp`, and a
    // repo can easily sit under one too. Handing back the unresolved path means
    // a caller that stores it cannot later match it against `list_worktrees` or
    // remove it by path.
    let resolved = std::fs::canonicalize(&worktree_path)
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or(worktree_str);

    Ok(WorktreeInfo {
        path: resolved,
        branch: Some(branch_name),
        is_bare: false,
        is_detached: false,
        is_prunable: false,
    })
}

pub fn remove_worktree(repo: &Path, worktree: &str, force: bool) -> std::io::Result<()> {
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(worktree);
    run(&args, repo)?;
    Ok(())
}

/// The response stays below the NDJSON ceiling after base64 encoding.
const MAX_PATCH_BYTES: usize = 2 * 1024 * 1024;
const MAX_DIFF_RESPONSE_BYTES: usize = MAX_PATCH_BYTES + 16 * 1024;
const REMOTE_GIT_TIMEOUT: Duration = Duration::from_secs(30);
const DIFF_RESPONSE_MARKER: &[u8] = b"DIRI_GIT_V1\0";

/// Dynamic values arrive on stdin, leaving the SSH command itself fixed and
/// safe to pass through any account login shell. Session paths containing a
/// newline are rejected before this script is reached.
const WORKING_DIFF_SCRIPT: &str = r#"set -e
export LC_ALL=C LANG=C LANGUAGE=C
IFS= read -r cwd
IFS= read -r comparison
case "$cwd" in
  "~") cwd=$HOME ;;
  "~/"*) cwd=$HOME/${cwd#\~/} ;;
esac
cd "$cwd"
command -v git >/dev/null 2>&1 || { printf '%s\n' 'git is not installed on this host' >&2; exit 127; }
export GIT_TERMINAL_PROMPT=0 GIT_OPTIONAL_LOCKS=0
root=$(git rev-parse --show-toplevel)
cd "$root"
git status --porcelain=v1 -uno >/dev/null
base_ref=HEAD
base_commit=
if git rev-parse --verify HEAD >/dev/null 2>&1; then
  if [ "$comparison" = head ]; then
    base_commit=HEAD
  else
    origin_head=$(git symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null || true)
    base_ref=
    for candidate in "$origin_head" origin/main main origin/master master; do
      [ -n "$candidate" ] || continue
      if git rev-parse --verify --quiet "${candidate}^{commit}" >/dev/null; then
        base_ref="$candidate"
        break
      fi
    done
    if [ -n "$base_ref" ]; then
      base_commit=$(git merge-base "$base_ref" HEAD 2>/dev/null || true)
    fi
    if [ -z "$base_commit" ]; then
      base_ref=HEAD
      base_commit=HEAD
    fi
  fi
fi
printf 'DIRI_GIT_V1\0%s\0%s\0' "$root" "$base_ref"
(
  if [ -n "$base_commit" ]; then
    git diff --no-ext-diff --no-color --unified=3 "$base_commit" --
  else
    git diff --no-ext-diff --no-color --unified=3 --cached --
    git diff --no-ext-diff --no-color --unified=3 --
  fi
  git ls-files --others --exclude-standard -z | \
    xargs -0 -n 1 sh -c '
      [ "$#" -eq 0 ] && exit 0
      git diff --no-index --no-color --unified=3 -- /dev/null "$1"
      code=$?
      [ "$code" -eq 0 ] || [ "$code" -eq 1 ]
    ' sh
) | head -c 2097153
"#;

/// The working tree's diff for a session cwd, against the repository's
/// primary branch (merge-base) or plain HEAD.
///
/// One shell round trip, ported verbatim from `WorktreeDiffLoader`: validate
/// the cwd, emit `root\0base_ref\0`, then stream tracked + staged + untracked
/// changes through a hard byte cap. `xargs -0` keeps spaces and newlines in
/// untracked filenames intact.
pub fn working_diff(
    cwd: &Path,
    base: Option<&diri_proto::SessionDiffBase>,
) -> std::io::Result<diri_proto::SessionReadDiffResult> {
    let input = working_diff_input(&cwd.to_string_lossy(), base)?;
    let mut child = Command::new("/bin/sh")
        .args(["-c", WORKING_DIFF_SCRIPT])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    use std::io::Write as _;
    child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("git diff stdin is unavailable"))?
        .write_all(&input)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(diff_failure(&output.stderr));
    }
    parse_working_diff(output.stdout)
}

pub fn working_diff_remote(
    manager: &RemoteManager,
    host: &HostEntry,
    cwd: &str,
    base: Option<&diri_proto::SessionDiffBase>,
) -> std::io::Result<diri_proto::SessionReadDiffResult> {
    let input = working_diff_input(cwd, base)?;
    let output = manager.run_fixed_script(
        host,
        WORKING_DIFF_SCRIPT,
        input,
        REMOTE_GIT_TIMEOUT,
        MAX_DIFF_RESPONSE_BYTES,
    )?;
    if output.stdout_truncated {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "remote Git response exceeded its hard limit",
        ));
    }
    if !output.status.success() {
        return Err(diff_failure(&output.stderr));
    }
    parse_working_diff(output.stdout)
}

fn working_diff_input(
    cwd: &str,
    base: Option<&diri_proto::SessionDiffBase>,
) -> std::io::Result<Vec<u8>> {
    if cwd.is_empty() || cwd.as_bytes().contains(&0) || cwd.contains(['\r', '\n']) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session cwd is not representable by the Git inspector",
        ));
    }
    let comparison = match base {
        Some(diri_proto::SessionDiffBase::Head) => "head",
        _ => "defaultBranch",
    };
    Ok(format!("{cwd}\n{comparison}\n").into_bytes())
}

fn parse_working_diff(stdout: Vec<u8>) -> std::io::Result<diri_proto::SessionReadDiffResult> {
    let marker = stdout
        .windows(DIFF_RESPONSE_MARKER.len())
        .position(|window| window == DIFF_RESPONSE_MARKER)
        .ok_or_else(|| std::io::Error::other("git diff response marker is missing"))?;
    let payload = &stdout[marker + DIFF_RESPONSE_MARKER.len()..];
    let root_end = payload
        .iter()
        .position(|&byte| byte == 0)
        .ok_or_else(|| std::io::Error::other("git diff returned an invalid response"))?;
    let base_end = payload[root_end + 1..]
        .iter()
        .position(|&byte| byte == 0)
        .map(|offset| root_end + 1 + offset)
        .ok_or_else(|| std::io::Error::other("git diff returned an invalid response"))?;
    let repo_root = String::from_utf8_lossy(&payload[..root_end]).into_owned();
    let base_ref = String::from_utf8_lossy(&payload[root_end + 1..base_end]).into_owned();
    let patch = &payload[base_end + 1..];
    let truncated = patch.len() > MAX_PATCH_BYTES;
    Ok(diri_proto::SessionReadDiffResult {
        patch: patch[..patch.len().min(MAX_PATCH_BYTES)].to_vec(),
        repo_root,
        truncated,
        base_ref: Some(base_ref),
    })
}

fn diff_failure(stderr: &[u8]) -> std::io::Error {
    let detail = String::from_utf8_lossy(stderr).trim().to_string();
    std::io::Error::other(if detail.is_empty() {
        "session cwd is not inside a Git repository".to_string()
    } else {
        detail
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_diff_parser_ignores_login_shell_noise_before_its_marker() {
        let mut output = b"welcome from shell rc\nDIRI_GIT_V1\0/srv/app\0HEAD\0".to_vec();
        output.extend_from_slice(b"diff --git a/a b/a\n");
        let result = parse_working_diff(output).expect("parse");
        assert_eq!(result.repo_root, "/srv/app");
        assert_eq!(result.base_ref.as_deref(), Some("HEAD"));
        assert!(result.patch.starts_with(b"diff --git"));
    }

    #[test]
    fn a_generated_branch_name_is_readable_and_namespaced() {
        let name = generated_branch_name();
        assert!(name.starts_with("dirijor/"), "got {name}");
        let tail = &name["dirijor/".len()..];
        let parts: Vec<&str> = tail.split('-').collect();
        assert_eq!(parts.len(), 3, "adjective-noun-hex: {name}");
        assert_eq!(parts[2].len(), 4, "four hex digits: {name}");
        assert!(ADJECTIVES.contains(&parts[0]));
        assert!(NOUNS.contains(&parts[1]));
    }

    #[test]
    fn generated_names_differ() {
        // Not a uniqueness guarantee, just a check that randomness is wired up.
        let names: std::collections::HashSet<String> =
            (0..16).map(|_| generated_branch_name()).collect();
        assert!(names.len() > 1, "every generated name was identical");
    }

    #[test]
    fn a_branch_becomes_a_usable_directory_slug() {
        assert_eq!(
            branch_to_path_slug("dirijor/swift-wren-1a2b"),
            "dirijor-swift-wren-1a2b"
        );
        assert_eq!(
            branch_to_path_slug("feature/JIRA-123_fix"),
            "feature-jira-123-fix"
        );
        assert_eq!(
            branch_to_path_slug("--weird//name--"),
            "weird-name",
            "leading and trailing dashes are trimmed and runs collapse"
        );
    }

    /// Exercises create/list/remove against real git in a temp repo.
    #[test]
    fn worktrees_can_be_created_listed_and_removed() {
        let temp = tempfile::tempdir().expect("temp");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");

        // A minimal repo with one commit, identity passed per-command so no
        // global config is needed.
        let git = |args: &[&str]| {
            let status = std::process::Command::new("/usr/bin/git")
                .args(args)
                .current_dir(&repo)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", temp.path())
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(repo.join("f.txt"), b"x").expect("write");
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "init",
        ]);

        let created = create_worktree(&repo, None, None).expect("create");
        assert!(
            created.path.contains("repo-dirijor-"),
            "the worktree sits beside its repo: {}",
            created.path
        );
        assert_eq!(
            created.path,
            std::fs::canonicalize(&created.path)
                .expect("exists")
                .to_string_lossy(),
            "the reported path is the resolved one git also reports"
        );
        assert!(Path::new(&created.path).is_dir());
        assert!(
            is_linked_worktree(Path::new(&created.path)),
            "a created worktree is a linked one"
        );
        assert_eq!(
            branch(Path::new(&created.path)).as_deref(),
            created.branch.as_deref(),
            "it is on the branch it reported"
        );

        let listed = list_worktrees(&repo).expect("list");
        assert_eq!(listed.len(), 2, "the main checkout plus the new worktree");
        assert!(listed.iter().any(|entry| entry.path == created.path));

        remove_worktree(&repo, &created.path, true).expect("remove");
        assert_eq!(list_worktrees(&repo).expect("list").len(), 1);
    }

    #[test]
    fn porcelain_parsing_handles_a_main_checkout_and_a_linked_worktree() {
        let porcelain = "worktree /repo\nHEAD abc123\nbranch refs/heads/main\n\
             \n\
             worktree /repo/.claude/worktrees/bright-fox\nHEAD def456\n\
             branch refs/heads/worktree-bright-fox\n\n";
        let worktrees = parse_porcelain(porcelain);

        assert_eq!(worktrees.len(), 2);
        assert_eq!(worktrees[0].path, "/repo");
        assert_eq!(worktrees[0].branch.as_deref(), Some("main"));
        assert_eq!(
            worktrees[1].branch.as_deref(),
            Some("worktree-bright-fox"),
            "the refs/heads/ prefix is stripped"
        );
    }

    #[test]
    fn a_final_block_without_a_trailing_blank_line_is_not_dropped() {
        let worktrees = parse_porcelain("worktree /repo\nbranch refs/heads/main");
        assert_eq!(worktrees.len(), 1, "the last block must still flush");
        assert_eq!(worktrees[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn bare_detached_and_prunable_are_recognized() {
        let porcelain = "worktree /repo\nbare\n\n\
             worktree /repo/wt\nHEAD abc\ndetached\nprunable gitdir file points to non-existent\n";
        let worktrees = parse_porcelain(porcelain);

        assert!(worktrees[0].is_bare);
        assert!(worktrees[1].is_detached);
        assert!(
            worktrees[1].is_prunable,
            "prunable carries a reason after it"
        );
        assert!(
            !worktrees[0].is_detached,
            "flags do not leak between blocks"
        );
    }

    #[test]
    fn head_parsing_reads_a_branch_a_detached_sha_and_nothing() {
        let temp = tempfile::tempdir().expect("temp");
        let repo = temp.path().join("repo");
        let git = repo.join(".git");
        std::fs::create_dir_all(&git).expect("mkdir");

        std::fs::write(git.join("HEAD"), "ref: refs/heads/feature/login\n").expect("write");
        assert_eq!(branch(&repo).as_deref(), Some("feature/login"));

        std::fs::write(git.join("HEAD"), "9f8e7d6c5b4a3210\n").expect("write");
        assert_eq!(
            branch(&repo).as_deref(),
            Some("9f8e7d6c"),
            "a detached HEAD shows a short sha"
        );

        assert_eq!(branch(temp.path()).as_deref(), None, "not a repository");
    }

    #[test]
    fn a_branch_is_found_from_a_subdirectory() {
        let temp = tempfile::tempdir().expect("temp");
        let repo = temp.path().join("repo");
        let nested = repo.join("src/deep/inner");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::create_dir_all(repo.join(".git")).expect("mkdir");
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").expect("write");

        assert_eq!(branch(&nested).as_deref(), Some("main"));
    }

    #[test]
    fn a_linked_worktree_is_told_apart_from_the_main_checkout() {
        let temp = tempfile::tempdir().expect("temp");
        let main = temp.path().join("repo");
        std::fs::create_dir_all(main.join(".git")).expect("mkdir");
        assert!(!is_linked_worktree(&main), ".git is a directory here");

        let linked = temp.path().join("wt");
        std::fs::create_dir_all(&linked).expect("mkdir");
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}/.git/worktrees/wt\n", main.display()),
        )
        .expect("write");
        assert!(is_linked_worktree(&linked), ".git is a file here");
    }

    #[test]
    fn a_worktrees_head_is_followed_through_the_indirection() {
        let temp = tempfile::tempdir().expect("temp");
        let main = temp.path().join("repo");
        let worktree_git = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(&worktree_git).expect("mkdir");
        std::fs::write(worktree_git.join("HEAD"), "ref: refs/heads/side-branch\n").expect("write");

        let linked = temp.path().join("wt");
        std::fs::create_dir_all(&linked).expect("mkdir");
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", worktree_git.display()),
        )
        .expect("write");

        assert_eq!(
            branch(&linked).as_deref(),
            Some("side-branch"),
            "a linked worktree has its own HEAD"
        );
    }

    #[test]
    fn remote_working_diff_pins_machine_readable_locale() {
        assert!(WORKING_DIFF_SCRIPT.contains("export LC_ALL=C LANG=C LANGUAGE=C"));
        assert!(WORKING_DIFF_SCRIPT.contains("export GIT_TERMINAL_PROMPT=0"));
    }
}
