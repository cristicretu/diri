//! Local Codex login slots. Conversation and tool state stays in one shared home.
use super::account_switch::PreparedTab;
use super::*;
use base64::Engine as _;
use diri_proto::{AgentAccountProfile, AgentKind, SwitchAccountResult};
use std::time::SystemTime;
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
};

const LIMIT: u64 = 1024 * 1024;
fn failure() -> ControlError {
    ControlError::bad_request(
        "Cannot safely read or save Codex login. Check file ownership, permissions and credential backend.",
    )
}
fn directory(path: &Path) -> Result<(), ControlError> {
    if !path.exists() {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| failure())?;
    }
    let m = fs::symlink_metadata(path).map_err(|_| failure())?;
    if !m.is_dir()
        || m.file_type().is_symlink()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o022 != 0
    {
        return Err(failure());
    }
    Ok(())
}
fn read(path: &Path) -> Result<Option<Vec<u8>>, ControlError> {
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(failure()),
    };
    let m = file.metadata().map_err(|_| failure())?;
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
        || m.len() > LIMIT
    {
        return Err(failure());
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| failure())?;
    if bytes.len() as u64 > LIMIT {
        return Err(failure());
    }
    Ok(Some(bytes))
}
fn login(path: &Path) -> Result<Vec<u8>, ControlError> {
    let bytes = read(path)?.ok_or_else(|| {
        ControlError::bad_request(
            "Sign in to this profile, or save its current login, before switching.",
        )
    })?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| failure())?;
    let nonempty = |v: &Value| v.as_str().is_some_and(|s| !s.is_empty());
    if !(nonempty(&v["OPENAI_API_KEY"])
        || (nonempty(&v["tokens"]["access_token"]) && nonempty(&v["tokens"]["refresh_token"])))
    {
        return Err(failure());
    }
    Ok(bytes)
}
fn identity(bytes: &[u8]) -> Option<(String, String)> {
    let v: Value = serde_json::from_slice(bytes).ok()?;
    let token = v["tokens"]["id_token"].as_str()?;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token.split('.').nth(1)?)
        .ok()?;
    let claims: Value = serde_json::from_slice(&payload).ok()?;
    Some((
        v["tokens"]["account_id"].as_str()?.into(),
        claims["sub"].as_str()?.into(),
    ))
}
fn write(path: &Path, bytes: &[u8]) -> Result<(), ControlError> {
    directory(path.parent().ok_or_else(failure)?)?;
    let expected = read(path)?;
    let temp = path.with_file_name(format!(".auth-{}.tmp", crate::inject::uuid_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)
            .map_err(|_| failure())?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| failure())?;
        if read(path)? != expected {
            return Err(ControlError::bad_request(
                "Codex login changed during switching. Retry.",
            ));
        }
        fs::rename(&temp, path).map_err(|_| failure())?;
        fs::File::open(path.parent().ok_or_else(failure)?)
            .and_then(|f| f.sync_all())
            .map_err(|_| failure())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
fn file_backend(home: &Path) -> Result<(), ControlError> {
    let path = home.join("config.toml");
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(failure()),
    };
    let meta = file.metadata().map_err(|_| failure())?;
    if !meta.is_file() || meta.len() > LIMIT {
        return Err(failure());
    }
    let mut text = String::new();
    file.take(LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(|_| failure())?;
    let config: toml_edit::DocumentMut = text.parse().map_err(|_| failure())?;
    if config
        .get("cli_auth_credentials_store")
        .is_some_and(|v| v.as_str() != Some("file"))
    {
        return Err(ControlError::bad_request(
            "Account switching currently requires Codex's file credential backend. The configured backend was not changed.",
        ));
    }
    Ok(())
}

impl ControlServer {
    fn codex_profile(&self, id: &str) -> Result<AgentAccountProfile, ControlError> {
        self.accounts
            .lock()
            .map_err(poisoned)?
            .resolve(Some(id), AgentKind::CODEX_ID, None)?
            .ok_or_else(failure)
    }
    fn codex_home(&self) -> Result<PathBuf, ControlError> {
        let home = PathBuf::from(std::env::var("HOME").map_err(|_| failure())?).join(".codex");
        directory(&home)?;
        file_backend(&home)?;
        Ok(home)
    }
    fn codex_slot(&self, id: &str) -> Result<PathBuf, ControlError> {
        if id.is_empty()
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(failure());
        }
        let root = self
            .socket_path
            .parent()
            .ok_or_else(failure)?
            .join("codex-logins");
        directory(&root)?;
        let slot = root.join(id);
        directory(&slot)?;
        Ok(slot)
    }
    fn ensure_login_finished(&self, slot: &Path) -> Result<(), ControlError> {
        if self
            .registry
            .lock()
            .map_err(poisoned)?
            .records()
            .iter()
            .any(|r| {
                Path::new(&r.cwd) == slot
                    && !matches!(r.status, diri_proto::SessionStatus::Exited(_))
            })
        {
            return Err(ControlError::bad_request(
                "Finish or close the sign-in tab first.",
            ));
        }
        Ok(())
    }
    pub(super) fn codex_account_capture(
        &self,
        params: Option<Value>,
    ) -> Result<Value, ControlError> {
        let p: diri_proto::AgentAccountId = decode(params)?;
        let mut profile = self.codex_profile(&p.id)?;
        let home = self.codex_home()?;
        let bytes = login(&home.join("auth.json"))?;
        let slot = self.codex_slot(&p.id)?;
        self.ensure_login_finished(&slot)?;
        write(&slot.join("auth.json"), &bytes)?;
        profile.config_home = home.to_string_lossy().into_owned();
        encode(&self.accounts.lock().map_err(poisoned)?.upsert(profile)?)
    }
    pub(super) fn codex_account_login(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: diri_proto::AgentAccountId = decode(params)?;
        let profile = self.codex_profile(&p.id)?;
        let slot = self.codex_slot(&p.id)?;
        let title = format!("Codex sign in · {}", profile.label);
        self.ensure_login_finished(&slot)?;
        let binary = self.resolve_local_agent_executable("codex", "codex")?;
        // Fixed script; profile paths and executable are positional arguments, never shell code.
        let script = "exec /usr/bin/env -u OPENAI_API_KEY -u OPENAI_BASE_URL -u CODEX_API_KEY CODEX_HOME=\"$1\" \"$2\" -c 'cli_auth_credentials_store=\"file\"' login";
        self.session_spawn(Some(
            json!({"kind": AgentKind::SHELL, "cwd": slot, "title": title,
            "argv": ["/bin/zsh", "-lc", script, "diri-codex-login", slot, binary]}),
        ))
    }
    pub(super) fn switch_codex(&self, profile: AgentAccountProfile) -> Result<Value, ControlError> {
        let home = self.codex_home()?;
        self.switch_codex_at(profile, home)
    }
    fn switch_codex_at(
        &self,
        mut profile: AgentAccountProfile,
        home: PathBuf,
    ) -> Result<Value, ControlError> {
        let slot = self.codex_slot(&profile.id)?;
        self.ensure_login_finished(&slot)?;
        // Import only the login from profiles created by the older isolated-home UI.
        if read(&slot.join("auth.json"))?.is_some() {
            login(&slot.join("auth.json"))?;
        } else {
            let expanded = profile
                .config_home
                .strip_prefix("~/")
                .map(|p| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(p))
                .unwrap_or_else(|| PathBuf::from(&profile.config_home));
            if expanded == home {
                return Err(ControlError::bad_request(
                    "Save current login for this profile, or sign in, before switching.",
                ));
            }
            directory(&expanded)?;
            file_backend(&expanded)?;
            let bytes = login(&expanded.join("auth.json"))?;
            write(&slot.join("auth.json"), &bytes)?;
        };
        let records = self.open_local_tabs(&AgentKind::CODEX)?;
        let mut result = SwitchAccountResult::default();
        let mut prepared = Vec::new();
        let mut guards = Vec::new();
        profile.config_home = home.to_string_lossy().into_owned();
        profile.is_default = true;
        // Threads already owned by a tab can never be handed to another one.
        let mut bound: std::collections::HashSet<String> = records
            .iter()
            .filter_map(|r| r.agent_session_id.clone())
            .collect();
        for mut record in records {
            if record
                .account_profile
                .as_ref()
                .is_some_and(|p| p.config_home != "~/.codex" && Path::new(&p.config_home) != home)
            {
                result.unchanged.push(record.id);
                continue;
            }
            if record.agent_session_id.as_deref().is_none_or(str::is_empty) {
                // No notify callback bound this tab yet (it never finished a
                // turn, or predates id binding). Codex wrote its rollout at
                // launch, so identify the thread from the launch itself.
                let launched_at = SystemTime::UNIX_EPOCH
                    + Duration::from_millis(record.created_at.0.max(0.0) as u64);
                if let Some((thread, path)) = crate::history::find_codex_thread_for_launch(
                    &home.join("sessions"),
                    &record.cwd,
                    launched_at,
                    &bound,
                ) {
                    let transcript = path.to_string_lossy().into_owned();
                    self.registry
                        .lock()
                        .map_err(poisoned)?
                        .update_record(&record.id.0, |r| {
                            r.agent_session_id = Some(thread.clone());
                            r.transcript_path = Some(transcript.clone());
                        });
                    bound.insert(thread.clone());
                    record.agent_session_id = Some(thread);
                    record.transcript_path = Some(transcript);
                }
            }
            let Some(native) = record.agent_session_id.clone() else {
                // Codex reads auth.json once at launch and keeps the login in
                // memory, so this tab is safe to leave alone: it continues on
                // the previous account until it is restarted. Never a reason
                // to refuse the switch for everything else.
                result.deferred.push(diri_proto::AccountSwitchFailure {
                    session_id: record.id,
                    message: "This tab's conversation could not be identified, so it keeps the previous login until it is restarted.".into(),
                });
                continue;
            };
            guards.push(account_handoff::SessionOperation::for_session(
                self,
                &record.id.0,
            )?);
            let registry = self.registry.lock().map_err(poisoned)?;
            let running = registry.get(&record.id.0).is_some()
                && !matches!(record.status, diri_proto::SessionStatus::Exited(_));
            let mut spec =
                self.resume_spec(&registry, &record.id.0, "codex", &record.cwd, Some(&native))?;
            crate::accounts::bind_pty(&mut profile.clone(), &mut spec.pty)?;
            prepared.push(PreparedTab {
                record,
                running,
                spec,
            });
        }
        // Validate current credentials before interrupting any process.
        let _ = read(&home.join("auth.json"))?;
        let installation = (|| -> Result<(), ControlError> {
            for tab in &prepared {
                if tab.running {
                    self.terminate_session_unlocked(&tab.record.id.0, Duration::from_secs(3))?;
                }
            }
            // Keep refreshed credentials for every saved slot matching this account identity.
            if let Some(current) = read(&home.join("auth.json"))? {
                write(
                    &slot
                        .parent()
                        .ok_or_else(failure)?
                        .join("previous-auth.json"),
                    &current,
                )?;
                let catalog = self.accounts.lock().map_err(poisoned)?.catalog()?;
                for other in catalog
                    .profiles
                    .iter()
                    .filter(|p| p.agent == "codex" && p.host.is_none())
                {
                    let path = self.codex_slot(&other.id)?.join("auth.json");
                    if let Some(saved) = read(&path)?
                        && identity(&current).is_some()
                        && identity(&saved) == identity(&current)
                    {
                        write(&path, &current)?;
                    }
                }
            }
            // Re-read: selecting the currently active account must retain its latest refresh.
            let target = login(&slot.join("auth.json"))?;
            let previous = read(&home.join("auth.json"))?;
            write(&home.join("auth.json"), &target)?;
            if let Err(error) = self
                .accounts
                .lock()
                .map_err(poisoned)?
                .upsert(profile.clone())
            {
                if let Some(previous) = &previous {
                    write(&home.join("auth.json"), previous)?;
                } else {
                    fs::remove_file(home.join("auth.json")).map_err(|_| failure())?;
                }
                return Err(error);
            }
            Ok(())
        })();
        let installed = installation.is_ok();
        result.default_changed = installed;
        if let Err(error) = installation {
            result.default_error = Some(format!(
                "Login switch failed: {}. Open conversations were resumed with the available login; any failed restarts can be retried with Resume.",
                error.message
            ));
        }
        self.relaunch_switched(prepared, &profile, installed, &mut result);
        encode(&result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn auth(account: &str, refresh: &str) -> Vec<u8> {
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"sub":"fixture-user"}"#);
        serde_json::to_vec(&json!({"tokens":{"account_id":account,"id_token":format!("x.{payload}.x"),"access_token":"fixture-access","refresh_token":refresh}})).unwrap()
    }
    #[test]
    fn login_files_are_private_atomic_and_reject_symlinks_and_malformed_auth() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("auth.json");
        write(&path, &auth("a", "old")).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        write(&path, &auth("b", "new")).unwrap();
        assert_eq!(login(&path).unwrap(), auth("b", "new"));
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(write(&link, b"bad").is_err());
        assert!(login(&link).is_err());
        write(&path, b"{}").unwrap();
        assert!(login(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read(&path).is_err());
    }
    #[test]
    fn login_is_a_separate_setup_session_and_cancel_does_not_switch_auth() {
        let tmp = tempfile::tempdir().unwrap();
        let server = super::super::tests::server(tmp.path());
        let home = tmp.path().join("shared");
        directory(&home).unwrap();
        write(&home.join("auth.json"), &auth("original", "current")).unwrap();
        let profile = AgentAccountProfile {
            id: "new".into(),
            label: "New".into(),
            agent: "codex".into(),
            host: None,
            config_home: home.to_string_lossy().into_owned(),
            is_default: false,
            login_store: None,
        };
        server
            .accounts
            .lock()
            .unwrap()
            .upsert(profile.clone())
            .unwrap();
        server
            .agent_catalog
            .lock()
            .unwrap()
            .configure(
                None,
                "codex",
                crate::agent_catalog::AgentPreference {
                    executable_path: Some("/usr/bin/false".into()),
                    show_in_quick_create: Some(true),
                },
            )
            .unwrap();
        let response = server
            .codex_account_login(Some(json!({"id":"new"})))
            .unwrap();
        let record: diri_proto::SessionRecord = serde_json::from_value(response.clone()).unwrap();
        assert_eq!(record.kind, AgentKind::SHELL);
        assert!(record.agent_session_id.is_none());
        assert!(record.account_profile.is_none());
        assert!(!response.to_string().contains("refresh_token"));
        server
            .session_kill(Some(json!({"sessionID":record.id})))
            .unwrap();
        assert!(server.switch_codex_at(profile, home.clone()).is_err());
        assert_eq!(
            login(&home.join("auth.json")).unwrap(),
            auth("original", "current")
        );
        assert!(
            read(&server.codex_slot("new").unwrap().join("auth.json"))
                .unwrap()
                .is_none()
        );
        assert!(!server.codex_slot("new").unwrap().join("sessions").exists());
    }

    #[test]
    fn non_file_backend_is_rejected_without_rewriting_configuration() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "cli_auth_credentials_store='keyring'\n").unwrap();
        assert!(file_backend(tmp.path()).is_err());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "cli_auth_credentials_store='keyring'\n"
        );
    }
    #[test]
    fn shared_switch_restarts_open_tabs_without_reading_history_or_touching_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let server = super::super::tests::server(tmp.path());
        let home = tmp.path().join("shared");
        directory(&home).unwrap();
        let executable = tmp.path().join("codex");
        fs::write(&executable, "#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        server
            .agent_catalog
            .lock()
            .unwrap()
            .configure(
                None,
                "codex",
                crate::agent_catalog::AgentPreference {
                    executable_path: Some(executable.to_string_lossy().into_owned()),
                    show_in_quick_create: Some(true),
                },
            )
            .unwrap();
        let profiles = ["one", "two"].map(|id| AgentAccountProfile {
            id: id.into(),
            label: id.into(),
            agent: "codex".into(),
            host: None,
            config_home: home.to_string_lossy().into_owned(),
            is_default: false,
            login_store: None,
        });
        for p in &profiles {
            server.accounts.lock().unwrap().upsert(p.clone()).unwrap();
            write(
                &server.codex_slot(&p.id).unwrap().join("auth.json"),
                &auth(&p.id, "saved"),
            )
            .unwrap();
        }
        write(&home.join("auth.json"), &auth("one", "refreshed")).unwrap();
        let config = "[mcp_servers.fixture]\nurl='https://example.test/mcp'\n";
        fs::write(home.join("config.toml"), config).unwrap();
        fs::write(home.join(".credentials.json"), b"tool-credentials").unwrap();
        // Deliberately invalid and huge: the switch must never parse or copy this file.
        let history = home.join("history.jsonl");
        fs::File::create(&history)
            .unwrap()
            .set_len(800 * 1024 * 1024)
            .unwrap();
        let before = fs::metadata(&history).unwrap();
        let mut sources = Vec::new();
        for i in 0..3 {
            let record: diri_proto::SessionRecord = serde_json::from_value(
                server
                    .session_spawn(Some(
                        json!({"kind":AgentKind::CODEX,"cwd":tmp.path(),"accountProfileId":"one"}),
                    ))
                    .unwrap(),
            )
            .unwrap();
            server
                .registry
                .lock()
                .unwrap()
                .update_record(&record.id.0, |r| {
                    r.agent_session_id = Some(format!("native-{i}"));
                    r.transcript_path = Some(history.to_string_lossy().into_owned());
                });
            if i == 1 {
                server
                    .registry
                    .lock()
                    .unwrap()
                    .hibernate(&record.id.0, diri_proto::HibernationReason::Manual)
                    .unwrap();
            }
            if i == 2 {
                server
                    .session_kill(Some(json!({"sessionID":record.id})))
                    .unwrap();
            }
            server.workspace_mutate(Some(json!({"expectedRevision":server.workspaces.snapshot().unwrap().revision,"mutation":{"type":"openProjectAgent","sessionId":record.id}}))).unwrap();
            sources.push(record);
        }
        let mut closed = super::super::tests::test_record("closed");
        closed.kind = AgentKind::CODEX;
        server
            .registry
            .lock()
            .unwrap()
            .insert_record(closed.clone());
        let result: SwitchAccountResult = serde_json::from_value(
            server
                .switch_codex_at(profiles[1].clone(), home.clone())
                .unwrap(),
        )
        .unwrap();
        assert!(result.default_changed);
        assert!(result.failures.is_empty(), "{:?}", result.failures);
        assert_eq!(result.switched.len(), 3);
        assert_eq!(
            login(&home.join("auth.json")).unwrap(),
            auth("two", "saved")
        );
        assert_eq!(
            login(&server.codex_slot("one").unwrap().join("auth.json")).unwrap(),
            auth("one", "refreshed")
        );
        assert_eq!(
            fs::read_to_string(home.join("config.toml")).unwrap(),
            config
        );
        assert_eq!(
            fs::read(home.join(".credentials.json")).unwrap(),
            b"tool-credentials"
        );
        let after = fs::metadata(&history).unwrap();
        assert_eq!(
            (before.ino(), before.mtime(), before.len()),
            (after.ino(), after.mtime(), after.len())
        );
        for (i, source) in sources.iter().enumerate() {
            let r = server
                .registry
                .lock()
                .unwrap()
                .record(&source.id.0)
                .unwrap();
            assert_eq!(r.agent_session_id, Some(format!("native-{i}")));
            assert_eq!(r.hibernation.is_some(), i == 1);
            assert_eq!(
                server.registry.lock().unwrap().get(&source.id.0).is_some(),
                i != 2
            );
        }
        assert_eq!(
            server
                .registry
                .lock()
                .unwrap()
                .record("closed")
                .unwrap()
                .account_profile,
            closed.account_profile
        );
        let result: SwitchAccountResult = serde_json::from_value(
            server
                .switch_codex_at(profiles[0].clone(), home.clone())
                .unwrap(),
        )
        .unwrap();
        assert!(result.default_changed);
        assert_eq!(
            login(&home.join("auth.json")).unwrap(),
            auth("one", "refreshed")
        );
        for source in sources {
            let _ = server.session_kill(Some(json!({"sessionID":source.id})));
        }
    }

    #[test]
    fn shared_switch_identifies_unbound_tabs_from_their_rollout_and_never_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let server = super::super::tests::server(tmp.path());
        let home = tmp.path().join("shared");
        directory(&home).unwrap();
        let executable = tmp.path().join("codex");
        fs::write(&executable, "#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        server
            .agent_catalog
            .lock()
            .unwrap()
            .configure(
                None,
                "codex",
                crate::agent_catalog::AgentPreference {
                    executable_path: Some(executable.to_string_lossy().into_owned()),
                    show_in_quick_create: Some(true),
                },
            )
            .unwrap();
        let profiles = ["one", "two"].map(|id| AgentAccountProfile {
            id: id.into(),
            label: id.into(),
            agent: "codex".into(),
            host: None,
            config_home: home.to_string_lossy().into_owned(),
            is_default: false,
            login_store: None,
        });
        for p in &profiles {
            server.accounts.lock().unwrap().upsert(p.clone()).unwrap();
            write(
                &server.codex_slot(&p.id).unwrap().join("auth.json"),
                &auth(&p.id, "saved"),
            )
            .unwrap();
        }
        write(&home.join("auth.json"), &auth("one", "current")).unwrap();
        let cwd = tmp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let spawn = || -> diri_proto::SessionRecord {
            let record: diri_proto::SessionRecord = serde_json::from_value(
                server
                    .session_spawn(Some(
                        json!({"kind":AgentKind::CODEX,"cwd":cwd,"accountProfileId":"one"}),
                    ))
                    .unwrap(),
            )
            .unwrap();
            server.workspace_mutate(Some(json!({"expectedRevision":server.workspaces.snapshot().unwrap().revision,"mutation":{"type":"openProjectAgent","sessionId":record.id}}))).unwrap();
            record
        };
        let identified = spawn();
        let orphan = spawn();
        for record in [&identified, &orphan] {
            assert!(
                server
                    .registry
                    .lock()
                    .unwrap()
                    .record(&record.id.0)
                    .unwrap()
                    .agent_session_id
                    .is_none(),
                "Codex does not pre-mint conversation ids"
            );
        }
        // Codex wrote these as the tabs started: the tab's own root thread, a
        // subagent rollout in the same directory, and a thread elsewhere.
        let day = home.join("sessions/2026/09/18");
        fs::create_dir_all(&day).unwrap();
        let rollout = |name: &str, meta: serde_json::Value| {
            fs::write(
                day.join(format!("rollout-2026-09-18T15-00-00-{name}.jsonl")),
                format!("{}\n", json!({"type":"session_meta","payload":meta})),
            )
            .unwrap();
        };
        rollout("thread-a", json!({"id":"thread-a","cwd":cwd}));
        rollout(
            "thread-sub",
            json!({"id":"thread-sub","cwd":cwd,"source":{"subagent":{"thread_spawn":{"parent_thread_id":"thread-a","depth":1}}}}),
        );
        rollout(
            "thread-elsewhere",
            json!({"id":"thread-elsewhere","cwd":tmp.path()}),
        );
        let result: SwitchAccountResult = serde_json::from_value(
            server
                .switch_codex_at(profiles[1].clone(), home.clone())
                .unwrap(),
        )
        .unwrap();
        assert!(result.failures.is_empty(), "{:?}", result.failures);
        assert!(result.default_changed, "{:?}", result.default_error);
        assert_eq!(
            login(&home.join("auth.json")).unwrap(),
            auth("two", "saved"),
            "an unidentified tab must not block the login swap"
        );
        assert_eq!(result.switched.len(), 1);
        assert_eq!(result.deferred.len(), 1);
        let switched = &result.switched[0];
        let deferred = &result.deferred[0];
        let bound_one = identified.id == switched.id && orphan.id == deferred.session_id;
        let bound_other = orphan.id == switched.id && identified.id == deferred.session_id;
        assert!(
            bound_one || bound_other,
            "exactly one tab claims the one matching rollout"
        );
        assert_eq!(switched.agent_session_id.as_deref(), Some("thread-a"));
        assert!(
            switched
                .transcript_path
                .as_deref()
                .is_some_and(|p| p.ends_with("rollout-2026-09-18T15-00-00-thread-a.jsonl"))
        );
        assert_eq!(
            switched.account_profile.as_ref().map(|p| p.id.as_str()),
            Some("two")
        );
        let untouched = server
            .registry
            .lock()
            .unwrap()
            .record(&deferred.session_id.0)
            .unwrap();
        assert!(untouched.agent_session_id.is_none());
        assert!(
            server
                .registry
                .lock()
                .unwrap()
                .get(&deferred.session_id.0)
                .is_some(),
            "a deferred tab keeps running on the previous login"
        );
        for record in [&identified, &orphan] {
            let _ = server.session_kill(Some(json!({"sessionID":record.id})));
        }
    }
}
