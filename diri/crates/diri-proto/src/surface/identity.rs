//! What an agent decides about how its app looks and runs: the icon, accent
//! and style it shows with, and (for a Lua app) how its `sync` keeps it
//! current.
//!
//! Nothing here names a vendor. An agent that wants a Slack glyph says so
//! (`{"symbol": "slack"}`); one that wants a 🎧 says that.

use serde::{Deserialize, Serialize};

use super::SurfaceError;

/// The glyph a surface shows in the sidebar, the panel tab, ⌘K and its
/// header, and on links and checklist chips.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Icon {
    /// One emoji (or a short grapheme cluster): `{"emoji": "🎧"}`.
    Emoji(String),
    /// A name from Diri's built-in icon set: `{"symbol": "chart.bar"}`.
    Symbol(String),
    /// An image in the surface's own folder: `{"asset": "icon.svg"}`.
    Asset(String),
}

impl Icon {
    pub fn validate(&self, path: &str) -> Result<(), SurfaceError> {
        let bad = |message: &str| Err(SurfaceError::new("invalid_icon", message, path));
        match self {
            Self::Emoji(text) => {
                if text.trim().is_empty() || text.len() > 32 || text.chars().count() > 8 {
                    return bad("an emoji icon is one emoji (at most 8 characters)");
                }
                if text.chars().any(|c| c.is_ascii_alphanumeric() || c.is_control()) {
                    return bad("an emoji icon holds no letters or digits; use symbol");
                }
            }
            Self::Symbol(name) => {
                let valid = !name.is_empty()
                    && name.len() <= 64
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.');
                if !valid {
                    return bad("a symbol icon is a built-in icon name like chart.bar");
                }
            }
            Self::Asset(file) => {
                if !is_valid_app_path(file) || !IMAGE_EXTENSIONS.iter().any(|ext| has_ext(file, ext))
                {
                    return bad("an asset icon is a png, svg, jpg or webp file in the surface folder");
                }
            }
        }
        Ok(())
    }
}

/// How dense the surface reads and how its canvas is painted.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    #[default]
    Comfortable,
    Compact,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Canvas {
    /// The theme's own background.
    #[default]
    Plain,
    /// A faint wash of the accent behind everything.
    Tinted,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Header {
    /// Title, subtitle, freshness line and actions.
    #[default]
    Full,
    /// One line: icon, title and actions.
    Minimal,
}

/// The agent's look for its surface. Every field has a default, so a
/// surface that says nothing still looks like Diri.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Style {
    #[serde(default)]
    pub density: Density,
    #[serde(default)]
    pub canvas: Canvas,
    #[serde(default)]
    pub header: Header,
    /// Card corner radius in points (0-24); the theme's when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<f32>,
}

impl Style {
    pub fn validate(&self) -> Result<(), SurfaceError> {
        if self.radius.is_some_and(|r| !(0.0..=24.0).contains(&r)) {
            return Err(SurfaceError::new("invalid_value", "radius is 0-24", "style"));
        }
        Ok(())
    }
}

/// `#rrggbb`, parsed.
pub fn parse_accent(text: &str) -> Option<(u8, u8, u8)> {
    let hex = text.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

/// A native Lua app: `app.lua` in the surface folder defines `render`,
/// `on_event` and `sync` (`diri_script::app`). Diri draws what `render`
/// returns with GPUI; no web view, no other process.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct AppSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncSpec>,
}

/// When `sync(ctx)` runs. It refreshes `data.json` without an agent;
/// `ctx.exec` and `ctx.fetch` work only once the person approved this exact
/// `app.lua` (its SHA-256), so an edited app is asked again.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct SyncSpec {
    /// Re-run every this many seconds while the app is on screen (never
    /// while it is hidden). Absent: only on open and on Refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_s: Option<u32>,
    /// Run when the app opens.
    #[serde(default = "yes")]
    pub on_open: bool,
    #[serde(default = "default_timeout")]
    pub timeout_s: u32,
}

impl Default for SyncSpec {
    fn default() -> Self {
        Self {
            every_s: None,
            on_open: true,
            timeout_s: default_timeout(),
        }
    }
}

fn yes() -> bool {
    true
}

fn default_timeout() -> u32 {
    30
}

pub const MIN_SYNC_INTERVAL_S: u32 = 15;
pub const MAX_SYNC_INTERVAL_S: u32 = 3600;
pub const MAX_SYNC_TIMEOUT_S: u32 = 120;
/// The most `sync` may return as data.
pub const MAX_DATA_BYTES: usize = 1024 * 1024;
/// The entry point of every Lua app.
pub const APP_ENTRY: &str = "app.lua";

impl SyncSpec {
    pub fn validate(&self) -> Result<(), SurfaceError> {
        if self
            .every_s
            .is_some_and(|s| !(MIN_SYNC_INTERVAL_S..=MAX_SYNC_INTERVAL_S).contains(&s))
        {
            return Err(SurfaceError::new(
                "invalid_sync",
                format!("every_s is {MIN_SYNC_INTERVAL_S}-{MAX_SYNC_INTERVAL_S}"),
                "app.sync",
            ));
        }
        if !(1..=MAX_SYNC_TIMEOUT_S).contains(&self.timeout_s) {
            return Err(SurfaceError::new(
                "invalid_sync",
                format!("timeout_s is 1-{MAX_SYNC_TIMEOUT_S}"),
                "app.sync",
            ));
        }
        Ok(())
    }
}

impl AppSpec {
    pub fn validate(&self) -> Result<(), SurfaceError> {
        match &self.sync {
            Some(sync) => sync.validate(),
            None => Ok(()),
        }
    }
}

pub const IMAGE_EXTENSIONS: &[&str] = &["png", "svg", "jpg", "jpeg", "webp", "gif"];

/// File types an app folder may hold.
pub const APP_EXTENSIONS: &[&str] = &[
    "lua", "json", "svg", "png", "jpg", "jpeg", "webp", "gif", "txt", "md",
];
pub const MAX_APP_FILE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_APP_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_APP_FILES: usize = 128;

fn has_ext(path: &str, ext: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, found)| found.eq_ignore_ascii_case(ext))
}

/// A path inside a surface folder: `a/b.js`, at most 3 segments of
/// `[A-Za-z0-9_.-]`, no hidden or parent segments, an allowed extension,
/// and never the reserved `manifest.json`.
pub fn is_valid_app_path(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    if path.len() > 200 || segments.len() > 3 || path == "manifest.json" || path == "state.json" {
        return false;
    }
    let segments_ok = segments.iter().all(|segment| {
        !segment.is_empty()
            && !segment.starts_with('.')
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    });
    segments_ok && APP_EXTENSIONS.iter().any(|ext| has_ext(path, ext))
}

/// Bounded state a Lua app keeps between events.
pub const MAX_APP_STATE_BYTES: usize = 64 * 1024;

pub(super) fn validate_accent(accent: Option<&str>) -> Result<(), SurfaceError> {
    match accent {
        Some(accent) if parse_accent(accent).is_none() => Err(SurfaceError::new(
            "invalid_value",
            "accent is a #rrggbb color",
            "accent",
        )),
        _ => Ok(()),
    }
}

pub(super) fn validate_app_state(state: Option<&serde_json::Value>) -> Result<(), SurfaceError> {
    let Some(state) = state else { return Ok(()) };
    if !state.is_object() {
        return Err(SurfaceError::new("invalid_value", "app state is a JSON object", "state.app"));
    }
    let size = serde_json::to_vec(state).map_or(usize::MAX, |bytes| bytes.len());
    if size > MAX_APP_STATE_BYTES {
        return Err(SurfaceError::new(
            "too_large",
            format!("app state is {size} bytes; the limit is {MAX_APP_STATE_BYTES}"),
            "state.app",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_are_checked() {
        assert!(Icon::Emoji("🎧".into()).validate("").is_ok());
        assert!(Icon::Emoji("abc".into()).validate("").is_err());
        assert!(Icon::Symbol("chart.bar".into()).validate("").is_ok());
        assert!(Icon::Symbol("../x".into()).validate("").is_err());
        assert!(Icon::Asset("icon.svg".into()).validate("").is_ok());
        assert!(Icon::Asset("../icon.svg".into()).validate("").is_err());
        assert!(Icon::Asset("app.lua".into()).validate("").is_err());
    }

    #[test]
    fn app_paths_stay_inside_the_folder() {
        for ok in ["app.lua", "lib/chart.lua", "assets/img/a.png", "data.json"] {
            assert!(is_valid_app_path(ok), "{ok}");
        }
        for bad in ["../x.lua", "a/../b.lua", ".env", "a/.git/x.lua", "manifest.json", "x.exe", "/abs.lua", "a/b/c/d.lua", "index.html", "x.js"] {
            assert!(!is_valid_app_path(bad), "{bad}");
        }
    }

    #[test]
    fn sync_specs_are_bounded() {
        let sync = |every: Option<u32>, timeout_s: u32| SyncSpec {
            every_s: every,
            on_open: true,
            timeout_s,
        };
        assert!(sync(Some(60), 30).validate().is_ok());
        assert!(sync(Some(1), 30).validate().is_err(), "polling is never faster than 15 s");
        assert!(sync(None, 0).validate().is_err());
        assert!(sync(None, 999).validate().is_err());
    }
}
