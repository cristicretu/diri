//! Host-native process birth identity. Numeric PIDs alone are reusable.
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// One boot UUID, normalized independently of the OS's textual casing.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BootId([u8; 16]);

impl BootId {
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        let bytes = value.as_bytes();
        if bytes.len() != 36 || [8, 13, 18, 23].iter().any(|&i| bytes[i] != b'-') {
            return Err("boot identity must be a UUID");
        }
        let mut out = [0; 16];
        let mut nibble = 0;
        for (index, byte) in bytes.iter().enumerate() {
            if [8, 13, 18, 23].contains(&index) {
                continue;
            }
            let digit = (*byte as char)
                .to_digit(16)
                .ok_or("invalid boot UUID digit")? as u8;
            out[nibble / 2] = (out[nibble / 2] << 4) | digit;
            nibble += 1;
        }
        if out == [0; 16] {
            return Err("boot identity must not be nil");
        }
        Ok(Self(out))
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl Serialize for BootId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut value = String::with_capacity(36);
        use std::fmt::Write;
        for (index, byte) in self.0.iter().enumerate() {
            if [4, 6, 8, 10].contains(&index) {
                value.push('-');
            }
            write!(&mut value, "{byte:02x}").expect("write String");
        }
        serializer.serialize_str(&value)
    }
}
impl<'de> Deserialize<'de> for BootId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// No conversion to rounded seconds: each variant names its native units.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(
    tag = "platform",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ProcessBirth {
    /// `/proc/PID/stat` field 22, in `_SC_CLK_TCK` units since this boot.
    Linux {
        boot_id: BootId,
        start_ticks: u64,
        clock_ticks_per_second: u32,
    },
    /// `proc_bsdinfo` start timeval, with `kern.bootsessionuuid` (not volume UUID).
    Macos {
        boot_session: BootId,
        start_seconds: u64,
        start_microseconds: u32,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "IdentityWire")]
pub struct ProcessIdentity {
    pid: u32,
    birth: ProcessBirth,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityWire {
    pid: u32,
    birth: ProcessBirth,
}
impl TryFrom<IdentityWire> for ProcessIdentity {
    type Error = &'static str;
    fn try_from(value: IdentityWire) -> Result<Self, Self::Error> {
        Self::new(value.pid, value.birth)
    }
}
impl ProcessIdentity {
    pub fn new(pid: u32, birth: ProcessBirth) -> Result<Self, &'static str> {
        if pid == 0 || pid > i32::MAX as u32 {
            return Err("invalid process PID");
        }
        match birth {
            ProcessBirth::Linux {
                clock_ticks_per_second: 0,
                ..
            } => return Err("invalid process clock tick rate"),
            ProcessBirth::Macos {
                start_microseconds, ..
            } if start_microseconds >= 1_000_000 => {
                return Err("invalid process birth microseconds");
            }
            _ => {}
        }
        Ok(Self { pid, birth })
    }
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn birth(&self) -> ProcessBirth {
        self.birth
    }

    /// Stable v1 domain-separated bytes for durable identity binding. All
    /// integers are big-endian; boot UUIDs are their 16 normalized raw bytes.
    /// This format is independent of JSON field order and must never be changed
    /// in place. New formats need a new version byte after the fixed prefix.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"diri-process-identity\0\x01".to_vec();
        bytes.extend_from_slice(&self.pid.to_be_bytes());
        match self.birth {
            ProcessBirth::Linux {
                boot_id,
                start_ticks,
                clock_ticks_per_second,
            } => {
                bytes.push(1);
                bytes.extend_from_slice(boot_id.as_bytes());
                bytes.extend_from_slice(&start_ticks.to_be_bytes());
                bytes.extend_from_slice(&clock_ticks_per_second.to_be_bytes());
            }
            ProcessBirth::Macos {
                boot_session,
                start_seconds,
                start_microseconds,
            } => {
                bytes.push(2);
                bytes.extend_from_slice(boot_session.as_bytes());
                bytes.extend_from_slice(&start_seconds.to_be_bytes());
                bytes.extend_from_slice(&start_microseconds.to_be_bytes());
            }
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_round_trip_normalizes_uuid_and_preserves_native_units() {
        let value = r#"{"pid":42,"birth":{"platform":"linux","bootId":"AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE","startTicks":12345,"clockTicksPerSecond":100}}"#;
        let identity: ProcessIdentity = serde_json::from_str(value).unwrap();
        let encoded = serde_json::to_string(&identity).unwrap();
        assert!(encoded.contains("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"));
        assert_eq!(
            serde_json::from_str::<ProcessIdentity>(&encoded).unwrap(),
            identity
        );
        let mut expected = b"diri-process-identity\0\x01\0\0\0*\x01".to_vec();
        expected.extend_from_slice(&[
            0xaa, 0xaa, 0xaa, 0xaa, 0xbb, 0xbb, 0xcc, 0xcc, 0xdd, 0xdd, 0xee, 0xee, 0xee, 0xee,
            0xee, 0xee,
        ]);
        expected.extend_from_slice(&12345u64.to_be_bytes());
        expected.extend_from_slice(&100u32.to_be_bytes());
        assert_eq!(identity.canonical_bytes(), expected);
        let mac = ProcessIdentity::new(
            42,
            ProcessBirth::Macos {
                boot_session: BootId::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap(),
                start_seconds: 12345,
                start_microseconds: 100,
            },
        )
        .unwrap();
        assert_ne!(
            identity.canonical_bytes(),
            mac.canonical_bytes(),
            "platform units cannot alias"
        );
    }
    #[test]
    fn malformed_or_unknown_identity_fails_closed() {
        for value in [
            r#"{"pid":0,"birth":{"platform":"linux","bootId":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","startTicks":1,"clockTicksPerSecond":100}}"#,
            r#"{"pid":1,"birth":{"platform":"future"}}"#,
            r#"{"pid":1,"birth":{"platform":"macos","bootSession":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","startSeconds":1,"startMicroseconds":1000000}}"#,
        ] {
            assert!(serde_json::from_str::<ProcessIdentity>(value).is_err());
        }
        for value in [
            "",
            "00000000-0000-0000-0000-000000000000",
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeeg",
        ] {
            assert!(BootId::parse(value).is_err());
        }
    }
}
