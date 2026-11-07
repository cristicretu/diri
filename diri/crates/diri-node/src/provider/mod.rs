mod claude;
mod codex;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_proto::{
    AccountInstallation, AccountLoginStartParams, AccountProfile, InstallationStatus,
    LoginChallenge, LoginInputParams, LoginMode, LoginOutcome, LoginSessionParams,
    ProviderCallParams, ProviderCallResult, ProviderKind,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;

use crate::accounts::{AccountStore, now_seconds, profile_environment};
use crate::config::random_hex;
use crate::error::{NodeError, NodeResult};

use self::claude::{claude_call, claude_status};
use self::codex::CodexAppServer;

const MAX_LOGIN_OUTPUT: usize = 128 * 1024;
/// How long a Codex device-code or browser login stays open. Codex device
/// codes expire well before this; past it the login is reported failed
/// instead of pending forever.
const CODEX_LOGIN_TTL: Duration = Duration::from_secs(20 * 60);

enum LoginHandle {
    Codex {
        profile_id: String,
        challenge: LoginChallenge,
        /// The app-server that started the login; its login server dies
        /// with it.
        server: u64,
        expires: Instant,
    },
    Interactive {
        profile_id: String,
        child: Child,
        stdin: Option<ChildStdin>,
        output: Arc<Mutex<Vec<u8>>>,
    },
}

pub struct ProviderManager {
    codex_program: OsString,
    /// One app-server per profile, with the generation it was spawned as.
    codex: HashMap<String, (u64, CodexAppServer)>,
    codex_spawned: u64,
    logins: HashMap<String, LoginHandle>,
}

impl Default for ProviderManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderManager {
    pub fn new() -> Self {
        Self::with_codex_program("codex")
    }

    fn with_codex_program(program: impl Into<OsString>) -> Self {
        Self {
            codex_program: program.into(),
            codex: HashMap::new(),
            codex_spawned: 0,
            logins: HashMap::new(),
        }
    }

    pub fn active_logins(&self) -> usize {
        self.logins.len()
    }

    pub async fn status(
        &mut self,
        store: &AccountStore,
        profile_id: &str,
    ) -> NodeResult<AccountInstallation> {
        let profile = store.profile(profile_id)?.clone();
        let config_home = store.config_home(&profile);
        match profile.provider {
            ProviderKind::Codex => {
                let value = self.codex(&profile, &config_home).await?.account().await?;
                Ok(codex_installation(store, &profile, value))
            }
            ProviderKind::Claude => {
                let value = claude_status(&config_home).await?;
                Ok(claude_installation(store, &profile, value))
            }
        }
    }

    pub async fn begin_login(
        &mut self,
        store: &AccountStore,
        params: AccountLoginStartParams,
    ) -> NodeResult<LoginChallenge> {
        let profile = store.profile(&params.profile_id)?.clone();
        let config_home = store.config_home(&profile);
        match profile.provider {
            ProviderKind::Codex => {
                if params.mode == LoginMode::Interactive {
                    return Err(NodeError::BadRequest(
                        "Codex supports device-code or browser login".into(),
                    ));
                }
                let response = self
                    .codex(&profile, &config_home)
                    .await?
                    .begin_login(params.mode)
                    .await?;
                let server = self.codex_generation(&profile.id);
                let challenge = codex_challenge(&profile.id, params.mode, response)?;
                self.logins.insert(
                    challenge.login_id.clone(),
                    LoginHandle::Codex {
                        profile_id: profile.id,
                        challenge: challenge.clone(),
                        server,
                        expires: Instant::now() + CODEX_LOGIN_TTL,
                    },
                );
                Ok(challenge)
            }
            ProviderKind::Claude => {
                if params.mode == LoginMode::DeviceCode {
                    return Err(NodeError::BadRequest(
                        "Claude login is interactive; use the forwarded browser flow".into(),
                    ));
                }
                self.begin_claude_login(profile, &config_home).await
            }
        }
    }

    /// Reports a login's progress. The handle stays registered until the
    /// login completes either way; a failed poll (the provider could not be
    /// asked) keeps it, so the next poll can still finish it.
    pub async fn poll_login(
        &mut self,
        store: &AccountStore,
        params: LoginSessionParams,
    ) -> NodeResult<LoginChallenge> {
        let Some(mut handle) = self.logins.remove(&params.login_id) else {
            return Err(NodeError::NotFound(format!(
                "login session `{}`",
                params.login_id
            )));
        };
        let result = match &mut handle {
            LoginHandle::Codex {
                profile_id,
                challenge,
                server,
                expires,
            } => {
                let outcome = self
                    .poll_codex(store, profile_id, &challenge.login_id, *server, *expires)
                    .await;
                outcome.map(|outcome| {
                    challenge.set_outcome(outcome);
                    challenge.clone()
                })
            }
            LoginHandle::Interactive {
                profile_id,
                child,
                output,
                ..
            } => match child.try_wait() {
                Ok(exit) => {
                    let bytes = output.lock().await.clone();
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    let mut challenge = LoginChallenge {
                        login_id: params.login_id.clone(),
                        profile_id: profile_id.clone(),
                        kind: LoginMode::Interactive,
                        verification_url: first_url(&text),
                        user_code: None,
                        output: text,
                        complete: false,
                        success: false,
                        error: None,
                    };
                    challenge.set_outcome(match exit {
                        None => LoginOutcome::Pending,
                        Some(status) if status.success() => LoginOutcome::Succeeded,
                        Some(status) => {
                            LoginOutcome::Failed(format!("login process exited with {status}"))
                        }
                    });
                    Ok(challenge)
                }
                Err(error) => Err(error.into()),
            },
        };
        if !result.as_ref().is_ok_and(|challenge| challenge.complete) {
            self.logins.insert(params.login_id, handle);
        }
        result
    }

    async fn poll_codex(
        &mut self,
        store: &AccountStore,
        profile_id: &str,
        login_id: &str,
        server: u64,
        expires: Instant,
    ) -> NodeResult<LoginOutcome> {
        if Instant::now() >= expires {
            if let Some((generation, codex)) = self.codex.get_mut(profile_id)
                && *generation == server
            {
                let _ = codex
                    .call("account/login/cancel", json!({"loginId": login_id}))
                    .await;
            }
            return Ok(LoginOutcome::Failed(
                "the login expired; start it again".into(),
            ));
        }
        // Reading the account also reads any completion Codex announced.
        let installation = self.status(store, profile_id).await?;
        if installation.status == InstallationStatus::Ready {
            return Ok(LoginOutcome::Succeeded);
        }
        let Some((generation, codex)) = self.codex.get_mut(profile_id) else {
            return Ok(LoginOutcome::Pending);
        };
        if *generation != server {
            return Ok(LoginOutcome::Failed(
                "Codex restarted during the login; start it again".into(),
            ));
        }
        Ok(match codex.take_login_result(login_id) {
            Some(Err(error)) => LoginOutcome::Failed(error),
            // Signed in, and the account read will show it next.
            Some(Ok(())) | None => LoginOutcome::Pending,
        })
    }

    pub async fn login_input(&mut self, params: LoginInputParams) -> NodeResult<()> {
        let Some(LoginHandle::Interactive { stdin, .. }) = self.logins.get_mut(&params.login_id)
        else {
            return Err(NodeError::BadRequest(
                "this login does not accept terminal input".into(),
            ));
        };
        let stdin = stdin
            .as_mut()
            .ok_or_else(|| NodeError::Conflict("login input is closed".into()))?;
        stdin.write_all(params.text.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    pub async fn cancel_login(&mut self, params: LoginSessionParams) -> NodeResult<()> {
        let Some(handle) = self.logins.remove(&params.login_id) else {
            return Err(NodeError::NotFound(format!(
                "login session `{}`",
                params.login_id
            )));
        };
        match handle {
            LoginHandle::Interactive { mut child, .. } => {
                child.kill().await?;
            }
            LoginHandle::Codex { profile_id, .. } => {
                if let Some((_, server)) = self.codex.get_mut(&profile_id) {
                    let _ = server
                        .call("account/login/cancel", json!({"loginId": params.login_id}))
                        .await;
                }
            }
        }
        Ok(())
    }

    pub async fn call(
        &mut self,
        store: &AccountStore,
        params: ProviderCallParams,
    ) -> NodeResult<ProviderCallResult> {
        let profile = store.profile(&params.profile_id)?.clone();
        let config_home = store.config_home(&profile);
        let result = match profile.provider {
            ProviderKind::Codex => {
                validate_codex_method(&params.method)?;
                self.codex(&profile, &config_home)
                    .await?
                    .call(&params.method, params.params)
                    .await?
            }
            ProviderKind::Claude => {
                claude_call(&config_home, &params.method, params.params).await?
            }
        };
        Ok(ProviderCallResult {
            provider: profile.provider,
            method: params.method,
            result,
        })
    }

    async fn codex<'a>(
        &'a mut self,
        profile: &AccountProfile,
        config_home: &Path,
    ) -> NodeResult<&'a mut CodexAppServer> {
        // A dead app-server would fail every later call; replace it.
        if self
            .codex
            .get_mut(&profile.id)
            .is_some_and(|(_, server)| !server.is_alive())
        {
            self.codex.remove(&profile.id);
        }
        if !self.codex.contains_key(&profile.id) {
            let server = CodexAppServer::spawn(&self.codex_program, config_home).await?;
            self.codex_spawned += 1;
            self.codex
                .insert(profile.id.clone(), (self.codex_spawned, server));
        }
        Ok(&mut self.codex.get_mut(&profile.id).expect("inserted above").1)
    }

    fn codex_generation(&self, profile_id: &str) -> u64 {
        self.codex
            .get(profile_id)
            .map_or(0, |(generation, _)| *generation)
    }

    async fn begin_claude_login(
        &mut self,
        profile: AccountProfile,
        config_home: &Path,
    ) -> NodeResult<LoginChallenge> {
        let (variable, value) = profile_environment(ProviderKind::Claude, config_home);
        let mut command = Command::new("claude");
        command
            .args(["auth", "login"])
            .env(variable, value)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| NodeError::Provider(format!("could not start Claude: {error}")))?;
        let stdin = child.stdin.take();
        let output = Arc::new(Mutex::new(Vec::new()));
        if let Some(stdout) = child.stdout.take() {
            capture_output(stdout, Arc::clone(&output));
        }
        if let Some(stderr) = child.stderr.take() {
            capture_output(stderr, Arc::clone(&output));
        }
        let login_id = format!("claude-{}", random_hex(12)?);
        self.logins.insert(
            login_id.clone(),
            LoginHandle::Interactive {
                profile_id: profile.id.clone(),
                child,
                stdin,
                output,
            },
        );
        Ok(LoginChallenge {
            login_id,
            profile_id: profile.id,
            kind: LoginMode::Interactive,
            verification_url: None,
            user_code: None,
            output: "Claude login started on this node; poll for the local sign-in URL.".into(),
            complete: false,
            success: false,
            error: None,
        })
    }
}

fn capture_output(
    mut reader: impl AsyncReadExt + Unpin + Send + 'static,
    output: Arc<Mutex<Vec<u8>>>,
) {
    tokio::spawn(async move {
        let mut buffer = [0_u8; 4096];
        while let Ok(read) = reader.read(&mut buffer).await {
            if read == 0 {
                break;
            }
            let mut destination = output.lock().await;
            destination.extend_from_slice(&buffer[..read]);
            if destination.len() > MAX_LOGIN_OUTPUT {
                let overflow = destination.len() - MAX_LOGIN_OUTPUT;
                destination.drain(..overflow);
            }
        }
    });
}

fn codex_challenge(
    profile_id: &str,
    requested: LoginMode,
    response: Value,
) -> NodeResult<LoginChallenge> {
    let login_id = response
        .get("loginId")
        .and_then(Value::as_str)
        .ok_or_else(|| NodeError::Provider("Codex login returned no login id".into()))?;
    Ok(LoginChallenge {
        login_id: login_id.to_owned(),
        profile_id: profile_id.to_owned(),
        kind: requested,
        verification_url: response
            .get("verificationUrl")
            .or_else(|| response.get("authUrl"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        user_code: response
            .get("userCode")
            .and_then(Value::as_str)
            .map(str::to_owned),
        output: String::new(),
        complete: false,
        success: false,
        error: None,
    })
}

fn codex_installation(
    store: &AccountStore,
    profile: &AccountProfile,
    value: Value,
) -> AccountInstallation {
    let account = value.get("account");
    let mut installation = store.installation(
        profile,
        if account.is_some_and(|account| !account.is_null()) {
            InstallationStatus::Ready
        } else {
            InstallationStatus::SignedOut
        },
    );
    installation.identity = account
        .and_then(|account| account.get("email"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| profile.email.clone());
    installation.plan = account
        .and_then(|account| account.get("planType"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    installation.checked_at = Some(now_seconds());
    installation
}

fn claude_installation(
    store: &AccountStore,
    profile: &AccountProfile,
    value: Value,
) -> AccountInstallation {
    let ready = value
        .get("loggedIn")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut installation = store.installation(
        profile,
        if ready {
            InstallationStatus::Ready
        } else {
            InstallationStatus::SignedOut
        },
    );
    installation.identity = value
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| profile.email.clone());
    installation.plan = value
        .get("subscriptionType")
        .and_then(Value::as_str)
        .map(str::to_owned);
    installation.checked_at = Some(now_seconds());
    installation
}

fn validate_codex_method(method: &str) -> NodeResult<()> {
    const ALLOWED: &[&str] = &[
        "account/read",
        "account/rateLimits/read",
        "account/usage/read",
        "thread/list",
        "thread/read",
        "thread/start",
        "thread/resume",
        "thread/fork",
        "turn/start",
        "turn/interrupt",
    ];
    if ALLOWED.contains(&method) {
        Ok(())
    } else {
        Err(NodeError::BadRequest(format!(
            "Codex app-server method `{method}` is not exposed by the node"
        )))
    }
}

fn first_url(text: &str) -> Option<String> {
    let start = text.find("https://").or_else(|| text.find("http://"))?;
    let url = text[start..]
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|character: char| matches!(character, ')' | ']' | '}' | ',' | ';'));
    (!url.is_empty()).then(|| url.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_codex_methods_cross_the_adapter() {
        assert!(validate_codex_method("thread/list").is_ok());
        assert!(validate_codex_method("fs/remove").is_err());
        assert!(validate_codex_method("account/login/start").is_err());
    }

    /// A stand-in `codex app-server`: signed out, starts login `login-1`,
    /// and announces that login failed alongside every account read.
    fn fake_codex(directory: &Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = directory.join("codex");
        std::fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) echo "{\"id\":$id,\"result\":{}}" ;;
    *'"method":"account/login/start"'*)
      echo "{\"id\":$id,\"result\":{\"loginId\":\"login-1\",\"userCode\":\"ABCD\"}}" ;;
    *'"method":"account/read"'*)
      echo '{"method":"account/login/completed","params":{"loginId":"login-1","success":false,"error":"denied"}}'
      echo "{\"id\":$id,\"result\":{\"account\":null}}" ;;
    *'"id":'*) echo "{\"id\":$id,\"result\":{}}" ;;
  esac
done
"#,
        )
        .expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .expect("executable");
        script
    }

    fn codex_store(directory: &Path) -> AccountStore {
        let paths = crate::config::NodePaths::for_root(directory.join("node"));
        paths.create_layout().expect("layout");
        let mut store = AccountStore::load(paths, "local").expect("store");
        store
            .upsert(diri_proto::AccountUpsertParams {
                id: "work".into(),
                provider: ProviderKind::Codex,
                label: "Work".into(),
                email: None,
                organization: None,
                tags: Vec::new(),
            })
            .expect("profile");
        store
    }

    fn start(mode: LoginMode) -> AccountLoginStartParams {
        AccountLoginStartParams {
            profile_id: "work".into(),
            mode,
        }
    }

    fn poll(login_id: &str) -> LoginSessionParams {
        LoginSessionParams {
            login_id: login_id.into(),
        }
    }

    #[tokio::test]
    async fn a_codex_login_reports_the_failure_codex_announced() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = codex_store(directory.path());
        let mut manager = ProviderManager::with_codex_program(fake_codex(directory.path()));
        let challenge = manager
            .begin_login(&store, start(LoginMode::DeviceCode))
            .await
            .expect("login starts");
        assert_eq!(challenge.outcome(), LoginOutcome::Pending);
        let polled = manager
            .poll_login(&store, poll(&challenge.login_id))
            .await
            .expect("poll");
        assert_eq!(polled.outcome(), LoginOutcome::Failed("denied".into()));
        assert_eq!(manager.active_logins(), 0);
    }

    #[tokio::test]
    async fn an_expired_codex_login_fails_instead_of_staying_pending() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = codex_store(directory.path());
        let mut manager = ProviderManager::with_codex_program(fake_codex(directory.path()));
        let challenge = manager
            .begin_login(&store, start(LoginMode::DeviceCode))
            .await
            .expect("login starts");
        let Some(LoginHandle::Codex { expires, .. }) = manager.logins.get_mut("login-1") else {
            panic!("a Codex login handle");
        };
        *expires = Instant::now();
        let polled = manager
            .poll_login(&store, poll(&challenge.login_id))
            .await
            .expect("poll");
        assert!(
            matches!(polled.outcome(), LoginOutcome::Failed(error) if error.contains("expired"))
        );
        assert_eq!(manager.active_logins(), 0);
    }

    #[tokio::test]
    async fn a_dead_codex_app_server_is_replaced_and_its_login_fails() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = codex_store(directory.path());
        let mut manager = ProviderManager::with_codex_program(fake_codex(directory.path()));
        let challenge = manager
            .begin_login(&store, start(LoginMode::DeviceCode))
            .await
            .expect("login starts");
        let (_, server) = manager.codex.get_mut("work").expect("server");
        server.kill_for_test();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let status = manager.status(&store, "work").await.expect("respawned");
        assert_eq!(status.status, InstallationStatus::SignedOut);
        assert_eq!(manager.codex_generation("work"), 2);
        let polled = manager
            .poll_login(&store, poll(&challenge.login_id))
            .await
            .expect("poll");
        assert!(
            matches!(polled.outcome(), LoginOutcome::Failed(error) if error.contains("restarted"))
        );
    }

    #[tokio::test]
    async fn a_failed_poll_keeps_the_login() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = codex_store(directory.path());
        let mut manager = ProviderManager::with_codex_program(fake_codex(directory.path()));
        let challenge = manager
            .begin_login(&store, start(LoginMode::DeviceCode))
            .await
            .expect("login starts");
        // Codex can no longer be started: polling errors, but the login is
        // still there to poll again.
        manager.codex_program = directory.path().join("missing").into();
        let (_, server) = manager.codex.get_mut("work").expect("server");
        server.kill_for_test();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            manager
                .poll_login(&store, poll(&challenge.login_id))
                .await
                .is_err()
        );
        assert_eq!(manager.active_logins(), 1);
    }

    #[test]
    fn login_outcomes_keep_the_wire_fields_consistent() {
        let mut challenge =
            codex_challenge("work", LoginMode::DeviceCode, json!({"loginId": "login-1"}))
                .expect("challenge");
        challenge.set_outcome(LoginOutcome::Failed("denied".into()));
        assert!(challenge.complete && !challenge.success);
        assert_eq!(challenge.error.as_deref(), Some("denied"));
        challenge.set_outcome(LoginOutcome::Succeeded);
        assert!(challenge.complete && challenge.success && challenge.error.is_none());
    }

    #[test]
    fn forwarded_login_output_discovers_a_browser_url() {
        assert_eq!(
            first_url("Open https://example.com/login?code=abc and continue").as_deref(),
            Some("https://example.com/login?code=abc")
        );
    }
}
