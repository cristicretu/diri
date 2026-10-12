use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use serde::{Deserialize, Serialize};

use crate::error::{NodeError, NodeResult};

const CONFIG_VERSION: u32 = 1;
const DEFAULT_LISTEN: &str = "127.0.0.1:7337";

#[derive(Clone, Debug)]
pub struct NodePaths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub accounts: PathBuf,
    pub accounts_root: PathBuf,
    pub usage_db: PathBuf,
    pub checkpoints: PathBuf,
    pub blobs: PathBuf,
    pub restores: PathBuf,
    pub moves: PathBuf,
}

impl NodePaths {
    pub fn for_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            config: root.join("node.json"),
            accounts: root.join("accounts.json"),
            accounts_root: root.join("accounts"),
            usage_db: root.join("usage.sqlite3"),
            checkpoints: root.join("checkpoints"),
            blobs: root.join("blobs/sha256"),
            restores: root.join("restores"),
            moves: root.join("moves"),
            root,
        }
    }

    pub fn discover() -> Self {
        if let Some(root) = env::var_os("DIRIJOR_NODE_HOME") {
            return Self::for_root(root);
        }
        let home = env::var_os("HOME").map_or_else(env::temp_dir, PathBuf::from);
        #[cfg(target_os = "macos")]
        let root = home.join("Library/Application Support/Dirijor/node");
        #[cfg(not(target_os = "macos"))]
        let root = env::var_os("XDG_DATA_HOME").map_or_else(
            || home.join(".local/share/dirijor/node"),
            |data| PathBuf::from(data).join("dirijor/node"),
        );
        Self::for_root(root)
    }

    pub fn create_layout(&self) -> io::Result<()> {
        for directory in [
            &self.root,
            &self.accounts_root,
            &self.checkpoints,
            &self.blobs,
            &self.restores,
            &self.moves,
        ] {
            fs::create_dir_all(directory)?;
            set_owner_directory(directory)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeConfig {
    pub version: u32,
    pub node_id: String,
    pub display_name: String,
    pub listen: String,
    /// App-layer capability token. It never leaves the node except during an
    /// explicit enrollment and is redacted from every protocol result.
    pub auth_token: String,
}

impl NodeConfig {
    pub fn load_or_initialize(paths: &NodePaths) -> NodeResult<Self> {
        paths.create_layout()?;
        if paths.config.exists() {
            let bytes = fs::read(&paths.config)?;
            let config: Self = serde_json::from_slice(&bytes)?;
            config.validate()?;
            return Ok(config);
        }

        let hostname = env::var("HOSTNAME")
            .ok()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "Dirijor Node".to_owned());
        let config = Self {
            version: CONFIG_VERSION,
            node_id: format!("node-{}", random_hex(8)?),
            display_name: hostname,
            listen: DEFAULT_LISTEN.to_owned(),
            auth_token: random_hex(32)?,
        };
        config.save(&paths.config)?;
        Ok(config)
    }

    pub fn load(path: impl AsRef<Path>) -> NodeResult<Self> {
        let config: Self = serde_json::from_slice(&fs::read(path)?)?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> NodeResult<()> {
        self.validate()?;
        atomic_json(path.as_ref(), self)?;
        Ok(())
    }

    pub fn token_matches(&self, candidate: &str) -> bool {
        constant_time_eq(self.auth_token.as_bytes(), candidate.as_bytes())
    }

    fn validate(&self) -> NodeResult<()> {
        if self.version != CONFIG_VERSION {
            return Err(NodeError::Protocol(format!(
                "unsupported node config version {}",
                self.version
            )));
        }
        if self.node_id.trim().is_empty()
            || self.display_name.trim().is_empty()
            || self.auth_token.len() < 32
        {
            return Err(NodeError::Protocol("invalid node configuration".into()));
        }
        self.listen
            .parse::<std::net::SocketAddr>()
            .map_err(|error| NodeError::Protocol(format!("invalid listen address: {error}")))?;
        Ok(())
    }
}

pub(crate) fn atomic_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;
    set_owner_directory(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("data");
    let temporary = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    let mut bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let result = (|| {
        fs::write(&temporary, bytes)?;
        set_owner_file(&temporary)?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn set_owner_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub(crate) fn set_owner_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub(crate) fn random_hex(bytes: usize) -> NodeResult<String> {
    let mut random = vec![0_u8; bytes];
    getrandom::fill(&mut random)
        .map_err(|error| NodeError::Io(io::Error::other(error.to_string())))?;
    Ok(hex_encode(&random))
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(char::from(HEX[usize::from(byte >> 4)]));
        result.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    result
}

pub(crate) fn hex_decode(value: &str) -> NodeResult<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return Err(NodeError::BadRequest("hex payload has odd length".into()));
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = hex_digit(pair[0])?;
            let low = hex_digit(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_digit(byte: u8) -> NodeResult<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(NodeError::BadRequest("invalid hex payload".into())),
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let left = left.get(index).copied().unwrap_or_default();
        let right = right.get(index).copied().unwrap_or_default();
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initializes_owner_only_config_once() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = NodePaths::for_root(directory.path().join("node"));
        let first = NodeConfig::load_or_initialize(&paths).expect("first init");
        let second = NodeConfig::load_or_initialize(&paths).expect("reload");
        assert_eq!(first, second);
        assert_eq!(first.auth_token.len(), 64);
        assert!(first.token_matches(&first.auth_token));
        assert!(!first.token_matches("wrong"));
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&paths.config)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn hex_round_trip_and_validation() {
        let bytes = b"Dirijor\0node";
        assert_eq!(hex_decode(&hex_encode(bytes)).expect("decode"), bytes);
        assert!(hex_decode("abc").is_err());
        assert!(hex_decode("zz").is_err());
    }
}
