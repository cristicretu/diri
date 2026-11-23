//! What may travel. Export is an allowlist: a file is written only when it
//! classifies as a portable class, and every JSON document additionally
//! passes [`redact_secret_fields`]. Secret and runtime classes exist so the
//! exclusion is explicit and testable (`formal/MigrationSync.lean`).

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArtifactClass {
    // Portable profile.
    Note,
    AppSkill,
    LaunchRecipe,
    Schedule,
    Preference,
    InitLua,
    ProjectEntry,
    ManifestOverride,
    // Migration only.
    SessionRecord,
    ClaudeTranscript,
    CodexTranscript,
    // Never exported.
    Credential,
    ApiKey,
    AccountToken,
    SshKey,
    BrowserCookie,
    UnixSocket,
    HolderPid,
    TerminalSwap,
    AccountProfile,
    HostConfig,
    RemoteBinding,
    PhoneToken,
    McpHttpKey,
    TelemetryState,
    RunLog,
}

impl ArtifactClass {
    pub const ALL: [Self; 26] = [
        Self::Note,
        Self::AppSkill,
        Self::LaunchRecipe,
        Self::Schedule,
        Self::Preference,
        Self::InitLua,
        Self::ProjectEntry,
        Self::ManifestOverride,
        Self::SessionRecord,
        Self::ClaudeTranscript,
        Self::CodexTranscript,
        Self::Credential,
        Self::ApiKey,
        Self::AccountToken,
        Self::SshKey,
        Self::BrowserCookie,
        Self::UnixSocket,
        Self::HolderPid,
        Self::TerminalSwap,
        Self::AccountProfile,
        Self::HostConfig,
        Self::RemoteBinding,
        Self::PhoneToken,
        Self::McpHttpKey,
        Self::TelemetryState,
        Self::RunLog,
    ];

    pub const fn secret(self) -> bool {
        matches!(
            self,
            Self::Credential
                | Self::ApiKey
                | Self::AccountToken
                | Self::SshKey
                | Self::BrowserCookie
                | Self::UnixSocket
                | Self::HolderPid
                | Self::TerminalSwap
                | Self::AccountProfile
                | Self::HostConfig
                | Self::RemoteBinding
                | Self::PhoneToken
                | Self::McpHttpKey
                | Self::TelemetryState
                | Self::RunLog
        )
    }
}

/// Classes a portable profile (sync folder, Git repository) may hold.
pub const fn profile_allowed(class: ArtifactClass) -> bool {
    matches!(
        class,
        ArtifactClass::Note
            | ArtifactClass::AppSkill
            | ArtifactClass::LaunchRecipe
            | ArtifactClass::Schedule
            | ArtifactClass::Preference
            | ArtifactClass::InitLua
            | ArtifactClass::ProjectEntry
            | ArtifactClass::ManifestOverride
    )
}

/// Classes a migration bundle may hold: the profile plus sessions and their
/// conversations.
pub const fn migration_allowed(class: ArtifactClass) -> bool {
    profile_allowed(class)
        || matches!(
            class,
            ArtifactClass::SessionRecord
                | ArtifactClass::ClaudeTranscript
                | ArtifactClass::CodexTranscript
        )
}

/// Classifies a path inside a profile tree (`notes/x.md`). `None` for
/// anything outside the layout, which is then never read or written.
pub fn classify_profile_path(rel: &str) -> Option<ArtifactClass> {
    if !crate::files::safe_relative(rel) {
        return None;
    }
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.iter().any(|part| part.starts_with('.')) {
        return None;
    }
    let class = match parts.as_slice() {
        ["notes", name] if name.ends_with(".md") => ArtifactClass::Note,
        ["app-skills", _bundle, "app.json"] => ArtifactClass::AppSkill,
        ["app-skills", _bundle, "skills", name] if name.ends_with(".json") => {
            ArtifactClass::AppSkill
        }
        ["recipes.json"] => ArtifactClass::LaunchRecipe,
        ["schedules.json"] => ArtifactClass::Schedule,
        ["prefs.json"] => ArtifactClass::Preference,
        ["projects.json"] => ArtifactClass::ProjectEntry,
        ["config", "init.lua"] => ArtifactClass::InitLua,
        ["manifests", "overrides", name] if name.ends_with(".json") => {
            ArtifactClass::ManifestOverride
        }
        _ => return None,
    };
    debug_assert!(profile_allowed(class));
    Some(class)
}

/// Classifies a path inside a migration bundle.
pub fn classify_bundle_path(rel: &str) -> Option<ArtifactClass> {
    if let Some(profile) = rel.strip_prefix("profile/") {
        return classify_profile_path(profile);
    }
    if !crate::files::safe_relative(rel) {
        return None;
    }
    let parts: Vec<&str> = rel.split('/').collect();
    match parts.as_slice() {
        ["bundle.json"] | ["sessions.json"] => Some(ArtifactClass::SessionRecord),
        ["transcripts", "claude", name] if name.ends_with(".jsonl") => {
            Some(ArtifactClass::ClaudeTranscript)
        }
        ["transcripts", "codex", y, m, d, name]
            if name.ends_with(".jsonl")
                && [y, m, d]
                    .iter()
                    .all(|part| part.bytes().all(|b| b.is_ascii_digit())) =>
        {
            Some(ArtifactClass::CodexTranscript)
        }
        _ => None,
    }
}

/// Whether a JSON object key names something secret.
pub fn secret_key(key: &str) -> bool {
    let key: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    const WORDS: [&str; 12] = [
        "token",
        "secret",
        "password",
        "passwd",
        "apikey",
        "credential",
        "cookie",
        "authorization",
        "privatekey",
        "bearer",
        "sessionkey",
        "accesskey",
    ];
    WORDS.iter().any(|word| key.contains(word))
}

/// Removes every object member, at any depth, whose key is [`secret_key`],
/// and every `env`-style member whose *value* is keyed by one (`{"env":
/// {"OPENAI_API_KEY": ..}}` is caught by the key itself). Returns how many
/// members were removed.
pub fn redact_secret_fields(value: &mut Value) -> usize {
    match value {
        Value::Object(map) => {
            let before = map.len();
            map.retain(|key, _| !secret_key(key));
            let mut removed = before - map.len();
            for child in map.values_mut() {
                removed += redact_secret_fields(child);
            }
            removed
        }
        Value::Array(items) => items.iter_mut().map(redact_secret_fields).sum(),
        _ => 0,
    }
}

/// Redacts a JSON document's bytes. Non-JSON input is refused (`None`): a
/// file that should be JSON but is not cannot be checked, so it never leaves.
pub fn redact_json_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(bytes).ok()?;
    if redact_secret_fields(&mut value) == 0 {
        return Some(bytes.to_vec());
    }
    serde_json::to_vec_pretty(&value).ok()
}

/// Lines that look like they hold a credential, for text artifacts (notes,
/// `init.lua`). Such files still travel — they are the user's own writing —
/// but export reports them so the user can see it happened.
pub fn looks_like_secret_text(text: &str) -> bool {
    const MARKERS: [&str; 6] = [
        "-----BEGIN OPENSSH PRIVATE KEY-----",
        "-----BEGIN RSA PRIVATE KEY-----",
        "-----BEGIN PRIVATE KEY-----",
        "sk-ant-",
        "ghp_",
        "xoxb-",
    ];
    MARKERS.iter().any(|marker| text.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlists_never_admit_a_secret_class() {
        for class in ArtifactClass::ALL {
            if class.secret() {
                assert!(!profile_allowed(class), "{class:?}");
                assert!(!migration_allowed(class), "{class:?}");
            }
            if profile_allowed(class) {
                assert!(migration_allowed(class));
            }
        }
    }

    #[test]
    fn only_the_layout_classifies() {
        assert_eq!(
            classify_profile_path("notes/a.md"),
            Some(ArtifactClass::Note)
        );
        assert_eq!(
            classify_profile_path("app-skills/com.apple.Notes/skills/x.json"),
            Some(ArtifactClass::AppSkill)
        );
        for rejected in [
            "notes/../x.md",
            "/etc/passwd",
            "notes/.history/a.md",
            "app-skills/com.x/runs.jsonl",
            "accounts.json",
            "hosts.json",
            "daemon.sock",
            "mcp-http.key",
            "claude-logins/a/.credentials.json",
            "logs/s_1.bin",
        ] {
            assert_eq!(classify_profile_path(rejected), None, "{rejected}");
            assert_eq!(classify_bundle_path(rejected), None, "{rejected}");
        }
        assert_eq!(
            classify_bundle_path("transcripts/codex/2026/11/01/rollout-x.jsonl"),
            Some(ArtifactClass::CodexTranscript)
        );
        assert_eq!(
            classify_bundle_path("transcripts/codex/../../x.jsonl"),
            None
        );
    }

    #[test]
    fn redaction_removes_secret_members_at_every_depth() {
        let mut value = serde_json::json!({
            "name": "claude",
            "env": {"ANTHROPIC_API_KEY": "sk-ant-x", "TERM": "xterm"},
            "auth": {"accessToken": "t", "kind": "oauth"},
            "list": [{"client_secret": "s", "ok": 1}],
        });
        assert_eq!(redact_secret_fields(&mut value), 3);
        let text = value.to_string();
        assert!(!text.contains("sk-ant"));
        assert!(text.contains("TERM"));
        assert!(text.contains("oauth"));
        assert_eq!(redact_secret_fields(&mut value), 0);
    }

    #[test]
    fn unreadable_json_never_leaves() {
        assert_eq!(redact_json_bytes(b"{not json"), None);
    }
}
