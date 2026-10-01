//! WSL distro discovery is Engine-owned; all session actions use the existing
//! Linux Helper protocol, never a Windows PTY wrapped around wsl.exe.
#[cfg(windows)]
use diri_proto::HostTransport;
use diri_proto::{HostEntry, HostsConfig};
use std::path::Path;

pub fn catalog(path: impl AsRef<Path>) -> HostsConfig {
    let mut catalog = HostsConfig::load(path);
    for host in discovered() {
        if !catalog.hosts.iter().any(|existing| {
            existing.id == host.id || existing.wsl_distribution() == host.wsl_distribution()
        }) {
            catalog.hosts.push(host);
        }
    }
    catalog
}

#[cfg(not(windows))]
fn discovered() -> Vec<HostEntry> {
    Vec::new()
}

#[cfg(windows)]
fn discovered() -> Vec<HostEntry> {
    use std::{
        sync::{Mutex, OnceLock},
        time::{Duration, Instant},
    };
    type CachedHosts = Option<(Instant, Vec<HostEntry>)>;
    static CACHE: OnceLock<Mutex<CachedHosts>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some((at, hosts)) = cache.as_ref()
        && at.elapsed() < Duration::from_secs(30)
    {
        return hosts.clone();
    }
    let executor = crate::remote::executor::ProcessExecutor::default();
    let result = executor.run(
        crate::remote::ssh::CommandSpec {
            program: "wsl.exe".into(),
            arguments: ["--list", "--quiet"].map(Into::into).into(),
        },
        Vec::new(),
        Duration::from_secs(3),
        64 * 1024,
    );
    let hosts = result
        .ok()
        .filter(|r| r.status.success() && !r.stdout_truncated)
        .and_then(|r| parse_distributions(&r.stdout).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|distribution| {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(distribution.as_bytes());
            let key = digest[..12]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            HostEntry {
                id: format!("wsl-{key}"),
                name: Some(format!("{distribution} (WSL)")),
                ssh: String::new(),
                default_cwd: Some("~".into()),
                node: None,
                transport: HostTransport::Wsl { distribution },
            }
        })
        .collect::<Vec<_>>();
    *cache = Some((Instant::now(), hosts.clone()));
    hosts
}

/// wsl.exe uses UTF-16LE when redirected. Accept UTF-8 from newer versions
/// too; reject malformed names instead of silently replacing code units.
#[cfg(any(windows, test))]
fn parse_distributions(bytes: &[u8]) -> std::io::Result<Vec<String>> {
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid WSL distribution list",
        )
    };
    let text = if bytes.starts_with(&[0xff, 0xfe]) || bytes.contains(&0) {
        let bytes = bytes.strip_prefix(&[0xff, 0xfe]).unwrap_or(bytes);
        if !bytes.len().is_multiple_of(2) {
            return Err(invalid());
        }
        let units = bytes
            .chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .collect::<Vec<_>>();
        String::from_utf16(&units).map_err(|_| invalid())?
    } else {
        String::from_utf8(bytes.to_vec()).map_err(|_| invalid())?
    };
    let mut names = Vec::new();
    for name in text.lines().map(str::trim).filter(|s| !s.is_empty()) {
        if name.len() > 256 || name.chars().any(char::is_control) {
            return Err(invalid());
        }
        if !names.iter().any(|n| n == name) {
            names.push(name.to_owned());
        }
        if names.len() > 128 {
            return Err(invalid());
        }
    }
    Ok(names)
}

/// Explorer's WSL UNC paths name a Linux host, never a native Windows cwd.
pub fn route_unc(path: &str, catalog: &HostsConfig) -> std::io::Result<Option<(String, String)>> {
    let normalized = path.replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();
    let prefix = ["//wsl$/", "//wsl.localhost/"]
        .into_iter()
        .find(|prefix| lower.starts_with(prefix));
    let Some(prefix) = prefix else {
        return Ok(None);
    };
    let mut parts = normalized[prefix.len()..].split('/');
    let distribution = parts.next().unwrap_or_default();
    let host = catalog
        .hosts
        .iter()
        .find(|host| {
            host.wsl_distribution()
                .is_some_and(|name| name.eq_ignore_ascii_case(distribution))
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "WSL distribution is not registered",
            )
        })?;
    let parts = parts.filter(|part| !part.is_empty()).collect::<Vec<_>>();
    if parts
        .iter()
        .any(|part| matches!(*part, "." | "..") || part.contains('\0'))
    {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    Ok(Some((host.id.clone(), format!("/{}", parts.join("/")))))
}

pub fn explorer_path(host: &HostEntry, linux: &str) -> std::io::Result<String> {
    let distribution = host
        .wsl_distribution()
        .ok_or(std::io::ErrorKind::InvalidInput)?;
    if !linux.starts_with('/')
        || linux.contains(['\\', '\0'])
        || linux.split('/').any(|part| matches!(part, "." | ".."))
    {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    Ok(format!(
        "\\\\wsl.localhost\\{}\\{}",
        distribution,
        linux.trim_start_matches('/').replace('/', "\\")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirected_distribution_lists_accept_utf16_and_utf8_without_replacements() {
        let list = "Ubuntu\r\nDebian dev\r\nUbuntu\r\n";
        let encoded: Vec<u8> = list.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(
            parse_distributions(&encoded).unwrap(),
            ["Ubuntu", "Debian dev"]
        );
        let mut bom = vec![0xff, 0xfe];
        bom.extend_from_slice(&encoded);
        assert_eq!(parse_distributions(&bom).unwrap(), ["Ubuntu", "Debian dev"]);
        assert_eq!(
            parse_distributions(list.as_bytes()).unwrap(),
            ["Ubuntu", "Debian dev"]
        );
        assert!(parse_distributions(&[0xff, 0xfe, 1]).is_err());
        assert!(parse_distributions(&[0, 0xd8]).is_err());
        assert!(parse_distributions(b"bad\x1bname").is_err());
    }

    #[test]
    fn explorer_paths_route_only_to_the_registered_distro_without_traversal() {
        let host = HostEntry {
            id: "wsl-test".into(),
            name: None,
            ssh: String::new(),
            default_cwd: None,
            node: None,
            transport: diri_proto::HostTransport::Wsl {
                distribution: "Ubuntu dev".into(),
            },
        };
        let mut catalog = HostsConfig::default();
        catalog.hosts.push(host.clone());
        let path = explorer_path(&host, "/home/me/project space").unwrap();
        assert_eq!(
            route_unc(&path, &catalog).unwrap(),
            Some(("wsl-test".into(), "/home/me/project space".into()))
        );
        assert!(route_unc(r"\\wsl$\Missing\home", &catalog).is_err());
        assert!(route_unc(r"\\wsl$\Ubuntu dev\..\other", &catalog).is_err());
        assert!(explorer_path(&host, "/home/../other").is_err());
        assert_eq!(route_unc(r"C:\project", &catalog).unwrap(), None);
    }
}
