//! One-time retirement of remote sessions created by the pre-Holder transport.
//!
//! # Why this exists
//!
//! Before the Holder, a remote session was an `ssh -t` into a tmux pane on the
//! host. That transport — and with it `migrate::kill_remote_tmux` and
//! `remote_tmux_session_name` — is gone. What is NOT gone is a user's state
//! file: after upgrading they still hold `SessionRecord`s with `host: Some(..)`
//! and no [`RemoteBinding`], because no Holder ever owned them. Nothing in the
//! new Engine can reach those records, so they sit in the sidebar claiming to
//! be live forever, and the tmux server on the host keeps their agents running
//! indefinitely (measured on the author's box: panes with 17 days of uptime).
//!
//! Leaving that to "the user ssh's in and cleans up" is not a migration. This
//! module is the migration.
//!
//! # This is not a tmux fallback
//!
//! `AGENTS.md` forbids tmux as a transport, a feature flag, a migration path
//! for *running* sessions, or a runtime fallback. Nothing here starts, attaches
//! to, resumes, or reads a tmux pane. The only tmux verb used is
//! `kill-session`, aimed at a name this program itself generated in a previous
//! version, exactly once per record, and never again. It is a janitor for
//! litter we dropped, not a way to run anything. When every user has launched
//! the new Engine once, this file can be deleted whole and nothing else
//! changes.
//!
//! # Safety: whose pane is it
//!
//! The old naming convention (recovered verbatim from
//! `origin/main:crates/diri-engine/src/remote.rs`) is a pure truncation, not a
//! hash: strip the `s_` prefix from the persisted session id and take the first
//! eight characters, so `s_4b99600fd4f1` became the tmux session `diri-4b99600f`.
//! A target is therefore only ever derived from a record this Engine already
//! owns, and it is additionally required to match `diri-` plus lowercase hex —
//! a record id with an unexpected shape produces no target at all.
//!
//! Two further guards matter:
//!
//! * `kill-session -t` in tmux is *not* an exact match. tmux falls back to
//!   prefix and then fnmatch matching, so a bare `-t diri-4b99600f` could take
//!   out `diri-4b99600f-something` and a target containing `*` would be a
//!   wildcard. Every target here is sent as `=name`, tmux's documented
//!   exact-match form, and shell-quoted on the way out.
//! * Panes named `diri-*` that no record refers to are **inventoried and
//!   reported, never killed**. See [`Outcome::orphan_panes`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use diri_proto::{
    ExitInfo, ExitReason, HostEntry, HostsConfig, Resumability, SessionRecord, SessionStatus,
};
use serde::{Deserialize, Serialize};

use crate::hosts::{ShellOutput, shell_quote};
use crate::registry::Registry;
use crate::remote::binding::RemoteBindingStore;

/// How the module runs a shell command. Production passes
/// [`crate::hosts::run_shell`]; tests pass a fake ssh.
///
/// The lifetime is spelled out because a bare `dyn Trait` alias defaults to
/// `'static`, which would force every caller's recorder to be a global.
pub type ShellRun<'a> = dyn Fn(Option<&HostEntry>, &str, Duration) -> Option<ShellOutput> + 'a;

/// How many launches may attempt the remote half before it is abandoned.
///
/// See the module docs on [`retire_legacy_remote_sessions`] for why the local
/// half is never retried and the remote half is.
const MAX_PANE_ATTEMPTS: u32 = 5;

/// Every line the remote script prints is prefixed with this, so a host that
/// answers with a login banner, a MOTD, or an ssh error cannot be mistaken for
/// a definitive answer.
const SENTINEL: &str = "diri-legacy ";

const REMOTE_TIMEOUT: Duration = Duration::from_secs(30);

/// The tmux session name the pre-Holder transport derived from a session id.
///
/// Recovered verbatim from `origin/main:crates/diri-engine/src/remote.rs`, where
/// it was `remote_tmux_session_name`. It truncates; it does not hash. Keeping
/// the original doc-comment's reasoning: the name was derived from the
/// persisted session id so respawning the same diri session reattached its
/// remote tmux rather than starting a second agent beside it.
///
/// Named `legacy_pane_name` and not `legacy_tmux_*` on purpose — `AGENTS.md`
/// reserves that spelling for the deleted transport, and nothing here is a
/// transport.
pub fn legacy_pane_name(session_id: &str) -> String {
    let raw = session_id.strip_prefix("s_").unwrap_or(session_id);
    format!("diri-{}", raw.chars().take(8).collect::<String>())
}

/// Whether a name is one this program could have generated: the literal
/// `diri-` prefix plus one to eight lowercase hex digits and nothing else.
///
/// Tighter than a prefix match on purpose. A user's own `diri-notes` pane, or
/// anything else that merely starts with `diri-`, is not ours and is neither
/// killed nor reported.
fn is_diri_generated_name(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix("diri-") else {
        return false;
    };
    !suffix.is_empty()
        && suffix.len() <= 8
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

// MARK: Marker

/// The on-disk record of what this migration has already done.
///
/// A sidecar file rather than a field on `SessionRecord`: the record is a wire
/// type shared with the app and with the Swift-era state file, so a field for a
/// one-shot concern would outlive the concern in every record forever and would
/// have to be negotiated across crates. This file is deletable in one `rm` on
/// the day the migration is retired.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Marker {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    sessions: BTreeMap<String, SessionMark>,
    /// `diri-*` panes seen on a host that no record accounts for, keyed by host
    /// id. Reported, never acted on — see [`Outcome::orphan_panes`].
    #[serde(default)]
    orphan_panes: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionMark {
    /// Set the first time the record itself was rewritten. Its presence is what
    /// makes the local half exactly-once.
    record_migrated_at_millis: u64,
    /// `killed`, `absent`, `no-tmux`, or `gave-up`. `None` means the host has
    /// not yet given a definitive answer, so the pane is still owed a try.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pane: Option<String>,
    #[serde(default)]
    pane_attempts: u32,
}

impl Marker {
    fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Atomic, owner-only. A half-written marker would either redo the local
    /// half (harmless but noisy) or lose the record of a killed pane, so the
    /// rename is not optional.
    fn save(&mut self, path: &Path) {
        self.version = 1;
        let Ok(bytes) = serde_json::to_vec(self) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let temporary = path.with_extension("json.tmp");
        if std::fs::write(&temporary, &bytes).is_err() {
            let _ = std::fs::remove_file(&temporary);
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600));
        }
        if std::fs::rename(&temporary, path).is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

// MARK: Outcome

#[derive(Debug, Default)]
pub struct Outcome {
    /// Records rewritten from "live on a transport that no longer exists" to
    /// exited-and-resumable. Each id appears here only on the launch that
    /// actually rewrote it.
    pub migrated: Vec<String>,
    pub panes_killed: Vec<String>,
    /// The pane was already gone — the host answered, there was nothing there.
    pub panes_absent: Vec<String>,
    /// The host did not answer; these are owed another attempt next launch.
    pub panes_deferred: Vec<String>,
    /// The host has failed [`MAX_PANE_ATTEMPTS`] times; no further remote work
    /// will be done for these.
    pub panes_given_up: Vec<String>,
    /// `diri-*` panes with no matching record, by host id. **Never killed.**
    ///
    /// The Engine cannot prove these are its own to end. Two ordinary
    /// situations produce them and both are indistinguishable from litter:
    /// another machine still running the old build against the same shared
    /// host, and this machine's own state file having been wiped (a documented
    /// incident class in this project). An unattended startup sweep that kills
    /// them would, in the second case, destroy exactly the fleet the user is
    /// trying to recover. So they are counted, named, and persisted for a
    /// user-triggered cleanup to act on — not swept.
    pub orphan_panes: BTreeMap<String, Vec<String>>,
}

impl Outcome {
    fn is_empty(&self) -> bool {
        self.migrated.is_empty()
            && self.panes_killed.is_empty()
            && self.panes_absent.is_empty()
            && self.panes_deferred.is_empty()
            && self.panes_given_up.is_empty()
            && self.orphan_panes.is_empty()
    }

    /// A one-paragraph account for stderr, or `None` when there was nothing to
    /// do (the steady state after the first launch).
    pub fn summary(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        if !self.migrated.is_empty() {
            lines.push(format!(
                "diri-engine: migrated {} pre-Holder remote session(s) to exited+resumable: {:?}",
                self.migrated.len(),
                self.migrated
            ));
        }
        if !self.panes_killed.is_empty() {
            lines.push(format!(
                "diri-engine: retired {} orphaned remote pane(s) diri had created: {:?}",
                self.panes_killed.len(),
                self.panes_killed
            ));
        }
        if !self.panes_absent.is_empty() {
            lines.push(format!(
                "diri-engine: {} pre-Holder pane(s) were already gone",
                self.panes_absent.len()
            ));
        }
        if !self.panes_deferred.is_empty() {
            lines.push(format!(
                "diri-engine: {} pre-Holder pane(s) could not be reached; will retry next launch: {:?}",
                self.panes_deferred.len(),
                self.panes_deferred
            ));
        }
        if !self.panes_given_up.is_empty() {
            lines.push(format!(
                "diri-engine: giving up on {} unreachable pre-Holder pane(s) after {MAX_PANE_ATTEMPTS} attempts: {:?}",
                self.panes_given_up.len(),
                self.panes_given_up
            ));
        }
        for (host, panes) in &self.orphan_panes {
            lines.push(format!(
                "diri-engine: {} tmux pane(s) named diri-* on host {host} match no session record and were NOT touched: {:?}",
                panes.len(),
                panes
            ));
        }
        Some(lines.join("\n"))
    }
}

// MARK: The migration

/// Everything the migration needs, so the caller decides where state lives.
pub struct Plan<'a> {
    pub registry: &'a Mutex<Registry>,
    /// `None` disables the whole migration: without the binding store the
    /// Engine cannot prove a record has no Holder, and guessing would retire
    /// live sessions.
    pub bindings: Option<&'a RemoteBindingStore>,
    pub hosts: &'a HostsConfig,
    pub marker_path: PathBuf,
}

/// Retires pre-Holder remote sessions. Safe to call on every launch; does
/// remote work only for records it has not already finished.
///
/// # The local half and the remote half are marked separately
///
/// Rewriting the record — exited, conversation id kept, log kept — is what the
/// user sees, and it never depends on the network. It happens on the first
/// launch after the upgrade, whether or not the host is up, and is then never
/// repeated. A user whose laptop is off the VPN still gets a working Resume
/// button immediately.
///
/// Killing the leaked pane is janitorial and *does* depend on the network, so
/// it is retried. Marking it done on an unreachable host would permanently leak
/// the pane for anyone who happened to launch diri while away from the box —
/// the exact failure being fixed. Retrying forever would instead spend an ssh
/// connect timeout on every launch for a host that has been decommissioned. So
/// it retries until the host gives a definitive answer, up to
/// [`MAX_PANE_ATTEMPTS`] launches, and then gives up loudly with the pane named
/// in the log. Neither branch can strand a user: the user-visible state was
/// already correct after launch one.
pub fn retire_legacy_remote_sessions(plan: &Plan<'_>, run: &ShellRun<'_>) -> Outcome {
    let mut outcome = Outcome::default();
    let Some(bindings) = plan.bindings else {
        return outcome;
    };
    let Ok(bound) = bindings.load_all() else {
        // An unreadable binding store is not evidence of absence.
        return outcome;
    };
    let bound: BTreeSet<String> = bound
        .into_iter()
        .map(|binding| binding.session_id)
        .collect();

    let mut marker = Marker::load(&plan.marker_path);

    // Phase 1 — local, under the registry lock, no I/O beyond the state file.
    let Ok(mut registry) = plan.registry.lock() else {
        return outcome;
    };
    let records = registry.records();
    let legacy: Vec<(String, String)> = records
        .iter()
        .filter(|record| !bound.contains(&record.id.0))
        .filter_map(|record| record.host.clone().map(|host| (record.id.0.clone(), host)))
        .collect();
    // Every record's derived name, legacy or not, so the orphan report cannot
    // accuse a pane that some session on this host does account for.
    let accounted: BTreeSet<String> = records
        .iter()
        .filter(|record| record.host.is_some())
        .map(|record| legacy_pane_name(&record.id.0))
        .collect();
    drop(records);

    let mut rewrote_any = false;
    for (id, _) in &legacy {
        if marker.sessions.contains_key(id) {
            continue;
        }
        registry.update_record(id, retire_record);
        marker.sessions.insert(
            id.clone(),
            SessionMark {
                record_migrated_at_millis: now_millis(),
                pane: None,
                pane_attempts: 0,
            },
        );
        outcome.migrated.push(id.clone());
        rewrote_any = true;
    }
    if rewrote_any {
        // `persist` is trailing-edge: inside its debounce window it only marks
        // dirty, and the daemon starts inside that window. A migration that
        // ran but did not reach the disk would run again next launch and, more
        // to the point, would leave the sidebar lying until the flusher
        // happened to fire — so this one is forced through.
        let _ = registry.persist();
        let _ = registry.flush_dirty();
    }
    drop(registry);

    // The marker lands before any ssh so that a crash mid-sweep cannot rewrite
    // a record twice — and, more importantly, cannot lose the fact that the
    // user-visible half is already done.
    if rewrote_any {
        marker.save(&plan.marker_path);
    }

    // Phase 2 — remote, unlocked, grouped by host so nine records on one box
    // cost one connection instead of nine. Each record still contributes its
    // own explicit `kill-session` target.
    let mut by_host: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (id, host_id) in &legacy {
        let Some(mark) = marker.sessions.get(id) else {
            continue;
        };
        if mark.pane.is_some() {
            continue; // already settled — no remote work, ever again
        }
        let target = legacy_pane_name(id);
        if !is_diri_generated_name(&target) {
            // An id we would not have produced this name from. Settle it
            // locally rather than aiming an unverified string at tmux.
            outcome.panes_absent.push(target);
            if let Some(mark) = marker.sessions.get_mut(id) {
                mark.pane = Some("absent".into());
            }
            continue;
        }
        by_host
            .entry(host_id.clone())
            .or_default()
            .push((id.clone(), target));
    }

    let mut marker_dirty = !by_host.is_empty() || !outcome.panes_absent.is_empty();
    for (host_id, entries) in by_host {
        let Some(host) = plan.hosts.host(&host_id) else {
            // The host was removed from hosts.json. There is no way to reach
            // the pane and never will be; settle it rather than retrying.
            for (id, target) in entries {
                outcome.panes_given_up.push(target);
                if let Some(mark) = marker.sessions.get_mut(&id) {
                    mark.pane = Some("gave-up".into());
                }
            }
            continue;
        };
        let targets: Vec<String> = entries.iter().map(|(_, target)| target.clone()).collect();
        let sweep = sweep_host(host, &targets, run);
        match sweep {
            Some(sweep) => {
                for (id, target) in entries {
                    let state = if sweep.no_tmux {
                        "no-tmux"
                    } else if sweep.killed.contains(&target) {
                        outcome.panes_killed.push(target.clone());
                        "killed"
                    } else {
                        outcome.panes_absent.push(target.clone());
                        "absent"
                    };
                    if let Some(mark) = marker.sessions.get_mut(&id) {
                        mark.pane = Some(state.into());
                        mark.pane_attempts += 1;
                    }
                }
                let orphans: Vec<String> = sweep
                    .panes
                    .into_iter()
                    .filter(|pane| is_diri_generated_name(pane) && !accounted.contains(pane))
                    .collect();
                if !orphans.is_empty() {
                    marker.orphan_panes.insert(host_id.clone(), orphans.clone());
                    outcome.orphan_panes.insert(host_id.clone(), orphans);
                }
            }
            None => {
                for (id, target) in entries {
                    let attempts = marker
                        .sessions
                        .get(&id)
                        .map_or(0, |mark| mark.pane_attempts)
                        + 1;
                    if let Some(mark) = marker.sessions.get_mut(&id) {
                        mark.pane_attempts = attempts;
                        if attempts >= MAX_PANE_ATTEMPTS {
                            mark.pane = Some("gave-up".into());
                        }
                    }
                    if attempts >= MAX_PANE_ATTEMPTS {
                        outcome.panes_given_up.push(target);
                    } else {
                        outcome.panes_deferred.push(target);
                    }
                }
            }
        }
        marker_dirty = true;
    }
    if marker_dirty {
        marker.save(&plan.marker_path);
    }
    outcome
}

/// The record rewrite. Deliberately narrow: identity, conversation id,
/// transcript path, host, cwd and the output log are all left alone, because
/// they are what makes the conversation recoverable on the new transport.
fn retire_record(record: &mut SessionRecord) {
    record.status = SessionStatus::Exited(ExitInfo {
        // Not `Exited`: nothing reported an exit code. From this Engine's
        // point of view the process ended outside its control, which is what
        // `External` means.
        reason: ExitReason::External,
        code: None,
        signal: None,
    });
    // Stale liveness decorations from the old transport.
    record.needs_input = None;
    record.hibernation = None;
    record.memory_bytes = None;
    // `Live` is not a claim that the session is running — it means the agent
    // named its conversation while it was. `Registry::fold_live` turns that
    // into `Resumable` or `NotResumable` by asking whether the manifest can
    // re-enter that conversation id, which is the question Resume actually
    // asks. Setting it only when there IS a conversation id leaves a record
    // with nothing to resume into exactly as it was.
    if record.agent_session_id.is_some() {
        record.resumability = Resumability::Live;
    }
}

struct HostSweep {
    killed: Vec<String>,
    panes: Vec<String>,
    no_tmux: bool,
}

/// One bounded, non-interactive command per host. `None` means the host gave no
/// answer we can trust — unreachable, wedged, or an ssh failure — which is the
/// retry signal.
fn sweep_host(host: &HostEntry, targets: &[String], run: &ShellRun<'_>) -> Option<HostSweep> {
    let list = targets
        .iter()
        .map(|target| shell_quote(target))
        .collect::<Vec<_>>()
        .join(" ");
    // `-t "=$target"` is tmux's exact-match form. Without the `=`, tmux would
    // fall back to prefix and fnmatch matching against every session on the
    // box, which is precisely how a cleanup turns into someone else's outage.
    let command = format!(
        "if ! command -v tmux >/dev/null 2>&1; then printf 'diri-legacy no-tmux\\n'; exit 0; fi; \
         tmux list-sessions -F 'diri-legacy pane #{{session_name}}' 2>/dev/null || true; \
         for target in {list}; do \
         if tmux kill-session -t \"=$target\" >/dev/null 2>&1; then \
         printf 'diri-legacy killed %s\\n' \"$target\"; \
         else printf 'diri-legacy absent %s\\n' \"$target\"; fi; \
         done"
    );
    let result = run(Some(host), &command, REMOTE_TIMEOUT)?;
    parse_sweep(&result.stdout)
}

/// Parsed strictly off our own sentinel, never off the exit status: a login
/// banner, a MOTD, or `Permission denied` must not read as "there was no pane".
fn parse_sweep(stdout: &str) -> Option<HostSweep> {
    let mut sweep = HostSweep {
        killed: Vec::new(),
        panes: Vec::new(),
        no_tmux: false,
    };
    let mut saw_sentinel = false;
    for line in stdout.lines() {
        let Some(rest) = line.trim().strip_prefix(SENTINEL) else {
            continue;
        };
        saw_sentinel = true;
        match rest.split_once(' ') {
            Some(("pane", name)) => sweep.panes.push(name.to_string()),
            Some(("killed", name)) => sweep.killed.push(name.to_string()),
            Some(("absent", _)) => {}
            _ if rest == "no-tmux" => sweep.no_tmux = true,
            _ => {}
        }
    }
    saw_sentinel.then_some(sweep)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The convention has to survive this file, because the code that created
    /// these panes has been deleted. `s_4b99600fd4f1` is a real id shape.
    #[test]
    fn the_legacy_name_truncates_the_session_id_and_never_hashes_it() {
        assert_eq!(legacy_pane_name("s_4b99600fd4f1"), "diri-4b99600f");
        assert_eq!(legacy_pane_name("s_228ca6dbe1a3"), "diri-228ca6db");
        // Idempotent, and the `s_` prefix is stripped rather than counted.
        assert_eq!(
            legacy_pane_name("s_4b99600fd4f1"),
            legacy_pane_name("s_4b99600fd4f1")
        );
        assert!(!legacy_pane_name("s_4b99600fd4f1").contains("s_"));
    }

    #[test]
    fn only_names_this_program_could_have_generated_qualify() {
        assert!(is_diri_generated_name("diri-4b99600f"));
        assert!(is_diri_generated_name("diri-228ca6db"));
        // A user's own pane that merely starts with the prefix is not ours.
        assert!(!is_diri_generated_name("diri-notes"));
        assert!(!is_diri_generated_name("diri-"));
        assert!(!is_diri_generated_name("diri-4b99600f-extra"));
        assert!(!is_diri_generated_name("dirijor-1"));
        assert!(!is_diri_generated_name("diri-4B99600F"));
        assert!(!is_diri_generated_name("diri-*"));
    }

    #[test]
    fn a_host_that_says_nothing_we_recognize_is_not_a_definitive_answer() {
        assert!(parse_sweep("Welcome to Ubuntu 24.04\nLast login: today\n").is_none());
        assert!(parse_sweep("").is_none());
        let sweep = parse_sweep("diri-legacy no-tmux\n").expect("sentinel seen");
        assert!(sweep.no_tmux);
    }
}
