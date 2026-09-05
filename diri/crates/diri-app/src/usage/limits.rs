//! Provider-reported subscription windows. These are independent of transcript
//! cost estimates and context occupancy. Credentials stay in provider storage;
//! only parsed percentages, reset times and account labels reach the UI.
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use diri_proto::AgentAccountCatalog;
use serde_json::Value;
use tokio::{io::AsyncWriteExt, process::Command};

#[derive(Clone, Debug, PartialEq)]
pub struct LimitWindow {
    pub label: String,
    pub used_percent: f64,
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AccountLimits {
    pub provider: &'static str,
    pub account: String,
    pub windows: Vec<LimitWindow>,
    pub checked_at: i64,
    pub error: Option<&'static str>,
}

struct AccountSource {
    provider: &'static str,
    label: String,
    directory: PathBuf,
    #[cfg(target_os = "macos")]
    keychain: Option<String>,
}

fn sources(home: &Path, accounts: &AgentAccountCatalog) -> Vec<AccountSource> {
    [
        ("claude-code", "Claude", ".claude", "CLAUDE_CONFIG_DIR"),
        ("codex", "Codex", ".codex", "CODEX_HOME"),
    ]
    .into_iter()
    .map(|(agent, provider, directory, variable)| {
        let profile = accounts
            .profiles
            .iter()
            .find(|p| p.agent == agent && p.host.is_none() && p.is_default);
        let config = profile.map(|p| PathBuf::from(&p.config_home)).or_else(|| {
            std::env::var_os(variable)
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
        });
        let config = config.map(|path| {
            path.to_str()
                .and_then(|path| path.strip_prefix("~/"))
                .map_or_else(|| path.clone(), |relative| home.join(relative))
        });
        // Explicit profiles scrub ambient credential overrides at launch.
        // Otherwise mirror Claude's credential-store override, including its
        // empty-string pin to the default store.
        let config = if agent == "claude-code" && profile.is_none() {
            match std::env::var_os("CLAUDE_SECURESTORAGE_CONFIG_DIR") {
                Some(value) => (!value.is_empty()).then(|| PathBuf::from(value)),
                None => config,
            }
        } else {
            config
        };
        #[cfg(target_os = "macos")]
        let keychain = (agent == "claude-code").then(|| {
            config.as_ref().map_or_else(
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
        });
        AccountSource {
            provider,
            label: profile.map_or_else(|| "CLI account".into(), |p| p.label.clone()),
            #[cfg(target_os = "macos")]
            keychain,
            directory: config.unwrap_or_else(|| home.join(directory)),
        }
    })
    .collect()
}

pub(crate) async fn refresh(home: &Path, accounts: &AgentAccountCatalog) -> Vec<AccountLimits> {
    let mut result = Vec::new();
    for source in sources(home, accounts) {
        let now = super::Clock::read(&super::SystemClock).unix_seconds;
        let fetched = fetch(&source).await;
        let (windows, error) = match fetched {
            Ok(windows) => (windows, None),
            // A sign-in can change at the same path. Never reuse another
            // account's last percentages when its replacement fails to load.
            Err(error) => (Vec::new(), Some(error)),
        };
        result.push(AccountLimits {
            provider: source.provider,
            account: source.label,
            windows,
            checked_at: now,
            error,
        });
    }
    result
}

async fn fetch(source: &AccountSource) -> Result<Vec<LimitWindow>, &'static str> {
    let (url, mut headers) = if source.provider == "Claude" {
        let value = claude_credentials(source)
            .await
            .ok_or("Sign in to Claude to see limits")?;
        let token = value
            .pointer("/claudeAiOauth/accessToken")
            .and_then(Value::as_str)
            .ok_or("Claude subscription sign-in unavailable")?;
        (
            "https://api.anthropic.com/api/oauth/usage",
            vec![
                format!("Authorization: Bearer {token}"),
                "anthropic-beta: oauth-2025-04-20".into(),
                "User-Agent: claude-code/2.1.0".into(),
            ],
        )
    } else {
        let value = read_json(&source.directory.join("auth.json"))
            .await
            .ok_or("Sign in to Codex to see limits")?;
        let token = value
            .pointer("/tokens/access_token")
            .and_then(Value::as_str)
            .ok_or("Codex subscription sign-in unavailable")?;
        let mut headers = vec![
            format!("Authorization: Bearer {token}"),
            "User-Agent: diri".into(),
        ];
        if let Some(account) = value.pointer("/tokens/account_id").and_then(Value::as_str) {
            headers.push(format!("ChatGPT-Account-Id: {account}"));
        }
        ("https://chatgpt.com/backend-api/wham/usage", headers)
    };
    headers.push("Accept: application/json".into());
    let body = http_get(url, &headers).await?;
    let windows = if source.provider == "Claude" {
        parse_claude(&body)
    } else {
        parse_codex(&body)
    };
    if windows.is_empty() {
        Err("No subscription limits reported")
    } else {
        Ok(windows)
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
    #[cfg(target_os = "macos")]
    if let Some(service) = &source.keychain {
        let mut command = Command::new("/usr/bin/security");
        command
            .args(["find-generic-password", "-s", service, "-w"])
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
    read_json(&source.directory.join(".credentials.json")).await
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
        "401" | "403" => Err("Sign in again to refresh limits"),
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

/// Design fixture; never mixed into the live account snapshot.
pub(crate) fn preview() -> Vec<AccountLimits> {
    vec![AccountLimits {
        provider: "Claude",
        account: "CLI account".into(),
        windows: vec![
            LimitWindow {
                label: "5-hour limit".into(),
                used_percent: 37.0,
                resets_at: Some(super::Clock::read(&super::SystemClock).unix_seconds + 8_040),
            },
            LimitWindow {
                label: "Weekly limit".into(),
                used_percent: 68.0,
                resets_at: Some(super::Clock::read(&super::SystemClock).unix_seconds + 172_800),
            },
        ],
        checked_at: super::Clock::read(&super::SystemClock).unix_seconds,
        error: None,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn limits_resolve_local_default_profiles_without_other_account_fallback() {
        let profile = |id: &str, agent: &str, host: Option<&str>, directory: &str| {
            diri_proto::AgentAccountProfile {
                id: id.into(),
                label: id.into(),
                agent: agent.into(),
                host: host.map(str::to_owned),
                config_home: directory.into(),
                is_default: true,
            }
        };
        let catalog = AgentAccountCatalog {
            profiles: vec![
                profile("remote", "claude-code", Some("server"), "/remote/claude"),
                profile("work", "claude-code", None, "~/accounts/work-claude"),
                profile("personal", "codex", None, "/accounts/codex"),
            ],
        };
        let resolved = sources(Path::new("/local/home"), &catalog);
        assert_eq!(
            resolved[0].directory,
            Path::new("/local/home/accounts/work-claude")
        );
        assert_eq!(resolved[0].label, "work");
        assert_eq!(resolved[1].directory, Path::new("/accounts/codex"));
        #[cfg(target_os = "macos")]
        assert_ne!(
            resolved[0].keychain.as_deref(),
            Some("Claude Code-credentials")
        );
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
