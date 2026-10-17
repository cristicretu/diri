//! The Engine-served `dirijor` MCP endpoint (Streamable HTTP on loopback).
//!
//! Every local Agent session used to start its own `dirijor-mcp` stdio
//! process just to forward tool calls to this Engine. Agent CLIs that speak
//! MCP over HTTP (Claude Code, Codex) now call the Engine directly, so a
//! session costs no extra process. The transport itself lives in
//! [`dirijor_mcp::http`] and reuses the stdio server's message handling.
//!
//! Identity: a session's bearer token is `<session id>.<HMAC-SHA256(key, id)>`
//! under a 32-byte key kept in an owner-only file. The token reaches the
//! Agent only through its PTY environment ([`TOKEN_ENV`]); the shared Claude
//! config names the variable, never the value. The key persists so sessions
//! that outlive an Engine restart (Holders) keep authenticating, and a token
//! is honoured only while its session is live in the registry, so it dies
//! with the session.
//!
//! Port: the endpoint prefers the port it used last time, again so surviving
//! sessions keep reaching it. If that port is taken it binds a fresh one;
//! new sessions use it and older ones lose the tools until resumed.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use crate::agent::{AgentDescriptor, InjectionSpec};
use crate::cli_version::{CliVersions, Version};
use crate::registry::Registry;

/// The oldest Claude Code verified end to end against this endpoint: an
/// `--mcp-config` server with `"type": "http"` and `${VAR}` expansion in its
/// headers, listing and calling tools. Older releases keep stdio.
pub const CLAUDE_CODE_MIN_VERSION: Version = Version::new(2, 1, 289);
/// The oldest Codex verified end to end: `mcp_servers.<name>.url` with
/// `bearer_token_env_var`, listing and calling tools. Older releases keep
/// stdio, since an unknown config key can stop Codex from starting at all.
pub const CODEX_MIN_VERSION: Version = Version::new(0, 160, 0);

/// The CLI release an Agent needs before it may use the HTTP endpoint, or
/// `None` when its MCP mechanism never does (Cursor, no MCP).
pub fn minimum_version(injection: &InjectionSpec) -> Option<Version> {
    if injection.claude_mcp {
        Some(CLAUDE_CODE_MIN_VERSION)
    } else if injection.codex_mcp {
        Some(CODEX_MIN_VERSION)
    } else {
        None
    }
}

/// Whether a launch of `descriptor` may be pointed at the endpoint: only
/// when the CLI the Engine would run is known to be at or above the
/// verified minimum. An unresolvable binary or a version not probed yet
/// keeps stdio for this launch; the probe runs in the background.
pub fn http_allowed(descriptor: &AgentDescriptor, versions: &CliVersions) -> bool {
    let Some(minimum) = minimum_version(&descriptor.injection) else {
        return false;
    };
    let Some(binary) = descriptor.binary.as_deref() else {
        return false;
    };
    let Some(path) = crate::agent_catalog::resolve_local(binary, None).detected_path else {
        return false;
    };
    versions
        .get(Path::new(&path))
        .is_some_and(|version| version >= minimum)
}

/// Probes every HTTP-capable Agent CLI once, so the first spawn after the
/// Engine starts already knows its version. One background thread, run once.
pub fn warm_versions(descriptors: Vec<AgentDescriptor>, versions: CliVersions) {
    let _ = std::thread::Builder::new()
        .name("diri-cli-version-warmup".into())
        .spawn(move || {
            for descriptor in descriptors {
                if minimum_version(&descriptor.injection).is_none() {
                    continue;
                }
                if let Some(path) = descriptor.binary.as_deref().and_then(|binary| {
                    crate::agent_catalog::resolve_local(binary, None).detected_path
                }) {
                    let _ = versions.probe_now(Path::new(&path));
                }
            }
        });
}

/// The PTY environment variable carrying the session's bearer token.
pub const TOKEN_ENV: &str = crate::inject::MCP_TOKEN_ENV;
const KEY_FILE: &str = "mcp-http.key";
const PORT_FILE: &str = "mcp-http.port";
const KEY_BYTES: usize = 32;

/// A bound endpoint: where Agents reach it and how their tokens are minted.
#[derive(Clone)]
pub struct McpHttpEndpoint {
    url: String,
    key: Arc<[u8; KEY_BYTES]>,
}

impl std::fmt::Debug for McpHttpEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the key.
        f.debug_struct("McpHttpEndpoint")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl McpHttpEndpoint {
    pub fn new(port: u16, key: [u8; KEY_BYTES]) -> Self {
        Self {
            url: format!("http://127.0.0.1:{port}{}", dirijor_mcp::http::MCP_PATH),
            key: Arc::new(key),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// The bearer token for `session_id`. Deterministic, so a resumed or
    /// adopted session is handed the same token it had.
    pub fn token(&self, session_id: &str) -> String {
        format!(
            "{session_id}.{}",
            hex(&hmac_sha256(&self.key, session_id.as_bytes()))
        )
    }

    /// The session a token was minted for, if its MAC is authentic. Liveness
    /// is checked separately against the registry.
    pub fn verify<'a>(&self, token: &'a str) -> Option<&'a str> {
        let (session_id, mac) = token.rsplit_once('.')?;
        if session_id.is_empty() {
            return None;
        }
        let expected = hex(&hmac_sha256(&self.key, session_id.as_bytes()));
        constant_time_eq(expected.as_bytes(), mac.as_bytes()).then_some(session_id)
    }
}

/// Whether `session_id` names a session whose Agent is running here: tokens
/// of exited, removed, or remote sessions are refused.
pub fn session_is_live(registry: &Registry, session_id: &str) -> bool {
    registry.get(session_id).is_some()
        && registry.record(session_id).is_some_and(|record| {
            record.host.is_none() && !matches!(record.status, diri_proto::SessionStatus::Exited(_))
        })
}

/// Binds the loopback listener, preferring the last port, and returns it
/// with the endpoint description. Records the chosen port for next time.
pub fn bind(config_dir: &Path) -> io::Result<(TcpListener, McpHttpEndpoint)> {
    let key = load_or_create_key(&config_dir.join(KEY_FILE))?;
    let port_file = config_dir.join(PORT_FILE);
    let preferred = std::fs::read_to_string(&port_file)
        .ok()
        .and_then(|text| text.trim().parse::<u16>().ok())
        .filter(|port| *port != 0);
    let listener = preferred
        .and_then(|port| TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).ok())
        .map_or_else(
            || TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
            Ok,
        )?;
    let port = listener.local_addr()?.port();
    if preferred != Some(port) {
        let _ = write_private(&port_file, format!("{port}\n").as_bytes());
    }
    Ok((listener, McpHttpEndpoint::new(port, key)))
}

/// Serves the endpoint on its own accept thread until the process exits.
pub fn spawn(
    listener: TcpListener,
    endpoint: McpHttpEndpoint,
    registry: Arc<Mutex<Registry>>,
    socket_path: PathBuf,
    notes_dir: Option<PathBuf>,
) -> io::Result<()> {
    let server = dirijor_mcp::http::HttpServer::new(dirijor_mcp::http::HttpConfig {
        socket_path,
        notes_dir,
        auth: Box::new(move |token: &str| {
            let session_id = endpoint.verify(token)?;
            let registry = registry.lock().ok()?;
            session_is_live(&registry, session_id).then(|| session_id.to_owned())
        }),
    });
    std::thread::Builder::new()
        .name("diri-mcp-http-accept".into())
        .spawn(move || server.serve(listener))
        .map(drop)
}

fn load_or_create_key(path: &Path) -> io::Result<[u8; KEY_BYTES]> {
    let mut existing = Vec::new();
    if let Ok(mut file) = std::fs::File::open(path) {
        file.read_to_end(&mut existing)?;
        if let Ok(key) = <[u8; KEY_BYTES]>::try_from(existing.as_slice()) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
            return Ok(key);
        }
    }
    let mut key = [0_u8; KEY_BYTES];
    getrandom::fill(&mut key).map_err(io::Error::other)?;
    write_private(path, &key)?;
    Ok(key)
}

/// Writes `contents` atomically to an owner-only (`0600`) file.
fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut nonce = [0_u8; 8];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    let tmp = path.with_extension(format!("{}.tmp", hex(&nonce)));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = options
        .open(&tmp)
        .and_then(|mut file| file.write_all(contents).and_then(|()| file.sync_all()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn hmac_sha256(key: &[u8; KEY_BYTES], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut padded = [0_u8; BLOCK];
    padded[..KEY_BYTES].copy_from_slice(key);
    let mut inner = Sha256::new();
    inner.update(padded.map(|byte| byte ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(padded.map(|byte| byte ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_openssl() {
        // `printf 'Hi There' | openssl dgst -sha256 -mac HMAC -macopt hexkey:0b…0b` (32 bytes).
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 32], b"Hi There")),
            "198a607eb44bfbc69903a0f1cf2bbdc5ba0aa3f3d9ae3c1c7a3b1696a0b68cf7"
        );
    }

    #[test]
    fn tokens_resolve_only_their_own_session() {
        let endpoint = McpHttpEndpoint::new(1, [7; 32]);
        let token = endpoint.token("s-abc");
        assert_eq!(endpoint.verify(&token), Some("s-abc"));
        assert_eq!(endpoint.token("s-abc"), token, "stable across resumes");
        assert_ne!(endpoint.token("s-abd"), token);

        let other_engine = McpHttpEndpoint::new(1, [8; 32]);
        assert_eq!(other_engine.verify(&token), None);

        let (id, mac) = token.rsplit_once('.').unwrap();
        assert_eq!(
            endpoint.verify(&format!("s-abd.{mac}")),
            None,
            "MAC is bound to the id"
        );
        assert_eq!(endpoint.verify(id), None);
        assert_eq!(endpoint.verify(&format!(".{mac}")), None);
        assert_eq!(endpoint.verify(""), None);
        let mut tampered = token.clone();
        tampered.pop();
        tampered.push('x');
        assert_eq!(endpoint.verify(&tampered), None);
    }

    #[test]
    fn debug_never_prints_the_key() {
        let endpoint = McpHttpEndpoint::new(4242, [0xab; 32]);
        let printed = format!("{endpoint:?}");
        assert!(printed.contains("4242"));
        assert!(!printed.contains("abab"));
    }

    #[test]
    fn key_is_owner_only_and_survives_restarts_and_the_port_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let (listener, first) = bind(dir.path()).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(first.url(), format!("http://127.0.0.1:{port}/mcp"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let token = first.token("s-1");
        drop(listener);

        // A restarted Engine answers the same URL and the same tokens. Another
        // process (or a parallel test binding port 0) can take the freed
        // ephemeral port first; then a fresh port is the right answer, so
        // restart again from the port it recorded.
        let mut first = first;
        let mut attempts = 0;
        let _listener = loop {
            let (listener, second) = bind(dir.path()).unwrap();
            assert_eq!(second.verify(&token), Some("s-1"));
            if second.url() == first.url() {
                break listener;
            }
            attempts += 1;
            assert!(attempts < 5, "the recorded port was never reused");
            first = second;
        };

        // Port taken by someone else: a fresh one, same key.
        let (_third_listener, third) = bind(dir.path()).unwrap();
        assert_ne!(third.url(), first.url());
        assert_eq!(third.verify(&token), Some("s-1"));
    }

    #[cfg(unix)]
    fn fake_cli(dir: &Path, name: &str, version_line: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\necho '{version_line}'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    fn descriptor(binary: Option<String>, injection: InjectionSpec) -> AgentDescriptor {
        AgentDescriptor {
            binary,
            injection,
            ..Default::default()
        }
    }

    #[cfg(unix)]
    #[test]
    fn http_is_gated_on_the_verified_cli_version() {
        let dir = tempfile::tempdir().unwrap();
        let claude = InjectionSpec {
            claude_hooks: true,
            claude_mcp: true,
            ..Default::default()
        };
        let codex = InjectionSpec {
            codex_notify: true,
            codex_mcp: true,
            ..Default::default()
        };
        let cases = [
            ("claude-old", "2.1.288 (Claude Code)", &claude, false),
            ("claude-at", "2.1.289 (Claude Code)", &claude, true),
            ("claude-new", "2.2.0 (Claude Code)", &claude, true),
            ("claude-pre", "2.1.289-beta.1 (Claude Code)", &claude, false),
            ("claude-odd", "Claude Code (unknown build)", &claude, false),
            ("codex-old", "codex-cli 0.159.9", &codex, false),
            ("codex-at", "codex-cli 0.160.0", &codex, true),
            ("codex-new", "codex-cli 1.0.0", &codex, true),
            ("codex-odd", "codex-cli dev", &codex, false),
        ];
        let versions = CliVersions::default();
        for (name, line, injection, expected) in cases {
            let binary = fake_cli(dir.path(), name, line);
            let agent = descriptor(Some(binary.clone()), *injection);
            // Never probed: unknown, so stdio, and the spawn path does not wait.
            assert!(!http_allowed(&agent, &versions), "{name} before probing");
            versions.probe_now(Path::new(&binary));
            assert_eq!(http_allowed(&agent, &versions), expected, "{name}: {line}");
        }

        // Cursor never uses HTTP; neither does a missing or bare-unresolvable CLI.
        let cursor = InjectionSpec {
            cursor_mcp: true,
            ..Default::default()
        };
        let new_cli = fake_cli(dir.path(), "cursor-agent", "2099.1.1");
        versions.probe_now(Path::new(&new_cli));
        assert!(!http_allowed(&descriptor(Some(new_cli), cursor), &versions));
        let missing = dir.path().join("gone").to_string_lossy().into_owned();
        assert!(!http_allowed(&descriptor(Some(missing), claude), &versions));
        assert!(!http_allowed(
            &descriptor(Some("diri-no-such-agent-cli".into()), claude),
            &versions
        ));
        assert!(!http_allowed(&descriptor(None, claude), &versions));
    }

    #[test]
    fn the_minimums_are_the_releases_verified_end_to_end() {
        assert_eq!(CLAUDE_CODE_MIN_VERSION, Version::new(2, 1, 289));
        assert_eq!(CODEX_MIN_VERSION, Version::new(0, 160, 0));
        let claude = InjectionSpec {
            claude_mcp: true,
            ..Default::default()
        };
        assert_eq!(minimum_version(&claude), Some(CLAUDE_CODE_MIN_VERSION));
        assert_eq!(minimum_version(&InjectionSpec::default()), None);
    }

    #[test]
    fn a_damaged_key_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(KEY_FILE), b"short").unwrap();
        let key = load_or_create_key(&dir.path().join(KEY_FILE)).unwrap();
        assert_eq!(std::fs::read(dir.path().join(KEY_FILE)).unwrap(), key);
    }
}
