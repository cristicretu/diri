//! On-demand local worktree inventory and conservative, confirmed cleanup.
//! Git and gh are read through bounded subprocesses; no fetch or branch deletion.
use std::collections::{HashMap, HashSet};
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
    let mut paths: Vec<_> = trees.iter().map(|t| t.path.clone()).collect();
    paths.sort();
    for tree in &mut trees {
        let index = paths.binary_search(&tree.path).expect("listed path");
        if !tree.protected
            && paths
                .get(index + 1)
                .is_some_and(|next| next.starts_with(&tree.path))
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
struct RepoFacts<'a> {
    base: Option<String>,
    pulls: Option<Vec<Value>>,
    merged_heads: HashSet<String>,
    sessions: &'a HashMap<PathBuf, &'a SessionRecord>,
}
fn session_index(records: &[SessionRecord]) -> HashMap<PathBuf, &SessionRecord> {
    let mut index = HashMap::new();
    for record in records.iter().filter(|r| r.host.is_none()) {
        for path in [Some(record.cwd.as_str()), record.worktree_path.as_deref()]
            .into_iter()
            .flatten()
        {
            let path = Path::new(path)
                .canonicalize()
                .unwrap_or_else(|_| path.into());
            for ancestor in path.ancestors() {
                let entry = index.entry(ancestor.to_path_buf()).or_insert(record);
                if !matches!(record.status, SessionStatus::Exited(_)) {
                    *entry = record;
                }
            }
        }
    }
    index
}
impl<'a> RepoFacts<'a> {
    fn new(
        root: &Path,
        pulls: Option<Vec<Value>>,
        sessions: &'a HashMap<PathBuf, &'a SessionRecord>,
    ) -> Self {
        let base = default_ref(root);
        // One reachability walk for every local branch, instead of one Git
        // process and graph walk for each worktree. Detached trees stay protected.
        let merged_heads = base
            .as_deref()
            .and_then(|base| {
                git(
                    root,
                    &[
                        "for-each-ref",
                        "--merged",
                        base,
                        "--format=%(objectname)",
                        "refs/heads/",
                    ],
                )
            })
            .map(|s| s.lines().map(str::to_owned).collect())
            .unwrap_or_default();
        Self {
            base,
            pulls,
            merged_heads,
            sessions,
        }
    }
}
fn inspect(
    root: &Path,
    tree: &Tree,
    pulls: Option<&[Value]>,
    records: &[SessionRecord],
    disk: bool,
) -> WorktreeOverviewEntry {
    let index = session_index(records);
    let facts = RepoFacts::new(root, pulls.map(<[Value]>::to_vec), &index);
    inspect_cached(root, tree, &facts, disk)
}
fn inspect_cached(
    root: &Path,
    tree: &Tree,
    facts: &RepoFacts<'_>,
    disk: bool,
) -> WorktreeOverviewEntry {
    let path = &tree.path;
    let base_name = facts
        .base
        .as_deref()
        .map(|b| b.strip_prefix("refs/remotes/origin/").unwrap_or(b));
    let pulls = facts.pulls.as_deref();
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
    let merged = facts.merged_heads.contains(&tree.head);
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
    let record = facts.sessions.get(path).copied();
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
    let disk_bytes = (disk && protection.is_none())
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

/// Publish cheap discovery before any GitHub request or working-directory walk.
/// The sink supplies cancellation between every bounded subprocess operation.
pub(crate) fn scan(
    projects: &[Value],
    records: &[SessionRecord],
    measure_disk: bool,
    emit: &mut crate::worktree_scan::Emit<'_>,
) -> Result<(), String> {
    let index = session_index(records);
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
    let mut known_paths = HashSet::new();
    let mut repositories = Vec::new();
    for root in roots {
        if !emit(None, false) {
            return Err("Scan paused after leaving Worktrees. Refresh to continue.".into());
        }
        let root = root.canonicalize().unwrap_or(root);
        // Session subdirectories reuse discovered checkouts, but an inner
        // .git boundary may be a separately saved nested repository/submodule.
        let mut discovered = false;
        for ancestor in root.ancestors() {
            if known_paths.contains(ancestor) {
                discovered = true;
                break;
            }
            if ancestor.join(".git").exists() {
                break;
            }
        }
        if discovered {
            continue;
        }
        let Some(trees) = trees(&root) else {
            continue;
        };
        if trees.is_empty() || trees.iter().all(|t| known_paths.contains(&t.path)) {
            continue;
        }
        let root = trees[0].path.clone();
        for tree in &trees {
            known_paths.insert(tree.path.clone());
            let record = index.get(&tree.path).copied();
            let age_days = std::fs::metadata(&tree.path)
                .ok()
                .and_then(|m| m.created().ok())
                .and_then(|t| t.elapsed().ok())
                .map(|d| (d.as_secs() / 86400) as i64)
                .unwrap_or(-1);
            let entry = WorktreeOverviewEntry {
                path: tree.path.to_string_lossy().into_owned(),
                branch: tree.branch.clone(),
                project_root: root.to_string_lossy().into_owned(),
                session_id: record.map(|r| r.id.clone()),
                session_status: record.map(|r| r.status.clone()),
                dirty: true,
                merged: false,
                age_days,
                stale_suggestion: false,
                health: WorktreeHealth {
                    head: Some(tree.head.clone()),
                    pr_state: "Checking…".into(),
                    protection: Some("Checking…".into()),
                    ..Default::default()
                },
            };
            if !emit(Some(entry), false) {
                return Err("Scan paused. Refresh to continue.".into());
            }
        }
        repositories.push((root, trees));
    }
    for (root, trees) in repositories {
        if !emit(None, false) {
            return Err("Scan paused. Refresh to continue.".into());
        }
        let facts = RepoFacts::new(&root, prs(&root), &index);
        for tree in trees {
            if !emit(None, false) {
                return Err("Scan paused. Refresh to continue.".into());
            }
            let entry = inspect_cached(&root, &tree, &facts, measure_disk);
            if !emit(Some(entry), true) {
                return Err("Scan paused. Refresh to continue.".into());
            }
        }
    }
    Ok(())
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
    fn worktree_discovery_publishes_before_pr_or_status_checks_and_can_cancel() {
        let (_temp, root, tree) = fixture();
        let start = Instant::now();
        let mut found = Vec::new();
        let result = scan(
            &[json!({"root":root})],
            &[],
            false,
            &mut |entry, checked| {
                assert!(!checked, "discovery must precede expensive checks");
                if let Some(entry) = entry {
                    assert!(!entry.stale_suggestion);
                    assert_eq!(entry.health.pr_state, "Checking…");
                    found.push(entry.path);
                }
                found.len() < 2
            },
        );
        assert!(
            result.is_err(),
            "consumer cancellation stops before enrichment"
        );
        assert!(found.contains(&tree.path.to_string_lossy().into_owned()));
        eprintln!(
            "First inventory (2 worktrees) before any enrichment: {:?}",
            start.elapsed()
        );
        assert!(start.elapsed() < Duration::from_secs(1));
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
