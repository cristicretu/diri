//! On-demand local worktree inventory and conservative, confirmed cleanup.
//! Git and gh are read through bounded subprocesses; no fetch or branch deletion.
use std::collections::HashSet;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use diri_proto::{
    SessionRecord, SessionStatus, WorktreeCleanupParams, WorktreeHealth, WorktreeOverviewEntry,
};
use serde_json::Value;

// Drain while the child is running: large status/PR output must not fill its pipe.
fn output(program: &str, args: &[&str], cwd: &Path, timeout: Duration) -> Option<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let fd = stdout.as_raw_fd();
    // SAFETY: stdout owns this descriptor for the duration of the read loop.
    let configured = unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } >= 0;
    let start = Instant::now();
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    let result = (|| {
        if !configured {
            return None;
        }
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) => {
                    if let Some(status) = child.try_wait().ok()? {
                        return status.success().then_some(bytes);
                    }
                }
                Ok(n) => {
                    bytes.extend_from_slice(&buffer[..n]);
                    if bytes.len() > 2 * 1024 * 1024 {
                        return None;
                    }
                    if start.elapsed() < timeout {
                        continue;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return None,
            }
            if start.elapsed() >= timeout {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}
fn git(path: &Path, args: &[&str]) -> Option<String> {
    String::from_utf8(output("git", args, path, Duration::from_secs(3))?)
        .ok()
        .map(|s| s.trim().to_owned())
}

#[derive(Clone, Debug)]
struct Tree {
    path: PathBuf,
    branch: Option<String>,
    head: String,
    protected: bool,
    protection_reason: &'static str,
}
fn trees(root: &Path) -> Option<Vec<Tree>> {
    let bytes = output(
        "git",
        &["worktree", "list", "--porcelain", "-z"],
        root,
        Duration::from_secs(3),
    )?;
    let mut trees = parse_trees(&bytes)?;
    let paths: Vec<_> = trees.iter().map(|t| t.path.clone()).collect();
    for tree in &mut trees {
        if !tree.protected
            && paths
                .iter()
                .any(|p| p != &tree.path && p.starts_with(&tree.path))
        {
            tree.protected = true;
            tree.protection_reason = "Contains another worktree";
        }
    }
    Some(trees)
}
fn parse_trees(bytes: &[u8]) -> Option<Vec<Tree>> {
    let mut result = Vec::new();
    let mut current: Option<Tree> = None;
    for field in bytes.split(|b| *b == 0) {
        let field = std::str::from_utf8(field).ok()?;
        if let Some(path) = field.strip_prefix("worktree ") {
            if let Some(tree) = current.take() {
                result.push(tree);
            }
            current = Some(Tree {
                path: path.into(),
                branch: None,
                head: String::new(),
                protected: result.is_empty(),
                protection_reason: "Main checkout",
            });
        } else if let Some(tree) = current.as_mut() {
            if let Some(head) = field.strip_prefix("HEAD ") {
                tree.head = head.into();
            }
            if let Some(branch) = field.strip_prefix("branch refs/heads/") {
                tree.branch = Some(branch.into());
            }
            if field == "bare" || field.starts_with("locked") || field.starts_with("prunable") {
                tree.protected = true;
                tree.protection_reason = if field == "bare" {
                    "Bare repository"
                } else if field.starts_with("locked") {
                    "Locked worktree"
                } else {
                    "Missing checkout"
                };
            }
        }
    }
    if let Some(tree) = current {
        result.push(tree);
    }
    Some(result)
}
fn prs(root: &Path) -> Option<Vec<Value>> {
    let bytes = output(
        "gh",
        &[
            "pr",
            "list",
            "--state",
            "all",
            "--limit",
            "1000",
            "--json",
            "number,url,state,headRefName,headRefOid,baseRefName",
        ],
        root,
        Duration::from_secs(8),
    )?;
    serde_json::from_slice(&bytes).ok()
}
fn default_ref(root: &Path) -> Option<String> {
    git(root, &["symbolic-ref", "refs/remotes/origin/HEAD"]).or_else(|| {
        ["main", "master"]
            .into_iter()
            .find(|name| git(root, &["rev-parse", "--verify", name]).is_some())
            .map(str::to_owned)
    })
}
fn local_session<'a>(records: &'a [SessionRecord], path: &Path) -> Option<&'a SessionRecord> {
    records
        .iter()
        .filter(|r| r.host.is_none())
        .filter(|r| {
            [Some(r.cwd.as_str()), r.worktree_path.as_deref()]
                .into_iter()
                .flatten()
                .any(|p| {
                    let p = Path::new(p)
                        .canonicalize()
                        .unwrap_or_else(|_| PathBuf::from(p));
                    p.starts_with(path)
                })
        })
        .max_by_key(|r| !matches!(r.status, SessionStatus::Exited(_)))
}
fn inspect(
    root: &Path,
    tree: &Tree,
    pulls: Option<&[Value]>,
    records: &[SessionRecord],
    disk: bool,
) -> WorktreeOverviewEntry {
    let path = &tree.path;
    let base = default_ref(root);
    let base_name = base
        .as_deref()
        .map(|b| b.strip_prefix("refs/remotes/origin/").unwrap_or(b));
    let pr = pulls.and_then(|prs| {
        prs.iter()
            .filter(|pr| pr["headRefName"].as_str() == tree.branch.as_deref())
            .max_by_key(|pr| {
                (
                    pr["state"].as_str() == Some("OPEN"),
                    pr["number"].as_u64().unwrap_or(0),
                )
            })
    });
    let pr_state = match pr.and_then(|pr| pr["state"].as_str()) {
        Some("OPEN") => "Open",
        Some("MERGED") => "Merged",
        Some("CLOSED") => "Closed",
        _ if pulls.is_some() => "No recent PR",
        _ => "Unavailable",
    };
    let merged = base.as_ref().is_some_and(|base| {
        git(root, &["merge-base", "--is-ancestor", &tree.head, base]).is_some()
    });
    let pr_merged = pr.is_some_and(|pr| {
        pr_state == "Merged"
            && pr["headRefOid"].as_str() == Some(&tree.head)
            && pr["baseRefName"].as_str() == base_name
    });
    let status = git(
        path,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
    );
    let dirty = status.as_ref().is_none_or(|s| !s.is_empty());
    let record = local_session(records, path);
    let active = record.is_some_and(|r| !matches!(r.status, SessionStatus::Exited(_)));
    let protection = if tree.protected {
        Some(tree.protection_reason)
    } else if tree.branch.is_none() {
        Some("Detached HEAD")
    } else if tree.branch.as_deref() == base_name {
        Some("Default branch")
    } else if active {
        Some("Session in use or status unknown")
    } else if status.is_none() {
        Some("Git status unavailable")
    } else if dirty {
        Some("Local changes")
    } else if pr_state == "Open" {
        Some("Open pull request")
    } else if !merged && !pr_merged {
        Some("Merge not verified")
    } else {
        None
    };
    // Age means checkout age, not an assertion about last use or inactivity.
    let age_days = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.created().ok())
        .and_then(|t| t.elapsed().ok())
        .map(|d| (d.as_secs() / 86400) as i64)
        .unwrap_or(-1);
    let disk_bytes = disk
        .then(|| {
            output(
                "du",
                &["-sk", "-P", path.to_str()?],
                root,
                Duration::from_secs(2),
            )
        })
        .flatten()
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.split_whitespace().next()?.parse::<u64>().ok())
        .and_then(|kb| kb.checked_mul(1024));
    WorktreeOverviewEntry {
        path: path.to_string_lossy().into_owned(),
        branch: tree.branch.clone(),
        project_root: root.to_string_lossy().into_owned(),
        session_id: record.map(|r| r.id.clone()),
        session_status: record.map(|r| r.status.clone()),
        dirty,
        merged: merged || pr_merged,
        age_days,
        stale_suggestion: protection.is_none(),
        health: WorktreeHealth {
            head: Some(tree.head.clone()),
            disk_bytes,
            pr_number: pr.and_then(|p| p["number"].as_u64()),
            pr_url: pr.and_then(|p| p["url"].as_str()).map(str::to_owned),
            pr_state: pr_state.into(),
            protection: protection.map(str::to_owned),
        },
    }
}

pub(crate) fn overview(
    projects: &[Value],
    records: &[SessionRecord],
) -> Vec<WorktreeOverviewEntry> {
    let mut roots: Vec<_> = projects
        .iter()
        .filter(|p| p.get("host").is_none_or(Value::is_null))
        .filter_map(|p| p["root"].as_str())
        .map(PathBuf::from)
        .collect();
    roots.extend(
        records
            .iter()
            .filter(|r| r.host.is_none())
            .map(|r| PathBuf::from(&r.cwd)),
    );
    roots.sort();
    roots.dedup();
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for root in roots {
        let Some(trees) = trees(&root) else {
            continue;
        };
        if trees.iter().all(|t| seen.contains(&t.path)) {
            continue;
        }
        let root = trees[0].path.clone();
        let pulls = prs(&root);
        for tree in trees {
            if seen.insert(tree.path.clone()) {
                entries.push(inspect(&root, &tree, pulls.as_deref(), records, true));
            }
        }
    }
    entries
}
fn refused(reason: &str) -> io::Error {
    io::Error::other(reason)
}
fn cleanup_tree(p: &WorktreeCleanupParams) -> io::Result<Tree> {
    let root = Path::new(&p.repo_path);
    let path = Path::new(&p.worktree_path).canonicalize()?;
    if path != Path::new(&p.worktree_path) {
        return Err(refused("Worktree path changed; refresh before cleanup"));
    }
    trees(root)
        .and_then(|trees| trees.into_iter().find(|t| t.path == path))
        .ok_or_else(|| refused("Worktree is no longer registered in this repository"))
}
pub(crate) fn inspect_cleanup(p: &WorktreeCleanupParams) -> io::Result<WorktreeOverviewEntry> {
    let tree = cleanup_tree(p)?;
    let root = Path::new(&p.repo_path);
    Ok(inspect(root, &tree, prs(root).as_deref(), &[], false))
}
pub(crate) fn cleanup(
    p: &WorktreeCleanupParams,
    inspection: &WorktreeOverviewEntry,
    records: &[SessionRecord],
) -> io::Result<()> {
    let tree = cleanup_tree(p)?;
    if tree.branch != inspection.branch
        || p.expected_head.is_empty()
        || tree.head != p.expected_head
        || inspection.health.head.as_ref() != Some(&p.expected_head)
    {
        return Err(refused("Worktree HEAD changed; refresh before cleanup"));
    }
    if let Some(reason) = &inspection.health.protection {
        return Err(refused(reason));
    }
    if tree.protected
        || local_session(records, &tree.path)
            .is_some_and(|r| !matches!(r.status, SessionStatus::Exited(_)))
    {
        return Err(refused("Worktree is protected or has a session in use"));
    }
    // Non-force Git removal rechecks dirty/untracked files and locks at mutation.
    // Branches (including their commits) are deliberately retained.
    crate::git::remove_worktree(Path::new(&p.repo_path), &p.worktree_path, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fn fixture() -> (tempfile::TempDir, PathBuf, Tree) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["commit", "--allow-empty", "-m", "initial"]);
        let path = root.parent().unwrap().join("feature space");
        run(
            &root,
            &["worktree", "add", "-b", "feature", path.to_str().unwrap()],
        );
        let tree = trees(&root)
            .unwrap()
            .into_iter()
            .find(|t| t.path == path)
            .unwrap();
        (temp, root, tree)
    }
    fn params(root: &Path, tree: &Tree) -> WorktreeCleanupParams {
        WorktreeCleanupParams {
            repo_path: root.to_str().unwrap().into(),
            worktree_path: tree.path.to_str().unwrap().into(),
            expected_head: tree.head.clone(),
        }
    }
    #[test]
    fn worktree_cleanup_keeps_branch_and_removes_ignored_build_output() {
        let (_temp, root, tree) = fixture();
        run(&tree.path, &["config", "core.excludesFile", "/dev/null"]);
        std::fs::write(root.join(".git/info/exclude"), "build/\n").unwrap();
        std::fs::create_dir(tree.path.join("build")).unwrap();
        std::fs::write(tree.path.join("build/binary"), vec![0; 4096]).unwrap();
        let entry = inspect(&root, &tree, Some(&[]), &[], true);
        assert!(entry.stale_suggestion);
        assert!(entry.health.disk_bytes.unwrap() >= 4096);
        cleanup(&params(&root, &tree), &entry, &[]).unwrap();
        assert!(!tree.path.exists());
        assert!(git(&root, &["rev-parse", "--verify", "refs/heads/feature"]).is_some());
        assert!(root.exists());
    }
    #[test]
    fn worktree_cleanup_rechecks_dirty_files_locks_and_head() {
        let (_temp, root, tree) = fixture();
        let p = params(&root, &tree);
        let entry = inspect(&root, &tree, None, &[], false);
        std::fs::write(tree.path.join("untracked"), "keep me").unwrap();
        assert!(cleanup(&p, &entry, &[]).is_err());
        assert!(tree.path.join("untracked").exists());
        std::fs::remove_file(tree.path.join("untracked")).unwrap();
        run(&root, &["worktree", "lock", tree.path.to_str().unwrap()]);
        assert!(cleanup(&p, &entry, &[]).is_err());
        run(&root, &["worktree", "unlock", tree.path.to_str().unwrap()]);
        run(&tree.path, &["commit", "--allow-empty", "-m", "new work"]);
        assert!(
            cleanup(&p, &entry, &[])
                .unwrap_err()
                .to_string()
                .contains("HEAD changed")
        );
        assert!(tree.path.exists());
    }
    #[test]
    fn worktree_cleanup_protects_unknown_sessions_in_symlinked_subdirectories() {
        let (_temp, root, tree) = fixture();
        let entry = inspect(&root, &tree, None, &[], false);
        std::fs::create_dir(tree.path.join("src")).unwrap();
        let link = root.parent().unwrap().join("alias");
        std::os::unix::fs::symlink(&tree.path, &link).unwrap();
        let mut record =
            crate::control::new_record("live", "shell", link.join("src").to_str().unwrap());
        record.status = SessionStatus::Unknown;
        assert!(cleanup(&params(&root, &tree), &entry, &[record.clone()]).is_err());
        record.host = Some("remote".into());
        assert!(local_session(&[record], &tree.path).is_none());
    }
    #[test]
    fn worktree_inspection_distinguishes_pr_states_and_squash_merge_head() {
        let (_temp, root, mut tree) = fixture();
        run(&tree.path, &["commit", "--allow-empty", "-m", "feature"]);
        tree.head = git(&tree.path, &["rev-parse", "HEAD"]).unwrap();
        let pull = json!({"number": 1, "headRefName":"feature", "headRefOid":tree.head, "baseRefName":"main", "state":"MERGED"});
        let entry = inspect(&root, &tree, Some(std::slice::from_ref(&pull)), &[], false);
        assert!(entry.stale_suggestion, "matching squash-merged PR is safe");
        let mut wrong = pull.clone();
        wrong["headRefOid"] = json!("old head");
        assert!(!inspect(&root, &tree, Some(&[wrong]), &[], false).stale_suggestion);
        for state in ["OPEN", "CLOSED"] {
            let mut pr = pull.clone();
            pr["state"] = json!(state);
            assert!(!inspect(&root, &tree, Some(&[pr]), &[], false).stale_suggestion);
        }
        let unavailable = inspect(&root, &tree, None, &[], false);
        assert_eq!(unavailable.health.pr_state, "Unavailable");
        assert!(!unavailable.stale_suggestion);
    }
    #[test]
    fn worktree_main_and_nested_checkouts_are_protected() {
        let (_temp, root, tree) = fixture();
        let main = trees(&tree.path).unwrap().remove(0);
        assert!(!inspect(&tree.path, &main, Some(&[]), &[], false).stale_suggestion);
        let nested = tree.path.join("nested");
        run(
            &root,
            &["worktree", "add", "-b", "nested", nested.to_str().unwrap()],
        );
        assert!(
            trees(&root)
                .unwrap()
                .into_iter()
                .find(|t| t.path == tree.path)
                .unwrap()
                .protected
        );
    }
    #[test]
    fn worktree_nul_parser_preserves_newlines_and_lock_reasons() {
        let trees = parse_trees(b"worktree /main\0HEAD abc\0branch refs/heads/main\0\0worktree /feature\nname\0HEAD def\0branch refs/heads/feature\0locked reason\0\0").unwrap();
        assert_eq!(trees[1].path, Path::new("/feature\nname"));
        assert!(trees[1].protected);
    }
    #[test]
    fn worktree_process_output_is_drained_and_timed_out() {
        let temp = tempfile::tempdir().unwrap();
        let bytes = output(
            "sh",
            &[
                "-c",
                "i=0; while [ $i -lt 10000 ]; do printf '0123456789'; i=$((i+1)); done",
            ],
            temp.path(),
            Duration::from_secs(3),
        )
        .unwrap();
        assert_eq!(bytes.len(), 100000);
        let start = Instant::now();
        assert!(
            output(
                "sh",
                &["-c", "while :; do :; done"],
                temp.path(),
                Duration::from_millis(50)
            )
            .is_none()
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(output("sh", &["-c", "exit 1"], temp.path(), Duration::from_secs(1)).is_none());
    }
}
