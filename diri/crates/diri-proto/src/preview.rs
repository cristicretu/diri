//! Read-only Engine-local terminal previews. This is not a Remote Holder attach.
use crate::model::SessionId;
use serde::{Deserialize, Serialize};

pub const PREVIEW_VERSION: u16 = 1;
pub const MAX_PREVIEWS: usize = 16;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewRequest {
    pub preview: SessionId,
    pub version: u16,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewReady {
    pub preview: SessionId,
    pub version: u16,
}
