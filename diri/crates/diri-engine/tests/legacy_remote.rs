//! The one-time retirement of pre-Holder (`ssh -t` + tmux) remote sessions.
//!
//! These drive the real migration against a fake `ssh` that puts a fake `tmux`
//! on the remote PATH, in the style of `diri-remote/tests/engine_remote_e2e.rs`.
//! The fake tmux is deliberately strict — it *refuses* a target that is not in
//! tmux's `=exact` form — so a regression to prefix/fnmatch matching fails a
//! test rather than someone's unrelated pane.

#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use diri_engine::detect::ManifestEngine;
use diri_engine::hosts::{SSH_OPTIONS, ShellOutput};
use diri_engine::legacy_remote::{Outcome, Plan, legacy_pane_name};
use diri_engine::registry::Registry;
use diri_engine::remote::binding::{RemoteBinding, RemoteBindingStore};
use diri_proto::remote_pty::{ProtocolVersion, SessionToken};
use diri_proto::{
    AgentKind, DateMillis, ExitReason, HostEntry, HostsConfig, ProjectId, Resumability, SessionId,
    SessionRecord, SessionStatus, TitleSource,
};

// MARK: Fixtures

fn engine() -> std::sync::Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, failed) = ManifestEngine::load_dir(&dir).expect("load manifests");
    assert!(failed.is_empty(), "manifests failed: {failed:?}");
    std::sync::Arc::new(engine)
}

fn record(id: &str, host: Option<&str>, agent_session_id: Option<&str>) -> SessionRecord {
    SessionRecord {
        id: SessionId(id.into()),
        kind: AgentKind::CLAUDE_CODE,
        cwd: "/home/user/code/app".into(),
        project_id: ProjectId("p".into()),
        worktree_path: None,
        git_branch: None,
        title: "a remote conversation".into(),
        title_source: TitleSource::FirstPrompt,
        originating_prompt: None,
        agent_session_id: agent_session_id.map(ToString::to_string),
        transcript_path: None,
        // What the old transport left behind: a record that still claims to be
        // running, on a transport that no longer exists.
        status: SessionStatus::Working,
        status_evidence: None,
        needs_input: None,
        resumability: Resumability::Live,
        parent: None,
        created_at: DateMillis(0.0),
        updated_at: DateMillis(0.0),
        last_turn_completed_at: None,
        last_seen_at: None,
        pinned: false,
        archived_at: None,
        host: host.map(ToString::to_string),
        remote_persistence: None,
        hibernation: None,
        memory_bytes: None,
        artifacts: None,
        pull_requests: None,
        listening_ports: None,
        foreground_agent: None,
    }
}

fn host_entry(id: &str) -> HostEntry {
    HostEntry {
        id: id.into(),
        name: None,
        ssh: format!("user@{id}"),
        default_cwd: None,
        node: None,
    }
}

fn write_executable(path: &Path, contents: &str) {
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o700)
        .open(path)
        .unwrap_or_else(|error| panic!("create {}: {error}", path.display()));
    file.write_all(contents.as_bytes()).expect("write script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("mode");
}

/// A world the fake ssh lands in: a remote PATH holding a fake tmux, a file of
/// live tmux session names, and logs of everything either program was asked.
struct FakeHost {
    ssh: PathBuf,
    ssh_log: PathBuf,
    tmux_log: PathBuf,
    sessions_file: PathBuf,
}

impl FakeHost {
    /// `tmux: None` builds a host with no tmux at all on its PATH.
    fn new(root: &Path, tmux: Option<&[&str]>) -> Self {
        let bin = root.join("remote-bin");
        fs::create_dir_all(&bin).expect("remote bin");
        let ssh_log = root.join("ssh.log");
        let tmux_log = root.join("tmux.log");
        let sessions_file = root.join("tmux-sessions");
        fs::write(
            &sessions_file,
            tmux.unwrap_or(&[])
                .iter()
                .map(|name| format!("{name}\n"))
                .collect::<String>(),
        )
        .expect("sessions fixture");

        if tmux.is_some() {
            write_executable(
                &bin.join("tmux"),
                // Strict on purpose: `kill-session -t` must arrive in tmux's
                // documented `=exact` form. A bare name is refused so that a
                // regression to prefix/fnmatch matching cannot pass.
                r#"#!/bin/sh
printf '%s\n' "$*" >> "$DIRI_TEST_TMUX_LOG"
sessions="$DIRI_TEST_SESSIONS"
case "$1" in
  list-sessions)
    fmt="$3"
    while IFS= read -r name; do
      [ -n "$name" ] || continue
      printf '%s\n' "$fmt" | sed "s/#{session_name}/$name/g"
    done < "$sessions"
    exit 0
    ;;
  kill-session)
    target="$3"
    case "$target" in
      "="*) name=`printf '%s' "$target" | cut -c2-` ;;
      *) printf 'inexact target refused: %s\n' "$target" >&2; exit 2 ;;
    esac
    if grep -qx -- "$name" "$sessions"; then
      grep -vx -- "$name" "$sessions" > "$sessions.new"
      mv "$sessions.new" "$sessions"
      exit 0
    fi
    exit 1
    ;;
esac
exit 0
"#,
            );
        }

        let ssh = root.join("ssh");
        write_executable(
            &ssh,
            &format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$*\" >> '{ssh_log}'\n\
                 for last; do :; done\n\
                 PATH='{bin}':/usr/bin:/bin\n\
                 export PATH\n\
                 DIRI_TEST_SESSIONS='{sessions}'\n\
                 DIRI_TEST_TMUX_LOG='{tmux_log}'\n\
                 export DIRI_TEST_SESSIONS DIRI_TEST_TMUX_LOG\n\
                 exec /bin/sh -c \"$last\"\n",
                ssh_log = ssh_log.display(),
                bin = bin.display(),
                sessions = sessions_file.display(),
                tmux_log = tmux_log.display(),
            ),
        );

        Self {
            ssh,
            ssh_log,
            tmux_log,
            sessions_file,
        }
    }

    fn live_sessions(&self) -> Vec<String> {
        fs::read_to_string(&self.sessions_file)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(ToString::to_string)
            .collect()
    }

    fn ssh_log(&self) -> String {
        fs::read_to_string(&self.ssh_log).unwrap_or_default()
    }

    fn tmux_log(&self) -> String {
        fs::read_to_string(&self.tmux_log).unwrap_or_default()
    }
}

/// Mirrors `hosts::run_shell`'s argv exactly, with the fake standing in for the
/// real `ssh` binary — the seam the migration takes so a test never needs to
/// mutate PATH out from under a parallel test binary.
fn ssh_runner(
    ssh: PathBuf,
    calls: &AtomicUsize,
) -> impl Fn(Option<&HostEntry>, &str, Duration) -> Option<ShellOutput> + '_ {
    move |host, command, _timeout| {
        calls.fetch_add(1, Ordering::SeqCst);
        let host = host.expect("the migration only ever runs remote commands");
        let mut args: Vec<String> = SSH_OPTIONS.iter().map(ToString::to_string).collect();
        args.push(host.ssh.clone());
        args.push("--".into());
        args.push(command.to_string());
        let output = std::process::Command::new(&ssh)
            .args(&args)
            .output()
            .expect("fake ssh");
        Some(ShellOutput {
            ok: output.status.success(),
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Everything the migration reads and writes, in one temp directory.
struct World {
    _temp: tempfile::TempDir,
    root: PathBuf,
    registry: Mutex<Registry>,
    bindings: RemoteBindingStore,
    hosts: HostsConfig,
    logs_dir: PathBuf,
}

impl World {
    fn new(records: Vec<SessionRecord>, hosts: Vec<HostEntry>) -> Self {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().to_path_buf();
        let logs_dir = root.join("logs");
        fs::create_dir_all(&logs_dir).expect("logs");
        let mut registry = Registry::new(engine(), root.join("state.json"));
        for record in records {
            // Every session has an output log; the migration must never take
            // one away.
            fs::write(
                logs_dir.join(format!("{}.bin", record.id.0)),
                b"scrollback the user can still read\n",
            )
            .expect("log fixture");
            registry.insert_record(record);
        }
        registry.persist().expect("persist");
        let bindings =
            RemoteBindingStore::new(root.join("remote-bindings")).expect("binding store");
        Self {
            _temp: temp,
            root,
            registry: Mutex::new(registry),
            bindings,
            hosts: HostsConfig { hosts },
            logs_dir,
        }
    }

    fn marker(&self) -> PathBuf {
        self.root.join("legacy-remote-migration.json")
    }

    fn plan(&self) -> Plan<'_> {
        Plan {
            registry: &self.registry,
            bindings: Some(&self.bindings),
            hosts: &self.hosts,
            marker_path: self.marker(),
        }
    }

    fn run(&self, ssh: &Path, calls: &AtomicUsize) -> Outcome {
        diri_engine::legacy_remote::retire_legacy_remote_sessions(
            &self.plan(),
            &ssh_runner(ssh.to_path_buf(), calls),
        )
    }

    fn record(&self, id: &str) -> SessionRecord {
        self.registry
            .lock()
            .expect("registry")
            .records()
            .into_iter()
            .find(|record| record.id.0 == id)
            .unwrap_or_else(|| panic!("record {id} must survive the migration"))
    }

    fn log_exists(&self, id: &str) -> bool {
        self.logs_dir.join(format!("{id}.bin")).exists()
    }
}

// MARK: Tests

/// The whole point: after the upgrade the user can press Resume and land back
/// in the same Claude conversation on the new transport.
#[test]
fn a_legacy_record_is_marked_exited_and_stays_resumable() {
    let temp = tempfile::tempdir().expect("temp");
    let fake = FakeHost::new(temp.path(), Some(&["diri-4b99600f"]));
    let world = World::new(
        vec![
            record(
                "s_4b99600fd4f1",
                Some("forge"),
                Some("8f0b1d64-1111-4444-8888-aaaaaaaaaaaa"),
            ),
            // No conversation id: nothing to re-enter, and the migration must
            // not pretend otherwise.
            record("s_1122334455aa", Some("forge"), None),
        ],
        vec![host_entry("forge")],
    );
    let calls = AtomicUsize::new(0);
    let outcome = world.run(&fake.ssh, &calls);

    assert_eq!(
        outcome.migrated.len(),
        2,
        "both legacy records were rewritten"
    );

    let resumable = world.record("s_4b99600fd4f1");
    assert!(
        matches!(
            &resumable.status,
            SessionStatus::Exited(info) if info.reason == ExitReason::External
        ),
        "status must be exited, not a stale live status: {:?}",
        resumable.status
    );
    assert_eq!(
        resumable.resumability,
        Resumability::Resumable,
        "the conversation must still be re-enterable"
    );
    assert_eq!(
        resumable.agent_session_id.as_deref(),
        Some("8f0b1d64-1111-4444-8888-aaaaaaaaaaaa"),
        "the conversation id is what Resume needs — it must be retained"
    );
    assert_eq!(
        resumable.host.as_deref(),
        Some("forge"),
        "the session still belongs to that host"
    );
    assert!(
        world.log_exists("s_4b99600fd4f1"),
        "the output log must not be deleted"
    );

    let unresumable = world.record("s_1122334455aa");
    assert!(matches!(unresumable.status, SessionStatus::Exited(_)));
    assert_eq!(
        unresumable.resumability,
        Resumability::NotResumable,
        "a record with no conversation id must not be advertised as resumable"
    );

    // The rewrite survives a restart, which is what the user actually sees.
    let mut reloaded = Registry::new(engine(), world.root.join("state.json"));
    reloaded.load().expect("reload");
    let persisted = reloaded.record("s_4b99600fd4f1").expect("record persisted");
    assert!(matches!(persisted.status, SessionStatus::Exited(_)));
    assert_eq!(persisted.resumability, Resumability::Resumable);
}

/// The safety property. The old convention truncates the session id, so the
/// target is derived from a record diri already owns — and it goes out in
/// tmux's `=exact` form so neither a prefix collision nor a stranger's pane can
/// be caught by it.
#[test]
fn only_the_exact_pane_diri_named_is_killed() {
    let temp = tempfile::tempdir().expect("temp");
    let fake = FakeHost::new(
        temp.path(),
        Some(&[
            "diri-4b99600f",       // ours: a record points at it
            "diri-4b99600f-extra", // a prefix collision — tmux would match this
            "diri-aabbccdd",       // diri-shaped, but no record accounts for it
            "work",                // somebody's own session
        ]),
    );
    let world = World::new(
        vec![record(
            "s_4b99600fd4f1",
            Some("forge"),
            Some("conversation"),
        )],
        vec![host_entry("forge")],
    );
    let calls = AtomicUsize::new(0);
    let outcome = world.run(&fake.ssh, &calls);

    assert_eq!(
        legacy_pane_name("s_4b99600fd4f1"),
        "diri-4b99600f",
        "the recovered convention truncates the id; it does not hash it"
    );
    assert_eq!(outcome.panes_killed, vec!["diri-4b99600f".to_string()]);

    // What actually crossed the wire.
    let ssh_log = fake.ssh_log();
    assert!(
        ssh_log.contains("'diri-4b99600f'"),
        "the exact, shell-quoted target must be what ssh carried: {ssh_log}"
    );
    assert!(
        !ssh_log.contains("diri-4b99600f-extra"),
        "nothing may name the colliding session"
    );
    let tmux_log = fake.tmux_log();
    assert!(
        tmux_log.contains("kill-session -t =diri-4b99600f"),
        "tmux must be given its exact-match form, or it falls back to prefix \
         and fnmatch matching: {tmux_log}"
    );
    assert_eq!(
        tmux_log.matches("kill-session").count(),
        1,
        "one kill per legacy record, no sweeps: {tmux_log}"
    );

    let survivors = fake.live_sessions();
    assert!(!survivors.contains(&"diri-4b99600f".to_string()));
    for untouched in ["diri-4b99600f-extra", "diri-aabbccdd", "work"] {
        assert!(
            survivors.contains(&untouched.to_string()),
            "{untouched} was not diri's to kill"
        );
    }

    // The record-less `diri-*` pane is reported, never acted on. `work` is not
    // even reported: it is provably not a name this program generates.
    assert_eq!(
        outcome.orphan_panes.get("forge"),
        Some(&vec!["diri-aabbccdd".to_string()]),
        "record-less diri panes are surfaced for a human decision"
    );
    let summary = outcome.summary().expect("something happened");
    assert!(summary.contains("were NOT touched"), "{summary}");
}

/// A host that is off, wedged, or behind a VPN the user is not on. The
/// user-visible half must still land, and the janitorial half must come back
/// for it.
#[test]
fn an_unreachable_host_keeps_the_record_the_log_and_the_retry() {
    let world = World::new(
        vec![record(
            "s_4b99600fd4f1",
            Some("forge"),
            Some("conversation"),
        )],
        vec![host_entry("forge")],
    );
    let attempts = AtomicUsize::new(0);
    // What `run_shell` returns for a host that never answers.
    let unreachable = |_: Option<&HostEntry>, _: &str, _: Duration| -> Option<ShellOutput> {
        attempts.fetch_add(1, Ordering::SeqCst);
        None
    };

    let first =
        diri_engine::legacy_remote::retire_legacy_remote_sessions(&world.plan(), &unreachable);
    assert_eq!(first.migrated, vec!["s_4b99600fd4f1".to_string()]);
    assert_eq!(
        first.panes_deferred,
        vec!["diri-4b99600f".to_string()],
        "the pane is owed another try, not written off"
    );
    assert!(first.panes_killed.is_empty());

    let record = world.record("s_4b99600fd4f1");
    assert!(
        matches!(record.status, SessionStatus::Exited(_)),
        "the Resume button must work without the network"
    );
    assert_eq!(record.resumability, Resumability::Resumable);
    assert!(world.log_exists("s_4b99600fd4f1"), "the log survives");

    // Later launches keep trying the pane, but never rewrite the record again.
    let second =
        diri_engine::legacy_remote::retire_legacy_remote_sessions(&world.plan(), &unreachable);
    assert!(second.migrated.is_empty(), "the local half is exactly once");
    assert_eq!(second.panes_deferred, vec!["diri-4b99600f".to_string()]);
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "it retried");

    // ...and eventually stops, so a decommissioned host is not an ssh connect
    // timeout on every launch forever.
    let mut last = second;
    for _ in 0..8 {
        last =
            diri_engine::legacy_remote::retire_legacy_remote_sessions(&world.plan(), &unreachable);
    }
    assert!(
        last.panes_deferred.is_empty() && last.panes_given_up.is_empty(),
        "once abandoned there is no further remote work at all: {last:?}"
    );
    let settled = attempts.load(Ordering::SeqCst);
    assert!(
        (2..=5).contains(&settled),
        "attempts must be capped, saw {settled}"
    );
    assert!(
        matches!(
            world.record("s_4b99600fd4f1").status,
            SessionStatus::Exited(_)
        ),
        "the record is still there, still exited, still resumable"
    );
    assert!(world.log_exists("s_4b99600fd4f1"));
}

/// Every launch runs this. Only the first may do remote work.
#[test]
fn the_migration_does_not_run_twice() {
    let temp = tempfile::tempdir().expect("temp");
    let fake = FakeHost::new(temp.path(), Some(&["diri-4b99600f"]));
    let world = World::new(
        vec![record(
            "s_4b99600fd4f1",
            Some("forge"),
            Some("conversation"),
        )],
        vec![host_entry("forge")],
    );
    let calls = AtomicUsize::new(0);

    let first = world.run(&fake.ssh, &calls);
    assert_eq!(first.migrated.len(), 1);
    assert_eq!(first.panes_killed, vec!["diri-4b99600f".to_string()]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(world.marker().exists(), "the marker records what was done");

    let second = world.run(&fake.ssh, &calls);
    assert!(second.migrated.is_empty());
    assert!(second.panes_killed.is_empty());
    assert!(
        second.summary().is_none(),
        "a settled migration says nothing on stderr"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the second launch must not touch the host at all"
    );
    assert_eq!(
        fake.tmux_log().matches("kill-session").count(),
        1,
        "and certainly must not kill anything twice"
    );
}

/// A session the new transport owns. It has a Holder, it is reachable, and the
/// migration has no business anywhere near it.
#[test]
fn a_record_with_a_live_binding_is_untouched() {
    let temp = tempfile::tempdir().expect("temp");
    let fake = FakeHost::new(temp.path(), Some(&["diri-4b99600f"]));
    let world = World::new(
        vec![record(
            "s_4b99600fd4f1",
            Some("forge"),
            Some("conversation"),
        )],
        vec![host_entry("forge")],
    );
    world
        .bindings
        .save(&RemoteBinding {
            session_id: "s_4b99600fd4f1".into(),
            host_id: "forge".into(),
            helper_build_id: "build-1".into(),
            protocol: ProtocolVersion::CURRENT,
            session_token: SessionToken::new("0123456789abcdef").expect("token"),
            session_incarnation: "incarnation-1".into(),
            last_output_offset: 0,
        })
        .expect("save binding");

    let calls = AtomicUsize::new(0);
    let outcome = world.run(&fake.ssh, &calls);

    assert!(outcome.migrated.is_empty());
    assert!(outcome.summary().is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no ssh, no tmux, nothing");
    assert!(!world.marker().exists());

    let record = world.record("s_4b99600fd4f1");
    assert_eq!(
        record.status,
        SessionStatus::Working,
        "a Holder-owned session must keep its live status"
    );
    assert!(
        fake.live_sessions().contains(&"diri-4b99600f".to_string()),
        "and nothing on the host may be touched"
    );
}

/// A host with no tmux is a definitive answer — there is no pane there to leak
/// — so it settles rather than retrying forever.
#[test]
fn a_host_without_tmux_settles_without_error() {
    let temp = tempfile::tempdir().expect("temp");
    let fake = FakeHost::new(temp.path(), None);
    let world = World::new(
        vec![record(
            "s_4b99600fd4f1",
            Some("forge"),
            Some("conversation"),
        )],
        vec![host_entry("forge")],
    );
    let calls = AtomicUsize::new(0);

    let first = world.run(&fake.ssh, &calls);
    assert_eq!(first.migrated.len(), 1);
    assert!(first.panes_killed.is_empty());
    assert!(first.panes_deferred.is_empty());

    let second = world.run(&fake.ssh, &calls);
    assert!(second.migrated.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1, "settled on the first pass");
}

/// A reachable host whose ssh fails after a banner must not be read as "there
/// was no pane". The migration parses its own sentinel, never the exit status.
#[test]
fn a_login_banner_is_not_mistaken_for_an_answer() {
    let temp = tempfile::tempdir().expect("temp");
    let root = temp.path();
    let ssh = root.join("ssh-denied");
    write_executable(
        &ssh,
        "#!/bin/sh\nprintf 'Welcome to Ubuntu 24.04 LTS\\n'\n\
         printf 'Permission denied (publickey).\\n' >&2\nexit 255\n",
    );
    let world = World::new(
        vec![record(
            "s_4b99600fd4f1",
            Some("forge"),
            Some("conversation"),
        )],
        vec![host_entry("forge")],
    );
    let calls = AtomicUsize::new(0);
    let outcome = world.run(&ssh, &calls);

    assert_eq!(
        outcome.panes_deferred,
        vec!["diri-4b99600f".to_string()],
        "an ssh failure is inconclusive, so the pane is retried"
    );
    assert!(outcome.panes_absent.is_empty());
    assert!(matches!(
        world.record("s_4b99600fd4f1").status,
        SessionStatus::Exited(_)
    ));
}
