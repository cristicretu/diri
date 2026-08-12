//! Privacy-safe diagnostics assembled independently from GPUI and clipboard
//! code. The report boundary accepts catalog/config records, but selects only
//! explicitly allowlisted metadata fields; there is no raw-log or session
//! payload slot to accidentally populate.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use diri_proto::{AgentReadinessResult, HelloResult, HostEntry};

use crate::store::DaemonState;

const REPORT_LIMIT_BYTES: usize = 12 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlatformMetadata {
    pub os_version: String,
    pub architecture: String,
}

impl PlatformMetadata {
    #[must_use]
    pub fn current() -> Self {
        Self {
            os_version: macos_product_version(),
            architecture: std::env::consts::ARCH.to_owned(),
        }
    }
}

pub struct DiagnosticsInput<'a> {
    pub app_version: &'a str,
    pub app_build: &'a str,
    pub update_channel: &'a str,
    pub platform: &'a PlatformMetadata,
    pub daemon_state: &'a DaemonState,
    pub daemon_identity: Option<&'a HelloResult>,
    pub agents: &'a AgentReadinessResult,
    pub hosts: &'a [HostEntry],
    pub active_host_ids: &'a HashSet<String>,
    pub storage_reachable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticsReport {
    text: String,
}

impl DiagnosticsReport {
    /// Builds a bounded report from an explicit metadata allowlist.
    ///
    /// The implementation never reads agent binary paths, SSH destinations,
    /// node endpoints/token paths, sessions, terminal output, environment
    /// variables, logs, repositories, transcripts, or provider identifiers.
    #[must_use]
    pub fn generate(input: DiagnosticsInput<'_>) -> Self {
        let mut lines = vec![
            "# Diri diagnostics".to_owned(),
            "Review this report before posting it publicly.".to_owned(),
            String::new(),
            format!(
                "App: {} (build {}, channel {})",
                bounded_identifier(input.app_version),
                bounded_identifier(input.app_build),
                bounded_identifier(input.update_channel)
            ),
            format!(
                "macOS: {} ({})",
                bounded_atom(&input.platform.os_version),
                bounded_atom(&input.platform.architecture)
            ),
        ];

        match (input.daemon_state, input.daemon_identity) {
            (DaemonState::Connected, Some(identity)) => lines.push(format!(
                "Daemon: reachable; build {}; pid {}; protocol {}",
                bounded_identifier(&identity.build),
                identity.pid,
                identity.proto
            )),
            (DaemonState::Connected, None) => {
                lines.push("Daemon: reachable; identity unavailable".to_owned());
            }
            (DaemonState::Connecting, _) => {
                lines.push("Daemon: connecting automatically".to_owned());
            }
            (DaemonState::Unreachable(_), _) => {
                // The underlying error is intentionally omitted: transport
                // errors can embed socket paths or command diagnostics.
                lines.push("Daemon: unreachable".to_owned());
            }
        }

        lines.push(format!(
            "Session/state storage: {}",
            if input.storage_reachable {
                "reachable"
            } else {
                "unreachable"
            }
        ));
        lines.push(String::new());
        lines.push("Agents:".to_owned());
        if input.agents.agents.is_empty() {
            lines.push("- catalog unavailable".to_owned());
        } else {
            let mut agents = input.agents.agents.iter().collect::<Vec<_>>();
            agents.sort_by(|left, right| left.kind.id().cmp(right.kind.id()));
            lines.extend(agents.into_iter().map(|agent| {
                format!(
                    "- {}: {}",
                    bounded_identifier(agent.kind.id()),
                    if agent.available() {
                        "installed"
                    } else {
                        "unavailable"
                    }
                )
            }));
        }

        lines.push(String::new());
        lines.push("Remote hosts:".to_owned());
        if input.hosts.is_empty() {
            lines.push("- none configured".to_owned());
        } else {
            let mut hosts = input.hosts.iter().collect::<Vec<_>>();
            hosts.sort_by(|left, right| left.id.cmp(&right.id));
            lines.extend(hosts.into_iter().map(|host| {
                let reachability = if !matches!(input.daemon_state, DaemonState::Connected) {
                    "not checked (daemon unavailable)"
                } else if input.active_host_ids.contains(&host.id) {
                    "reachable (active session)"
                } else {
                    "not checked"
                };
                format!("- {}: {reachability}", bounded_identifier(&host.id))
            }));
        }

        lines.push(String::new());
        lines.push(
            "Excluded by design: terminal content, prompts, logs, environment variables, paths, SSH destinations, tokens, and account identifiers."
                .to_owned(),
        );

        let mut text = lines.join("\n");
        if text.len() > REPORT_LIMIT_BYTES {
            text.truncate(REPORT_LIMIT_BYTES);
            text.push_str("\n[report truncated]");
        }
        Self { text }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

#[must_use]
pub fn storage_reachable(home: &Path) -> bool {
    let directory = diri_proto::paths::DirijorPaths::app_support(home);
    directory.is_dir()
}

fn bounded_atom(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(96)
        .collect()
}

/// Copies untrusted identifiers only when they fit a deliberately narrow,
/// path-free alphabet. Manifests and host catalogs can be user-authored, so a
/// length cap alone would still allow a value to masquerade as a filesystem
/// path, environment assignment, or pasted log line.
fn bounded_identifier(value: &str) -> String {
    if value.is_empty()
        || value.len() > 80
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.+:".contains(character))
    {
        return "[invalid identifier omitted]".to_owned();
    }
    value.to_owned()
}

fn macos_product_version() -> String {
    Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|version| bounded_atom(&version))
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use diri_proto::{AgentKind, AgentReadinessItem, HostNodeConfig, RUST_ENGINE_KIND};

    use super::*;

    #[test]
    fn sensitive_catalog_and_host_fixture_values_cannot_enter_the_report() {
        let agents = AgentReadinessResult {
            agents: vec![AgentReadinessItem {
                kind: AgentKind::CODEX,
                binary: "codex".to_owned(),
                path: Some("/Users/alice/secret-repo/bin/codex".to_owned()),
                descriptor: None,
                ..AgentReadinessItem::default()
            }],
            ..AgentReadinessResult::default()
        };
        let hosts = vec![HostEntry {
            id: "forge".to_owned(),
            name: Some("Personal production box".to_owned()),
            ssh: "alice@10.0.0.7".to_owned(),
            default_cwd: Some("/srv/private/customer-repo".to_owned()),
            node: Some(HostNodeConfig {
                endpoint: "tcp://10.0.0.7:7337".to_owned(),
                token_file: "/Users/alice/.secrets/diri.token".to_owned(),
                node_id: Some("provider-account-123".to_owned()),
            }),
        }];
        let platform = PlatformMetadata {
            os_version: "15.5".to_owned(),
            architecture: "aarch64".to_owned(),
        };
        let identity = HelloResult {
            proto: 1,
            build: "0.5.0".to_owned(),
            pid: 42,
            engine_kind: Some(RUST_ENGINE_KIND.to_owned()),
            executable_hash: Some("private-full-executable-hash".to_owned()),
        };
        let active = HashSet::from(["forge".to_owned()]);
        let report = DiagnosticsReport::generate(DiagnosticsInput {
            app_version: "0.5.0",
            app_build: "release",
            update_channel: "stable",
            platform: &platform,
            daemon_state: &DaemonState::Connected,
            daemon_identity: Some(&identity),
            agents: &agents,
            hosts: &hosts,
            active_host_ids: &active,
            storage_reachable: true,
        });

        let text = report.as_str();
        for sensitive in [
            "/Users/alice",
            "secret-repo",
            "alice@10.0.0.7",
            "/srv/private",
            "10.0.0.7:7337",
            "diri.token",
            "provider-account-123",
            "private-full-executable-hash",
        ] {
            assert!(!text.contains(sensitive), "leaked {sensitive:?}: {text}");
        }
        assert!(text.contains("codex: installed"));
        assert!(text.contains("forge: reachable (active session)"));
        assert!(text.contains("Review this report before posting it publicly."));
    }

    #[test]
    fn unreachable_transport_error_is_never_rendered() {
        let platform = PlatformMetadata {
            os_version: "15.5".to_owned(),
            architecture: "aarch64".to_owned(),
        };
        let report = DiagnosticsReport::generate(DiagnosticsInput {
            app_version: "0.5.0",
            app_build: "release",
            update_channel: "stable",
            platform: &platform,
            daemon_state: &DaemonState::Unreachable(
                "failed at /Users/alice/private/daemon.sock with SECRET=token".to_owned(),
            ),
            daemon_identity: None,
            agents: &AgentReadinessResult::default(),
            hosts: &[],
            active_host_ids: &HashSet::new(),
            storage_reachable: false,
        });
        assert!(report.as_str().contains("Daemon: unreachable"));
        assert!(!report.as_str().contains("/Users/alice"));
        assert!(!report.as_str().contains("SECRET"));
    }

    #[test]
    fn user_authored_identifiers_are_bounded_and_fail_closed() {
        for unsafe_value in [
            "/Users/alice/private/manifest",
            "rule\nSECRET=token",
            "SECRET=token\n",
            &"x".repeat(81),
            "",
        ] {
            assert_eq!(
                bounded_identifier(unsafe_value),
                "[invalid identifier omitted]"
            );
        }
        assert_eq!(bounded_identifier("codex-working_2.1"), "codex-working_2.1");
    }
}
