//! Inline terminal images (Kitty graphics, iTerm2 `OSC 1337 File`).
//!
//! The Engine's emulator places an image by giving the cells it covers a
//! hyperlink whose URI is an [`ImageAnchor`]. The anchor rides the existing
//! grid metadata, so a placement scrolls into history, is overwritten by
//! later output and is evicted with its rows exactly like text. Pixels never
//! travel with the grid: a client that meets an unknown key reads them once
//! with `session.read_image`.

use serde::{Deserialize, Serialize};

/// Scheme of an anchor URI. Never a navigable link.
pub const ANCHOR_SCHEME: &str = "diri-image:";

/// Largest stored image a client may be asked to read, in bytes. Engines
/// refuse larger transmissions, so a reader can size its buffer from it.
pub const MAX_IMAGE_BYTES: usize = 24 * 1024 * 1024;

/// Bytes returned per `session.read_image` call: well inside the 4 MiB
/// control-line limit once base64-encoded.
pub const READ_IMAGE_CHUNK_BYTES: usize = 2 * 1024 * 1024;

/// One row of one image placement, as carried by a cell hyperlink.
///
/// `row` is this grid row's offset inside the image; `col` is the column the
/// image's left edge was placed at; `cols` × `rows` is the cell box the image
/// is fitted into.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageAnchor {
    pub key: String,
    pub row: u16,
    pub col: u16,
    pub cols: u16,
    pub rows: u16,
}

impl ImageAnchor {
    #[must_use]
    pub fn to_uri(&self) -> String {
        format!(
            "{ANCHOR_SCHEME}{}/{}/{}/{}/{}",
            self.key, self.row, self.col, self.cols, self.rows
        )
    }

    /// `None` for any URI that is not a well-formed anchor.
    #[must_use]
    pub fn parse(uri: &str) -> Option<Self> {
        let rest = uri.strip_prefix(ANCHOR_SCHEME)?;
        let mut parts = rest.split('/');
        let key = parts.next()?;
        if !valid_key(key) {
            return None;
        }
        let mut number = || parts.next()?.parse::<u16>().ok();
        let anchor = Self {
            key: key.to_owned(),
            row: number()?,
            col: number()?,
            cols: number()?,
            rows: number()?,
        };
        if parts.next().is_some()
            || anchor.cols == 0
            || anchor.rows == 0
            || anchor.row >= anchor.rows
        {
            return None;
        }
        Some(anchor)
    }
}

/// Whether `uri` is an image anchor, without parsing it.
#[must_use]
pub fn is_anchor(uri: &str) -> bool {
    uri.starts_with(ANCHOR_SCHEME)
}

/// Keys are short and URI-safe: ASCII letters, digits, `.` and `-`.
#[must_use]
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 40
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-')
}

/// How a stored image's bytes are encoded.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InlineImageFormat {
    Png,
    Jpeg,
    Gif,
    /// 8-bit RGBA, `width * height * 4` bytes (Kitty `f=32`).
    Rgba,
    /// 8-bit RGB, `width * height * 3` bytes (Kitty `f=24`).
    Rgb,
}

impl InlineImageFormat {
    /// Bytes per pixel of a raw format.
    #[must_use]
    pub const fn raw_channels(self) -> Option<usize> {
        match self {
            Self::Rgba => Some(4),
            Self::Rgb => Some(3),
            Self::Png | Self::Jpeg | Self::Gif => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_round_trip_and_reject_junk() {
        let anchor = ImageAnchor {
            key: "1a2b3c4d.7".into(),
            row: 2,
            col: 4,
            cols: 10,
            rows: 3,
        };
        let uri = anchor.to_uri();
        assert_eq!(uri, "diri-image:1a2b3c4d.7/2/4/10/3");
        assert_eq!(ImageAnchor::parse(&uri), Some(anchor));
        for junk in [
            "https://example.com",
            "diri-image:",
            "diri-image:k/0/0/1",
            "diri-image:k/0/0/1/1/9",
            "diri-image:k/3/0/1/3",
            "diri-image:k/0/0/0/3",
            "diri-image:a b/0/0/1/1",
            "diri-image:k/-1/0/1/1",
        ] {
            assert_eq!(ImageAnchor::parse(junk), None, "{junk}");
        }
    }
}
