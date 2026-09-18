//! Local Claude Code logins that share one `~/.claude`.
//!
//! Claude Code keeps its OAuth credential in a store it derives from
//! `CLAUDE_SECURESTORAGE_CONFIG_DIR` (a macOS Keychain item named after the
//! path, or `<dir>/.credentials.json` elsewhere) while everything else --
//! conversations, MCP servers, settings, plugins -- stays in the config home.
//! Each profile therefore owns a private slot directory that only names its
//! credential store; Diri never copies tokens on a switch. Selecting a profile
//! relaunches the open Claude tabs with that store, resuming their existing
//! conversations in place.
use super::account_switch::PreparedTab;
use super::*;
use diri_proto::{AgentAccountProfile, AgentKind, SwitchAccountResult};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    process::{Command, Stdio},
    time::{Instant, SystemTime},
};

const LIMIT: u64 = 1024 * 1024;
const SECURITY: &str = "/usr/bin/security";
/// Claude Code's active OAuth item for the default store, and the prefix it
/// derives per credential directory (`Claude Code-credentials-<sha256[..8]>`).
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const NOT_FOUND: i32 = 44;

fn failure() -> ControlError {
    ControlError::bad_request(
        "Cannot safely read or save the Claude login. Check file ownership and permissions.",
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

fn read_private(path: &Path) -> Result<Option<Vec<u8>>, ControlError> {
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
    if !m.is_file() || m.uid() != unsafe { libc::geteuid() } || m.len() > LIMIT {
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

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), ControlError> {
    directory(path.parent().ok_or_else(failure)?)?;
    let temp = path.with_file_name(format!(".write-{}.tmp", crate::inject::uuid_v4()));
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
        fs::rename(&temp, path).map_err(|_| failure())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

/// The Keychain service Claude Code derives for a credential directory: the
/// raw path string hashed as Claude hashes it. Diri passes exactly this string
/// in the environment, so the two derivations agree.
pub(crate) fn keychain_service(store: &str) -> String {
    let digest = Sha256::digest(store.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{KEYCHAIN_SERVICE}-{}", &hex[..8])
}

fn keychain_account() -> String {
    std::env::var("USER")
        .ok()
        .filter(|user| !user.is_empty())
        .unwrap_or_else(|| "claude-code-user".to_owned())
}

/// The credential directory a store-less launch (the default store) uses.
fn default_store(home: &Path) -> PathBuf {
    home.to_path_buf()
}

#[derive(Clone, Copy)]
enum Store<'a> {
    Default,
    Slot(&'a str),
}

impl Store<'_> {
    fn service(self) -> String {
        match self {
            Store::Default => KEYCHAIN_SERVICE.to_owned(),
            Store::Slot(path) => keychain_service(path),
        }
    }
    fn file(self, home: &Path) -> PathBuf {
        match self {
            Store::Default => default_store(home).join(".credentials.json"),
            Store::Slot(path) => Path::new(path).join(".credentials.json"),
        }
    }
}

fn security(args: &[&str], stdin: Option<&str>) -> Result<(i32, String), ControlError> {
    let mut command = Command::new(SECURITY);
    command
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| failure())?;
    if let Some(input) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        pipe.write_all(input.as_bytes()).map_err(|_| failure())?;
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(status) = child.try_wait().map_err(|_| failure())? {
            let mut out = String::new();
            if let Some(mut stdout) = child.stdout.take() {
                let _ = stdout.read_to_string(&mut out);
            }
            return Ok((status.code().unwrap_or(1), out));
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err(ControlError::bad_request(
                "The Keychain did not answer. Unlock the login keychain and retry.",
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Whether `store` holds a login, without reading any secret. Claude Code
/// falls back to the credentials file when the Keychain is unavailable, so a
/// file counts on every platform; on macOS the Keychain item is the norm.
fn has_login(store: Store<'_>, home: &Path) -> Result<bool, ControlError> {
    if read_private(&store.file(home))?.is_some() {
        return Ok(true);
    }
    if cfg!(target_os = "macos") {
        let (code, _) = security(
            &[
                "find-generic-password",
                "-a",
                &keychain_account(),
                "-s",
                &store.service(),
            ],
            None,
        )?;
        return match code {
            0 => Ok(true),
            NOT_FOUND => Ok(false),
            _ => Err(ControlError::bad_request(
                "The Keychain refused the lookup. Unlock the login keychain and retry.",
            )),
        };
    }
    Ok(false)
}

/// Copy the active login of `from` into `to`. On macOS the secret moves
/// between Keychain items through `security -i` so it never appears in argv;
/// elsewhere the credentials file is copied with owner-only permissions.
fn copy_login(from: Store<'_>, to: Store<'_>, home: &Path) -> Result<(), ControlError> {
    if cfg!(target_os = "macos") {
        let account = keychain_account();
        let (code, secret) = security(
            &[
                "find-generic-password",
                "-a",
                &account,
                "-s",
                &from.service(),
                "-w",
            ],
            None,
        )?;
        if code != 0 {
            return Err(ControlError::bad_request(
                "No Claude login is active on this Mac. Sign in with Open Agent first.",
            ));
        }
        let secret = secret.strip_suffix('\n').unwrap_or(&secret);
        if secret.is_empty() || !secret.starts_with('{') {
            return Err(failure());
        }
        let hex: String = secret.bytes().map(|b| format!("{b:02x}")).collect();
        let quote =
            |value: &str| format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""));
        let line = format!(
            "add-generic-password -U -a {} -s {} -X {hex}\n",
            quote(&account),
            quote(&to.service())
        );
        let (code, _) = if line.len() <= 4096 - 64 {
            security(&["-i"], Some(&line))?
        } else {
            security(
                &[
                    "add-generic-password",
                    "-U",
                    "-a",
                    &account,
                    "-s",
                    &to.service(),
                    "-X",
                    &hex,
                ],
                None,
            )?
        };
        if code != 0 {
            return Err(ControlError::bad_request(
                "The Keychain refused to save the login. Unlock the login keychain and retry.",
            ));
        }
        return Ok(());
    }
    let bytes = read_private(&from.file(home))?.ok_or_else(|| {
        ControlError::bad_request(
            "No Claude login is active on this machine. Sign in with Open Agent first.",
        )
    })?;
    write_private(&to.file(home), &bytes)
}

// MARK: Account identity shown by Claude's /status

/// `~/.claude.json` carries the signed-in account's identity for display.
/// Claude writes it at login, so after a switch it names the last account
/// that logged in, not the one now in use. Keep a copy per slot and swap it
/// alongside the login so the UI tells the truth. Cosmetic: never fails a
/// switch.
fn global_config_path(home: &Path) -> PathBuf {
    home.parent()
        .map_or_else(|| PathBuf::from(".claude.json"), |p| p.join(".claude.json"))
}

fn read_global_config(home: &Path) -> Option<serde_json::Map<String, Value>> {
    let path = global_config_path(home);
    let bytes = read_private(&path).ok()??;
    match serde_json::from_slice::<Value>(&bytes).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// Claude Code guards `~/.claude.json` with a `proper-lockfile` directory
/// lock beside it (stale after 10s, touched every 5s). Take it the same way.
fn with_global_config_lock<T>(home: &Path, f: impl FnOnce() -> T) -> Option<T> {
    let lock = global_config_path(home).with_extension("json.lock");
    let start = Instant::now();
    loop {
        match fs::create_dir(&lock) {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Ok(m) = fs::metadata(&lock)
                    && m.modified()
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .is_some_and(|age| age > Duration::from_secs(10))
                {
                    let _ = fs::remove_dir(&lock);
                    continue;
                }
                if start.elapsed() > Duration::from_secs(6) {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(_) => return None,
        }
    }
    let result = f();
    let _ = fs::remove_dir(&lock);
    Some(result)
}

fn snapshot_path(store: &str) -> PathBuf {
    Path::new(store).join("oauth-account.json")
}

fn snapshot_identity(home: &Path, store: &str) {
    if let Some(config) = read_global_config(home)
        && let Some(account) = config.get("oauthAccount").filter(|v| v.is_object())
        && let Ok(bytes) = serde_json::to_vec_pretty(account)
    {
        let _ = write_private(&snapshot_path(store), &bytes);
    }
}

fn install_identity(home: &Path, account: Value) {
    let _ = with_global_config_lock(home, || {
        let Some(mut config) = read_global_config(home) else {
            return;
        };
        if config.get("oauthAccount") == Some(&account) {
            return;
        }
        config.insert("oauthAccount".into(), account);
        if let Ok(bytes) = serde_json::to_vec_pretty(&Value::Object(config)) {
            let _ = write_private(&global_config_path(home), &bytes);
        }
    });
}

impl ControlServer {
    fn claude_profile(&self, id: &str) -> Result<AgentAccountProfile, ControlError> {
        self.accounts
            .lock()
            .map_err(poisoned)?
            .resolve(Some(id), AgentKind::CLAUDE_CODE_ID, None)?
            .ok_or_else(failure)
    }

    fn claude_home(&self) -> Result<PathBuf, ControlError> {
        let home = PathBuf::from(std::env::var("HOME").map_err(|_| failure())?).join(".claude");
        directory(&home)?;
        Ok(home)
    }

    fn claude_slot(&self, id: &str) -> Result<PathBuf, ControlError> {
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
            .join("claude-logins");
        directory(&root)?;
        let slot = root.join(id);
        directory(&slot)?;
        Ok(slot)
    }

    fn ensure_claude_login_finished(&self, slot: &Path) -> Result<(), ControlError> {
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

    /// Bind the profile to its private store and the shared home, durably,
    /// so a login that lands there is usable by the next switch.
    fn adopt_claude_slot(
        &self,
        mut profile: AgentAccountProfile,
        home: &Path,
        slot: &Path,
    ) -> Result<AgentAccountProfile, ControlError> {
        profile.config_home = home.to_string_lossy().into_owned();
        profile.login_store = Some(slot.to_string_lossy().into_owned());
        self.accounts
            .lock()
            .map_err(poisoned)?
            .upsert(profile.clone())?;
        Ok(profile)
    }

    /// Remember the login currently active in the shared home as this profile.
    pub(super) fn claude_account_capture(
        &self,
        params: Option<Value>,
    ) -> Result<Value, ControlError> {
        let p: diri_proto::AgentAccountId = decode(params)?;
        let profile = self.claude_profile(&p.id)?;
        let home = self.claude_home()?;
        let slot = self.claude_slot(&p.id)?;
        self.ensure_claude_login_finished(&slot)?;
        let store = slot.to_string_lossy().into_owned();
        // The active login lives in whichever store the current default
        // profile uses; a machine that never switched uses the default store.
        let current = self.current_claude_store()?;
        let from = current.as_deref().map_or(Store::Default, Store::Slot);
        if current.as_deref() != Some(store.as_str()) {
            copy_login(from, Store::Slot(&store), &home)?;
        }
        snapshot_identity(&home, &store);
        self.adopt_claude_slot(profile, &home, &slot)?;
        encode(&self.accounts.lock().map_err(poisoned)?.catalog()?)
    }

    /// Open a sign-in tab whose login lands in this profile's own store. The
    /// shared login is untouched until the profile is selected.
    pub(super) fn claude_account_login(
        &self,
        params: Option<Value>,
    ) -> Result<Value, ControlError> {
        let p: diri_proto::AgentAccountId = decode(params)?;
        let profile = self.claude_profile(&p.id)?;
        let home = self.claude_home()?;
        let slot = self.claude_slot(&p.id)?;
        self.ensure_claude_login_finished(&slot)?;
        let profile = self.adopt_claude_slot(profile, &home, &slot)?;
        let binary = self.resolve_local_agent_executable("claude-code", "claude")?;
        let title = format!("Claude sign in · {}", profile.label);
        // Fixed script; the store path and executable are positional arguments, never shell code.
        let script = "exec /usr/bin/env -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN -u CLAUDE_CODE_OAUTH_TOKEN -u CLAUDE_CONFIG_DIR CLAUDE_SECURESTORAGE_CONFIG_DIR=\"$1\" \"$2\" auth login";
        self.session_spawn(Some(
            json!({"kind": AgentKind::SHELL, "cwd": slot, "title": title,
            "argv": ["/bin/zsh", "-lc", script, "diri-claude-login", slot, binary]}),
        ))
    }

    /// The store the default local Claude profile launches with, if any.
    fn current_claude_store(&self) -> Result<Option<String>, ControlError> {
        Ok(self
            .accounts
            .lock()
            .map_err(poisoned)?
            .catalog()?
            .profiles
            .into_iter()
            .find(|p| p.is_default && p.agent == AgentKind::CLAUDE_CODE_ID && p.host.is_none())
            .and_then(|p| p.login_store))
    }

    /// Whether the conversation has a transcript Claude can resume; a tab
    /// that never sent a message must relaunch fresh, keeping its id.
    fn claude_transcript_exists(
        record: &diri_proto::SessionRecord,
        home: &Path,
        conversation: &str,
    ) -> bool {
        let nonempty = |path: &Path| fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0);
        if let Some(path) = &record.transcript_path
            && nonempty(Path::new(path))
        {
            return true;
        }
        nonempty(
            &home
                .join("projects")
                .join(crate::inject::claude_project_slug(&record.cwd))
                .join(format!("{conversation}.jsonl")),
        )
    }

    pub(super) fn switch_claude(
        &self,
        profile: AgentAccountProfile,
    ) -> Result<Value, ControlError> {
        let home = self.claude_home()?;
        self.switch_claude_at(profile, home)
    }

    fn switch_claude_at(
        &self,
        mut profile: AgentAccountProfile,
        home: PathBuf,
    ) -> Result<Value, ControlError> {
        let store = profile.login_store.clone().ok_or_else(|| {
            ControlError::bad_request(
                "Sign in to this profile, or save its current login, before switching.",
            )
        })?;
        self.ensure_claude_login_finished(Path::new(&store))?;
        if !has_login(Store::Slot(&store), &home)? {
            return Err(ControlError::bad_request(
                "Sign in to this profile, or save its current login, before switching.",
            ));
        }
        let previous = self.current_claude_store()?;
        let records = self.open_local_tabs(&AgentKind::CLAUDE_CODE)?;
        let mut result = SwitchAccountResult::default();
        let mut prepared = Vec::new();
        let mut guards = Vec::new();
        profile.config_home = home.to_string_lossy().into_owned();
        profile.is_default = true;
        for record in records {
            if record.account_profile.as_ref().is_some_and(|p| {
                p.login_store.is_none()
                    && p.config_home != "~/.claude"
                    && Path::new(&p.config_home) != home
            }) {
                // An isolated-home profile from the older design keeps its own
                // conversations; it is not part of the shared login.
                result.unchanged.push(record.id);
                continue;
            }
            let Some(conversation) = record.agent_session_id.clone().filter(|s| !s.is_empty())
            else {
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
            let mut spec = if Self::claude_transcript_exists(&record, &home, &conversation) {
                self.resume_spec(
                    &registry,
                    &record.id.0,
                    AgentKind::CLAUDE_CODE_ID,
                    &record.cwd,
                    Some(&conversation),
                )?
            } else {
                self.fresh_spec(
                    &registry,
                    &record.id.0,
                    AgentKind::CLAUDE_CODE_ID,
                    &record.cwd,
                    Some(&conversation),
                )?
            };
            crate::accounts::bind_pty(&mut profile.clone(), &mut spec.pty)?;
            prepared.push(PreparedTab {
                record,
                running,
                spec,
            });
        }
        let installation = (|| -> Result<(), ControlError> {
            for tab in &prepared {
                if tab.running {
                    self.terminate_session_unlocked(&tab.record.id.0, Duration::from_secs(3))?;
                }
            }
            // The identity Claude shows follows the login: keep the outgoing
            // account's copy, then present the incoming one.
            if let Some(previous) = &previous
                && previous != &store
            {
                snapshot_identity(&home, previous);
            }
            if let Some(account) = read_private(&snapshot_path(&store))
                .ok()
                .flatten()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .filter(Value::is_object)
            {
                install_identity(&home, account);
            } else if let Some(account) = self.claude_identity_from_status(&store, &home) {
                install_identity(&home, account);
            }
            self.accounts
                .lock()
                .map_err(poisoned)?
                .upsert(profile.clone())?;
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

    /// Ask Claude which account a store holds, merged over the current display
    /// identity so fields Diri does not know are kept. Best effort.
    fn claude_identity_from_status(&self, store: &str, home: &Path) -> Option<Value> {
        let binary = self
            .resolve_local_agent_executable("claude-code", "claude")
            .ok()?;
        let mut child = Command::new(binary)
            .args(["auth", "status", "--json"])
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
            .env("CLAUDE_SECURESTORAGE_CONFIG_DIR", store)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().ok()? {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        if !status.success() {
            return None;
        }
        let mut out = String::new();
        child.stdout.take()?.read_to_string(&mut out).ok()?;
        let reported: Value = serde_json::from_str(&out).ok()?;
        if reported.get("loggedIn") != Some(&Value::Bool(true)) {
            return None;
        }
        let email = reported.get("email")?.as_str()?.to_owned();
        let mut account = read_global_config(home)
            .and_then(|c| c.get("oauthAccount").cloned())
            .and_then(|v| match v {
                Value::Object(map) => Some(map),
                _ => None,
            })
            .unwrap_or_default();
        account.insert("emailAddress".into(), Value::String(email));
        for (from, to) in [
            ("orgId", "organizationUuid"),
            ("orgName", "organizationName"),
        ] {
            if let Some(value) = reported.get(from).filter(|v| v.is_string()) {
                account.insert(to.into(), value.clone());
            }
        }
        Some(Value::Object(account))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn keychain_service_matches_claude_derivation() {
        // sha256("/tmp/slot")[..8] as Claude Code computes it for the raw string.
        let digest = Sha256::digest(b"/tmp/slot");
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            keychain_service("/tmp/slot"),
            format!("Claude Code-credentials-{}", &hex[..8])
        );
        assert_ne!(
            keychain_service("/tmp/slot"),
            keychain_service("/tmp/slot/")
        );
    }

    #[test]
    fn identity_snapshot_and_install_round_trip_under_the_config_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(".claude");
        directory(&home).unwrap();
        write_private(
            &global_config_path(&home),
            br#"{"oauthAccount":{"emailAddress":"a@example.test","accountUuid":"a"},"projects":{"/x":{}}}"#,
        )
        .unwrap();
        let slot = tmp.path().join("slot");
        directory(&slot).unwrap();
        snapshot_identity(&home, slot.to_str().unwrap());
        let saved: Value = serde_json::from_slice(
            &read_private(&snapshot_path(slot.to_str().unwrap()))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(saved["emailAddress"], "a@example.test");
        install_identity(&home, serde_json::json!({"emailAddress":"b@example.test"}));
        let config = read_global_config(&home).unwrap();
        assert_eq!(config["oauthAccount"]["emailAddress"], "b@example.test");
        assert!(config.contains_key("projects"), "other keys are preserved");
        assert_eq!(
            fs::metadata(global_config_path(&home))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(
            !global_config_path(&home)
                .with_extension("json.lock")
                .exists()
        );
        // A live holder blocks the cosmetic update instead of racing it.
        fs::create_dir(global_config_path(&home).with_extension("json.lock")).unwrap();
        install_identity(&home, serde_json::json!({"emailAddress":"c@example.test"}));
        assert_eq!(
            read_global_config(&home).unwrap()["oauthAccount"]["emailAddress"],
            "b@example.test"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn file_backed_login_copies_into_the_slot_with_private_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(".claude");
        directory(&home).unwrap();
        write_private(&home.join(".credentials.json"), b"{\"claudeAiOauth\":{}}").unwrap();
        let slot = tmp.path().join("slot");
        assert!(!has_login(Store::Slot(slot.to_str().unwrap()), &home).unwrap());
        copy_login(Store::Default, Store::Slot(slot.to_str().unwrap()), &home).unwrap();
        assert!(has_login(Store::Slot(slot.to_str().unwrap()), &home).unwrap());
        assert_eq!(
            fs::metadata(slot.join(".credentials.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn shared_claude_switch_relaunches_open_tabs_with_the_profile_store() {
        let tmp = tempfile::tempdir().unwrap();
        let server = super::super::tests::server(tmp.path());
        let home = tmp.path().join(".claude");
        directory(&home).unwrap();
        // Claude's display identity lives beside the config home.
        write_private(
            &global_config_path(&home),
            br#"{"oauthAccount":{"emailAddress":"one@example.test","accountUuid":"acct-1","organizationName":"One"},"mcpServers":{"fixture":{}}}"#,
        )
        .unwrap();
        let executable = tmp.path().join("claude");
        fs::write(
            &executable,
            "#!/bin/sh\nif [ \"$1\" = auth ]; then printf '%s' '{\"loggedIn\":true,\"email\":\"two@example.test\",\"orgId\":\"org-2\",\"orgName\":\"Two\"}'; exit 0; fi\n{ printf 'launch\\n'; printf '%s\\n' \"$@\"; printf 'config-dir=%s\\n' \"${CLAUDE_CONFIG_DIR:-unset}\"; } >> \"$CLAUDE_SECURESTORAGE_CONFIG_DIR/launches\"\nexec /bin/sleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        server
            .agent_catalog
            .lock()
            .unwrap()
            .configure(
                None,
                "claude-code",
                crate::agent_catalog::AgentPreference {
                    executable_path: Some(executable.to_string_lossy().into_owned()),
                    show_in_quick_create: Some(true),
                },
            )
            .unwrap();
        let slots = ["one", "two"].map(|id| tmp.path().join("claude-logins").join(id));
        let profiles = ["one", "two"].map(|id| AgentAccountProfile {
            id: id.into(),
            label: id.into(),
            agent: "claude-code".into(),
            host: None,
            config_home: home.to_string_lossy().into_owned(),
            is_default: id == "one",
            login_store: Some(
                tmp.path()
                    .join("claude-logins")
                    .join(id)
                    .to_string_lossy()
                    .into_owned(),
            ),
        });
        for (profile, slot) in profiles.iter().zip(&slots) {
            server
                .accounts
                .lock()
                .unwrap()
                .upsert(profile.clone())
                .unwrap();
            fs::create_dir_all(slot.parent().unwrap()).unwrap();
            directory(slot).unwrap();
            write_private(&slot.join(".credentials.json"), b"{\"claudeAiOauth\":{}}").unwrap();
        }
        let cwd = tmp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let spawn = || -> diri_proto::SessionRecord {
            let record: diri_proto::SessionRecord = serde_json::from_value(
                server
                    .session_spawn(Some(
                        json!({"kind":AgentKind::CLAUDE_CODE,"cwd":cwd,"accountProfileId":"one"}),
                    ))
                    .unwrap(),
            )
            .unwrap();
            server.workspace_mutate(Some(json!({"expectedRevision":server.workspaces.snapshot().unwrap().revision,"mutation":{"type":"openProjectAgent","sessionId":record.id}}))).unwrap();
            record
        };
        let resumable = spawn();
        let fresh = spawn();
        let conversation = |record: &diri_proto::SessionRecord| {
            server
                .registry
                .lock()
                .unwrap()
                .record(&record.id.0)
                .unwrap()
                .agent_session_id
                .expect("Claude mints its conversation id at launch")
        };
        let transcripts = home
            .join("projects")
            .join(crate::inject::claude_project_slug(&cwd.to_string_lossy()));
        fs::create_dir_all(&transcripts).unwrap();
        fs::write(
            transcripts.join(format!("{}.jsonl", conversation(&resumable))),
            "{\"type\":\"user\"}\n",
        )
        .unwrap();
        let result: SwitchAccountResult = serde_json::from_value(
            server
                .switch_claude_at(profiles[1].clone(), home.clone())
                .unwrap(),
        )
        .unwrap();
        assert!(result.failures.is_empty(), "{:?}", result.failures);
        assert!(result.default_changed, "{:?}", result.default_error);
        assert_eq!(result.switched.len(), 2);
        assert!(result.deferred.is_empty());
        for record in &result.switched {
            assert_eq!(
                record.account_profile.as_ref().map(|p| p.id.as_str()),
                Some("two")
            );
        }
        let launches = slots[1].join("launches");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let launched = loop {
            let text = fs::read_to_string(&launches).unwrap_or_default();
            if text.matches("launch\n").count() >= 2 {
                break text;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fake Claude did not launch twice"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(
            launched.contains(&format!("--resume\n{}\n", conversation(&resumable))),
            "a saved conversation resumes in place: {launched}"
        );
        assert!(
            launched.contains(&format!("--session-id\n{}\n", conversation(&fresh))),
            "an unsaved conversation relaunches fresh with the same id: {launched}"
        );
        assert!(
            launched.contains("config-dir=unset\n") && !launched.contains("config-dir=/"),
            "the shared home stays the default config directory: {launched}"
        );
        let config = read_global_config(&home).unwrap();
        assert_eq!(config["oauthAccount"]["emailAddress"], "two@example.test");
        assert_eq!(config["oauthAccount"]["organizationName"], "Two");
        assert_eq!(
            config["oauthAccount"]["accountUuid"], "acct-1",
            "unknown fields are kept"
        );
        assert!(
            config.contains_key("mcpServers"),
            "MCP configuration is untouched"
        );
        let outgoing: Value = serde_json::from_slice(
            &read_private(&snapshot_path(slots[0].to_str().unwrap()))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            outgoing["emailAddress"], "one@example.test",
            "the outgoing identity is kept for the way back"
        );
        let catalog = server.accounts.lock().unwrap().catalog().unwrap();
        assert!(
            catalog
                .profiles
                .iter()
                .find(|p| p.id == "two")
                .unwrap()
                .is_default
        );
        for record in [&resumable, &fresh] {
            let _ = server.session_kill(Some(json!({"sessionID":record.id})));
        }
    }
}
