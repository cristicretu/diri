//! On-demand process observations. No command arguments or environment values.
use serde::{Deserialize, Serialize};

use crate::process::ProcessIdentity;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    Unsupported,
    PermissionDenied,
    NotFound,
    InvalidData,
    TimedOut,
    Busy,
    Io,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProcessValue<T> {
    Available { value: T },
    Unavailable { reason: UnavailableReason },
}

impl<T> ProcessValue<T> {
    pub fn available(value: T) -> Self {
        Self::Available { value }
    }

    pub fn unavailable(reason: UnavailableReason) -> Self {
        Self::Unavailable { reason }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessUserIds {
    pub real: u32,
    pub effective: u32,
}

/// Account database facts for the effective UID, not the launching environment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessAccount {
    pub uid: u32,
    pub name: String,
    pub home_directory: String,
}

/// Each field was observed between matching native birth identities. Fields
/// need not be simultaneous: a live child can change cwd, executable or UID.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessFacts {
    pub identity: ProcessIdentity,
    pub executable: ProcessValue<String>,
    pub working_directory: ProcessValue<String>,
    pub user_ids: ProcessValue<ProcessUserIds>,
    pub account: ProcessValue<ProcessAccount>,
    /// Native process-group ID; never labeled as an individual PID.
    pub process_group: ProcessValue<u32>,
    /// Available(None) means the kernel reports no controlling foreground group.
    pub foreground_process_group: ProcessValue<Option<u32>>,
}

impl ProcessFacts {
    pub fn validate(&self) -> Result<(), &'static str> {
        for field in [&self.executable, &self.working_directory] {
            if let ProcessValue::Available { value } = field
                && (!value.starts_with('/') || value.len() > 16 * 1024 || value.contains('\0'))
            {
                return Err("invalid native process path");
            }
        }
        if matches!(self.process_group, ProcessValue::Available { value } if value == 0 || value > i32::MAX as u32)
            || matches!(self.foreground_process_group, ProcessValue::Available { value: Some(value) } if value == 0 || value > i32::MAX as u32)
        {
            return Err("invalid native process group");
        }
        validate_account(&self.account).map_err(|_| "invalid process account")?;
        if let ProcessValue::Available { value: account } = &self.account
            && !matches!(&self.user_ids, ProcessValue::Available { value: ids } if ids.effective == account.uid)
        {
            return Err("process account does not match effective UID");
        }
        Ok(())
    }
}

/// A read-only observation of the currently bound session child.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionProcessInfo {
    #[serde(rename = "sessionID")]
    pub session_id: crate::SessionId,
    pub host: Option<String>,
    pub observed_at: crate::DateMillis,
    pub process: ProcessFacts,
}

pub const MAX_ACCOUNT_REPLY_BYTES: usize = 8192;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AccountReply {
    version: u8,
    result: ProcessValue<ProcessAccount>,
}

pub fn encode_account_reply(result: ProcessValue<ProcessAccount>) -> std::io::Result<Vec<u8>> {
    validate_account(&result)?;
    let bytes =
        serde_json::to_vec(&AccountReply { version: 1, result }).map_err(std::io::Error::other)?;
    if bytes.len() > MAX_ACCOUNT_REPLY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "account reply exceeds bound",
        ));
    }
    Ok(bytes)
}

pub fn decode_account_reply(
    bytes: &[u8],
    expected_uid: u32,
) -> std::io::Result<ProcessValue<ProcessAccount>> {
    if bytes.len() > MAX_ACCOUNT_REPLY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "account reply exceeds bound",
        ));
    }
    let reply: AccountReply = serde_json::from_slice(bytes).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid account reply")
    })?;
    if reply.version != 1
        || matches!(&reply.result, ProcessValue::Available { value } if value.uid != expected_uid)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unsupported or mismatched account reply",
        ));
    }
    validate_account(&reply.result)?;
    Ok(reply.result)
}

fn validate_account(result: &ProcessValue<ProcessAccount>) -> std::io::Result<()> {
    if let ProcessValue::Available { value } = result
        && (value.name.is_empty()
            || value.name.len() > 1024
            || value.name.contains('\0')
            || !value.home_directory.starts_with('/')
            || value.home_directory.len() > 4096
            || value.home_directory.contains('\0'))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid account fields",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_fields_are_explicit_and_unknown_reasons_stay_unavailable() {
        let value = ProcessValue::<String>::unavailable(UnavailableReason::TimedOut);
        assert_eq!(
            serde_json::to_value(&value).unwrap(),
            serde_json::json!({"status":"unavailable", "reason":"timed_out"})
        );
        let future: ProcessValue<String> = serde_json::from_value(
            serde_json::json!({"status":"unavailable", "reason":"future_reason"}),
        )
        .unwrap();
        assert_eq!(
            future,
            ProcessValue::unavailable(UnavailableReason::Unknown)
        );
    }
}
