//! Pure bootstrap decisions: platform parsing and packaged artifact checks.

use std::error::Error;
use std::fmt;
use std::io::Read;
use std::path::PathBuf;

use diri_proto::remote_pty::{HelperProbe, PROTOCOL_MAJOR};
use sha2::{Digest, Sha256};

pub const PLATFORM_PROBE_MARKER: &[u8] = b"__DIRI_PLATFORM_V1__\0";
pub const PLATFORM_PROBE_COMMAND: &str =
    "printf '__DIRI_PLATFORM_V1__\\0%s\\0%s\\0%s\\0' \"$(uname -s)\" \"$(uname -m)\" \"$HOME\"";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RemoteTarget {
    LinuxX86_64,
    LinuxAarch64,
    MacosAarch64,
}

impl RemoteTarget {
    pub const ALL: [Self; 3] = [Self::LinuxX86_64, Self::LinuxAarch64, Self::MacosAarch64];

    #[must_use]
    pub const fn artifact_name(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "x86_64-unknown-linux-musl",
            Self::LinuxAarch64 => "aarch64-unknown-linux-musl",
            Self::MacosAarch64 => "aarch64-apple-darwin",
        }
    }

    pub fn from_artifact_name(name: &str) -> Result<Self, BootstrapError> {
        match name {
            "x86_64-unknown-linux-musl" => Ok(Self::LinuxX86_64),
            "aarch64-unknown-linux-musl" => Ok(Self::LinuxAarch64),
            "aarch64-apple-darwin" => Ok(Self::MacosAarch64),
            other => Err(BootstrapError::UnsupportedArtifactTarget(other.to_string())),
        }
    }

    fn from_uname(os: &str, arch: &str) -> Result<Self, BootstrapError> {
        let os = os.trim().to_ascii_lowercase();
        let arch = arch.trim().to_ascii_lowercase();
        match (os.as_str(), arch.as_str()) {
            ("linux", "x86_64" | "amd64") => Ok(Self::LinuxX86_64),
            ("linux", "aarch64" | "arm64") => Ok(Self::LinuxAarch64),
            ("darwin", "aarch64" | "arm64") => Ok(Self::MacosAarch64),
            _ => Err(BootstrapError::UnsupportedPlatform { os, arch }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformProbe {
    pub target: RemoteTarget,
    pub home: String,
}

impl PlatformProbe {
    /// Finds the marker instead of assuming stdout is clean: login rc files
    /// and SSH banners may print arbitrary text before the fixed probe.
    pub fn parse(stdout: &[u8]) -> Result<Self, BootstrapError> {
        let marker = stdout
            .windows(PLATFORM_PROBE_MARKER.len())
            .position(|window| window == PLATFORM_PROBE_MARKER)
            .ok_or(BootstrapError::MissingPlatformMarker)?;
        let payload = &stdout[marker + PLATFORM_PROBE_MARKER.len()..];
        let mut fields = payload.split(|byte| *byte == 0);
        let os = field(&mut fields, "os")?;
        let arch = field(&mut fields, "arch")?;
        let home = field(&mut fields, "home")?;
        if home.is_empty() || !home.starts_with('/') {
            return Err(BootstrapError::InvalidRemoteHome(home.to_string()));
        }
        Ok(Self {
            target: RemoteTarget::from_uname(os, arch)?,
            home: home.to_string(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackagedArtifact {
    pub target: RemoteTarget,
    pub protocol_major: u16,
    pub build_id: String,
    pub length: u64,
    pub sha256: String,
    pub path: PathBuf,
}

impl PackagedArtifact {
    /// Verifies manifest metadata against the bytes packaged with the app.
    /// This runs before SSH starts, so a corrupt local resource never reaches
    /// a remote machine.
    pub fn verify(&self) -> Result<(), BootstrapError> {
        if self.protocol_major != PROTOCOL_MAJOR {
            return Err(BootstrapError::ProtocolMismatch {
                expected: PROTOCOL_MAJOR,
                actual: self.protocol_major,
            });
        }
        validate_component("build id", &self.build_id)?;
        validate_sha256(&self.sha256)?;
        let metadata =
            std::fs::metadata(&self.path).map_err(|error| BootstrapError::ArtifactIo {
                path: self.path.clone(),
                detail: error.to_string(),
            })?;
        if !metadata.is_file() {
            return Err(BootstrapError::ArtifactNotFile(self.path.clone()));
        }
        if metadata.len() != self.length {
            return Err(BootstrapError::ArtifactLength {
                expected: self.length,
                actual: metadata.len(),
            });
        }
        let actual = sha256_file(&self.path)?;
        if actual != self.sha256 {
            return Err(BootstrapError::ArtifactHash {
                expected: self.sha256.clone(),
                actual,
            });
        }
        Ok(())
    }

    /// Verifies a report from either the temporary upload or the final path.
    /// Holder readiness is negotiated separately.
    pub fn verify_probe(&self, probe: &HelperProbe) -> Result<(), BootstrapError> {
        if probe.protocol.major != self.protocol_major {
            return Err(BootstrapError::ProbeMismatch("protocol major"));
        }
        if probe.build_id != self.build_id {
            return Err(BootstrapError::ProbeMismatch("build id"));
        }
        if probe.artifact_sha256 != self.sha256 {
            return Err(BootstrapError::ProbeMismatch("artifact SHA-256"));
        }
        if probe.target != self.target.artifact_name() {
            return Err(BootstrapError::ProbeMismatch("target"));
        }
        if !probe.supported {
            return Err(BootstrapError::ProbeMismatch("target support"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteInstallLayout {
    build_id: String,
    nonce: String,
}

impl RemoteInstallLayout {
    pub fn new(
        build_id: impl Into<String>,
        nonce: impl Into<String>,
    ) -> Result<Self, BootstrapError> {
        let build_id = build_id.into();
        let nonce = nonce.into();
        validate_component("build id", &build_id)?;
        validate_component("upload nonce", &nonce)?;
        Ok(Self { build_id, nonce })
    }

    #[must_use]
    pub const fn root(&self) -> &'static str {
        "$HOME/.cache/diri"
    }

    #[must_use]
    pub const fn bin_dir(&self) -> &'static str {
        "$HOME/.cache/diri/bin"
    }

    #[must_use]
    pub fn protocol_dir(&self) -> String {
        format!("{}/protocol-{PROTOCOL_MAJOR}", self.bin_dir())
    }

    #[must_use]
    pub fn version_dir(&self) -> String {
        format!("{}/{}", self.protocol_dir(), self.build_id)
    }

    #[must_use]
    pub fn executable(&self) -> String {
        format!("{}/diri-remote", self.version_dir())
    }

    #[must_use]
    pub fn temporary(&self) -> String {
        format!("{}/.tmp-{}", self.version_dir(), self.nonce)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BootstrapError {
    MissingPlatformMarker,
    MissingPlatformField(&'static str),
    InvalidUtf8(&'static str),
    InvalidRemoteHome(String),
    UnsupportedPlatform { os: String, arch: String },
    UnsupportedArtifactTarget(String),
    InvalidComponent { field: &'static str, value: String },
    InvalidSha256(String),
    ProtocolMismatch { expected: u16, actual: u16 },
    ArtifactIo { path: PathBuf, detail: String },
    ArtifactNotFile(PathBuf),
    ArtifactLength { expected: u64, actual: u64 },
    ArtifactHash { expected: String, actual: String },
    ProbeMismatch(&'static str),
}

impl fmt::Display for BootstrapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPlatformMarker => formatter.write_str("remote platform marker is missing"),
            Self::MissingPlatformField(field) => {
                write!(formatter, "remote platform field {field} is missing")
            }
            Self::InvalidUtf8(field) => {
                write!(formatter, "remote platform field {field} is not UTF-8")
            }
            Self::InvalidRemoteHome(home) => {
                write!(formatter, "remote home is not absolute: {home:?}")
            }
            Self::UnsupportedPlatform { os, arch } => {
                write!(formatter, "unsupported remote platform {os}/{arch}")
            }
            Self::UnsupportedArtifactTarget(target) => {
                write!(formatter, "unsupported Helper artifact target {target:?}")
            }
            Self::InvalidComponent { field, value } => {
                write!(formatter, "invalid {field} {value:?}")
            }
            Self::InvalidSha256(value) => write!(formatter, "invalid artifact SHA-256 {value:?}"),
            Self::ProtocolMismatch { expected, actual } => write!(
                formatter,
                "artifact protocol major {actual} does not match engine protocol {expected}"
            ),
            Self::ArtifactIo { path, detail } => write!(
                formatter,
                "cannot read artifact {}: {detail}",
                path.display()
            ),
            Self::ArtifactNotFile(path) => {
                write!(formatter, "artifact is not a file: {}", path.display())
            }
            Self::ArtifactLength { expected, actual } => {
                write!(
                    formatter,
                    "artifact length is {actual}; expected {expected}"
                )
            }
            Self::ArtifactHash { expected, actual } => {
                write!(
                    formatter,
                    "artifact SHA-256 is {actual}; expected {expected}"
                )
            }
            Self::ProbeMismatch(field) => {
                write!(formatter, "remote Helper probe mismatched {field}")
            }
        }
    }
}

impl Error for BootstrapError {}

pub(crate) fn validate_component(field: &'static str, value: &str) -> Result<(), BootstrapError> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && value != "."
        && value != "..";
    if valid {
        Ok(())
    } else {
        Err(BootstrapError::InvalidComponent {
            field,
            value: value.to_string(),
        })
    }
}

fn validate_sha256(value: &str) -> Result<(), BootstrapError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(BootstrapError::InvalidSha256(value.to_string()))
    }
}

fn field<'a>(
    fields: &mut impl Iterator<Item = &'a [u8]>,
    name: &'static str,
) -> Result<&'a str, BootstrapError> {
    std::str::from_utf8(
        fields
            .next()
            .ok_or(BootstrapError::MissingPlatformField(name))?,
    )
    .map_err(|_| BootstrapError::InvalidUtf8(name))
}

fn sha256_file(path: &PathBuf) -> Result<String, BootstrapError> {
    let mut file = std::fs::File::open(path).map_err(|error| BootstrapError::ArtifactIo {
        path: path.clone(),
        detail: error.to_string(),
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| BootstrapError::ArtifactIo {
                path: path.clone(),
                detail: error.to_string(),
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut hexadecimal = String::with_capacity(64);
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        hexadecimal.push(DIGITS[usize::from(byte >> 4)] as char);
        hexadecimal.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    Ok(hexadecimal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noisy_platform_probe_maps_all_supported_targets() {
        for (os, arch, target) in [
            ("Linux", "x86_64", RemoteTarget::LinuxX86_64),
            ("Linux", "arm64", RemoteTarget::LinuxAarch64),
            ("Darwin", "aarch64", RemoteTarget::MacosAarch64),
        ] {
            let output = format!("banner\n__DIRI_PLATFORM_V1__\0{os}\0{arch}\0/home/dev\0noise");
            assert_eq!(
                PlatformProbe::parse(output.as_bytes()),
                Ok(PlatformProbe {
                    target,
                    home: "/home/dev".into(),
                })
            );
        }
    }

    #[test]
    fn unsupported_and_malformed_platforms_fail_closed() {
        assert!(matches!(
            RemoteTarget::from_artifact_name("x86_64-apple-darwin"),
            Err(BootstrapError::UnsupportedArtifactTarget(_))
        ));
        assert!(matches!(
            PlatformProbe::parse(b"__DIRI_PLATFORM_V1__\0FreeBSD\0x86_64\0/home/u\0"),
            Err(BootstrapError::UnsupportedPlatform { .. })
        ));
        assert!(matches!(
            PlatformProbe::parse(b"__DIRI_PLATFORM_V1__\0Darwin\0x86_64\0/Users/u\0"),
            Err(BootstrapError::UnsupportedPlatform { .. })
        ));
        assert_eq!(
            PlatformProbe::parse(b"ordinary shell output"),
            Err(BootstrapError::MissingPlatformMarker)
        );
        assert!(matches!(
            PlatformProbe::parse(b"__DIRI_PLATFORM_V1__\0Linux\0x86_64\0relative\0"),
            Err(BootstrapError::InvalidRemoteHome(_))
        ));
    }

    #[test]
    fn packaged_artifact_is_verified_before_upload() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("diri-remote");
        std::fs::write(&path, b"abc").expect("fixture");
        let artifact = PackagedArtifact {
            target: RemoteTarget::MacosAarch64,
            protocol_major: PROTOCOL_MAJOR,
            build_id: "build-1".into(),
            length: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
            path,
        };
        artifact.verify().expect("valid artifact");

        let mut corrupt = artifact.clone();
        corrupt.length = 4;
        assert_eq!(
            corrupt.verify(),
            Err(BootstrapError::ArtifactLength {
                expected: 4,
                actual: 3,
            })
        );

        let probe = HelperProbe {
            protocol: diri_proto::remote_pty::ProtocolVersion::CURRENT,
            build_id: artifact.build_id.clone(),
            artifact_sha256: artifact.sha256.clone(),
            target: artifact.target.artifact_name().into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            supported: true,
            holder_available: false,
            capabilities: Vec::new(),
        };
        artifact.verify_probe(&probe).expect("matching probe");
        let mut wrong_build = probe;
        wrong_build.build_id = "other-build".into();
        assert_eq!(
            artifact.verify_probe(&wrong_build),
            Err(BootstrapError::ProbeMismatch("build id"))
        );
    }

    #[test]
    fn build_ids_cannot_escape_the_version_directory() {
        let temp = tempfile::tempdir().expect("temp");
        let artifact = PackagedArtifact {
            target: RemoteTarget::LinuxX86_64,
            protocol_major: PROTOCOL_MAJOR,
            build_id: "../other".into(),
            length: 0,
            sha256: "0".repeat(64),
            path: temp.path().join("missing"),
        };
        assert!(matches!(
            artifact.verify(),
            Err(BootstrapError::InvalidComponent {
                field: "build id",
                ..
            })
        ));
    }

    #[test]
    fn install_layout_is_versioned_and_nonce_scoped() {
        let layout = RemoteInstallLayout::new("build-1", "nonce_2").expect("layout");
        assert_eq!(
            layout.executable(),
            "$HOME/.cache/diri/bin/protocol-1/build-1/diri-remote"
        );
        assert_eq!(
            layout.temporary(),
            "$HOME/.cache/diri/bin/protocol-1/build-1/.tmp-nonce_2"
        );
        assert!(RemoteInstallLayout::new("../escape", "nonce").is_err());
        assert!(RemoteInstallLayout::new("build", "bad/slash").is_err());
    }
}
