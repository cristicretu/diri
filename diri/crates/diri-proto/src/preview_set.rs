//! Versioned, receive-only local preview sets. Terminal frames retain their
//! existing codec; small JSON headers identify the subscription incarnation.
use crate::SessionId;
use crate::frames::{Frame, FrameCodec, FrameType, MAX_FRAME_BYTES};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io;

pub const PREVIEW_SET_VERSION: u16 = 1;
pub const MAX_PREVIEW_SET_MEMBERS: usize = 64;
pub const MAX_PREVIEW_SESSION_ID_BYTES: usize = 256;
pub const MAX_PREVIEW_SET_REQUEST_BYTES: usize = 64 * 1024;
pub const MAX_PREVIEW_SET_HEADER_BYTES: usize = 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewSetRequest {
    pub preview_set: bool,
    pub version: u16,
    /// Explicit opt-in for consumers that render terminal image state.
    #[serde(default)]
    pub terminal_graphics: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewSetReady {
    pub version: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewMember {
    pub session_id: SessionId,
    /// New on remove/re-add, including membership updates coalesced locally.
    pub generation: u64,
}

impl PreviewMember {
    pub fn validate(&self) -> io::Result<()> {
        if self.generation == 0
            || self.session_id.0.is_empty()
            || self.session_id.0.len() > MAX_PREVIEW_SESSION_ID_BYTES
            || self.session_id.0.contains('\0')
        {
            return Err(invalid("invalid preview subscription identity"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewSetMembership {
    pub members: Vec<PreviewMember>,
}

impl PreviewSetMembership {
    pub fn validate(&self) -> io::Result<()> {
        if self.members.len() > MAX_PREVIEW_SET_MEMBERS {
            return Err(invalid("too many preview subscriptions"));
        }
        let mut seen = HashSet::new();
        for member in &self.members {
            member.validate()?;
            if !seen.insert(&member.session_id) {
                return Err(invalid("duplicate preview session"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PreviewUnavailable {
    Missing,
    AdmissionLimit,
    AdmissionTimeout,
    PublisherUnavailable,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum PreviewSetHeader {
    Chunk {
        member: PreviewMember,
        frame_bytes: usize,
    },
    Unavailable {
        member: PreviewMember,
        reason: PreviewUnavailable,
    },
}

impl PreviewSetHeader {
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        self.validate()?;
        let mut bytes = serde_json::to_vec(self).map_err(invalid)?;
        if bytes.len() > MAX_PREVIEW_SET_HEADER_BYTES {
            return Err(invalid("oversized preview envelope"));
        }
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn validate(&self) -> io::Result<()> {
        match self {
            Self::Chunk {
                member,
                frame_bytes,
            } => {
                member.validate()?;
                if !(5..=MAX_FRAME_BYTES + 5).contains(frame_bytes) {
                    return Err(invalid("invalid preview frame length"));
                }
            }
            Self::Unavailable { member, .. } => member.validate()?,
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviewSetPacket {
    Chunk {
        member: PreviewMember,
        frame: Frame,
    },
    Unavailable {
        member: PreviewMember,
        reason: PreviewUnavailable,
    },
}

impl PreviewSetPacket {
    pub fn member(&self) -> &PreviewMember {
        match self {
            Self::Chunk { member, .. } | Self::Unavailable { member, .. } => member,
        }
    }
}

#[derive(Default)]
pub struct PreviewSetDecoder {
    header: Vec<u8>,
    pending: Option<(PreviewMember, usize)>,
    frames: FrameCodec,
}

impl PreviewSetDecoder {
    pub fn feed(&mut self, mut bytes: &[u8]) -> io::Result<Vec<PreviewSetPacket>> {
        let mut packets = Vec::new();
        while !bytes.is_empty() {
            if let Some((member, remaining)) = self.pending.as_mut() {
                let count = bytes.len().min(*remaining);
                let mut frames = self.frames.feed(&bytes[..count]).map_err(invalid)?;
                bytes = &bytes[count..];
                *remaining -= count;
                if *remaining != 0 {
                    if !frames.is_empty() {
                        return Err(invalid("preview frame length mismatch"));
                    }
                    continue;
                }
                if frames.len() != 1 || self.frames.buffered_len() != 0 {
                    return Err(invalid("preview envelope must contain exactly one frame"));
                }
                let frame = frames.pop().unwrap();
                if !matches!(frame.frame_type, FrameType::Grid | FrameType::Modes) {
                    return Err(invalid("unexpected frame on receive-only preview set"));
                }
                packets.push(PreviewSetPacket::Chunk {
                    member: member.clone(),
                    frame,
                });
                self.pending = None;
                continue;
            }
            let newline = bytes.iter().position(|byte| *byte == b'\n');
            let count = newline.unwrap_or(bytes.len());
            if self.header.len().saturating_add(count) > MAX_PREVIEW_SET_HEADER_BYTES {
                return Err(invalid("oversized preview envelope"));
            }
            self.header.extend_from_slice(&bytes[..count]);
            bytes = &bytes[count..];
            if newline.is_none() {
                break;
            }
            bytes = &bytes[1..];
            let header: PreviewSetHeader = serde_json::from_slice(&self.header).map_err(invalid)?;
            self.header.clear();
            header.validate()?;
            match header {
                PreviewSetHeader::Chunk {
                    member,
                    frame_bytes,
                } => self.pending = Some((member, frame_bytes)),
                PreviewSetHeader::Unavailable { member, reason } => {
                    packets.push(PreviewSetPacket::Unavailable { member, reason })
                }
            }
        }
        Ok(packets)
    }

    pub fn has_partial_packet(&self) -> bool {
        !self.header.is_empty() || self.pending.is_some()
    }
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn member() -> PreviewMember {
        PreviewMember {
            session_id: SessionId::new("fixture"),
            generation: 2,
        }
    }
    fn wire(frame: &Frame) -> Vec<u8> {
        let body = FrameCodec::encode(frame).unwrap();
        let mut bytes = PreviewSetHeader::Chunk {
            member: member(),
            frame_bytes: body.len(),
        }
        .encode()
        .unwrap();
        bytes.extend(body);
        bytes
    }
    #[test]
    fn every_fragmentation_preserves_identity_frame_and_following_unavailable() {
        let frame = Frame::modes(false, crate::terminal::MouseModes::OFF);
        let mut bytes = wire(&frame);
        bytes.extend(
            PreviewSetHeader::Unavailable {
                member: member(),
                reason: PreviewUnavailable::Missing,
            }
            .encode()
            .unwrap(),
        );
        let expected = vec![
            PreviewSetPacket::Chunk {
                member: member(),
                frame,
            },
            PreviewSetPacket::Unavailable {
                member: member(),
                reason: PreviewUnavailable::Missing,
            },
        ];
        for split in 0..=bytes.len() {
            let mut decoder = PreviewSetDecoder::default();
            let mut packets = decoder.feed(&bytes[..split]).unwrap();
            packets.extend(decoder.feed(&bytes[split..]).unwrap());
            assert_eq!(packets, expected);
            assert!(!decoder.has_partial_packet());
        }
    }
    #[test]
    fn rejects_mutations_oversized_headers_lengths_and_duplicate_members() {
        assert!(
            PreviewSetDecoder::default()
                .feed(&wire(&Frame::input(b"no")))
                .is_err()
        );
        assert!(
            PreviewSetDecoder::default()
                .feed(&vec![b'x'; MAX_PREVIEW_SET_HEADER_BYTES + 1])
                .is_err()
        );
        assert!(
            PreviewSetHeader::Chunk {
                member: member(),
                frame_bytes: MAX_FRAME_BYTES + 6
            }
            .encode()
            .is_err()
        );
        assert!(
            PreviewSetMembership {
                members: vec![member(), member()]
            }
            .validate()
            .is_err()
        );
        let mut bad = member();
        bad.generation = 0;
        assert!(bad.validate().is_err());
    }
    #[test]
    fn rejects_envelope_with_extra_partial_frame() {
        let body =
            FrameCodec::encode(&Frame::modes(false, crate::terminal::MouseModes::OFF)).unwrap();
        let mut bytes = PreviewSetHeader::Chunk {
            member: member(),
            frame_bytes: body.len() + 1,
        }
        .encode()
        .unwrap();
        bytes.extend(body);
        bytes.push(8);
        assert!(PreviewSetDecoder::default().feed(&bytes).is_err());
    }
}
