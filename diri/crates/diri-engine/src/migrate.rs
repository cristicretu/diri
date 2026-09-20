//! The git + transcript legwork of `session.migrate`: one-click handoff of a
//! live Claude session between machines, preserving conversation context
//! (`claude --resume`) and code state.
//!
//! Code state moves losslessly in both directions: committed work travels by
//! push + hard-sync of the target checkout, while uncommitted work travels as
//! a binary diff applied to the target tree — so dirty state arrives dirty,
//! origin only ever sees real commits, and a session can bounce between local
//! and a host without leaving `WIP:` commits on the branch. A snapshot commit
//! of the dirty state is left behind on the source as a recovery net.
//!
//! The slow transfer runs while the source agent is still alive and writing,
//! so it moves a snapshot, not the final state: once the agent is stopped,
//! `reconcile` carries whatever changed since, before anything resumes.
//!
//! Ported from `SessionMigrator`. Both sides run through the same bounded
//! shell, so "source" and "target" can each be the local machine or a remote
//! host. The control server owns orchestration (preconditions, kill,
//! respawn); this module owns the mechanical steps and is deliberately
//! record-in, values-out.

use std::path::Path;
use std::time::Duration;

use diri_proto::HostEntry;

use crate::hosts::{SSH_OPTIONS, run_shell, shell_quote, shell_quote_path};
use crate::inject::{claude_project_slug, claude_transcript_path};

/// A migrate failure the user can act on (preconditions, dirty trees) versus
/// one they can't (plumbing).
#[derive(Debug)]
pub enum MigrateError {
    BadRequest(String),
    Internal(String),
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(message) | Self::Internal(message) => f.write_str(message),
        }
    }
}

/// Everything decided before the point of no return (killing the source
/// agent).
#[derive(Debug)]
pub struct Prepared {
    pub branch: String,
    pub source_repo_root: String,
    pub target_repo_root: String,
    pub wip_committed: bool,
    /// The target checkout is a linked worktree (the moved record keeps its
    /// worktree identity).
    pub target_is_worktree: bool,
    /// Uncommitted source changes traveled as a patch and arrived
    /// uncommitted.
    pub carried_dirty: bool,
    /// The source commit whose tree the target checkout now holds. The agent
    /// was still alive while it traveled, so `reconcile` measures everything
    /// the source gained afterwards against it.
    pub source_tip: String,
}

pub struct TranscriptShuttle {
    pub migrated: bool,
    /// For a LOCAL target: where the record's transcriptPath should point.
    pub local_target_path: Option<String>,
    pub warning: Option<String>,
}

/// Subject prefix that marks a source-side snapshot commit. `prepare` detects
/// it on the tip so a retried run still carries the same changes as dirty
/// state instead of pushing them as a commit.
const HANDOFF_SUBJECT_PREFIX: &str = "WIP: handoff to ";

pub fn wip_commit_message(target_name: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{HANDOFF_SUBJECT_PREFIX}{target_name} @{now}")
}

/// Runs a command and maps any failure to a clear precondition error that
/// includes the underlying stderr. Returns trimmed stdout.
fn require(
    host: Option<&HostEntry>,
    command: &str,
    message: &str,
    timeout: Duration,
) -> Result<String, MigrateError> {
    let result = run_shell(host, command, timeout)
        .ok_or_else(|| MigrateError::Internal(format!("{message}: timed out")))?;
    if !result.ok {
        let stderr = result.stderr.trim();
        return Err(MigrateError::BadRequest(if stderr.is_empty() {
            message.to_string()
        } else {
            format!("{message}: {stderr}")
        }));
    }
    Ok(result.stdout.trim().to_string())
}

/// Phase 1 — code state, safe while the source agent is still alive and
/// idempotent throughout: snapshot-commit a dirty source tree on its CURRENT
/// branch, push the real commits (never the snapshot, never force), fetch +
/// hard-sync the target checkout — refusing a dirty target or a target branch
/// holding commits origin has never seen, and giving a linked source worktree
/// its own worktree next to the target clone — then
/// re-apply the snapshot's changes to the target tree as uncommitted state.
/// The agent may keep writing throughout; `reconcile` picks that up once it
/// has been stopped.
pub fn prepare(
    source_cwd: &str,
    source_host: Option<&HostEntry>,
    target_host: Option<&HostEntry>,
    target_repo_root: &str,
    target_name: &str,
) -> Result<Prepared, MigrateError> {
    let thirty = Duration::from_secs(30);
    let two_minutes = Duration::from_secs(120);
    let cwd = shell_quote_path(source_cwd);

    let root = require(
        source_host,
        &format!("cd {cwd} && git rev-parse --show-toplevel"),
        &format!("session cwd is not inside a git repository: {source_cwd}"),
        thirty,
    )?;
    let root_q = shell_quote(&root);

    let branch = require(
        source_host,
        &format!("git -C {root_q} rev-parse --abbrev-ref HEAD"),
        &format!("could not determine the current branch in {root}"),
        thirty,
    )?;
    if branch == "HEAD" {
        return Err(MigrateError::BadRequest(
            "cannot migrate a detached HEAD checkout — check out a branch first".into(),
        ));
    }
    let branch_q = shell_quote(&branch);

    let status = require(
        source_host,
        &format!("git -C {root_q} status --porcelain"),
        "could not read the source checkout status",
        thirty,
    )?;
    let mut wip_committed = false;
    if !status.is_empty() {
        let message = wip_commit_message(target_name);
        require(
            source_host,
            &format!(
                "git -C {root_q} add -A && git -C {root_q} commit -m {}",
                shell_quote(&message)
            ),
            "could not create the snapshot commit",
            thirty,
        )?;
        wip_committed = true;
    }
    // A snapshot tip means dirty state should travel as dirty state — whether
    // the commit was made just above or by an earlier run that failed later
    // (idempotent retry).
    let (source_tip, tip_subject) = source_tip(source_host, &root)?;
    let carry_dirty = tip_subject.starts_with(HANDOFF_SUBJECT_PREFIX);

    // Push only real commits; the snapshot stays behind on the source. When
    // the branch already matches origin this is a no-op, and a genuinely
    // diverged origin still fails loudly here, before anything mutates. The
    // push names the commit read above rather than `HEAD`: the agent is still
    // alive and a commit it makes meanwhile must neither publish the snapshot
    // nor reach the target behind `reconcile`'s back.
    let published = if carry_dirty {
        format!("{source_tip}~1")
    } else {
        source_tip.clone()
    };
    require(
        source_host,
        &format!(
            "git -C {root_q} push origin {} && git -C {root_q} branch --set-upstream-to {} {branch_q}",
            shell_quote(&format!("{published}:refs/heads/{branch}")),
            shell_quote(&format!("origin/{branch}"))
        ),
        "git push to origin failed",
        two_minutes,
    )?;

    // A linked source worktree gets its own worktree next to the target
    // clone; parallel worktree agents would otherwise fight over the one
    // clone's checkout.
    let git_dir = require(
        source_host,
        &format!("git -C {root_q} rev-parse --absolute-git-dir"),
        "could not inspect the source checkout",
        thirty,
    )?;
    let source_is_worktree = git_dir.contains("/.git/worktrees/");
    let final_target_root = if source_is_worktree {
        ensure_target_worktree(target_host, target_repo_root, &branch)?
    } else {
        let target_q = shell_quote(target_repo_root);
        let target_status = require(
            target_host,
            &format!("git -C {target_q} status --porcelain"),
            &format!("target checkout {target_repo_root} is not a usable git repository"),
            thirty,
        )?;
        if !target_status.is_empty() {
            return Err(MigrateError::BadRequest(format!(
                "target checkout {target_repo_root} has uncommitted changes — commit or stash them there first"
            )));
        }
        require(
            target_host,
            &format!("git -C {target_q} fetch origin {branch_q}"),
            "git fetch on the target failed",
            two_minutes,
        )?;
        refuse_unpublished_commits(target_host, target_repo_root, &branch)?;
        // create-or-reset + checkout in one idempotent command (the tree was
        // verified clean and the branch verified published above).
        require(
            target_host,
            &format!(
                "git -C {target_q} checkout -B {branch_q} {}",
                shell_quote(&format!("origin/{branch}"))
            ),
            &format!("could not check out {branch} on the target"),
            thirty,
        )?;
        target_repo_root.to_string()
    };

    if carry_dirty {
        carry_dirty_state(
            source_host,
            &root,
            &published,
            &source_tip,
            target_host,
            &final_target_root,
        )?;
    }

    Ok(Prepared {
        branch,
        source_repo_root: root,
        target_repo_root: final_target_root,
        wip_committed,
        target_is_worktree: source_is_worktree,
        carried_dirty: carry_dirty,
        source_tip,
    })
}

/// The source tip's commit id and subject, read in one round trip.
fn source_tip(
    source_host: Option<&HostEntry>,
    source_root: &str,
) -> Result<(String, String), MigrateError> {
    let tip = require(
        source_host,
        &format!(
            "git -C {} log -1 --format='%H %s'",
            shell_quote(source_root)
        ),
        "could not read the source tip commit",
        Duration::from_secs(30),
    )?;
    let (id, subject) = tip.split_once(' ').unwrap_or((tip.as_str(), ""));
    Ok((id.to_string(), subject.to_string()))
}

/// Phase 1b — runs once the source agent is confirmed stopped, so the source
/// tree is finally still. `prepare` moved a snapshot taken while the agent was
/// alive; whatever it wrote afterwards (tracked edits, new files, deletions,
/// even commits) is folded into the source's snapshot commit and carried to
/// the target the same way, as uncommitted state on top of what `prepare`
/// left there. Returns whether anything had to travel.
///
/// Any error means the target is known to be missing work: the caller must
/// not cut over. The source keeps every change, committed in its snapshot.
pub fn reconcile(
    prepared: &Prepared,
    source_host: Option<&HostEntry>,
    target_host: Option<&HostEntry>,
    target_name: &str,
) -> Result<bool, MigrateError> {
    let thirty = Duration::from_secs(30);
    let root_q = shell_quote(&prepared.source_repo_root);

    let branch = require(
        source_host,
        &format!("git -C {root_q} rev-parse --abbrev-ref HEAD"),
        "could not re-read the source branch",
        thirty,
    )?;
    if branch != prepared.branch {
        return Err(MigrateError::BadRequest(format!(
            "the source checkout left {} for {branch} while the session was moving",
            prepared.branch
        )));
    }
    let status = require(
        source_host,
        &format!("git -C {root_q} status --porcelain"),
        "could not re-read the source checkout status",
        thirty,
    )?;
    if !status.is_empty() {
        // Grow the existing snapshot instead of stacking a second one: a
        // retried `prepare` publishes everything below a snapshot tip.
        let (_, tip_subject) = source_tip(source_host, &prepared.source_repo_root)?;
        let commit = if tip_subject.starts_with(HANDOFF_SUBJECT_PREFIX) {
            "commit -q --amend --no-edit --allow-empty".to_string()
        } else {
            format!(
                "commit -q -m {}",
                shell_quote(&wip_commit_message(target_name))
            )
        };
        require(
            source_host,
            &format!("git -C {root_q} add -A && git -C {root_q} {commit}"),
            "could not snapshot the changes made while the session was moving",
            thirty,
        )?;
    }
    let (final_tip, _) = source_tip(source_host, &prepared.source_repo_root)?;
    if final_tip == prepared.source_tip {
        return Ok(false);
    }
    carry_dirty_state(
        source_host,
        &prepared.source_repo_root,
        &prepared.source_tip,
        &final_tip,
        target_host,
        &prepared.target_repo_root,
    )?;
    Ok(true)
}

/// Ships the changes between two source commits to the target as uncommitted
/// state: a binary diff generated beside the source repo, copied across,
/// applied to the target tree, and removed on both sides. The target tree
/// holds exactly the content of `base` (just synced and verified clean for
/// `prepare`, left behind by `prepare` for `reconcile`), so a failure here
/// means plumbing, not conflicts — and the source snapshot commit still holds
/// everything.
fn carry_dirty_state(
    source_host: Option<&HostEntry>,
    source_root: &str,
    base: &str,
    tip: &str,
    target_host: Option<&HostEntry>,
    target_root: &str,
) -> Result<(), MigrateError> {
    let thirty = Duration::from_secs(30);
    let root_q = shell_quote(source_root);
    let source_patch = require(
        source_host,
        &format!(
            "t=$(mktemp -t diri-handoff.XXXXXX) && git -C {root_q} diff --binary --full-index {} {} > \"$t\" && echo \"$t\"",
            shell_quote(base),
            shell_quote(tip)
        ),
        "could not capture the uncommitted changes",
        thirty,
    )?;
    let target_patch = require(
        target_host,
        "mktemp -t diri-handoff.XXXXXX",
        "could not stage the uncommitted changes on the target",
        thirty,
    )?;
    let copied = copy_file(&source_patch, source_host, &target_patch, target_host);
    let _ = run_shell(
        source_host,
        &format!("rm -f {}", shell_quote(&source_patch)),
        thirty,
    );
    copied.map_err(|detail| {
        MigrateError::Internal(format!(
            "could not carry the uncommitted changes to the target ({detail})"
        ))
    })?;
    let patch_q = shell_quote(&target_patch);
    let applied = require(
        target_host,
        // `git apply` rejects an empty patch; commits that differ only in
        // identity have nothing to restore.
        &format!(
            "[ ! -s {patch_q} ] || git -C {} apply --whitespace=nowarn {patch_q}",
            shell_quote(target_root)
        ),
        "could not restore the uncommitted changes on the target",
        thirty,
    );
    let _ = run_shell(target_host, &format!("rm -f {patch_q}"), thirty);
    applied.map(|_| ())
}

/// Guards every `-B` reset of the target's `branch` to the just-fetched
/// `origin/<branch>`: a clean tree says nothing about commits the target never
/// pushed, and the reset would drop them from the branch. Only diri's own
/// snapshot commits may be left behind — their changes already traveled as
/// dirty state, which is what lets a session bounce back into the checkout it
/// left. Anything else is a hard stop before the target is mutated. A branch
/// the target does not have yet has nothing to lose.
fn refuse_unpublished_commits(
    target_host: Option<&HostEntry>,
    repo: &str,
    branch: &str,
) -> Result<(), MigrateError> {
    let repo_q = shell_quote(repo);
    let local_ref = format!("refs/heads/{branch}");
    let unpublished = require(
        target_host,
        &format!(
            "if git -C {repo_q} show-ref --verify --quiet {}; then git -C {repo_q} log --format=%s {}; fi",
            shell_quote(&local_ref),
            shell_quote(&format!("refs/remotes/origin/{branch}..{local_ref}"))
        ),
        &format!("could not compare {branch} in {repo} with origin"),
        Duration::from_secs(30),
    )?;
    let count = unpublished
        .lines()
        .filter(|subject| !subject.starts_with(HANDOFF_SUBJECT_PREFIX))
        .count();
    if count > 0 {
        return Err(MigrateError::BadRequest(format!(
            "{branch} in {repo} has {count} commit(s) that are not on origin/{branch} — push them or move them to another branch there first"
        )));
    }
    Ok(())
}

/// Creates or re-syncs the dedicated worktree for `branch` next to the
/// target's main clone. Idempotent; a dirty existing worktree is a hard stop.
fn ensure_target_worktree(
    target_host: Option<&HostEntry>,
    main_clone: &str,
    branch: &str,
) -> Result<String, MigrateError> {
    let thirty = Duration::from_secs(30);
    let two_minutes = Duration::from_secs(120);
    let repo_name = Path::new(main_clone)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let parent = Path::new(main_clone)
        .parent()
        .map(|parent| parent.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".into());
    // The same naming rule `git::create_worktree` uses, so a session moving
    // back home lands in the worktree diri originally made for it instead of
    // a near-duplicate beside it.
    let path = format!(
        "{parent}/{repo_name}-{}",
        crate::git::branch_to_path_slug(branch)
    );
    let path_q = shell_quote(&path);
    let main_q = shell_quote(main_clone);
    let branch_q = shell_quote(branch);
    let origin_ref = shell_quote(&format!("origin/{branch}"));

    // Linked worktrees keep `.git` as a file, so probe with -e, not -d.
    let probe = run_shell(
        target_host,
        &format!("[ -e {path_q}/.git ] && echo yes || echo no"),
        thirty,
    )
    .ok_or_else(|| MigrateError::Internal("target probe timed out".into()))?;
    if probe.stdout.trim() == "yes" {
        let status = require(
            target_host,
            &format!("git -C {path_q} status --porcelain"),
            &format!("target worktree {path} is not a usable git checkout"),
            thirty,
        )?;
        if !status.is_empty() {
            return Err(MigrateError::BadRequest(format!(
                "target worktree {path} has uncommitted changes — commit or stash them there first"
            )));
        }
        require(
            target_host,
            &format!("git -C {path_q} fetch origin {branch_q}"),
            "git fetch on the target failed",
            two_minutes,
        )?;
        refuse_unpublished_commits(target_host, &path, branch)?;
        require(
            target_host,
            &format!("git -C {path_q} checkout -B {branch_q} {origin_ref}"),
            &format!("could not check out {branch} in {path}"),
            thirty,
        )?;
    } else {
        require(
            target_host,
            &format!("git -C {main_q} fetch origin {branch_q}"),
            "git fetch on the target failed",
            two_minutes,
        )?;
        refuse_unpublished_commits(target_host, main_clone, branch)?;
        require(
            target_host,
            &format!("git -C {main_q} worktree add -B {branch_q} {path_q} {origin_ref}"),
            &format!(
                "could not create worktree {path} on the target (is {branch} checked out elsewhere there?)"
            ),
            two_minutes,
        )?;
    }
    Ok(path)
}

/// Phase 2 — transcript shuttle (the source agent is already stopped, so the
/// jsonl is final). Missing transcripts are non-fatal: the caller respawns a
/// fresh conversation and the result says so. The source copy is never
/// deleted.
pub fn shuttle_transcript(
    record_cwd: &str,
    record_transcript_path: Option<&str>,
    agent_session_id: Option<&str>,
    source_host: Option<&HostEntry>,
    target_host: Option<&HostEntry>,
    prepared: &Prepared,
    home: &Path,
) -> TranscriptShuttle {
    let missing = |warning: &str| TranscriptShuttle {
        migrated: false,
        local_target_path: None,
        warning: Some(warning.to_string()),
    };
    let Some(uuid) = agent_session_id else {
        return missing("no conversation id recorded — starting a fresh conversation");
    };

    let source_path = if source_host.is_none() {
        local_transcript(record_cwd, record_transcript_path, uuid, home)
    } else {
        remote_transcript(source_host, &prepared.source_repo_root, record_cwd, uuid)
    };
    let Some(source_path) = source_path else {
        return missing(
            "transcript not found on the source — code state moved, but the conversation restarts fresh",
        );
    };

    let slug = claude_project_slug(&prepared.target_repo_root);
    let failed = |detail: String| {
        missing(&format!(
            "transcript copy failed ({detail}) — code state moved, but the conversation restarts fresh"
        ))
    };
    if let Some(target) = target_host {
        let dir = format!(".claude/projects/{slug}");
        match run_shell(
            Some(target),
            &format!("mkdir -p {}", shell_quote(&dir)),
            Duration::from_secs(30),
        ) {
            Some(result) if result.ok => {}
            other => {
                return failed(
                    other
                        .map(|result| result.stderr.trim().to_string())
                        .unwrap_or_else(|| "timed out".into()),
                );
            }
        }
        match copy_file(
            &source_path,
            source_host,
            &format!("{dir}/{uuid}.jsonl"),
            Some(target),
        ) {
            Ok(()) => TranscriptShuttle {
                migrated: true,
                local_target_path: None,
                warning: None,
            },
            Err(detail) => failed(detail),
        }
    } else {
        let dir = home.join(format!(".claude/projects/{slug}"));
        if let Err(error) = std::fs::create_dir_all(&dir) {
            return failed(error.to_string());
        }
        let destination = dir.join(format!("{uuid}.jsonl"));
        match copy_file(
            &source_path,
            source_host,
            &destination.to_string_lossy(),
            None,
        ) {
            Ok(()) => TranscriptShuttle {
                migrated: true,
                local_target_path: Some(destination.to_string_lossy().into_owned()),
                warning: None,
            },
            Err(detail) => failed(detail),
        }
    }
}

fn local_transcript(
    record_cwd: &str,
    recorded_path: Option<&str>,
    uuid: &str,
    home: &Path,
) -> Option<String> {
    if let Some(path) = recorded_path
        && Path::new(path).exists()
    {
        return Some(path.to_string());
    }
    let predicted = claude_transcript_path(home, record_cwd, uuid);
    if predicted.exists() {
        return Some(predicted.to_string_lossy().into_owned());
    }
    // Claude relocates the jsonl when the agent enters a worktree — scan
    // every project dir before giving up.
    let projects = home.join(".claude/projects");
    for entry in std::fs::read_dir(projects).ok()?.flatten() {
        let candidate = entry.path().join(format!("{uuid}.jsonl"));
        if candidate.exists() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn remote_transcript(
    host: Option<&HostEntry>,
    source_cwd_abs: &str,
    record_cwd: &str,
    uuid: &str,
) -> Option<String> {
    let probes: Vec<String> = [source_cwd_abs, record_cwd]
        .iter()
        .map(|cwd| format!(".claude/projects/{}/{uuid}.jsonl", claude_project_slug(cwd)))
        .map(|path| {
            format!(
                "if [ -f {q} ]; then echo {q}; exit 0; fi",
                q = shell_quote(&path)
            )
        })
        .collect();
    let command = format!(
        "{}; ls -1 \"$HOME\"/.claude/projects/*/{uuid}.jsonl 2>/dev/null | head -n1",
        probes.join("; ")
    );
    let result = run_shell(host, &command, Duration::from_secs(20))?;
    let path = result.stdout.trim();
    (result.ok && !path.is_empty()).then(|| path.to_string())
}

/// cp locally, scp when either side is remote (`-3` routes remote→remote
/// through the daemon so the two hosts never need to reach each other).
pub fn copy_argv(
    from: &str,
    from_host: Option<&HostEntry>,
    to: &str,
    to_host: Option<&HostEntry>,
) -> Vec<String> {
    if from_host.is_none() && to_host.is_none() {
        return vec!["/bin/cp".into(), from.into(), to.into()];
    }
    let source = from_host.map_or_else(|| from.to_string(), |host| format!("{}:{from}", host.ssh));
    let destination = to_host.map_or_else(|| to.to_string(), |host| format!("{}:{to}", host.ssh));
    let mut argv = vec!["scp".to_string()];
    argv.extend(SSH_OPTIONS.iter().map(ToString::to_string));
    argv.push("-q".into());
    if from_host.is_some() && to_host.is_some() {
        argv.push("-3".into());
    }
    argv.push(source);
    argv.push(destination);
    argv
}

fn copy_file(
    from: &str,
    from_host: Option<&HostEntry>,
    to: &str,
    to_host: Option<&HostEntry>,
) -> Result<(), String> {
    let mut argv = copy_argv(from, from_host, to, to_host);
    let program = argv.remove(0);
    let output = std::process::Command::new(&program)
        .args(&argv)
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        Err(if stderr.is_empty() {
            format!("exit {}", output.status.code().unwrap_or(-1))
        } else {
            stderr.to_string()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn host(id: &str) -> HostEntry {
        HostEntry {
            id: id.into(),
            name: None,
            ssh: format!("user@{id}"),
            default_cwd: None,
            node: None,
        }
    }

    #[test]
    fn copy_argv_picks_cp_scp_and_relay() {
        assert_eq!(copy_argv("/a", None, "/b", None)[0], "/bin/cp");

        let push = copy_argv("/a", None, "/b", Some(&host("h")));
        assert_eq!(push[0], "scp");
        assert_eq!(push.last().unwrap(), "user@h:/b");
        assert!(!push.contains(&"-3".to_string()));

        let relay = copy_argv("/a", Some(&host("x")), "/b", Some(&host("y")));
        assert!(relay.contains(&"-3".to_string()), "remote→remote relays");
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git");
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A bare origin plus a seeded `source` clone (one `root` commit holding
    /// `file.txt`) and an empty `target` clone.
    fn seeded_repos(temp: &Path) -> (PathBuf, PathBuf) {
        let origin = temp.join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        let source = temp.join("source");
        git(temp, &["clone", "-q", origin.to_str().unwrap(), "source"]);
        // `prepare` invokes git in a separate shell and must not inherit a
        // developer machine's global identity. Give every fixture checkout
        // its own author, just as a real configured checkout has one.
        git(&source, &["config", "user.name", "Diri Test"]);
        git(
            &source,
            &["config", "user.email", "diri-test@example.invalid"],
        );
        std::fs::write(source.join("file.txt"), "v1\n").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-q", "-m", "root"]);
        git(&source, &["push", "-q", "-u", "origin", "main"]);
        let target = temp.join("target");
        git(temp, &["clone", "-q", origin.to_str().unwrap(), "target"]);
        git(&target, &["config", "user.name", "Diri Test"]);
        git(
            &target,
            &["config", "user.email", "diri-test@example.invalid"],
        );
        (source, target)
    }

    /// The whole flow against LOCAL repos — no ssh involved — through a full
    /// round trip: uncommitted work must arrive uncommitted on every leg,
    /// real commits must arrive committed, and origin must never see a
    /// snapshot commit.
    #[test]
    fn prepare_round_trips_dirty_state_between_local_checkouts() {
        let temp = tempfile::tempdir().expect("temp");
        let (source, target) = seeded_repos(temp.path());

        // Leg 1: a tracked edit plus a brand-new file move out.
        std::fs::write(source.join("file.txt"), "local edit\n").unwrap();
        std::fs::write(source.join("notes.md"), "scratch\n").unwrap();
        let out = prepare(
            source.to_str().unwrap(),
            None,
            None,
            target.to_str().unwrap(),
            "forge",
        )
        .expect("leg 1");
        assert_eq!(out.branch, "main");
        assert!(out.wip_committed && out.carried_dirty);
        assert!(!out.target_is_worktree);
        assert_eq!(
            std::fs::read_to_string(target.join("file.txt")).unwrap(),
            "local edit\n"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("notes.md")).unwrap(),
            "scratch\n"
        );
        assert!(
            !git_out(&target, &["status", "--porcelain"]).is_empty(),
            "dirty state arrives dirty"
        );
        assert_eq!(
            git_out(&target, &["log", "-1", "--format=%s", "origin/main"]),
            "root",
            "origin never sees the snapshot"
        );
        assert!(
            git_out(&source, &["log", "-1", "--format=%s"]).starts_with(HANDOFF_SUBJECT_PREFIX),
            "the source keeps a recovery snapshot"
        );

        // Work "in the cloud": one real commit plus further uncommitted edits.
        std::fs::write(target.join("real.md"), "done\n").unwrap();
        git(&target, &["add", "real.md"]);
        git(&target, &["commit", "-q", "-m", "real work"]);
        std::fs::write(target.join("notes.md"), "scratch v2\n").unwrap();

        // Leg 2: back home, into the original checkout.
        let back = prepare(
            target.to_str().unwrap(),
            None,
            None,
            source.to_str().unwrap(),
            "local",
        )
        .expect("leg 2");
        assert!(back.carried_dirty);
        assert_eq!(git_out(&source, &["log", "-1", "--format=%s"]), "real work");
        assert_eq!(
            std::fs::read_to_string(source.join("file.txt")).unwrap(),
            "local edit\n"
        );
        assert_eq!(
            std::fs::read_to_string(source.join("notes.md")).unwrap(),
            "scratch v2\n"
        );
        assert_eq!(
            std::fs::read_to_string(source.join("real.md")).unwrap(),
            "done\n"
        );
        assert!(!git_out(&source, &["status", "--porcelain"]).is_empty());
        assert!(
            !git_out(&source, &["log", "--format=%s", "origin/main"]).contains("WIP:"),
            "no snapshot ever lands on origin"
        );
    }

    /// A linked source worktree gets a worktree on the target, named by the
    /// same slug rule `git::create_worktree` uses — so moving back home later
    /// reuses the worktree diri originally created rather than growing a
    /// near-duplicate beside it.
    #[test]
    fn a_linked_worktree_source_gets_a_slugged_target_worktree() {
        let temp = tempfile::tempdir().expect("temp");
        let (source, target) = seeded_repos(temp.path());
        let worktree = temp.path().join("source-feature");
        git(
            &source,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "Feature/X",
                worktree.to_str().unwrap(),
            ],
        );
        std::fs::write(worktree.join("wip.txt"), "wt dirt\n").unwrap();

        let out = prepare(
            worktree.to_str().unwrap(),
            None,
            None,
            target.to_str().unwrap(),
            "forge",
        )
        .expect("prepare");
        assert!(out.target_is_worktree && out.carried_dirty);
        assert!(
            out.target_repo_root.ends_with("target-feature-x"),
            "slugged like create_worktree: {}",
            out.target_repo_root
        );
        let landed = Path::new(&out.target_repo_root);
        assert_eq!(
            git_out(landed, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "Feature/X"
        );
        assert_eq!(
            std::fs::read_to_string(landed.join("wip.txt")).unwrap(),
            "wt dirt\n"
        );
        assert!(!git_out(landed, &["status", "--porcelain"]).is_empty());
    }

    fn commit_file(dir: &Path, name: &str, subject: &str) -> String {
        std::fs::write(dir.join(name), "valuable work\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", subject]);
        git_out(dir, &["rev-parse", "HEAD"])
    }

    fn prepare_local(source: &Path, target: &Path) -> Result<Prepared, MigrateError> {
        prepare(
            source.to_str().unwrap(),
            None,
            None,
            target.to_str().unwrap(),
            "target",
        )
    }

    fn assert_refused_as_unpublished(result: Result<Prepared, MigrateError>) {
        match result {
            Err(MigrateError::BadRequest(message)) => {
                assert!(message.contains("not on origin"), "{message}");
            }
            other => panic!("unsafe destination must be rejected: {other:?}"),
        }
    }

    /// A clean working tree says nothing about commits the destination never
    /// pushed; resetting its branch would silently drop them.
    #[test]
    fn a_destination_ahead_of_origin_is_a_hard_stop() {
        let temp = tempfile::tempdir().expect("temp");
        let (source, target) = seeded_repos(temp.path());
        let before = commit_file(&target, "destination-only.txt", "unpushed destination work");
        assert!(git_out(&target, &["status", "--porcelain"]).is_empty());

        assert_refused_as_unpublished(prepare_local(&source, &target));
        assert_eq!(git_out(&target, &["rev-parse", "HEAD"]), before);
        assert!(target.join("destination-only.txt").exists());
    }

    #[test]
    fn a_destination_diverged_from_origin_is_a_hard_stop() {
        let temp = tempfile::tempdir().expect("temp");
        let (source, target) = seeded_repos(temp.path());
        commit_file(&source, "source-only.txt", "source work");
        let before = commit_file(&target, "destination-only.txt", "unpushed destination work");

        assert_refused_as_unpublished(prepare_local(&source, &target));
        assert_eq!(git_out(&target, &["rev-parse", "HEAD"]), before);
    }

    /// The branch does not have to be checked out to be reset by
    /// `checkout -B`.
    #[test]
    fn an_unpublished_branch_the_destination_has_parked_is_a_hard_stop() {
        let temp = tempfile::tempdir().expect("temp");
        let (source, target) = seeded_repos(temp.path());
        let before = commit_file(&target, "destination-only.txt", "unpushed destination work");
        git(&target, &["checkout", "-q", "-b", "parking", "origin/main"]);

        assert_refused_as_unpublished(prepare_local(&source, &target));
        assert_eq!(git_out(&target, &["rev-parse", "refs/heads/main"]), before);
        assert_eq!(
            git_out(&target, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "parking"
        );
    }

    fn feature_worktree(temp: &Path, source: &Path) -> PathBuf {
        let worktree = temp.join("source-feature");
        git(
            source,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "Feature/X",
                worktree.to_str().unwrap(),
            ],
        );
        std::fs::write(worktree.join("wip.txt"), "wt dirt\n").unwrap();
        worktree
    }

    #[test]
    fn a_new_target_worktree_never_resets_an_unpublished_branch() {
        let temp = tempfile::tempdir().expect("temp");
        let (source, target) = seeded_repos(temp.path());
        let worktree = feature_worktree(temp.path(), &source);
        git(&target, &["checkout", "-q", "-b", "Feature/X"]);
        let before = commit_file(&target, "destination-only.txt", "unpushed destination work");
        git(&target, &["checkout", "-q", "main"]);

        assert_refused_as_unpublished(prepare_local(&worktree, &target));
        assert_eq!(
            git_out(&target, &["rev-parse", "refs/heads/Feature/X"]),
            before
        );
        assert!(!temp.path().join("target-feature-x").exists());
    }

    #[test]
    fn an_existing_target_worktree_never_resets_an_unpublished_branch() {
        let temp = tempfile::tempdir().expect("temp");
        let (source, target) = seeded_repos(temp.path());
        let worktree = feature_worktree(temp.path(), &source);
        let out = prepare_local(&worktree, &target).expect("first move");
        let landed = PathBuf::from(out.target_repo_root);
        let before = commit_file(&landed, "destination-only.txt", "unpushed destination work");
        assert!(git_out(&landed, &["status", "--porcelain"]).is_empty());

        assert_refused_as_unpublished(prepare_local(&worktree, &target));
        assert_eq!(git_out(&landed, &["rev-parse", "HEAD"]), before);
    }

    /// Installs a fixture-only pre-push hook: a deterministic stand-in for an
    /// agent that keeps working while the slow transfer is under way.
    fn work_during_transfer(source: &Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let hook = source.join(".git/hooks/pre-push");
        git(source, &["config", "core.hooksPath", ".git/hooks"]);
        std::fs::write(&hook, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn assert_same_file(source: &Path, target: &Path, name: &str) {
        assert_eq!(
            std::fs::read(target.join(name)).ok(),
            std::fs::read(source.join(name)).ok(),
            "{name} differs between source and target"
        );
    }

    /// The agent is alive while `prepare` snapshots and transfers, so the
    /// snapshot can be stale by the time it is stopped. A tracked edit, a new
    /// untracked file and a deletion made in that window must all be on the
    /// target before anything resumes there.
    #[test]
    fn work_done_while_the_session_was_moving_reaches_the_target() {
        let temp = tempfile::tempdir().expect("temp");
        let (source, target) = seeded_repos(temp.path());
        std::fs::write(source.join("file.txt"), "snapshot-time edit\n").unwrap();
        std::fs::write(source.join("notes.md"), "scratch\n").unwrap();
        work_during_transfer(
            &source,
            "printf 'late agent edit\\n' > file.txt\nprintf 'late file\\n' > late.txt\nrm notes.md",
        );

        let prepared = prepare_local(&source, &target).expect("prepare");
        assert!(reconcile(&prepared, None, None, "target").expect("reconcile"));

        assert_eq!(
            std::fs::read_to_string(target.join("file.txt")).unwrap(),
            "late agent edit\n"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("late.txt")).unwrap(),
            "late file\n"
        );
        assert!(!target.join("notes.md").exists(), "deletions travel too");
        for name in ["file.txt", "late.txt", "notes.md"] {
            assert_same_file(&source, &target, name);
        }
        assert_eq!(
            git_out(&target, &["log", "-1", "--format=%s", "origin/main"]),
            "root",
            "origin never sees the snapshot"
        );
        // The late work joined the one recovery snapshot, so the source is
        // clean again and the session can still bounce back into it.
        assert!(git_out(&source, &["status", "--porcelain"]).is_empty());
        let unpublished = git_out(&source, &["log", "--format=%s", "origin/main..HEAD"]);
        assert_eq!(unpublished.lines().count(), 1, "{unpublished}");
        assert!(unpublished.starts_with(HANDOFF_SUBJECT_PREFIX));

        std::fs::write(target.join("late.txt"), "late file v2\n").unwrap();
        prepare_local(&target, &source).expect("back home");
        assert_eq!(
            std::fs::read_to_string(source.join("file.txt")).unwrap(),
            "late agent edit\n"
        );
        assert_eq!(
            std::fs::read_to_string(source.join("late.txt")).unwrap(),
            "late file v2\n"
        );
        assert!(!source.join("notes.md").exists());
    }

    /// A source that was clean at snapshot time has no snapshot commit to
    /// grow; late work still travels, and still as uncommitted state.
    #[test]
    fn a_clean_source_that_changed_while_moving_is_reconciled() {
        let temp = tempfile::tempdir().expect("temp");
        let (source, target) = seeded_repos(temp.path());
        work_during_transfer(&source, "printf 'late file\\n' > late.txt");

        let prepared = prepare_local(&source, &target).expect("prepare");
        assert!(!prepared.wip_committed && !prepared.carried_dirty);
        assert!(reconcile(&prepared, None, None, "target").expect("reconcile"));

        assert_same_file(&source, &target, "late.txt");
        assert!(target.join("late.txt").exists());
        assert!(!git_out(&target, &["status", "--porcelain"]).is_empty());
        assert!(
            git_out(&source, &["log", "-1", "--format=%s"]).starts_with(HANDOFF_SUBJECT_PREFIX)
        );
        assert_eq!(
            git_out(&target, &["log", "-1", "--format=%s", "origin/main"]),
            "root"
        );
    }

    /// A commit the agent makes on top of the snapshot mid-transfer must not
    /// drag the snapshot onto origin, and its content must still arrive.
    #[test]
    fn a_commit_made_while_moving_arrives_without_publishing_the_snapshot() {
        let temp = tempfile::tempdir().expect("temp");
        let (source, target) = seeded_repos(temp.path());
        std::fs::write(source.join("file.txt"), "snapshot-time edit\n").unwrap();
        work_during_transfer(
            &source,
            "printf 'late agent edit\\n' > file.txt\ngit add -A\ngit commit -q -m 'late commit'",
        );

        let prepared = prepare_local(&source, &target).expect("prepare");
        assert_eq!(
            git_out(&target, &["log", "-1", "--format=%s", "origin/main"]),
            "root",
            "the push is pinned below the snapshot"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("file.txt")).unwrap(),
            "snapshot-time edit\n"
        );
        assert!(reconcile(&prepared, None, None, "target").expect("reconcile"));
        assert_same_file(&source, &target, "file.txt");
        assert_eq!(
            std::fs::read_to_string(target.join("file.txt")).unwrap(),
            "late agent edit\n"
        );
    }

    #[test]
    fn a_source_that_stayed_still_has_nothing_to_reconcile() {
        let temp = tempfile::tempdir().expect("temp");
        let (source, target) = seeded_repos(temp.path());
        std::fs::write(source.join("file.txt"), "snapshot-time edit\n").unwrap();

        let prepared = prepare_local(&source, &target).expect("prepare");
        assert!(!reconcile(&prepared, None, None, "target").expect("reconcile"));
        assert_eq!(
            git_out(&source, &["rev-parse", "HEAD"]),
            prepared.source_tip
        );
        assert_same_file(&source, &target, "file.txt");
    }

    /// Late work that cannot be carried is an error, never a quiet cutover —
    /// and the source still holds all of it.
    #[test]
    fn a_failed_reconcile_is_an_error_and_the_source_keeps_the_late_work() {
        let temp = tempfile::tempdir().expect("temp");
        let (source, target) = seeded_repos(temp.path());
        std::fs::write(source.join("file.txt"), "snapshot-time edit\n").unwrap();
        let prepared = prepare_local(&source, &target).expect("prepare");

        std::fs::write(source.join("file.txt"), "late agent edit\n").unwrap();
        std::fs::write(source.join("late.txt"), "late file\n").unwrap();
        // Something else touched the target: the delta no longer applies.
        std::fs::write(target.join("file.txt"), "unrelated target edit\n").unwrap();

        let error = reconcile(&prepared, None, None, "target").expect_err("must fail closed");
        assert!(error.to_string().contains("could not restore"), "{error}");
        assert!(!target.join("late.txt").exists(), "apply is all-or-nothing");
        assert_eq!(
            git_out(&source, &["show", "HEAD:file.txt"]),
            "late agent edit"
        );
        assert_eq!(git_out(&source, &["show", "HEAD:late.txt"]), "late file");
        assert_eq!(
            std::fs::read_to_string(source.join("file.txt")).unwrap(),
            "late agent edit\n"
        );
    }

    #[test]
    fn a_source_that_switched_branches_while_moving_is_refused() {
        let temp = tempfile::tempdir().expect("temp");
        let (source, target) = seeded_repos(temp.path());
        let prepared = prepare_local(&source, &target).expect("prepare");
        git(&source, &["checkout", "-q", "-b", "elsewhere"]);

        let error = reconcile(&prepared, None, None, "target").expect_err("must refuse");
        assert!(error.to_string().contains("elsewhere"), "{error}");
    }

    #[test]
    fn a_dirty_target_is_a_hard_stop() {
        let temp = tempfile::tempdir().expect("temp");
        let (source, target) = seeded_repos(temp.path());
        std::fs::write(target.join("file.txt"), "target work in progress").unwrap();

        let error = prepare(
            source.to_str().unwrap(),
            None,
            None,
            target.to_str().unwrap(),
            "local",
        )
        .expect_err("dirty target must refuse");
        assert!(error.to_string().contains("uncommitted changes"), "{error}");
    }
}
