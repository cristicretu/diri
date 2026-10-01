use std::fmt;
use std::io;

#[derive(Debug)]
pub enum UpdateError {
    /// The running binary is not inside a `.app`, or the bundle is unsigned —
    /// a `cargo run` build has nothing to update and no signature to pin to.
    NotUpdatable(String),
    Network(String),
    /// The feed parsed as JSON but is not a feed we understand.
    Feed(String),
    /// A download URL that failed the origin/shape checks in `crate::net`.
    UntrustedUrl(String),
    Integrity(String),
    /// The downloaded bundle is not a notarized build of *this* app.
    Signature(String),
    /// The installed bundle sits somewhere this user cannot write.
    NotWritable(String),
    Io(io::Error),
    /// A helper (`curl`, `ditto`, `codesign`, …) exited non-zero.
    Tool {
        tool: &'static str,
        detail: String,
    },
}

impl UpdateError {
    #[cfg(unix)]
    pub(crate) fn tool(tool: &'static str, detail: impl Into<String>) -> Self {
        Self::Tool {
            tool,
            detail: detail.into(),
        }
    }

    /// One line, safe to show in the sidebar or settings pane.
    pub fn user_facing(&self) -> String {
        match self {
            Self::NotUpdatable(_) => "Updates are off for this build".to_owned(),
            Self::Network(_) => "Couldn't reach the releases host".to_owned(),
            Self::Feed(_) => "The update feed looks malformed".to_owned(),
            Self::UntrustedUrl(_) => "The update feed pointed somewhere unexpected".to_owned(),
            Self::Integrity(_) => "The download was incomplete or corrupt".to_owned(),
            Self::Signature(_) => "The download failed its signature check".to_owned(),
            Self::NotWritable(_) => "diri can't write to its own folder".to_owned(),
            Self::Io(_) | Self::Tool { .. } => "The update couldn't be installed".to_owned(),
        }
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUpdatable(detail) => write!(formatter, "not updatable: {detail}"),
            Self::Network(detail) => write!(formatter, "network error: {detail}"),
            Self::Feed(detail) => write!(formatter, "bad update feed: {detail}"),
            Self::UntrustedUrl(detail) => write!(formatter, "untrusted update URL: {detail}"),
            Self::Integrity(detail) => write!(formatter, "integrity check failed: {detail}"),
            Self::Signature(detail) => write!(formatter, "signature check failed: {detail}"),
            Self::NotWritable(detail) => write!(formatter, "install location: {detail}"),
            Self::Io(error) => write!(formatter, "io error: {error}"),
            Self::Tool { tool, detail } => write!(formatter, "{tool} failed: {detail}"),
        }
    }
}

impl std::error::Error for UpdateError {}

impl From<io::Error> for UpdateError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub type Result<T> = std::result::Result<T, UpdateError>;
