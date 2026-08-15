mod claude;
mod codex;

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use diri_proto::{
    AccountInstallation, AccountLoginStartParams, AccountProfile, InstallationStatus,
    LoginChallenge, LoginInputParams, LoginMode, LoginSessionParams, ProviderCallParams,
    ProviderCallResult, ProviderKind,
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

enum LoginHandle {
    Codex {
        profile_id: String,
        challenge: LoginChallenge,
    },
    Interactive {
        profile_id: String,
        provider: ProviderKind,
        child: Child,
        stdin: Option<ChildStdin>,
        output: Arc<Mutex<Vec<u8>>>,
    },
}

pub struct ProviderManager {
    codex: HashMap<String, CodexAppServer>,
    logins: HashMap<String, LoginHandle>,
}

impl Default for ProviderManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderManager {
    pub fn new() -> Self {
        Self {
            codex: HashMap::new(),
            logins: HashMap::new(),
        }
    }

    pub fn active_logins(&self) -> usize {
        self.logins.len()
    }

    /// Drop a cached provider process after its declarative configuration
    /// changes. Credentials remain in the profile home/keyring; the next call
    /// starts a fresh adapter that reads the newly applied settings.
    pub fn reset_profile(&mut self, profile_id: &str) {
        self.codex.remove(profile_id);
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
                let challenge = codex_challenge(&profile.id, params.mode, response)?;
                self.logins.insert(
                    challenge.login_id.clone(),
                    LoginHandle::Codex {
                        profile_id: profile.id,
                        challenge: challenge.clone(),
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

    pub async fn poll_login(
        &mut self,
        store: &AccountStore,
        params: LoginSessionParams,
    ) -> NodeResult<LoginChallenge> {
        let Some(handle) = self.logins.remove(&params.login_id) else {
            return Err(NodeError::NotFound(format!(
                "login session `{}`",
                params.login_id
            )));
        };
        match handle {
            LoginHandle::Codex {
                profile_id,
                mut challenge,
            } => {
                let installation = self.status(store, &profile_id).await?;
                challenge.complete = installation.status == InstallationStatus::Ready;
                challenge.success = challenge.complete;
                if !challenge.complete {
                    self.logins.insert(
                        params.login_id,
                        LoginHandle::Codex {
                            profile_id,
                            challenge: challenge.clone(),
                        },
                    );
                }
                Ok(challenge)
            }
            LoginHandle::Interactive {
                profile_id,
                provider,
                mut child,
                stdin,
                output,
            } => {
                let exit = child.try_wait()?;
                let bytes = output.lock().await.clone();
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let complete = exit.is_some();
                let success = exit.is_some_and(|status| status.success());
                let challenge = LoginChallenge {
                    login_id: params.login_id.clone(),
                    profile_id: profile_id.clone(),
                    kind: LoginMode::Interactive,
                    verification_url: first_url(&text),
                    user_code: None,
                    output: text,
                    complete,
                    success,
                    error: exit
                        .filter(|status| !status.success())
                        .map(|status| format!("login process exited with {status}")),
                };
                if !complete {
                    self.logins.insert(
                        params.login_id,
                        LoginHandle::Interactive {
                            profile_id,
                            provider,
                            child,
                            stdin,
                            output,
                        },
                    );
                }
                Ok(challenge)
            }
        }
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
                if let Some(server) = self.codex.get_mut(&profile_id) {
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
        if !self.codex.contains_key(&profile.id) {
            let server = CodexAppServer::spawn(config_home).await?;
            self.codex.insert(profile.id.clone(), server);
        }
        Ok(self.codex.get_mut(&profile.id).expect("inserted above"))
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
                provider: ProviderKind::Claude,
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

    #[test]
    fn forwarded_login_output_discovers_a_browser_url() {
        assert_eq!(
            first_url("Open https://example.com/login?code=abc and continue").as_deref(),
            Some("https://example.com/login?code=abc")
        );
    }
}
