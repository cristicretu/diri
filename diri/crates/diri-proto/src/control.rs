//! Control-channel envelopes from `DirijorProtocol/ControlMessage.swift`.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Map, Value};
use std::fmt;

/// Current additive-only wire protocol major.
pub const WIRE_VERSION: u32 = 1;

/// Maximum byte length of one newline-delimited control message.
pub const MAX_CONTROL_LINE_BYTES: usize = 4 * 1024 * 1024;

/// The protocol's untyped JSON payload.
pub type JsonValue = Value;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ControlError {
    pub code: String,
    pub message: String,
}

impl ControlError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new("bad_request", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("internal", message)
    }

    pub fn version_mismatch(message: impl Into<String>) -> Self {
        Self::new("version_mismatch", message)
    }

    pub fn unauthorized() -> Self {
        Self::new("unauthorized", "invalid or missing token")
    }
}

impl fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ControlError {}

/// One newline-delimited message on the control channel.
///
/// The envelope is intentionally not tagged. Swift discriminates requests,
/// events, failures, and successes by the presence of `method`, `event`, and
/// `err`, in that order; this implementation does the same.
#[derive(Clone, Debug, PartialEq)]
pub enum ControlMessage {
    Request {
        id: u64,
        method: String,
        params: Option<JsonValue>,
    },
    Response {
        id: u64,
        result: Result<JsonValue, ControlError>,
    },
    Event {
        name: String,
        seq: u64,
        params: JsonValue,
    },
}

impl Serialize for ControlMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Streamed field by field: a `session.list` reply or a record-bearing
        // event is hundreds of kilobytes, and building an intermediate object
        // first deep-cloned all of it on every write.
        use serde::ser::SerializeMap;
        match self {
            Self::Request { id, method, params } => {
                let mut object =
                    serializer.serialize_map(Some(2 + usize::from(params.is_some())))?;
                object.serialize_entry("id", id)?;
                object.serialize_entry("method", method)?;
                if let Some(params) = params {
                    object.serialize_entry("params", params)?;
                }
                object.end()
            }
            Self::Response { id, result: Ok(ok) } => {
                let mut object = serializer.serialize_map(Some(2))?;
                object.serialize_entry("id", id)?;
                object.serialize_entry("ok", ok)?;
                object.end()
            }
            Self::Response {
                id,
                result: Err(error),
            } => {
                let mut object = serializer.serialize_map(Some(2))?;
                object.serialize_entry("id", id)?;
                object.serialize_entry("err", error)?;
                object.end()
            }
            Self::Event { name, seq, params } => {
                let mut object = serializer.serialize_map(Some(3))?;
                object.serialize_entry("event", name)?;
                object.serialize_entry("seq", seq)?;
                object.serialize_entry("params", params)?;
                object.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ControlMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| de::Error::custom("control message must be a JSON object"))?;

        if present(object, "method") {
            return Ok(Self::Request {
                id: decode_required(object, "id")?,
                method: decode_required(object, "method")?,
                params: decode_optional(object, "params")?,
            });
        }

        if present(object, "event") {
            return Ok(Self::Event {
                name: decode_required(object, "event")?,
                seq: decode_required(object, "seq")?,
                params: decode_optional(object, "params")?.unwrap_or(Value::Null),
            });
        }

        let id = decode_required(object, "id")?;
        if present(object, "err") {
            Ok(Self::Response {
                id,
                result: Err(decode_required(object, "err")?),
            })
        } else {
            Ok(Self::Response {
                id,
                result: Ok(decode_optional(object, "ok")?.unwrap_or(Value::Null)),
            })
        }
    }
}

fn present(object: &Map<String, Value>, key: &str) -> bool {
    object.get(key).is_some_and(|value| !value.is_null())
}

fn decode_required<T, E>(object: &Map<String, Value>, key: &str) -> Result<T, E>
where
    T: serde::de::DeserializeOwned,
    E: de::Error,
{
    let value = object
        .get(key)
        .ok_or_else(|| E::custom(format!("missing field `{key}`")))?;
    serde_json::from_value(value.clone()).map_err(E::custom)
}

fn decode_optional<T, E>(object: &Map<String, Value>, key: &str) -> Result<Option<T>, E>
where
    T: serde::de::DeserializeOwned,
    E: de::Error,
{
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(E::custom),
    }
}

/// Serialize one message with its NDJSON line terminator.
pub fn encode_line(message: &ControlMessage) -> serde_json::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    Ok(line)
}

/// Decode one complete NDJSON line. A trailing CR/LF is accepted.
pub fn decode_line(mut line: &[u8]) -> serde_json::Result<ControlMessage> {
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line = &line[..line.len() - 1];
    }
    serde_json::from_slice(line)
}
