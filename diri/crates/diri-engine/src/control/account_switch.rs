//! One switch for every local account: pick a profile, every open tab of that
//! Agent follows it. Codex swaps the shared login file; Claude points its
//! credential store at the profile's slot. Both relaunch through here.
use super::*;
use diri_proto::{AgentAccountProfile, AgentKind, SwitchAccountResult};

/// An open tab whose relaunch is fully planned before anything is stopped.
pub(super) struct PreparedTab {
    pub(super) record: diri_proto::SessionRecord,
    pub(super) running: bool,
    pub(super) spec: crate::session::SessionSpec,
}

/// The argv of a sign-in tab: `command` run by the user's own shell as an
/// interactive login shell, exactly how Agent launches find their binary.
/// `zsh -lc` reads neither `.zshrc` nor fish/bash config, where nvm, mise
/// and most npm-global PATHs live, so `claude`/`codex` (or the `node` their
/// shebang names) was "command not found" and the tab died with 127.
/// Every argument is single-quoted; the command line is fixed by the caller.
pub(super) fn sign_in_argv(shell: Option<String>, command: &[String]) -> Vec<String> {
    let shell = shell
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| default_shell().to_owned());
    let line = command
        .iter()
        .map(|argument| crate::hosts::shell_quote(argument))
        .collect::<Vec<_>>()
        .join(" ");
    vec![
        shell,
        "-i".into(),
        "-l".into(),
        "-c".into(),
        format!("exec {line}"),
    ]
}

impl ControlServer {
    pub(super) fn account_switch_all(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: diri_proto::SwitchAccountParams = decode(params)?;
        let profile = self
            .accounts
            .lock()
            .map_err(poisoned)?
            .catalog()?
            .profiles
            .into_iter()
            .find(|profile| profile.id == p.account_profile_id)
            .ok_or_else(|| {
                ControlError::not_found("The selected account profile no longer exists")
            })?;
        if profile.host.is_some() {
            return Err(ControlError::bad_request(
                "Remote profiles are chosen per conversation; the shared switch applies to this Mac.",
            ));
        }
        let agent = profile.agent.clone();
        let result = match profile.agent.as_str() {
            AgentKind::CODEX_ID => self.switch_codex(profile),
            AgentKind::CLAUDE_CODE_ID => self.switch_claude(profile),
            _ => Err(ControlError::bad_request(
                "Account switching supports Claude Code and Codex",
            )),
        };
        if let Ok(value) = &result {
            record_switch(&agent, value);
        }
        result
    }

    /// Local, unarchived tabs of `kind` that are open in a Diri workspace.
    pub(super) fn open_local_tabs(
        &self,
        kind: &AgentKind,
    ) -> Result<Vec<diri_proto::SessionRecord>, ControlError> {
        let open = self.workspaces.snapshot()?.open_session_ids();
        let records = self.registry.lock().map_err(poisoned)?.records();
        Ok(records
            .into_iter()
            .filter(|r| {
                &r.kind == kind && r.host.is_none() && !r.is_archived() && open.contains(&r.id)
            })
            .collect())
    }

    /// Relaunch every prepared tab on `profile` once the login is installed.
    /// Sleeping tabs come back asleep, stopped tabs stay stopped, and a tab
    /// whose relaunch fails is reported rather than blocking the rest.
    pub(super) fn relaunch_switched(
        &self,
        prepared: Vec<PreparedTab>,
        profile: &AgentAccountProfile,
        installed: bool,
        result: &mut SwitchAccountResult,
    ) {
        for PreparedTab {
            record,
            running,
            spec,
        } in prepared
        {
            let change = (|| {
                let mut registry = self.registry.lock().map_err(poisoned)?;
                if installed {
                    registry.update_record(&record.id.0, |r| {
                        r.account_profile = Some(profile.clone());
                        r.hibernation = None;
                    });
                }
                registry.persist_now().map_err(io_control_error)?;
                if running
                    && (registry.get(&record.id.0).is_none()
                        || registry.record(&record.id.0).is_some_and(|r| {
                            matches!(r.status, diri_proto::SessionStatus::Exited(_))
                        }))
                {
                    registry.respawn(spec).map_err(io_control_error)?;
                    if let Some(sleep) = record.hibernation {
                        registry
                            .hibernate(&record.id.0, sleep.reason)
                            .map_err(io_control_error)?;
                    }
                }
                self.publish_updated(&registry, &record.id.0);
                registry.persist_now().map_err(io_control_error)?;
                registry
                    .record(&record.id.0)
                    .ok_or_else(|| ControlError::internal("Switched session vanished"))
            })();
            match change {
                Ok(r) if installed => result.switched.push(r),
                Ok(r) => result.unchanged.push(r.id),
                Err(e) => result.failures.push(diri_proto::AccountSwitchFailure {
                    session_id: record.id,
                    message: e.message,
                }),
            }
        }
    }
}

/// `account.switch`: how many open tabs moved to the new login, and each tab
/// that could not (codes only; the failure message stays in the reply).
fn record_switch(agent: &str, value: &Value) {
    let count = |key: &str| value.get(key).and_then(Value::as_array).map_or(0, Vec::len);
    diri_telemetry::event!(
        "account.switch",
        agent = diri_telemetry::id(agent),
        switched = count("switched"),
        unchanged = count("unchanged"),
        deferred = count("deferred"),
        failures = count("failures"),
        default_changed = value.get("defaultChanged").and_then(Value::as_bool),
    );
    for failure in value
        .get("failures")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        diri_telemetry::warn_event!(
            "account.switch_failed",
            agent = diri_telemetry::id(agent),
            session = failure
                .get("sessionId")
                .and_then(Value::as_str)
                .map(diri_telemetry::id),
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::sign_in_argv;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    /// An Agent CLI that only the interactive rc file puts on PATH, as nvm's
    /// installer does with `.zshrc`. The sign-in tab used to run `zsh -lc`,
    /// which never reads it, so the tab exited 127 within a second of opening.
    #[test]
    fn sign_in_finds_an_agent_installed_through_the_interactive_rc() {
        if !std::path::Path::new("/bin/zsh").exists() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("nvm-bin");
        std::fs::create_dir(&bin).unwrap();
        // Login startup files can add globally installed CLIs back to PATH even
        // after env_clear(). Never use a real CLI name in either launch below.
        let agent_name = format!(
            "diri-account-switch-fixture-{}",
            temp.path().file_name().unwrap().to_str().unwrap()
        );
        let agent = bin.join(&agent_name);
        std::fs::write(
            &agent,
            "#!/bin/sh\nprintf 'signed-in:%s:%s\\n' \"$*\" \"$STORE\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let store = temp.path().join("store with 'quote");
        std::fs::write(
            temp.path().join(".zshrc"),
            format!("export PATH=\"{}:$PATH\"\n", bin.display()),
        )
        .unwrap();
        let run = |argv: &[String]| {
            Command::new(&argv[0])
                .args(&argv[1..])
                .env_clear()
                .env("HOME", temp.path())
                .env("ZDOTDIR", temp.path())
                .env("PATH", "/usr/bin:/bin")
                .env("TERM", "dumb")
                .stdin(Stdio::null())
                .output()
                .unwrap()
        };
        let command = [
            "/usr/bin/env".to_owned(),
            format!("STORE={}", store.display()),
            agent_name.clone(),
            "auth".to_owned(),
            "login".to_owned(),
        ];

        let before = run(&[
            "/bin/zsh".into(),
            "-lc".into(),
            format!(
                "exec /usr/bin/env {} auth login",
                crate::hosts::shell_quote(&agent_name)
            ),
        ]);
        assert_eq!(
            before.status.code(),
            Some(127),
            "the old launch could not find it"
        );

        assert!(before.stdout.is_empty(), "{before:?}");
        assert!(
            String::from_utf8_lossy(&before.stderr).contains(&agent_name),
            "{before:?}"
        );

        let after = run(&sign_in_argv(Some("/bin/zsh".into()), &command));
        assert_eq!(after.status.code(), Some(0), "{after:?}");
        assert_eq!(
            after.stdout,
            format!("signed-in:auth login:{}\n", store.display()).as_bytes()
        );
        assert!(after.stderr.is_empty(), "{after:?}");
    }

    #[test]
    fn sign_in_uses_the_users_shell_and_quotes_every_argument() {
        let argv = sign_in_argv(
            Some("/opt/homebrew/bin/fish".into()),
            &["/usr/bin/env".into(), "X=it's".into(), "codex".into()],
        );
        assert_eq!(argv[..4], ["/opt/homebrew/bin/fish", "-i", "-l", "-c"]);
        assert_eq!(argv[4], r"exec '/usr/bin/env' 'X=it'\''s' 'codex'");
        assert_eq!(
            sign_in_argv(Some(String::new()), &[])[0],
            super::default_shell()
        );
    }
}
