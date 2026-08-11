//! Remote execution-host catalog (`hosts.json`).
//!
//! The file lives in the Engine's Application Support directory and is read by
//! both the Engine at spawn time and the local UI for host selection. A missing
//! or invalid file means "no remote hosts": the picker shows Local only.

use std::fs;
use std::io;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use serde::{Deserialize, Serialize};

/// First-party management endpoint for a host. The enrollment token itself is
/// kept in a separate owner-only file on the client machine.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostNodeConfig {
    /// `tcp://<tailscale-ip>:7337` (plain `IP:PORT` is also accepted).
    pub endpoint: String,
    /// Local path to the enrollment token, never the token value.
    pub token_file: String,
    /// Expected stable identity after the first successful hello. Optional
    /// during enrollment; clients reject a mismatch once pinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}

/// One configured SSH execution host with an optional first-party node used by
/// enhanced fleet operations. Remote Holder sessions always use `ssh`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostEntry {
    /// Stable identifier referenced by `SessionSpawnParams.host` /
    /// `SessionRecord.host`.
    pub id: String,
    /// Human-readable name for pickers and badges; falls back to `id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// SSH destination (`user@host`, or an ssh_config alias).
    pub ssh: String,
    /// Default remote working directory for new sessions (e.g. `~/code`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<HostNodeConfig>,
}

impl HostEntry {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct HostsConfig {
    #[serde(default)]
    pub hosts: Vec<HostEntry>,
}

impl HostsConfig {
    /// Parses the raw file contents; `None` on malformed JSON.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }

    /// Loads `hosts.json` from `path`. Missing or invalid file yields the
    /// empty catalog (Local only) — never an error.
    pub fn load(path: impl AsRef<Path>) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| Self::parse(&bytes))
            .unwrap_or_default()
    }

    /// Atomically writes the catalog with owner-only permissions. Keeping the
    /// persistence rules here gives every host-management surface the same
    /// behavior and prevents callers from hand-rolling the JSON schema.
    pub fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "host catalog path has no parent",
            )
        })?;
        fs::create_dir_all(parent)?;

        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("hosts.json");
        let temporary = parent.join(format!(".{file_name}.tmp-{}", std::process::id()));
        let mut data = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        data.push(b'\n');

        let result = (|| {
            fs::write(&temporary, data)?;
            #[cfg(unix)]
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
            fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn host(&self, id: &str) -> Option<&HostEntry> {
        self.hosts.iter().find(|host| host.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_schema() {
        let json = br#"{
            "hosts": [
                { "id": "forge", "name": "Forge", "ssh": "cristi@forge", "defaultCwd": "~/code",
                  "node": { "endpoint": "tcp://100.64.0.2:7337", "tokenFile": "~/.config/dirijor/forge.token", "nodeId": "node-forge" } },
                { "id": "studio", "name": "Studio Mac", "ssh": "studio.local" }
            ]
        }"#;
        let config = HostsConfig::parse(json).expect("valid schema");
        assert_eq!(config.hosts.len(), 2);
        let host = config.host("forge").expect("forge present");
        assert_eq!(host.display_name(), "Forge");
        assert_eq!(host.ssh, "cristi@forge");
        assert_eq!(host.default_cwd.as_deref(), Some("~/code"));
        assert_eq!(
            host.node.as_ref().map(|node| node.endpoint.as_str()),
            Some("tcp://100.64.0.2:7337")
        );
        let studio = config.host("studio").expect("studio present");
        assert_eq!(studio.display_name(), "Studio Mac");
        assert_eq!(studio.ssh, "studio.local");
        assert_eq!(studio.default_cwd, None);
        assert_eq!(studio.node, None);
        assert!(config.host("unknown").is_none());
    }

    #[test]
    fn minimal_entry_needs_only_id_and_ssh() {
        let json = br#"{ "hosts": [{ "id": "builder", "ssh": "root@1.2.3.4" }] }"#;
        let config = HostsConfig::parse(json).expect("minimal schema");
        let host = config.host("builder").expect("builder present");
        assert_eq!(host.display_name(), "builder");
        assert_eq!(host.default_cwd, None);
        assert_eq!(host.node, None);
    }

    #[test]
    fn missing_or_invalid_file_is_the_empty_catalog() {
        assert_eq!(
            HostsConfig::load("/nonexistent/hosts.json"),
            HostsConfig::default()
        );
        assert!(HostsConfig::parse(b"not json").is_none());
        assert_eq!(
            HostsConfig::parse(b"{}").expect("empty object").hosts,
            vec![]
        );
    }

    #[test]
    fn round_trips_through_serde() {
        let config = HostsConfig {
            hosts: vec![HostEntry {
                id: "forge".into(),
                name: Some("Forge".into()),
                ssh: "cristi@forge".into(),
                default_cwd: Some("~/code".into()),
                node: Some(HostNodeConfig {
                    endpoint: "tcp://100.64.0.2:7337".into(),
                    token_file: "~/.config/dirijor/forge.token".into(),
                    node_id: Some("node-forge".into()),
                }),
            }],
        };
        let json = serde_json::to_string(&config).expect("encodes");
        assert!(
            json.contains("\"defaultCwd\":\"~/code\""),
            "camelCase keys: {json}"
        );
        let back: HostsConfig = serde_json::from_str(&json).expect("decodes");
        assert_eq!(back, config);
    }

    #[test]
    fn save_is_atomic_reloadable_and_owner_only() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join("nested/hosts.json");
        let config = HostsConfig {
            hosts: vec![HostEntry {
                id: "forge".into(),
                name: Some("Forge".into()),
                ssh: "you@forge".into(),
                default_cwd: Some("~/code".into()),
                node: None,
            }],
        };

        config.save(&path).expect("save catalog");
        assert_eq!(HostsConfig::load(&path), config);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
        assert!(
            fs::read_dir(directory.path().join("nested"))
                .expect("read catalog directory")
                .all(|entry| !entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .contains(".tmp-"))
        );
    }
}
