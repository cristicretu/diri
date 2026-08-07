//! The git + transcript legwork of `session.migrate`: one-click handoff of a
//! live Claude session between machines, preserving conversation context
//! (`claude --resume`) and code state (WIP commit + push + hard-sync).
//!
//! Ported from `SessionMigrator`. Both sides run through the same bounded
//! shell, so "source" and "target" can each be the local machine or a remote
//! host. The control server owns orchestration (preconditions, kill,
//! respawn); this module owns the mechanical steps and is deliberately
//! record-in, values-out.

use std::path::Path;
use std::time::Duration;

use diri_proto::HostEntry;

use crate::hosts::{SSH_OPTIONS, run_shell};
use crate::inject::{claude_project_slug, claude_transcript_path};
use crate::remote::{remote_tmux_session_name, shell_quote, shell_quote_path};

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
}

pub struct TranscriptShuttle {
    pub migrated: bool,
    /// For a LOCAL target: where the record's transcriptPath should point.
    pub local_target_path: Option<String>,
    pub warning: Option<String>,
}

pub fn wip_commit_message(target_name: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("WIP: handoff to {target_name} @{now}")
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
/// idempotent throughout: WIP-commit a dirty source tree on its CURRENT
/// branch, push (setting upstream; never force), then fetch + hard-sync the
/// target checkout — refusing a dirty target, and giving a linked source
/// worktree its own worktree next to the target clone.
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
            "could not create the WIP handoff commit",
            thirty,
        )?;
        wip_committed = true;
    }
    require(
        source_host,
        &format!("git -C {root_q} push -u origin {branch_q}"),
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
    let final_target_root = if git_dir.contains("/.git/worktrees/") {
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
        // create-or-reset + checkout in one idempotent command (the tree was
        // verified clean above).
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

    Ok(Prepared {
        branch,
        source_repo_root: root,
        target_repo_root: final_target_root,
        wip_committed,
    })
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
    let path = format!("{parent}/{repo_name}-{}", branch.replace('/', "-"));
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

/// Best-effort: end the source-side tmux session after a remote→local
/// migration so the host doesn't keep a zombie agent. Never fatal.
pub fn kill_remote_tmux(host: &HostEntry, session_id: &str) -> Option<String> {
    let name = remote_tmux_session_name(session_id);
    let result = run_shell(
        Some(host),
        &format!(
            "tmux kill-session -t {} 2>/dev/null || true",
            shell_quote(&name)
        ),
        Duration::from_secs(15),
    );
    if result.map(|result| result.ok) != Some(true) {
        return Some(format!(
            "could not stop the tmux session on {} ({name}) — it may still be running there",
            host.display_name()
        ));
    }
    None
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

    /// The whole prepare flow against two LOCAL repos — no ssh involved,
    /// exactly how the Swift tests exercised it.
    #[test]
    fn prepare_moves_a_dirty_branch_between_local_checkouts() {
        let temp = tempfile::tempdir().expect("temp");
        let git = |dir: &Path, args: &[&str]| {
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
        };

        // A bare origin, a source clone with WIP, an empty target clone.
        let origin = temp.path().join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        let source = temp.path().join("source");
        git(
            temp.path(),
            &["clone", "-q", origin.to_str().unwrap(), "source"],
        );
        std::fs::write(source.join("file.txt"), "v1\n").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-q", "-m", "root"]);
        git(&source, &["push", "-q", "-u", "origin", "main"]);
        let target = temp.path().join("target");
        git(
            temp.path(),
            &["clone", "-q", origin.to_str().unwrap(), "target"],
        );

        // Dirty the source; prepare must WIP-commit, push, and sync target.
        std::fs::write(source.join("file.txt"), "wip changes\n").unwrap();
        let prepared = prepare(
            source.to_str().unwrap(),
            None,
            None,
            target.to_str().unwrap(),
            "local",
        )
        .expect("prepare");
        assert_eq!(prepared.branch, "main");
        assert!(prepared.wip_committed);
        assert_eq!(
            std::fs::read_to_string(target.join("file.txt")).unwrap(),
            "wip changes\n",
            "the target hard-synced to the pushed WIP"
        );

        // Idempotent: a second run with a clean source is a no-op success.
        let again = prepare(
            source.to_str().unwrap(),
            None,
            None,
            target.to_str().unwrap(),
            "local",
        )
        .expect("re-run");
        assert!(!again.wip_committed);
    }

    #[test]
    fn a_dirty_target_is_a_hard_stop() {
        let temp = tempfile::tempdir().expect("temp");
        let git = |dir: &Path, args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@t")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@t")
                    .status()
                    .expect("git")
                    .success()
            );
        };
        let origin = temp.path().join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        let source = temp.path().join("source");
        git(
            temp.path(),
            &["clone", "-q", origin.to_str().unwrap(), "source"],
        );
        std::fs::write(source.join("f"), "x").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-q", "-m", "root"]);
        git(&source, &["push", "-q", "-u", "origin", "main"]);
        let target = temp.path().join("target");
        git(
            temp.path(),
            &["clone", "-q", origin.to_str().unwrap(), "target"],
        );
        std::fs::write(target.join("f"), "target work in progress").unwrap();

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
