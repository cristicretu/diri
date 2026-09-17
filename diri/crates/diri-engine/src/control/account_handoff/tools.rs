//! Transfer direct MCP definitions and file-backed MCP OAuth grants only.
//! Agent login credentials (`auth.json`, `claudeAiOauth`) are never copied.
use super::*;
use std::collections::BTreeMap;
use toml_edit::{DocumentMut, Item};

mod assets;
#[cfg(target_os = "macos")]
mod keychain;

const MAX_SETTINGS: usize = 1024 * 1024;

pub(super) struct ToolTransfer;
struct Patch {
    location: Location,
    expected: Option<Vec<u8>>,
    bytes: Vec<u8>,
    remote: Option<(
        diri_proto::HostEntry,
        Arc<crate::remote::manager::RemoteManager>,
    )>,
}

impl ToolTransfer {
    pub(super) fn prepare(handoffs: &[PreparedHandoff]) -> Result<Self, ControlError> {
        Self::patches(handoffs)?;
        assets::preflight(handoffs)?;
        #[cfg(target_os = "macos")]
        keychain::preflight(handoffs)?;
        Ok(Self)
    }

    pub(super) fn install(&self, handoffs: &[PreparedHandoff]) -> Result<(), ControlError> {
        // Re-read after every source Agent has stopped, retaining freshly refreshed tokens.
        let patches = Self::patches(handoffs)?;
        assets::install(handoffs)?;
        #[cfg(target_os = "macos")]
        keychain::install(handoffs)?;
        for patch in patches.values() {
            let storage = Storage {
                remote: patch.remote.clone(),
            };
            replace(
                &storage,
                &patch.location,
                patch.expected.as_deref(),
                &patch.bytes,
            )?;
        }
        Ok(())
    }

    fn patches(handoffs: &[PreparedHandoff]) -> Result<BTreeMap<PathBuf, Patch>, ControlError> {
        let mut patches = BTreeMap::new();
        let mut visited = std::collections::HashSet::new();
        let mut grants = BTreeMap::new();
        for handoff in handoffs {
            if handoff.source_location.root == handoff.target_location.root
                || !visited.insert(handoff.source_location.root.clone())
            {
                continue;
            }
            let source = &handoff.source_location.root;
            let target = &handoff.target_location.root;
            let codex = handoff.source.kind == AgentKind::CODEX;
            let config_name = if codex { "config.toml" } else { ".claude.json" };
            let mut config_source = Location {
                root: source.clone(),
                relative: config_name.into(),
            };
            // Claude's default home stores this file alongside .claude, whereas
            // custom CLAUDE_CONFIG_DIR homes keep it inside the profile.
            if !codex && handoff.source.account_profile.is_none() {
                config_source.root = source.parent().ok_or_else(invalid_settings)?.to_owned();
            }
            let destination = Location {
                root: target.clone(),
                relative: config_name.into(),
            };
            let source_bytes = read_settings(&handoff.storage, &config_source)?;
            if let Some(source_bytes) = source_bytes {
                let patch = patch_for(&mut patches, destination, &handoff.storage)?;
                patch.bytes = if codex {
                    merge_codex(&source_bytes, &patch.bytes)?
                } else {
                    merge_claude(&source_bytes, &patch.bytes)?
                };
            }
            let credentials = Location {
                root: source.clone(),
                relative: ".credentials.json".into(),
            };
            if let Some(bytes) = read_settings(&handoff.storage, &credentials)? {
                let parsed = object(&bytes)?;
                let entries = if codex {
                    Some(&parsed)
                } else {
                    parsed.get("mcpOAuth").and_then(Value::as_object)
                };
                if let Some(entries) = entries {
                    for (key, value) in entries {
                        if let Some(previous) = grants.insert(key.clone(), value.clone())
                            && previous != *value
                        {
                            return Err(ControlError::bad_request(
                                "Source accounts have different MCP authorizations for the same connection. Use distinct server names before combining them.",
                            ));
                        }
                    }
                }
                let destination = Location {
                    root: target.clone(),
                    relative: ".credentials.json".into(),
                };
                let patch = patch_for(&mut patches, destination, &handoff.storage)?;
                patch.bytes = if codex {
                    merge_codex_credentials(&bytes, &patch.bytes)?
                } else {
                    merge_json_fields(&bytes, &patch.bytes, &["mcpOAuth"])?
                };
            }
        }
        patches.retain(|_, p| {
            p.expected.as_deref() != Some(p.bytes.as_slice())
                && !(p.expected.is_none() && (p.bytes.is_empty() || p.bytes == b"{}"))
        });
        Ok(patches)
    }
}

fn invalid_settings() -> ControlError {
    ControlError::bad_request(
        "MCP settings could not be preserved safely; check the source and destination configuration",
    )
}

fn read_settings(storage: &Storage, location: &Location) -> Result<Option<Vec<u8>>, ControlError> {
    let bytes = storage.read(location)?;
    if bytes.as_ref().is_some_and(|b| b.len() > MAX_SETTINGS) {
        return Err(invalid_settings());
    }
    Ok(bytes)
}

fn patch_for<'a>(
    patches: &'a mut BTreeMap<PathBuf, Patch>,
    location: Location,
    storage: &Storage,
) -> Result<&'a mut Patch, ControlError> {
    use std::collections::btree_map::Entry;
    Ok(match patches.entry(location.path()) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => {
            let expected = read_settings(storage, &location)?;
            entry.insert(Patch {
                bytes: expected.clone().unwrap_or_default(),
                location,
                expected,
                remote: storage.remote.clone(),
            })
        }
    })
}

fn parse_toml(bytes: &[u8]) -> Result<DocumentMut, ControlError> {
    std::str::from_utf8(bytes)
        .map_err(|_| invalid_settings())?
        .parse()
        .map_err(|_| invalid_settings())
}

fn same_toml(a: &Item, b: &Item) -> Result<bool, ControlError> {
    fn semantic(item: &Item) -> Result<Value, ControlError> {
        let mut document = DocumentMut::new();
        document["value"] = item.clone();
        toml_edit::de::from_str(&document.to_string()).map_err(|_| invalid_settings())
    }
    Ok(semantic(a)? == semantic(b)?)
}

fn merge_claude(source: &[u8], destination: &[u8]) -> Result<Vec<u8>, ControlError> {
    let merged = merge_json_fields(source, destination, &["mcpServers"])?;
    let source = object(source)?;
    let mut target = object(&merged)?;
    if let Some(projects) = source.get("projects").and_then(Value::as_object) {
        for (path, project) in projects {
            if let Some(servers) = project.get("mcpServers") {
                let projects = target
                    .entry("projects".to_owned())
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
                    .ok_or_else(invalid_settings)?;
                let destination = projects.entry(path.clone()).or_insert_with(|| json!({}));
                let source = serde_json::to_vec(&json!({"mcpServers":servers}))
                    .map_err(|_| invalid_settings())?;
                let bytes = serde_json::to_vec(destination).map_err(|_| invalid_settings())?;
                *destination =
                    serde_json::from_slice(&merge_json_fields(&source, &bytes, &["mcpServers"])?)
                        .map_err(|_| invalid_settings())?;
            }
        }
    }
    serde_json::to_vec_pretty(&target).map_err(|_| invalid_settings())
}

fn merge_codex(source: &[u8], destination: &[u8]) -> Result<Vec<u8>, ControlError> {
    let source = parse_toml(source)?;
    let mut target = parse_toml(destination)?;
    for section in ["mcp_servers", "plugins", "marketplaces"] {
        if let Some(servers) = source.get(section) {
            let servers = servers.as_table_like().ok_or_else(invalid_settings)?;
            if target.get(section).is_none() {
                target[section] = Item::Table(toml_edit::Table::new());
            }
            let table = target[section]
                .as_table_like_mut()
                .ok_or_else(invalid_settings)?;
            for (name, definition) in servers.iter() {
                if let Some(existing) = table.get(name) {
                    // Compare parsed semantics rather than comments/formatting.
                    if !same_toml(existing, definition)? {
                        return Err(ControlError::bad_request(
                            "Accounts have conflicting MCP or plugin definitions. Resolve the conflicting names before switching.",
                        ));
                    }
                } else {
                    table.insert(name, definition.clone());
                }
            }
        }
    }
    for key in [
        "mcp_oauth_credentials_store",
        "mcp_oauth_callback_port",
        "mcp_oauth_callback_url",
    ] {
        if let Some(value) = source.get(key) {
            if target
                .get(key)
                .is_some_and(|v| v.to_string().trim() != value.to_string().trim())
            {
                return Err(invalid_settings());
            }
            target[key] = value.clone();
        }
    }
    Ok(target.to_string().into_bytes())
}

fn object(bytes: &[u8]) -> Result<serde_json::Map<String, Value>, ControlError> {
    if bytes.is_empty() {
        return Ok(Default::default());
    }
    serde_json::from_slice::<Value>(bytes)
        .map_err(|_| invalid_settings())?
        .as_object()
        .cloned()
        .ok_or_else(invalid_settings)
}

fn merge_json_fields(
    source: &[u8],
    destination: &[u8],
    keys: &[&str],
) -> Result<Vec<u8>, ControlError> {
    let source = object(source)?;
    let mut target = object(destination)?;
    for key in keys {
        if let Some(value) = source.get(*key) {
            let entries = value.as_object().ok_or_else(invalid_settings)?;
            let target_entries = target
                .entry((*key).to_owned())
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or_else(invalid_settings)?;
            for (name, entry) in entries {
                if *key == "mcpServers" && target_entries.get(name).is_some_and(|old| old != entry)
                {
                    return Err(invalid_settings());
                }
                target_entries.insert(name.clone(), entry.clone());
            }
        }
    }
    serde_json::to_vec_pretty(&target).map_err(|_| invalid_settings())
}

fn merge_codex_credentials(source: &[u8], destination: &[u8]) -> Result<Vec<u8>, ControlError> {
    let source = object(source)?;
    let mut target = object(destination)?;
    for (key, value) in source {
        let name = value
            .get("server_name")
            .and_then(Value::as_str)
            .ok_or_else(invalid_settings)?;
        // Enterprise identity and hosted executor grants are account-bound.
        if name.starts_with("ema-idp:")
            || name.starts_with("executor:")
            || value.get("executor_owned") == Some(&Value::Bool(true))
        {
            continue;
        }
        if value.get("server_url").and_then(Value::as_str).is_none()
            || value.get("client_id").and_then(Value::as_str).is_none()
            || value.get("access_token").and_then(Value::as_str).is_none()
        {
            return Err(invalid_settings());
        }
        target.insert(key, value);
    }
    serde_json::to_vec_pretty(&target).map_err(|_| invalid_settings())
}

fn replace(
    storage: &Storage,
    location: &Location,
    expected: Option<&[u8]>,
    bytes: &[u8],
) -> Result<(), ControlError> {
    if bytes.len() > MAX_SETTINGS || read_settings(storage, location)?.as_deref() != expected {
        return Err(invalid_settings());
    }
    if let Some((host, manager)) = &storage.remote {
        let mut input = location.input();
        input.extend_from_slice(
            format!(
                "{}\n{}\n",
                crate::inject::uuid_v4(),
                expected.map_or(-1, |b| b.len() as i64)
            )
            .as_bytes(),
        );
        input.extend_from_slice(expected.unwrap_or_default());
        input.extend_from_slice(bytes);
        let output = manager
            .run_fixed_script(host, REPLACE_SETTINGS, input, Duration::from_secs(30), 4096)
            .map_err(|_| invalid_settings())?;
        if !output.status.success() {
            return Err(invalid_settings());
        }
        return Ok(());
    }
    check_directories(location, true)?;
    let temporary = location
        .directory()
        .join(format!(".diri-tools-{}.tmp", crate::inject::uuid_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|_| invalid_settings())?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| invalid_settings())?;
        if read_settings(storage, location)?.as_deref() != expected {
            return Err(invalid_settings());
        }
        fs::rename(&temporary, location.path()).map_err(|_| invalid_settings())
    })();
    let _ = fs::remove_file(temporary);
    result
}

// Only fixed file names reach this script, and all paths are stdin data. Compare
// the old bytes immediately before replacement; never overwrite a concurrent edit.
const REPLACE_SETTINGS: &str = r#"sh -c 'IFS= read -r root && IFS= read -r name && IFS= read -r nonce && IFS= read -r count || exit 73; case "$name" in config.toml|.claude.json|.credentials.json) ;; *) exit 73;; esac; umask 077; [ ! -L "$root" ] && [ -d "$root" ] && [ -O "$root" ] || exit 73; target="$root/$name"; tmp="$root/.diri-tools-$nonce.tmp"; old="$root/.diri-tools-$nonce.old"; set -C; : > "$tmp" && : > "$old" || exit 73; cleanup() { rm -f "$tmp" "$old"; }; trap cleanup 0; if [ "$count" -ge 0 ]; then dd bs=1 count="$count" >> "$old" 2>/dev/null || exit 73; fi; cat >> "$tmp" || exit 73; [ "$(wc -c < "$tmp")" -le 1048576 ] || exit 73; [ ! -L "$target" ] || exit 73; if [ "$count" -ge 0 ]; then [ -f "$target" ] && [ -O "$target" ] && cmp -s "$old" "$target" || exit 73; else [ ! -e "$target" ] || exit 73; fi; mv -f "$tmp" "$target"'"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn codex_merges_tools_without_copying_provider_or_permission_policy() {
        let source = b"model='source-model'\napproval_policy='never'\ncli_auth_credentials_store='file'\n[mcp_servers.docs]\ncommand='docs-server'\nargs=['--stdio']\n";
        let target = b"# Keep my policy\nmodel='destination-model'\n[mcp_servers.existing]\nurl='https://example.test/mcp'\n";
        let merged = merge_codex(source, target).unwrap();
        let config = parse_toml(&merged).unwrap();
        assert_eq!(config["model"].as_str(), Some("destination-model"));
        assert!(config.get("approval_policy").is_none());
        assert!(config.get("cli_auth_credentials_store").is_none());
        assert!(config["mcp_servers"].get("docs").is_some());
        assert!(config["mcp_servers"].get("existing").is_some());
        assert!(
            String::from_utf8(merged.clone())
                .unwrap()
                .contains("# Keep my policy")
        );
        assert_eq!(merge_codex(source, &merged).unwrap(), merged);
    }

    #[test]
    fn equivalent_toml_formatting_is_accepted_but_conflicting_identity_is_not() {
        let source = b"[mcp_servers.docs]\nurl='https://example.test'\nenabled=true\n";
        let same =
            b"[mcp_servers.docs]\n# a comment\nenabled = true\nurl = \"https://example.test\"\n";
        merge_codex(source, same).unwrap();
        assert!(
            merge_codex(
                source,
                b"[mcp_servers.docs]\nurl='https://different.test'\n"
            )
            .is_err()
        );
    }

    #[test]
    fn claude_preserves_global_and_local_tools_without_importing_login_or_trust() {
        let source = br#"{"oauthAccount":{"emailAddress":"source"},"mcpServers":{"docs":{"type":"http","url":"https://example.test"}},"projects":{"/repo":{"hasTrustDialogAccepted":true,"mcpServers":{"local":{"command":"local-server"}}}}}"#;
        let target = br#"{"oauthAccount":{"emailAddress":"target"},"projects":{"/repo":{"hasTrustDialogAccepted":false}}}"#;
        let merged: Value = serde_json::from_slice(&merge_claude(source, target).unwrap()).unwrap();
        assert_eq!(merged["oauthAccount"]["emailAddress"], "target");
        assert_eq!(merged["projects"]["/repo"]["hasTrustDialogAccepted"], false);
        assert!(merged["projects"]["/repo"]["mcpServers"]["local"].is_object());
        assert!(merged["mcpServers"]["docs"].is_object());
        let merged = merge_json_fields(br#"{"claudeAiOauth":{"accessToken":"source-login"},"mcpOAuth":{"docs":{"accessToken":"tool"}}}"#, br#"{"claudeAiOauth":{"accessToken":"target-login"}}"#, &["mcpOAuth"]).unwrap();
        let value: Value = serde_json::from_slice(&merged).unwrap();
        assert_eq!(value["claudeAiOauth"]["accessToken"], "target-login");
        assert_eq!(value["mcpOAuth"]["docs"]["accessToken"], "tool");
    }

    #[test]
    fn codex_oauth_excludes_hosted_and_enterprise_identities() {
        let entry = |name| json!({"server_name":name,"server_url":"https://example.test","client_id":"client","access_token":"tool-token"});
        let source = serde_json::to_vec(&json!({"docs|hash":entry("docs"),"enterprise":entry("ema-idp:workspace"),"hosted":entry("executor:hosted")})).unwrap();
        let result = object(&merge_codex_credentials(&source, b"{}").unwrap()).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains_key("docs|hash"));
        let error = merge_codex_credentials(b"secret-broken-json", b"{}").unwrap_err();
        assert!(!error.message.contains("secret-broken-json"));
    }

    #[test]
    fn local_settings_replacement_checks_edits_symlinks_and_private_modes() {
        let temp = tempfile::tempdir().unwrap();
        let storage = Storage { remote: None };
        let location = Location {
            root: temp.path().to_owned(),
            relative: "config.toml".into(),
        };
        replace(&storage, &location, None, b"# first\n").unwrap();
        assert_eq!(
            fs::metadata(location.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(replace(&storage, &location, None, b"wrong").is_err());
        assert_eq!(fs::read(location.path()).unwrap(), b"# first\n");
        replace(&storage, &location, Some(b"# first\n"), b"# second\n").unwrap();
        fs::remove_file(location.path()).unwrap();
        let victim = temp.path().join("victim");
        fs::write(&victim, b"unchanged").unwrap();
        symlink(&victim, location.path()).unwrap();
        assert!(replace(&storage, &location, Some(b"unchanged"), b"wrong").is_err());
        assert_eq!(fs::read(victim).unwrap(), b"unchanged");
    }

    #[test]
    fn remote_settings_script_preserves_literal_paths_and_rejects_concurrent_edits() {
        use std::process::{Command, Stdio};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tools ' $(touch SHOULD_NOT_EXIST)");
        fs::create_dir(&root).unwrap();
        let run = |expected: Option<&[u8]>, bytes: &[u8]| {
            let mut input = format!(
                "{}\nconfig.toml\nfixture\n{}\n",
                root.display(),
                expected.map_or(-1, |b| b.len() as i64)
            )
            .into_bytes();
            input.extend_from_slice(expected.unwrap_or_default());
            input.extend_from_slice(bytes);
            let mut child = Command::new("/bin/sh")
                .args(["-c", REPLACE_SETTINGS])
                .current_dir(temp.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(&input).unwrap();
            child.wait_with_output().unwrap().status.success()
        };
        assert!(run(None, b"first"));
        assert!(!run(Some(b"wrong"), b"bad"));
        assert!(run(Some(b"first"), b"second"));
        assert_eq!(fs::read(root.join("config.toml")).unwrap(), b"second");
        assert!(!temp.path().join("SHOULD_NOT_EXIST").exists());
        assert_eq!(fs::read_dir(root).unwrap().count(), 1);
    }
}
