//! Provider-auth-free configuration that can follow a session between nodes.
//!
//! This module is intentionally narrower than a dotfiles synchronizer. It
//! exports a fixed list of provider settings, extracts only MCP topology from
//! Claude's mixed global-state file, rejects known inline credential fields,
//! and never overwrites a target conflict.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use diri_proto::{
    AccountProfile, PortableConfigApplyResult, PortableConfigBundle, PortableConfigFile,
    PortableConfigOmission, ProviderKind,
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::config::{hex_encode, random_hex, set_owner_directory, set_owner_file};
use crate::error::{NodeError, NodeResult};

pub const PORTABLE_CONFIG_VERSION: u32 = 1;

const MAX_SOURCE_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_BUNDLE_BYTES: usize = 1024 * 1024;
const CODEX_PATHS: &[&str] = &["config.toml", "AGENTS.md"];
const CLAUDE_PATHS: &[&str] = &[
    "settings.json",
    "CLAUDE.md",
    "keybindings.json",
    ".claude.json",
];

/// Capture the small declarative part of one provider installation. The
/// resulting bundle contains no provider login or MCP OAuth state, caches,
/// transcripts, or arbitrary dotfiles. Known inline MCP credential fields fail
/// closed and are reported as omissions.
pub fn capture(profile: &AccountProfile, config_home: &Path) -> NodeResult<PortableConfigBundle> {
    let mut files = Vec::new();
    let mut omitted = Vec::new();
    let paths = allowed_paths(profile.provider);
    for &relative in paths {
        let source = config_home.join(relative);
        if !source.exists() {
            continue;
        }
        let metadata = fs::symlink_metadata(&source)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            omitted.push(omission(relative, "not a regular file"));
            continue;
        }
        if metadata.len() > MAX_SOURCE_FILE_BYTES {
            omitted.push(omission(relative, "exceeds the portable-config size limit"));
            continue;
        }
        let raw = match fs::read_to_string(&source) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                omitted.push(omission(relative, "is not UTF-8 text"));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let content = match (profile.provider, relative) {
            (ProviderKind::Codex, "config.toml") => {
                portable_codex_config(&raw).map_err(|reason| omission(relative, reason))
            }
            (ProviderKind::Claude, "settings.json") => {
                portable_claude_settings(&raw).map_err(|reason| omission(relative, reason))
            }
            (ProviderKind::Claude, ".claude.json") => {
                portable_claude_mcp(&raw, &mut omitted).map_err(|reason| omission(relative, reason))
            }
            _ => reject_private_key_material(&raw)
                .map(|()| Some(raw))
                .map_err(|reason| omission(relative, reason)),
        };
        match content {
            Ok(Some(content)) => files.push(portable_file(relative, content)),
            Ok(None) => {}
            Err(item) => omitted.push(item),
        }
    }

    record_auth_omission(profile.provider, config_home, &mut omitted);
    let bundle = PortableConfigBundle {
        version: PORTABLE_CONFIG_VERSION,
        provider: profile.provider,
        profile_id: profile.id.clone(),
        files,
        omitted,
    };
    ensure_bundle_size(&bundle)?;
    Ok(bundle)
}

/// Add a portable bundle to a target profile without replacing local choices.
/// Claude's `.claude.json` receives a field-level MCP merge so its OAuth and
/// machine state remain untouched; every other file uses add/same/conflict.
pub fn apply(
    profile: &AccountProfile,
    config_home: &Path,
    bundle: PortableConfigBundle,
) -> NodeResult<PortableConfigApplyResult> {
    validate_bundle(profile, &bundle)?;
    fs::create_dir_all(config_home)?;
    set_owner_directory(config_home)?;
    let mut result = PortableConfigApplyResult {
        provider: profile.provider,
        profile_id: profile.id.clone(),
        installed: Vec::new(),
        unchanged: Vec::new(),
        conflicts: Vec::new(),
        omitted: bundle.omitted,
    };
    for file in bundle.files {
        if profile.provider == ProviderKind::Claude && file.path == ".claude.json" {
            merge_claude_mcp(config_home, &file, &mut result)?;
        } else {
            apply_file(config_home, &file, &mut result)?;
        }
    }
    Ok(result)
}

fn allowed_paths(provider: ProviderKind) -> &'static [&'static str] {
    match provider {
        ProviderKind::Claude => CLAUDE_PATHS,
        ProviderKind::Codex => CODEX_PATHS,
    }
}

fn portable_file(path: &str, content: String) -> PortableConfigFile {
    PortableConfigFile {
        path: path.into(),
        sha256: sha256(content.as_bytes()),
        content,
    }
}

fn portable_codex_config(raw: &str) -> Result<Option<String>, &'static str> {
    reject_private_key_material(raw)?;
    let value: toml::Value =
        toml::from_str(raw).map_err(|_| "is not valid TOML and was left on the source")?;
    if let Some(servers) = value.get("mcp_servers").and_then(toml::Value::as_table) {
        for server in servers.values().filter_map(toml::Value::as_table) {
            if table_is_nonempty(server.get("env")) || table_is_nonempty(server.get("http_headers"))
            {
                return Err(
                    "contains inline MCP environment or header values; use environment-variable references before syncing",
                );
            }
            if server
                .get("args")
                .and_then(toml::Value::as_array)
                .is_some_and(|arguments| {
                    string_arguments_have_inline_secret(
                        arguments.iter().filter_map(toml::Value::as_str),
                    )
                })
                || server
                    .get("url")
                    .and_then(toml::Value::as_str)
                    .is_some_and(url_has_inline_secret)
            {
                return Err(
                    "contains a credential-shaped MCP argument or URL; use environment-variable references before syncing",
                );
            }
        }
    }
    Ok(Some(raw.to_owned()))
}

fn table_is_nonempty(value: Option<&toml::Value>) -> bool {
    value
        .and_then(toml::Value::as_table)
        .is_some_and(|table| !table.is_empty())
}

fn portable_claude_settings(raw: &str) -> Result<Option<String>, &'static str> {
    reject_private_key_material(raw)?;
    let value: Value =
        serde_json::from_str(raw).map_err(|_| "is not valid JSON and was left on the source")?;
    if value
        .get("env")
        .and_then(Value::as_object)
        .is_some_and(|env| env.keys().any(|name| sensitive_name(name)))
    {
        return Err(
            "contains credential-shaped environment values; move them to the target environment before syncing",
        );
    }
    Ok(Some(raw.to_owned()))
}

fn portable_claude_mcp(
    raw: &str,
    omitted: &mut Vec<PortableConfigOmission>,
) -> Result<Option<String>, &'static str> {
    reject_private_key_material(raw)?;
    let value: Value =
        serde_json::from_str(raw).map_err(|_| "is not valid JSON and was left on the source")?;
    let Some(servers) = value.get("mcpServers").and_then(Value::as_object) else {
        return Ok(None);
    };
    let mut portable = Map::new();
    for (name, server) in servers {
        if mcp_server_has_inline_secret(server) {
            omitted.push(omission(
                format!(".claude.json#mcpServers.{name}"),
                "contains inline credentials; use environment references or sign in on the target",
            ));
        } else {
            portable.insert(name.clone(), server.clone());
        }
    }
    if portable.is_empty() {
        return Ok(None);
    }
    let mut content = serde_json::to_string_pretty(&json!({ "mcpServers": portable }))
        .map_err(|_| "could not serialize portable MCP configuration")?;
    content.push('\n');
    Ok(Some(content))
}

fn mcp_server_has_inline_secret(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return true;
    };
    for (key, value) in object {
        let normalized = normalize_name(key);
        if matches!(
            normalized.as_str(),
            "token" | "accesstoken" | "refreshtoken" | "clientsecret" | "apikey" | "password"
        ) && !is_environment_reference(value)
        {
            return true;
        }
        if matches!(normalized.as_str(), "env" | "headers")
            && value.as_object().is_some_and(|entries| {
                entries
                    .iter()
                    .any(|(name, value)| sensitive_name(name) && !is_environment_reference(value))
            })
        {
            return true;
        }
        if normalized == "args" && arguments_have_inline_secret(value) {
            return true;
        }
        if normalized == "url" && value.as_str().is_some_and(url_has_inline_secret) {
            return true;
        }
        if value.is_object() && mcp_server_has_inline_secret(value) {
            return true;
        }
    }
    false
}

fn arguments_have_inline_secret(value: &Value) -> bool {
    let Some(arguments) = value.as_array() else {
        return false;
    };
    string_arguments_have_inline_secret(arguments.iter().filter_map(Value::as_str))
}

fn string_arguments_have_inline_secret<'a>(arguments: impl Iterator<Item = &'a str>) -> bool {
    let arguments = arguments.collect::<Vec<_>>();
    for (index, argument) in arguments.iter().enumerate() {
        if let Some((name, value)) = argument.split_once('=')
            && sensitive_name(name)
            && !contains_environment_reference(value)
        {
            return true;
        }
        if sensitive_name(argument)
            && arguments
                .get(index + 1)
                .is_some_and(|value| !contains_environment_reference(value))
        {
            return true;
        }
        let normalized = argument.to_ascii_lowercase();
        if (normalized.contains("authorization:") || normalized.contains("api-key:"))
            && !contains_environment_reference(argument)
        {
            return true;
        }
    }
    false
}

fn url_has_inline_secret(value: &str) -> bool {
    let after_scheme = value.split_once("://").map_or(value, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.contains('@') {
        return true;
    }
    value
        .split_once('?')
        .map(|(_, query)| query.split('#').next().unwrap_or(query))
        .into_iter()
        .flat_map(|query| query.split('&'))
        .filter_map(|pair| pair.split_once('='))
        .any(|(name, value)| sensitive_name(name) && !contains_environment_reference(value))
}

fn is_environment_reference(value: &Value) -> bool {
    value.as_str().is_some_and(|value| {
        let trimmed = value.trim();
        trimmed.starts_with("${") && trimmed.ends_with('}')
    })
}

fn contains_environment_reference(value: &str) -> bool {
    value.contains("${") && value.contains('}')
}

fn sensitive_name(name: &str) -> bool {
    let normalized = normalize_name(name);
    normalized.contains("password")
        || normalized.contains("secret")
        || normalized.contains("token")
        || normalized.contains("credential")
        || normalized.contains("privatekey")
        || normalized.contains("apikey")
        || normalized.contains("accesskey")
        || normalized == "authorization"
}

fn normalize_name(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn reject_private_key_material(raw: &str) -> Result<(), &'static str> {
    if raw.contains("-----BEGIN PRIVATE KEY-----")
        || raw.contains("-----BEGIN RSA PRIVATE KEY-----")
        || raw.contains("-----BEGIN OPENSSH PRIVATE KEY-----")
    {
        Err("contains private-key material")
    } else {
        Ok(())
    }
}

fn record_auth_omission(
    provider: ProviderKind,
    config_home: &Path,
    omitted: &mut Vec<PortableConfigOmission>,
) {
    let (path, exists) = match provider {
        ProviderKind::Codex => ("auth.json", config_home.join("auth.json").exists()),
        ProviderKind::Claude => (
            ".claude.json#app-state-and-auth",
            config_home.join(".claude.json").exists(),
        ),
    };
    if exists {
        omitted.push(omission(
            path,
            "authentication remains node-local; sign in the matching profile on the target",
        ));
    }
}

fn validate_bundle(profile: &AccountProfile, bundle: &PortableConfigBundle) -> NodeResult<()> {
    if bundle.version != PORTABLE_CONFIG_VERSION {
        return Err(NodeError::Protocol(format!(
            "unsupported portable-config version {}",
            bundle.version
        )));
    }
    if bundle.provider != profile.provider || bundle.profile_id != profile.id {
        return Err(NodeError::Conflict(
            "portable config does not match the target account profile".into(),
        ));
    }
    let allowed = allowed_paths(profile.provider);
    let mut seen = BTreeSet::new();
    for file in &bundle.files {
        if !allowed.contains(&file.path.as_str()) {
            return Err(NodeError::BadRequest(format!(
                "portable config path `{}` is not allowed",
                file.path
            )));
        }
        if !seen.insert(&file.path) {
            return Err(NodeError::BadRequest(format!(
                "portable config path `{}` appears more than once",
                file.path
            )));
        }
        if sha256(file.content.as_bytes()) != file.sha256 {
            return Err(NodeError::BadRequest(format!(
                "portable config digest mismatch for `{}`",
                file.path
            )));
        }
    }
    ensure_bundle_size(bundle)
}

fn ensure_bundle_size(bundle: &PortableConfigBundle) -> NodeResult<()> {
    if serde_json::to_vec(bundle)?.len() > MAX_BUNDLE_BYTES {
        return Err(NodeError::BadRequest(
            "portable provider configuration exceeds 1 MiB".into(),
        ));
    }
    Ok(())
}

fn apply_file(
    config_home: &Path,
    file: &PortableConfigFile,
    result: &mut PortableConfigApplyResult,
) -> NodeResult<()> {
    let destination = config_home.join(&file.path);
    match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            result.conflicts.push(file.path.clone());
        }
        Ok(_) => {
            if sha256(&fs::read(&destination)?) == file.sha256 {
                result.unchanged.push(file.path.clone());
            } else {
                result.conflicts.push(file.path.clone());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            atomic_text(&destination, &file.content)?;
            result.installed.push(file.path.clone());
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn merge_claude_mcp(
    config_home: &Path,
    file: &PortableConfigFile,
    result: &mut PortableConfigApplyResult,
) -> NodeResult<()> {
    let incoming: Value = serde_json::from_str(&file.content)?;
    let Some(incoming_servers) = incoming.get("mcpServers").and_then(Value::as_object) else {
        return Err(NodeError::BadRequest(
            "portable Claude MCP config omitted mcpServers".into(),
        ));
    };
    let destination = config_home.join(".claude.json");
    if fs::symlink_metadata(&destination)
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
    {
        result.conflicts.push(".claude.json".into());
        return Ok(());
    }
    let mut target = if destination.exists() {
        match fs::read_to_string(&destination)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        {
            Some(Value::Object(object)) => Value::Object(object),
            _ => {
                result.conflicts.push(".claude.json".into());
                return Ok(());
            }
        }
    } else {
        Value::Object(Map::new())
    };
    let target_object = target.as_object_mut().expect("constructed as an object");
    let target_servers = target_object
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(target_servers) = target_servers.as_object_mut() else {
        result.conflicts.push(".claude.json#mcpServers".into());
        return Ok(());
    };
    let mut changed = false;
    for (name, server) in incoming_servers {
        let path = format!(".claude.json#mcpServers.{name}");
        match target_servers.get(name) {
            Some(current) if current == server => result.unchanged.push(path),
            Some(_) => result.conflicts.push(path),
            None => {
                target_servers.insert(name.clone(), server.clone());
                result.installed.push(path);
                changed = true;
            }
        }
    }
    if changed {
        let mut content = serde_json::to_string_pretty(&target)?;
        content.push('\n');
        atomic_text(&destination, &content)?;
    }
    Ok(())
}

fn atomic_text(path: &Path, content: &str) -> NodeResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| NodeError::BadRequest("portable path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    set_owner_directory(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    let temporary: PathBuf = parent.join(format!(".{name}.portable-{}", random_hex(8)?));
    let result = (|| {
        fs::write(&temporary, content)?;
        set_owner_file(&temporary)?;
        fs::rename(&temporary, path)?;
        Ok::<_, std::io::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(Into::into)
}

fn omission(path: impl Into<String>, reason: impl Into<String>) -> PortableConfigOmission {
    PortableConfigOmission {
        path: path.into(),
        reason: reason.into(),
    }
}

fn sha256(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::AccountProfile;

    fn profile(provider: ProviderKind) -> AccountProfile {
        AccountProfile {
            id: "work".into(),
            provider,
            label: "Work".into(),
            email: None,
            organization: None,
            tags: Vec::new(),
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn codex_bundle_carries_env_references_but_never_auth() {
        let directory = tempfile::tempdir().expect("config home");
        fs::write(
            directory.path().join("config.toml"),
            r#"model = "gpt-5"
[mcp_servers.github]
url = "https://example.test/mcp"
bearer_token_env_var = "GITHUB_TOKEN"
"#,
        )
        .expect("config");
        fs::write(
            directory.path().join("auth.json"),
            r#"{"access_token":"secret"}"#,
        )
        .expect("auth");

        let bundle = capture(&profile(ProviderKind::Codex), directory.path()).expect("bundle");
        assert_eq!(bundle.files.len(), 1);
        assert!(bundle.files[0].content.contains("GITHUB_TOKEN"));
        let encoded = serde_json::to_string(&bundle).expect("json");
        assert!(!encoded.contains("access_token"));
        assert!(!encoded.contains("secret"));
        assert!(bundle.omitted.iter().any(|item| item.path == "auth.json"));
    }

    #[test]
    fn codex_config_with_inline_mcp_values_is_omitted() {
        let directory = tempfile::tempdir().expect("config home");
        fs::write(
            directory.path().join("config.toml"),
            r#"[mcp_servers.github]
command = "server"
[mcp_servers.github.env]
GITHUB_TOKEN = "do-not-send"
"#,
        )
        .expect("config");

        let bundle = capture(&profile(ProviderKind::Codex), directory.path()).expect("bundle");
        assert!(bundle.files.is_empty());
        assert_eq!(bundle.omitted[0].path, "config.toml");
        assert!(
            !serde_json::to_string(&bundle)
                .unwrap()
                .contains("do-not-send")
        );
    }

    #[test]
    fn codex_config_with_credentials_in_mcp_arguments_is_omitted() {
        let directory = tempfile::tempdir().expect("config home");
        fs::write(
            directory.path().join("config.toml"),
            r#"[mcp_servers.github]
command = "server"
args = ["--api-key=do-not-send"]
"#,
        )
        .expect("config");

        let bundle = capture(&profile(ProviderKind::Codex), directory.path()).expect("bundle");
        assert!(bundle.files.is_empty());
        assert!(
            !serde_json::to_string(&bundle)
                .unwrap()
                .contains("do-not-send")
        );
    }

    #[test]
    fn claude_global_state_exports_only_safe_mcp_topology() {
        let directory = tempfile::tempdir().expect("config home");
        fs::write(
            directory.path().join(".claude.json"),
            r#"{
  "oauthAccount": {"accessToken": "never-send"},
  "machineID": "local-only",
  "mcpServers": {
    "safe": {"type": "http", "url": "https://example.test/mcp", "headers": {"Authorization": "${MCP_TOKEN}"}},
    "unsafe": {"type": "http", "url": "https://example.test/mcp", "headers": {"Authorization": "Bearer literal"}}
  }
}"#,
        )
        .expect("state");

        let bundle = capture(&profile(ProviderKind::Claude), directory.path()).expect("bundle");
        assert_eq!(bundle.files.len(), 1);
        assert!(bundle.files[0].content.contains("safe"));
        assert!(!bundle.files[0].content.contains("unsafe"));
        let encoded = serde_json::to_string(&bundle).expect("json");
        assert!(!encoded.contains("never-send"));
        assert!(!encoded.contains("local-only"));
        assert!(
            bundle
                .omitted
                .iter()
                .any(|item| item.path.ends_with("mcpServers.unsafe"))
        );
    }

    #[test]
    fn claude_mcp_omits_credentials_in_arguments_and_urls() {
        let directory = tempfile::tempdir().expect("config home");
        fs::write(
            directory.path().join(".claude.json"),
            r#"{"mcpServers":{
  "argument":{"command":"server","args":["--api-key=do-not-send"]},
  "url":{"type":"http","url":"https://example.test/mcp?token=do-not-send"},
  "reference":{"command":"server","args":["--api-key=${MCP_TOKEN}"]}
}}"#,
        )
        .expect("state");

        let bundle = capture(&profile(ProviderKind::Claude), directory.path()).expect("bundle");
        let encoded = serde_json::to_string(&bundle).expect("json");
        assert!(!encoded.contains("do-not-send"));
        assert!(encoded.contains("reference"));
        assert!(
            bundle
                .omitted
                .iter()
                .any(|item| item.path.ends_with(".argument"))
        );
        assert!(
            bundle
                .omitted
                .iter()
                .any(|item| item.path.ends_with(".url"))
        );
    }

    #[test]
    fn claude_mcp_merge_preserves_target_auth_and_reports_conflicts() {
        let source = tempfile::tempdir().expect("source");
        fs::write(
            source.path().join(".claude.json"),
            r#"{"mcpServers":{"same":{"command":"same"},"new":{"command":"new"},"different":{"command":"source"}}}"#,
        )
        .expect("source state");
        let provider = profile(ProviderKind::Claude);
        let bundle = capture(&provider, source.path()).expect("bundle");

        let target = tempfile::tempdir().expect("target");
        fs::write(
            target.path().join(".claude.json"),
            r#"{"oauthAccount":{"accessToken":"keep-me"},"mcpServers":{"same":{"command":"same"},"different":{"command":"target"}}}"#,
        )
        .expect("target state");
        let result = apply(&provider, target.path(), bundle).expect("apply");
        assert!(result.installed.iter().any(|path| path.ends_with(".new")));
        assert!(result.unchanged.iter().any(|path| path.ends_with(".same")));
        assert!(
            result
                .conflicts
                .iter()
                .any(|path| path.ends_with(".different"))
        );
        let state = fs::read_to_string(target.path().join(".claude.json")).expect("state");
        assert!(state.contains("keep-me"));
        assert!(state.contains(r#""command": "target""#));
        assert!(state.contains(r#""command": "new""#));
    }

    #[test]
    fn direct_file_conflicts_are_never_overwritten() {
        let source = tempfile::tempdir().expect("source");
        fs::write(source.path().join("AGENTS.md"), "source\n").expect("source config");
        let provider = profile(ProviderKind::Codex);
        let bundle = capture(&provider, source.path()).expect("bundle");
        let target = tempfile::tempdir().expect("target");
        fs::write(target.path().join("AGENTS.md"), "target\n").expect("target config");

        let result = apply(&provider, target.path(), bundle).expect("apply");
        assert_eq!(result.conflicts, vec!["AGENTS.md"]);
        assert_eq!(
            fs::read_to_string(target.path().join("AGENTS.md")).expect("target config"),
            "target\n"
        );
    }

    #[test]
    fn omission_metadata_cannot_bypass_the_bundle_limit() {
        let provider = profile(ProviderKind::Codex);
        let bundle = PortableConfigBundle {
            version: PORTABLE_CONFIG_VERSION,
            provider: ProviderKind::Codex,
            profile_id: provider.id.clone(),
            files: Vec::new(),
            omitted: vec![omission("config.toml", "x".repeat(MAX_BUNDLE_BYTES))],
        };
        let target = tempfile::tempdir().expect("target");

        let error = apply(&provider, target.path(), bundle).expect_err("oversized bundle");
        assert!(error.to_string().contains("exceeds 1 MiB"));
    }
}
