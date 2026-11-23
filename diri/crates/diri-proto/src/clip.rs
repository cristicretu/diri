//! Context Ring: screen and browser captures ("clips") kept by the local
//! Engine, and the per-tab memory of which Session each page's feedback went
//! to.
//!
//! A clip is a cropped screenshot plus whatever text Diri could recover for
//! the captured region (the page selection, Accessibility text or on-device
//! OCR) and where it came from (app, window, browser tab). Clips live only on
//! this Mac, in an owner-only directory, and are bounded by count, bytes and
//! age. Nothing here is ever logged or sent to telemetry.

use serde::{Deserialize, Serialize};

use crate::model::{AgentKind, SessionId, SessionStatus};

/// The most clips listed or kept by default.
pub const DEFAULT_MAX_CLIPS: usize = 64;
/// The most bytes (screenshots plus text) the ring keeps by default.
pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// How long a clip is kept, in milliseconds: 7 days.
pub const DEFAULT_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// The largest screenshot a clip accepts.
pub const MAX_PNG_BYTES: u64 = 48 * 1024 * 1024;
/// The most extracted text a clip keeps, in bytes; longer text is cut at a
/// character boundary and marked truncated.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;
/// The longest prompt a dispatch accepts.
pub const MAX_PROMPT_BYTES: usize = 16 * 1024;

/// Where the clip's text came from, best first.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ClipTextOrigin {
    /// Text the person had selected on the page or in the app.
    Selection,
    /// Accessibility text inside the captured rectangle.
    Accessibility,
    /// On-device Vision text recognition over the screenshot.
    Ocr,
    /// No text was recovered.
    #[default]
    None,
}

/// What was in front when the clip was taken. Every field is optional:
/// a denied permission or an app without scripting leaves it empty.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClipSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    /// The active tab's URL, when the front app is a browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_title: Option<String>,
    /// The browser's display name (`Arc`, `Safari`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<String>,
}

/// One clip in the ring.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClipRecord {
    pub id: String,
    pub created_at_ms: u64,
    pub source: ClipSource,
    /// The canonical page key per-tab memory is filed under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default)]
    pub text_origin: ClipTextOrigin,
    #[serde(default)]
    pub text_truncated: bool,
    /// The screenshot inside the ring directory, owner-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub png_path: Option<String>,
    #[serde(default)]
    pub png_bytes: u64,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    /// Sessions this clip was sent to, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sent_to: Vec<SessionId>,
}

impl ClipRecord {
    /// Bytes this clip charges against the ring's budget.
    pub fn charged_bytes(&self) -> u64 {
        self.png_bytes + self.text.as_ref().map_or(0, |text| text.len() as u64)
    }

    /// The short pill the HUD and lists show: `Slack (Arc) · 412 chars + PNG`
    /// or `localhost:3000/settings`.
    pub fn summary(&self) -> String {
        let place = crate::clip::summary_place(&self.source, self.tab_key.as_deref());
        let chars = self.text.as_ref().map_or(0, |text| text.chars().count());
        let mut parts = Vec::new();
        if chars > 0 {
            parts.push(format!("{chars} chars"));
        }
        if self.png_path.is_some() {
            parts.push("PNG".to_owned());
        }
        if parts.is_empty() {
            place
        } else {
            format!("{place} · {}", parts.join(" + "))
        }
    }
}

fn summary_place(source: &ClipSource, tab_key: Option<&str>) -> String {
    let browser = source.browser.as_deref();
    if let Some(key) = tab_key
        && let Some(rest) = key.strip_prefix("http://localhost:")
    {
        return format!("localhost:{rest}");
    }
    let site = source
        .url
        .as_deref()
        .and_then(host_label)
        .or_else(|| source.app_name.clone())
        .unwrap_or_else(|| "Screen".to_owned());
    match browser {
        Some(browser) if !site.eq_ignore_ascii_case(browser) => format!("{site} ({browser})"),
        _ => site,
    }
}

/// `app.slack.com` → `Slack`, `github.com` → `GitHub`; otherwise the host.
fn host_label(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = rest.split(['/', '?', '#']).next()?.to_ascii_lowercase();
    let host = host.rsplit_once('@').map_or(host.as_str(), |(_, h)| h);
    let host = host.split(':').next().unwrap_or(host);
    let known = [
        ("slack.com", "Slack"),
        ("github.com", "GitHub"),
        ("linear.app", "Linear"),
        ("figma.com", "Figma"),
        ("notion.so", "Notion"),
        ("google.com", "Google"),
    ];
    for (suffix, label) in known {
        if host == suffix || host.ends_with(&format!(".{suffix}")) {
            return Some(label.to_owned());
        }
    }
    (!host.is_empty()).then(|| host.trim_start_matches("www.").to_owned())
}

/// `clip.save`: ingest a capture the app just took.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClipSaveParams {
    #[serde(default)]
    pub source: ClipSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default)]
    pub text_origin: ClipTextOrigin,
    /// A PNG the caller wrote, owned by this user. The Engine copies it
    /// into the ring; the caller removes its own file afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub png_path: Option<String>,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

/// `clip.list`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClipListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One line of `clip.list`: no text body, only its size and a short head.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipSummary {
    pub id: String,
    pub created_at_ms: u64,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_key: Option<String>,
    pub text_chars: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub png_path: Option<String>,
}

impl From<&ClipRecord> for ClipSummary {
    fn from(clip: &ClipRecord) -> Self {
        Self {
            id: clip.id.clone(),
            created_at_ms: clip.created_at_ms,
            summary: clip.summary(),
            url: clip.source.url.clone(),
            title: clip
                .source
                .page_title
                .clone()
                .or_else(|| clip.source.window_title.clone()),
            tab_key: clip.tab_key.clone(),
            text_chars: clip.text.as_ref().map_or(0, |text| text.chars().count()),
            png_path: clip.png_path.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipListResult {
    pub clips: Vec<ClipSummary>,
}

/// `clip.get` / `clip.delete` / `clip.targets`: a clip id, or `latest`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClipIdParams {
    /// A clip id, or `latest` for the newest clip.
    pub id: String,
}

/// The latest exchange of a Session, so the person can tell chats apart.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationPreview {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

impl ConversationPreview {
    pub fn is_empty(&self) -> bool {
        self.user.is_none() && self.agent.is_none()
    }
}

/// Why a target ranks where it does.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ClipTargetReason {
    /// Feedback from this page went here last time.
    Remembered,
    /// This Session serves the captured `localhost` port.
    ServesPort,
    /// Ordinary recency.
    Recent,
}

/// One Session the HUD can send a clip to.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipTarget {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub title: String,
    pub kind: AgentKind,
    pub status: SessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub reason: ClipTargetReason,
    #[serde(default, skip_serializing_if = "ConversationPreview::is_empty")]
    pub preview: ConversationPreview,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipTargetsResult {
    /// Best first; a remembered Session, when still open, is first.
    pub targets: Vec<ClipTarget>,
}

/// Where `clip.dispatch` sends the clip.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ClipDestination {
    /// An open Session.
    Session {
        #[serde(rename = "sessionID")]
        session_id: SessionId,
    },
    /// A new Agent in `cwd`, started with the clip as its first prompt.
    New { kind: AgentKind, cwd: String },
    /// Keep it in the ring only.
    Ring,
}

/// `clip.dispatch`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClipDispatchParams {
    /// A clip id, or `latest`.
    pub id: String,
    #[serde(default)]
    pub prompt: String,
    pub destination: ClipDestination,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipDispatchResult {
    pub clip_id: String,
    #[serde(rename = "sessionID", default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    /// Whether the prompt reached a terminal; false for `ring`.
    pub delivered: bool,
    /// The page key the destination is now remembered for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remembered_for: Option<String>,
}

/// `clip.capture_requested`: someone (the CLI, an Agent) asked the app to
/// open the capture overlay. Carries no content.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClipCaptureParams {
    /// `region` (drag a rectangle, the default) or `window`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(source: ClipSource, tab_key: Option<&str>, text: Option<&str>) -> ClipRecord {
        ClipRecord {
            id: "c".into(),
            created_at_ms: 0,
            source,
            tab_key: tab_key.map(str::to_owned),
            text: text.map(str::to_owned),
            text_origin: ClipTextOrigin::Ocr,
            text_truncated: false,
            png_path: Some("/x.png".into()),
            png_bytes: 10,
            width: 1,
            height: 1,
            sent_to: Vec::new(),
        }
    }

    #[test]
    fn summary_names_the_site_and_browser() {
        let source = ClipSource {
            url: Some("https://app.slack.com/client/T1/C2".into()),
            browser: Some("Arc".into()),
            ..Default::default()
        };
        let text = "x".repeat(412);
        assert_eq!(
            clip(source, None, Some(&text)).summary(),
            "Slack (Arc) · 412 chars + PNG"
        );
    }

    #[test]
    fn summary_shows_localhost_paths() {
        let source = ClipSource {
            url: Some("http://localhost:3000/settings?x=1".into()),
            browser: Some("Chrome".into()),
            ..Default::default()
        };
        let record = clip(source, Some("http://localhost:3000/settings"), None);
        assert_eq!(record.summary(), "localhost:3000/settings · PNG");
    }

    #[test]
    fn summary_falls_back_to_the_app() {
        let source = ClipSource {
            app_name: Some("Figma".into()),
            ..Default::default()
        };
        assert_eq!(clip(source, None, None).summary(), "Figma · PNG");
    }

    #[test]
    fn destinations_round_trip() {
        let value = serde_json::json!({"type":"session","sessionID":"s1"});
        let destination: ClipDestination = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&destination).unwrap(), value);
        let ring: ClipDestination =
            serde_json::from_value(serde_json::json!({"type":"ring"})).unwrap();
        assert_eq!(ring, ClipDestination::Ring);
    }
}
