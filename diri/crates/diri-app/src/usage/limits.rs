//! Provider-reported subscription windows for every local account. These are
//! independent of transcript cost estimates and context occupancy.
//! Credentials stay in provider storage; only parsed percentages, reset
//! times, plans and account labels reach the UI.
//!
//! An account is asked only while its short-lived access token is valid.
//! Diri never refreshes a token (that would rotate it behind the CLI's back),
//! so an idle account shows its last answer, remembered on disk, and the
//! time that window resets.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use diri_proto::{AgentAccountOverview, AgentKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{io::AsyncWriteExt, process::Command};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitWindow {
    pub label: String,
    pub used_percent: f64,
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AccountLimits {
    pub provider: &'static str,
    pub account: String,
    /// The saved profile; `None` for a login no profile holds yet.
    pub profile_id: Option<String>,
    /// New local tabs of this Agent use this login.
    pub live: bool,
    /// The plan the login states: "Max 20x", "Pro", "Plus", …
    pub plan: Option<String>,
    pub windows: Vec<LimitWindow>,
    /// When the provider reported `windows`: an idle account's are remembered.
    pub checked_at: i64,
    pub error: Option<&'static str>,
    /// Its sign-in tab is still open: never pointed out as the one to use.
    pub signing_in: bool,
}

/// How many percentage points more room another account needs before it is
/// pointed out: a slightly emptier one is not worth a switch.
pub(crate) const ROOM_MARGIN: f64 = 20.0;

/// The provider rejected a login whose token had not expired: it was revoked.
pub(crate) const SIGN_IN_AGAIN: &str = "Sign in again to refresh limits";

impl AccountLimits {
    /// The window that limits the account most right now. A window whose
    /// reset has passed counts as empty.
    pub fn binding(&self, now: i64) -> Option<LimitWindow> {
        self.windows
            .iter()
            .map(|window| {
                let mut window = window.clone();
                if window.resets_at.is_some_and(|reset| reset <= now) {
                    window.used_percent = 0.0;
                }
                window
            })
            .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent))
    }

    pub fn needs_sign_in(&self) -> bool {
        self.error == Some(SIGN_IN_AGAIN)
    }

    /// A window that was full when last seen has reset since.
    pub fn ready_again(&self, now: i64) -> bool {
        self.windows
            .iter()
            .any(|w| w.used_percent >= 99.5 && w.resets_at.is_some_and(|reset| reset <= now))
            && self.binding(now).is_some_and(|w| w.used_percent < 99.5)
    }
}

fn provider_of(agent: &str) -> &'static str {
    if agent == AgentKind::CODEX_ID {
        "Codex"
    } else {
        "Claude"
    }
}

/// The saved account of `agent` with the most room right now, when it has at
/// least `margin` percentage points more than the login in use. Only accounts
/// whose use is known compete: one never asked (its token expired before Diri
/// saw it) has no numbers to back the claim, and one that needs a new sign-in
/// cannot be switched to.
pub(crate) fn most_room(
    overview: &AgentAccountOverview,
    limits: &[AccountLimits],
    agent: &str,
    margin: f64,
    now: i64,
) -> Option<String> {
    roomiest(limits, agent, margin, now, |limits| {
        limits.profile_id.as_deref().is_some_and(|id| {
            overview
                .catalog
                .profiles
                .iter()
                .any(|p| p.id == id && p.host.is_none() && !overview.is_live(p))
                && overview
                    .login(id)
                    .is_some_and(|l| l.signed_in && !l.signing_in)
        })
    })
}

/// [`most_room`] from the limits alone, for views without the overview
/// (the command palette). `eligible` narrows the saved accounts that compete.
pub(crate) fn roomiest(
    limits: &[AccountLimits],
    agent: &str,
    margin: f64,
    now: i64,
    eligible: impl Fn(&AccountLimits) -> bool,
) -> Option<String> {
    let provider = provider_of(agent);
    let live = limits
        .iter()
        .find(|l| l.live && l.provider == provider)
        .and_then(|l| l.binding(now))
        .map(|w| w.used_percent);
    let (best, used) = limits
        .iter()
        .filter(|l| l.provider == provider && l.profile_id.is_some() && !l.live)
        .filter(|l| !l.signing_in && !l.needs_sign_in() && eligible(l))
        .filter_map(|l| Some((l, l.binding(now)?.used_percent)))
        .min_by(|a, b| a.1.total_cmp(&b.1))?;
    let room_now = 100.0 - live.unwrap_or(100.0);
    (100.0 - used >= room_now + margin)
        .then(|| best.profile_id.clone())
        .flatten()
}

enum Credential {
    Claude {
        #[cfg(target_os = "macos")]
        keychain: String,
        directory: PathBuf,
    },
    Codex {
        file: PathBuf,
    },
}

struct AccountSource {
    provider: &'static str,
    label: String,
    /// Remembers this login's last answer: profile, then the account's email,
    /// so a slot signed into another account never shows the old numbers.
    memory_key: String,
    profile_id: Option<String>,
    live: bool,
    signing_in: bool,
    credential: Credential,
}

/// The Keychain item Claude Code derives for a credential directory (the
/// default item without one).
#[cfg(target_os = "macos")]
fn claude_keychain(store: Option<&Path>) -> String {
    store.map_or_else(
        || "Claude Code-credentials".into(),
        |path| {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(path.to_string_lossy().as_bytes());
            format!(
                "Claude Code-credentials-{:02x}{:02x}{:02x}{:02x}",
                digest[0], digest[1], digest[2], digest[3]
            )
        },
    )
}

fn claude_credential(store: Option<PathBuf>, home: &Path) -> Credential {
    Credential::Claude {
        #[cfg(target_os = "macos")]
        keychain: claude_keychain(store.as_deref()),
        directory: store.unwrap_or_else(|| home.join(".claude")),
    }
}

/// Tells accounts apart in the remembered answers without writing their
/// email down: the first bytes of its SHA-256.
fn account_tag(email: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(email.unwrap_or_default().to_lowercase().as_bytes());
    digest[..6].iter().map(|b| format!("{b:02x}")).collect()
}

fn sources(home: &Path, overview: &AgentAccountOverview) -> Vec<AccountSource> {
    let mut out = Vec::new();
    for profile in overview.catalog.profiles.iter().filter(|p| {
        p.host.is_none()
            && matches!(
                p.agent.as_str(),
                AgentKind::CODEX_ID | AgentKind::CLAUDE_CODE_ID
            )
    }) {
        let Some(login) = overview.login(&profile.id).filter(|l| l.signed_in) else {
            continue;
        };
        let credential = if profile.agent == AgentKind::CODEX_ID {
            let Some(file) = &login.login_file else {
                continue;
            };
            Credential::Codex {
                file: PathBuf::from(file),
            }
        } else if overview.is_live(profile) && !profile.is_default {
            // Saved from Claude's default store before any switch: the tabs
            // keep refreshing that store, so the profile's copy goes stale.
            default_claude_credential(home)
        } else {
            let Some(store) = &profile.login_store else {
                continue;
            };
            claude_credential(Some(PathBuf::from(store)), home)
        };
        out.push(AccountSource {
            provider: provider_of(&profile.agent),
            label: profile.label.clone(),
            memory_key: format!(
                "{}:{}",
                profile.id,
                account_tag(login.identity.email.as_deref())
            ),
            profile_id: Some(profile.id.clone()),
            live: overview.is_live(profile),
            signing_in: login.signing_in,
            credential,
        });
    }
    // A login in use that no profile holds yet: where a launch without a
    // profile finds it, mirroring the provider's own overrides.
    for live in overview.live.iter().filter(|l| l.profile_id.is_none()) {
        let credential = match live.agent.as_str() {
            AgentKind::CLAUDE_CODE_ID => default_claude_credential(home),
            AgentKind::CODEX_ID => Credential::Codex {
                file: std::env::var_os("CODEX_HOME")
                    .filter(|p| !p.is_empty())
                    .map_or_else(|| home.join(".codex"), PathBuf::from)
                    .join("auth.json"),
            },
            _ => continue,
        };
        out.push(AccountSource {
            provider: provider_of(&live.agent),
            label: live
                .identity
                .email
                .clone()
                .unwrap_or_else(|| "CLI account".into()),
            memory_key: format!(
                "unsaved:{}:{}",
                live.agent,
                account_tag(live.identity.email.as_deref())
            ),
            profile_id: None,
            live: true,
            signing_in: false,
            credential,
        });
    }
    out
}

/// Where a Claude launch without a profile finds its login, mirroring
/// Claude's own overrides.
fn default_claude_credential(home: &Path) -> Credential {
    let store = match std::env::var_os("CLAUDE_SECURESTORAGE_CONFIG_DIR") {
        // Set but empty pins the default store.
        Some(value) => (!value.is_empty()).then(|| PathBuf::from(value)),
        None => std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from),
    };
    claude_credential(store, home)
}

/// The last answer each account gave, so an idle account (whose token
/// expired) still shows its windows and when they reset. Kept beside the
/// Engine's state, owner-only; percentages and reset times only.
#[derive(Default)]
pub(crate) struct LimitsMemory {
    path: Option<PathBuf>,
    answers: HashMap<String, Remembered>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Remembered {
    windows: Vec<LimitWindow>,
    checked_at: i64,
}

/// Answers older than the longest window are no help.
const MEMORY_DAYS: i64 = 8;

impl LimitsMemory {
    pub(crate) fn load(path: PathBuf) -> Self {
        let answers = std::fs::read(&path)
            .ok()
            .filter(|bytes| bytes.len() <= 1_048_576)
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path: Some(path),
            answers,
        }
    }

    fn save(&mut self, now: i64) {
        self.answers
            .retain(|_, r| now - r.checked_at < MEMORY_DAYS * 86_400);
        let Some(path) = &self.path else {
            return;
        };
        let Ok(bytes) = serde_json::to_vec(&self.answers) else {
            return;
        };
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        let written = (|| {
            use std::io::Write;
            #[cfg(unix)]
            use std::os::unix::fs::OpenOptionsExt;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary)?;
            file.write_all(&bytes)?;
            std::fs::rename(&temporary, path)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
    }
}

enum Answer {
    Windows(Vec<LimitWindow>),
    /// The access token expired: not asked, so it is never refreshed.
    Idle,
    Failed(&'static str),
}

pub(crate) async fn refresh(
    home: &Path,
    overview: &AgentAccountOverview,
    memory: &mut LimitsMemory,
) -> Vec<AccountLimits> {
    let now = super::Clock::read(&super::SystemClock).unix_seconds;
    let mut tasks = tokio::task::JoinSet::new();
    for (index, source) in sources(home, overview).into_iter().enumerate() {
        tasks.spawn(async move {
            let (plan, answer) = fetch(&source, now).await;
            (index, source, plan, answer)
        });
    }
    let mut answered = Vec::new();
    // One account's failed task must not end the others.
    while let Some(joined) = tasks.join_next().await {
        if let Ok(result) = joined {
            answered.push(result);
        }
    }
    answered.sort_by_key(|(index, ..)| *index);
    let result = answered
        .into_iter()
        .map(|(_, source, plan, answer)| {
            let remembered = memory.answers.get(&source.memory_key).cloned();
            let (windows, checked_at, error) = match answer {
                Answer::Windows(windows) => {
                    memory.answers.insert(
                        source.memory_key.clone(),
                        Remembered {
                            windows: windows.clone(),
                            checked_at: now,
                        },
                    );
                    (windows, now, None)
                }
                Answer::Idle => {
                    remembered.map_or((Vec::new(), now, None), |r| (r.windows, r.checked_at, None))
                }
                // A login that is gone shows no numbers; a passing failure
                // keeps the last ones, marked by the error.
                Answer::Failed(error) if error == SIGN_IN_AGAIN => (Vec::new(), now, Some(error)),
                Answer::Failed(error) => remembered.map_or((Vec::new(), now, Some(error)), |r| {
                    (r.windows, r.checked_at, Some(error))
                }),
            };
            AccountLimits {
                provider: source.provider,
                account: source.label,
                profile_id: source.profile_id,
                live: source.live,
                plan,
                windows,
                checked_at,
                error,
                signing_in: source.signing_in,
            }
        })
        .collect();
    memory.save(now);
    result
}

fn capitalized(plan: &str) -> String {
    let mut chars = plan.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// "Max 20x" from `subscriptionType: "max"` and `rateLimitTier: "default_claude_max_20x"`.
fn claude_plan(oauth: &Value) -> Option<String> {
    let kind = oauth.get("subscriptionType")?.as_str()?.trim();
    if kind.is_empty() {
        return None;
    }
    let tier = oauth
        .get("rateLimitTier")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let multiple = ["20x", "5x"].into_iter().find(|m| tier.contains(m));
    Some(match multiple {
        Some(multiple) => format!("{} {multiple}", capitalized(kind)),
        None => capitalized(kind),
    })
}

fn jwt_claims(token: &str) -> Option<Value> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The login's plan, and what its provider reports, when its token is valid.
async fn fetch(source: &AccountSource, now: i64) -> (Option<String>, Answer) {
    let (url, mut headers, plan, expires_at) = match &source.credential {
        Credential::Claude { .. } => {
            let Some(value) = claude_credentials(source).await else {
                return (None, Answer::Failed("Sign in to Claude to see limits"));
            };
            let oauth = value.get("claudeAiOauth").cloned().unwrap_or(Value::Null);
            let plan = claude_plan(&oauth);
            let Some(token) = oauth.get("accessToken").and_then(Value::as_str) else {
                return (
                    plan,
                    Answer::Failed("Claude subscription sign-in unavailable"),
                );
            };
            let expires_at = oauth
                .get("expiresAt")
                .and_then(Value::as_i64)
                .map(|ms| ms / 1000);
            (
                "https://api.anthropic.com/api/oauth/usage",
                vec![
                    format!("Authorization: Bearer {token}"),
                    "anthropic-beta: oauth-2025-04-20".into(),
                    "User-Agent: claude-code/2.1.0".into(),
                ],
                plan,
                expires_at,
            )
        }
        Credential::Codex { file } => {
            let Some(value) = read_json(file).await else {
                return (None, Answer::Failed("Sign in to Codex to see limits"));
            };
            let plan = value
                .pointer("/tokens/id_token")
                .and_then(Value::as_str)
                .and_then(jwt_claims)
                .and_then(|claims| {
                    claims
                        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_plan_type")
                        .and_then(Value::as_str)
                        .map(capitalized)
                });
            let Some(token) = value
                .pointer("/tokens/access_token")
                .and_then(Value::as_str)
            else {
                return (
                    plan,
                    Answer::Failed("Codex subscription sign-in unavailable"),
                );
            };
            let expires_at = jwt_claims(token).and_then(|c| c.get("exp").and_then(Value::as_i64));
            let mut headers = vec![
                format!("Authorization: Bearer {token}"),
                "User-Agent: diri".into(),
            ];
            if let Some(account) = value.pointer("/tokens/account_id").and_then(Value::as_str) {
                headers.push(format!("ChatGPT-Account-Id: {account}"));
            }
            (
                "https://chatgpt.com/backend-api/wham/usage",
                headers,
                plan,
                expires_at,
            )
        }
    };
    if expires_at.is_some_and(|expiry| expiry <= now + 30) {
        return (plan, Answer::Idle);
    }
    headers.push("Accept: application/json".into());
    let body = match http_get(url, &headers).await {
        Ok(body) => body,
        Err(error) => return (plan, Answer::Failed(error)),
    };
    let windows = if matches!(source.credential, Credential::Claude { .. }) {
        parse_claude(&body)
    } else {
        parse_codex(&body)
    };
    if windows.is_empty() {
        (plan, Answer::Failed("No subscription limits reported"))
    } else {
        (plan, Answer::Windows(windows))
    }
}

async fn read_json(path: &Path) -> Option<Value> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let file = options.open(path).ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(1_048_577).read_to_end(&mut bytes).ok()?;
        if bytes.len() > 1_048_576 {
            return None;
        }
        serde_json::from_slice(&bytes).ok()
    })
    .await
    .ok()
    .flatten()
}

async fn claude_credentials(source: &AccountSource) -> Option<Value> {
    let Credential::Claude {
        #[cfg(target_os = "macos")]
        keychain,
        directory,
    } = &source.credential
    else {
        return None;
    };
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("/usr/bin/security");
        command
            .args(["find-generic-password", "-s", keychain, "-w"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(8), command.output()).await
            && output.status.success()
            && let Ok(value) = serde_json::from_slice(&output.stdout)
        {
            return Some(value);
        }
    }
    read_json(&directory.join(".credentials.json")).await
}

/// Disable curl's user config, redirects and diagnostics. Authorization travels
/// via stdin, never process arguments, temp files, logs or error strings.
async fn http_get(url: &str, headers: &[String]) -> Result<Value, &'static str> {
    let mut config = String::new();
    for header in headers {
        if header.chars().any(char::is_control) {
            return Err("Invalid sign-in credentials");
        }
        config.push_str("header = \"");
        config.push_str(&header.replace('\\', "\\\\").replace('"', "\\\""));
        config.push_str("\"\n");
    }
    let mut child = Command::new("/usr/bin/curl")
        .args([
            "-q",
            "--silent",
            "--proto",
            "=https",
            "--max-time",
            "10",
            "--max-filesize",
            "1048576",
            "--write-out",
            "\n%{http_code}",
            "--config",
            "-",
            url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "Could not refresh limits")?;
    let mut stdin = child.stdin.take().ok_or("Could not refresh limits")?;
    stdin
        .write_all(config.as_bytes())
        .await
        .map_err(|_| "Could not refresh limits")?;
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(12), child.wait_with_output())
        .await
        .map_err(|_| "Limits refresh timed out")?
        .map_err(|_| "Could not refresh limits")?;
    if !output.status.success() {
        return Err("Could not refresh limits");
    }
    let raw = std::str::from_utf8(&output.stdout).map_err(|_| "Invalid usage response")?;
    let (body, status) = raw.rsplit_once('\n').ok_or("Invalid usage response")?;
    match status {
        "200" => serde_json::from_str(body).map_err(|_| "Invalid usage response"),
        "401" => Err(SIGN_IN_AGAIN),
        "403" => Err("Limits are not available for this account"),
        "429" => Err("Refresh limited; retrying shortly"),
        _ => Err("Could not refresh limits"),
    }
}

fn window(label: String, percent: Option<f64>, resets: Option<&Value>) -> Option<LimitWindow> {
    let percent = percent.filter(|value| value.is_finite())?;
    Some(LimitWindow {
        label,
        used_percent: percent.clamp(0.0, 100.0),
        resets_at: resets.and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(super::timestamp::parse_timestamp))
        }),
    })
}

fn parse_claude(body: &Value) -> Vec<LimitWindow> {
    let modern: Vec<_> = body
        .get("limits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let label = match entry.get("kind")?.as_str()? {
                "session" => "5-hour limit".into(),
                "weekly_all" => "Weekly limit".into(),
                "weekly_scoped" => format!(
                    "Weekly · {}",
                    entry
                        .pointer("/scope/model/display_name")
                        .and_then(Value::as_str)
                        .unwrap_or("model")
                ),
                _ => return None,
            };
            window(
                label,
                entry.get("percent").and_then(Value::as_f64),
                entry.get("resets_at"),
            )
        })
        .collect();
    if !modern.is_empty() {
        return modern;
    }
    [
        ("five_hour", "5-hour limit"),
        ("seven_day", "Weekly limit"),
        ("seven_day_opus", "Weekly · Opus"),
        ("seven_day_sonnet", "Weekly · Sonnet"),
    ]
    .into_iter()
    .filter_map(|(key, label)| {
        let entry = body.get(key)?;
        window(
            label.into(),
            entry.get("utilization").and_then(Value::as_f64),
            entry.get("resets_at"),
        )
    })
    .collect()
}

fn parse_codex(body: &Value) -> Vec<LimitWindow> {
    let mut result = Vec::new();
    let mut add = |rate: &Value, scope: Option<&str>| {
        for key in ["primary_window", "secondary_window"] {
            let Some(entry) = rate.get(key) else {
                continue;
            };
            let label = match entry.get("limit_window_seconds").and_then(Value::as_i64) {
                Some(604_800) => "Weekly limit".into(),
                Some(seconds) if seconds >= 86_400 => format!("{}-day limit", seconds / 86_400),
                Some(seconds) if seconds >= 3_600 => format!("{}-hour limit", seconds / 3_600),
                Some(seconds) if seconds > 0 => format!("{}-minute limit", seconds / 60),
                _ => if key == "primary_window" {
                    "Session limit"
                } else {
                    "Weekly limit"
                }
                .into(),
            };
            let label = scope.map_or_else(|| label.clone(), |scope| format!("{label} · {scope}"));
            if let Some(window) = window(
                label,
                entry.get("used_percent").and_then(Value::as_f64),
                entry.get("reset_at"),
            ) {
                result.push(window);
            }
        }
    };
    if let Some(rate) = body.get("rate_limit") {
        add(rate, None);
    }
    for entry in body
        .get("additional_rate_limits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(rate) = entry.get("rate_limit") {
            add(rate, entry.get("limit_name").and_then(Value::as_str));
        }
    }
    result
}

/// Design fixture: two Claude accounts and two Codex accounts, one of each in
/// use. Never mixed into the live account snapshot.
pub(crate) fn preview_overview() -> AgentAccountOverview {
    use diri_proto::{
        AgentAccountCatalog, AgentAccountIdentity, AgentAccountLogin, AgentAccountProfile,
        AgentLiveLogin,
    };
    let accounts = [
        (
            "preview-0",
            "Work",
            "claude-code",
            "alex@northwind.dev",
            "max",
        ),
        (
            "preview-1",
            "Personal",
            "claude-code",
            "alex@fabrikam.dev",
            "pro",
        ),
        ("preview-2", "Main", "codex", "alex@northwind.dev", "pro"),
        (
            "preview-3",
            "Side project",
            "codex",
            "alex@contoso.dev",
            "plus",
        ),
    ];
    let profiles = accounts
        .iter()
        .map(|(id, label, agent, ..)| AgentAccountProfile {
            id: (*id).into(),
            label: (*label).into(),
            agent: (*agent).into(),
            host: None,
            config_home: String::new(),
            is_default: matches!(*id, "preview-0" | "preview-2"),
            login_store: None,
        })
        .collect();
    let logins = accounts
        .iter()
        .map(|(id, _, _, email, plan)| AgentAccountLogin {
            profile_id: (*id).into(),
            identity: AgentAccountIdentity {
                email: Some((*email).into()),
                organization: None,
                plan: Some((*plan).into()),
            },
            signed_in: true,
            signing_in: false,
            login_file: None,
        })
        .collect();
    let live = [("claude-code", "preview-0"), ("codex", "preview-2")]
        .into_iter()
        .map(|(agent, id)| AgentLiveLogin {
            agent: agent.into(),
            profile_id: Some(id.into()),
            identity: AgentAccountIdentity::default(),
        })
        .collect();
    AgentAccountOverview {
        catalog: AgentAccountCatalog { profiles },
        logins,
        live,
    }
}

/// Design fixture: a first run with nothing saved in Diri and both logins
/// in use.
#[cfg(test)]
pub(crate) fn first_run_overview() -> AgentAccountOverview {
    use diri_proto::{AgentAccountIdentity, AgentLiveLogin};
    let mut overview = preview_overview();
    overview.catalog.profiles.clear();
    overview.logins.clear();
    overview.live = [
        ("claude-code", "alex@northwind.dev"),
        ("codex", "alex@northwind.dev"),
    ]
    .into_iter()
    .map(|(agent, email)| AgentLiveLogin {
        agent: agent.into(),
        profile_id: None,
        identity: AgentAccountIdentity {
            email: Some(email.into()),
            ..Default::default()
        },
    })
    .collect();
    overview
}

/// Design fixture matching [`preview_overview`].
pub(crate) fn preview() -> Vec<AccountLimits> {
    let now = super::Clock::read(&super::SystemClock).unix_seconds;
    let account =
        |id: &str, provider, label: &str, plan: &str, live, windows: Vec<(&str, f64, i64)>| {
            AccountLimits {
                provider,
                account: label.into(),
                profile_id: Some(id.into()),
                live,
                plan: Some(plan.into()),
                windows: windows
                    .into_iter()
                    .map(|(label, used_percent, resets_in)| LimitWindow {
                        label: label.into(),
                        used_percent,
                        resets_at: Some(now + resets_in),
                    })
                    .collect(),
                checked_at: now,
                error: None,
                signing_in: false,
            }
        };
    vec![
        account(
            "preview-0",
            "Claude",
            "Work",
            "Max 20x",
            true,
            vec![
                ("5-hour limit", 86.0, 4_320),
                ("Weekly limit", 41.0, 302_400),
            ],
        ),
        account(
            "preview-1",
            "Claude",
            "Personal",
            "Pro",
            false,
            vec![
                ("5-hour limit", 12.0, 9_000),
                ("Weekly limit", 18.0, 410_000),
            ],
        ),
        account(
            "preview-2",
            "Codex",
            "Main",
            "Pro",
            true,
            vec![
                ("5-hour limit", 23.0, 12_600),
                ("Weekly limit", 9.0, 500_000),
            ],
        ),
        // Idle: its token expired before it was ever asked.
        account("preview-3", "Codex", "Side project", "Plus", false, vec![]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::{
        AgentAccountCatalog, AgentAccountIdentity, AgentAccountLogin, AgentAccountProfile,
        AgentLiveLogin,
    };
    use serde_json::json;

    fn profile(
        id: &str,
        agent: &str,
        host: Option<&str>,
        store: Option<&str>,
    ) -> AgentAccountProfile {
        AgentAccountProfile {
            id: id.into(),
            label: id.into(),
            agent: agent.into(),
            host: host.map(str::to_owned),
            config_home: "/home/me/.claude".into(),
            is_default: false,
            login_store: store.map(str::to_owned),
        }
    }

    fn login(id: &str, file: Option<&str>) -> AgentAccountLogin {
        AgentAccountLogin {
            profile_id: id.into(),
            identity: AgentAccountIdentity {
                email: Some(format!("{id}@example.test")),
                ..Default::default()
            },
            signed_in: true,
            signing_in: false,
            login_file: file.map(str::to_owned),
        }
    }

    fn limits(id: Option<&str>, provider: &'static str, live: bool, used: f64) -> AccountLimits {
        AccountLimits {
            provider,
            account: id.unwrap_or("cli").into(),
            profile_id: id.map(str::to_owned),
            live,
            plan: None,
            windows: vec![LimitWindow {
                label: "5-hour limit".into(),
                used_percent: used,
                resets_at: Some(2_000),
            }],
            checked_at: 0,
            error: None,
            signing_in: false,
        }
    }

    #[test]
    fn every_signed_in_local_account_is_read_from_its_own_login() {
        let overview = AgentAccountOverview {
            catalog: AgentAccountCatalog {
                profiles: vec![
                    profile(
                        "work",
                        "claude-code",
                        None,
                        Some("/state/claude-logins/work"),
                    ),
                    profile("remote", "claude-code", Some("server"), Some("/remote")),
                    profile("main", "codex", None, None),
                    profile("new", "codex", None, None),
                ],
            },
            logins: vec![
                login("work", None),
                login("main", Some("/state/codex-logins/main/auth.json")),
                AgentAccountLogin {
                    signed_in: false,
                    ..login("new", None)
                },
            ],
            live: vec![AgentLiveLogin {
                agent: "claude-code".into(),
                profile_id: None,
                identity: AgentAccountIdentity {
                    email: Some("cli@example.test".into()),
                    ..Default::default()
                },
            }],
        };
        let found = sources(Path::new("/home/me"), &overview);
        let labels: Vec<_> = found.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            labels,
            ["work", "main", "cli@example.test"],
            "remote and signed-out skipped"
        );
        match &found[0].credential {
            Credential::Claude { directory, .. } => {
                assert_eq!(directory, Path::new("/state/claude-logins/work"));
                #[cfg(target_os = "macos")]
                {
                    let Credential::Claude { keychain, .. } = &found[0].credential else {
                        unreachable!()
                    };
                    assert_ne!(keychain, "Claude Code-credentials");
                }
            }
            Credential::Codex { .. } => panic!("a Claude login"),
        }
        assert!(matches!(&found[1].credential,
            Credential::Codex { file } if file == Path::new("/state/codex-logins/main/auth.json")));
        assert!(found[2].live && found[2].profile_id.is_none());
        assert_ne!(found[0].memory_key, found[2].memory_key);
    }

    #[test]
    fn a_claude_login_saved_from_the_default_store_is_read_where_tabs_refresh_it() {
        // Saved before any switch: in use through Claude's default store,
        // which the tabs keep rotating, while the profile holds a copy.
        let overview = AgentAccountOverview {
            catalog: AgentAccountCatalog {
                profiles: vec![profile(
                    "personal",
                    "claude-code",
                    None,
                    Some("/state/claude-logins/personal"),
                )],
            },
            logins: vec![login("personal", None)],
            live: vec![AgentLiveLogin {
                agent: "claude-code".into(),
                profile_id: Some("personal".into()),
                identity: AgentAccountIdentity::default(),
            }],
        };
        let home = Path::new("/home/me");
        let found = sources(home, &overview);
        let directory = |credential: &Credential| match credential {
            Credential::Claude { directory, .. } => directory.clone(),
            Credential::Codex { .. } => panic!("a Claude login"),
        };
        assert!(found[0].live);
        assert_eq!(
            directory(&found[0].credential),
            directory(&default_claude_credential(home))
        );
        assert_ne!(
            directory(&found[0].credential),
            Path::new("/state/claude-logins/personal")
        );
    }

    #[test]
    fn the_account_with_most_room_is_suggested_only_when_it_helps() {
        let mut overview = preview_overview();
        let mut all = vec![
            limits(Some("preview-0"), "Claude", true, 90.0),
            limits(Some("preview-1"), "Claude", false, 10.0),
        ];
        assert_eq!(
            most_room(&overview, &all, "claude-code", 20.0, 1_000).as_deref(),
            Some("preview-1")
        );
        // A window whose reset passed counts as empty.
        assert_eq!(all[0].binding(3_000).unwrap().used_percent, 0.0);
        assert_eq!(most_room(&overview, &all, "claude-code", 20.0, 3_000), None);
        // Close enough to the account in use: no nudge.
        all[0].windows[0].used_percent = 25.0;
        assert_eq!(most_room(&overview, &all, "claude-code", 20.0, 1_000), None);
        assert_eq!(
            most_room(&overview, &all, "claude-code", 0.0, 1_000).as_deref(),
            Some("preview-1"),
            "any gain counts when asked for"
        );
        // A revoked login and an open sign-in are never suggested.
        all[0].windows[0].used_percent = 95.0;
        all[1].error = Some(SIGN_IN_AGAIN);
        assert_eq!(most_room(&overview, &all, "claude-code", 20.0, 1_000), None);
        all[1].error = None;
        overview.logins[1].signing_in = true;
        assert_eq!(most_room(&overview, &all, "claude-code", 20.0, 1_000), None);
        // An account never asked has no numbers to claim room with, even
        // next to one that is nearly spent.
        overview.logins[1].signing_in = false;
        all.remove(1);
        assert_eq!(most_room(&overview, &all, "claude-code", 0.0, 1_000), None);
        assert_eq!(most_room(&overview, &all, "codex", 0.0, 1_000), None);
    }

    #[test]
    fn a_full_window_that_reset_since_reads_as_ready() {
        let mut account = limits(Some("a"), "Claude", false, 100.0);
        assert!(!account.ready_again(1_000));
        assert!(account.ready_again(2_000));
        account.windows.push(LimitWindow {
            label: "Weekly limit".into(),
            used_percent: 100.0,
            resets_at: Some(9_000),
        });
        assert!(
            !account.ready_again(2_000),
            "still limited by the weekly window"
        );
    }

    #[test]
    fn plans_read_as_the_provider_sells_them() {
        assert_eq!(
            claude_plan(
                &json!({"subscriptionType": "max", "rateLimitTier": "default_claude_max_20x"})
            )
            .as_deref(),
            Some("Max 20x")
        );
        assert_eq!(
            claude_plan(&json!({"subscriptionType": "pro"})).as_deref(),
            Some("Pro")
        );
        assert_eq!(claude_plan(&json!({})), None);
        assert_eq!(capitalized("plus"), "Plus");
    }

    #[tokio::test]
    async fn an_idle_account_is_not_asked_and_shows_its_last_answer() {
        use base64::Engine as _;
        let tmp = tempfile::tempdir().unwrap();
        let encode = |v: Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&v).unwrap())
        };
        let expired = format!("x.{}.y", encode(json!({"exp": 1})));
        let id = format!(
            "x.{}.y",
            encode(json!({"https://api.openai.com/auth": {"chatgpt_plan_type": "plus"}}))
        );
        let file = tmp.path().join("auth.json");
        std::fs::write(
            &file,
            json!({"tokens": {"access_token": expired, "id_token": id}}).to_string(),
        )
        .unwrap();
        let overview = AgentAccountOverview {
            catalog: AgentAccountCatalog {
                profiles: vec![profile("side", "codex", None, None)],
            },
            logins: vec![login("side", Some(file.to_str().unwrap()))],
            ..Default::default()
        };
        let memory_file = tmp.path().join("account-limits.json");
        let mut memory = LimitsMemory::load(memory_file.clone());
        let now = super::super::Clock::read(&super::super::SystemClock).unix_seconds;
        memory.answers.insert(
            format!("side:{}", account_tag(Some("side@example.test"))),
            Remembered {
                windows: vec![LimitWindow {
                    label: "5-hour limit".into(),
                    used_percent: 64.0,
                    resets_at: Some(now + 600),
                }],
                checked_at: now - 3_600,
            },
        );
        let result = refresh(tmp.path(), &overview, &mut memory).await;
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].plan.as_deref(), Some("Plus"));
        assert_eq!(
            result[0].windows[0].used_percent, 64.0,
            "the remembered answer"
        );
        assert_eq!(result[0].checked_at, now - 3_600, "dated when it was given");
        assert_eq!(result[0].error, None);
        let saved = LimitsMemory::load(memory_file.clone());
        assert_eq!(saved.answers.len(), 1);
        assert!(
            !std::fs::read_to_string(&memory_file)
                .unwrap()
                .contains("example.test"),
            "no email is written down"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&memory_file)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn provider_windows_keep_real_percentages_and_reset_times() {
        let legacy = parse_claude(
            &json!({"five_hour":{"utilization":37,"resets_at":"2026-09-05T12:00:00Z"},"seven_day":null}),
        );
        let modern = parse_claude(
            &json!({"limits":[{"kind":"session","percent":37,"resets_at":"2026-09-05T12:00:00Z"}]}),
        );
        assert_eq!(legacy, modern);
        assert_eq!(legacy[0].used_percent, 37.0);
        let codex = parse_codex(
            &json!({"rate_limit":{"primary_window":{"used_percent":42,"limit_window_seconds":18000,"reset_at":1788609600},"secondary_window":{"used_percent":0,"limit_window_seconds":604800,"reset_at":1789214400}}}),
        );
        assert_eq!(codex[0].resets_at, legacy[0].resets_at);
        assert_eq!(codex[1].label, "Weekly limit");
        assert_eq!(codex[1].used_percent, 0.0);
        assert!(parse_codex(&json!({"rate_limit":null})).is_empty());
    }
}
