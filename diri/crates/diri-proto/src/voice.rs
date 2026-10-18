//! "Talk with your agent": how Diri asks an Agent for a spoken summary of
//! its turn, and how that summary reaches the reader.
//!
//! Agents with Diri's MCP server call its [`SPEAK_TOOL`]; the Engine keeps
//! the text for that session's turn ([`SpeakParams`]) until the app collects
//! it when the turn ends ([`SpokenParams`]). Nothing is added to what the
//! agent shows: for agents without the tool, or a turn that skipped it, the
//! app summarizes the reply on this Mac. A reply that does end with a
//! paragraph starting [`SPOKEN_MARKER`] is still read as written.

use serde::{Deserialize, Serialize};

/// Diri's MCP tool an Agent calls with its spoken summary.
pub const SPEAK_TOOL: &str = "speak";

pub const SPEAK_TOOL_DESCRIPTION: &str = "Only when Diri's context for this prompt says the developer has Diri read your turns aloud; otherwise never call it. Then, at the end of the turn, after your work and before your final message, call this once with a spoken summary meant to be heard, not read, that lets the developer follow without the screen: what you did and the result, each option or decision you are offering (named briefly, one by one), and what you need from them. Usually three to eight sentences; fewer when there is little to say. No code, commands, file names, paths, links or Markdown. Use the language of your reply.";

/// Longest summary kept: a spoken summary is up to a long paragraph.
pub const MAX_SPOKEN_CHARS: usize = 3000;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceStateResult {
    pub talk_with_agent: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct SpeakParams {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct SpokenParams {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub turn: u64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct SpokenResult {
    pub text: Option<String>,
}

/// Starts the paragraph the app reads aloud.
pub const SPOKEN_MARKER: &str = "Spoken:";

/// Context added to each Claude Code prompt (its hook) and to Codex's
/// developer instructions (its launch) while the setting is on: a reminder
/// to call the [`SPEAK_TOOL`]. It never asks for text in the reply itself.
pub const SPOKEN_INSTRUCTION: &str = "The developer has Diri read each of your turns aloud and may be away from the screen. At the end of every turn, call Diri's speak tool once with a spoken summary meant to be heard, not read, that lets the developer follow without the screen: what you did and the result, each option or decision you are offering (named briefly, one by one), and what you need from them. Usually three to eight sentences; fewer when there is little to say. No code, commands, file names, paths, links or Markdown. If the speak tool is unavailable, skip it; do not add the summary to your reply, and never speak it yourself (no `say` or other speech commands).";

/// Whether the app's preferences file has the setting on. A missing or
/// unreadable file means off.
pub fn talk_with_agent_enabled(prefs: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(prefs)
        .ok()
        .and_then(|prefs| prefs.get("talkWithAgent")?.as_bool())
        .unwrap_or(false)
}

/// Whether the app reads `session_id`'s turns aloud, mirroring the app
/// store's `reads_aloud`: the setting is on, the tab follows or is an
/// exception to `talkReadsByDefault`, and its chat is not muted. Only such a
/// session is asked to call the [`SPEAK_TOOL`]; "read only this tab" must
/// not make every other Agent narrate its turns.
pub fn reads_session_aloud(prefs: &[u8], session_id: &str) -> bool {
    let Ok(prefs) = serde_json::from_slice::<serde_json::Value>(prefs) else {
        return false;
    };
    let listed = |key: &str| {
        prefs
            .get(key)
            .and_then(serde_json::Value::as_array)
            .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(session_id)))
    };
    let by_default = prefs
        .get("talkReadsByDefault")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    prefs
        .get("talkWithAgent")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
        && by_default != listed("talkExceptions")
        && !listed("mutedNotificationSessions")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_true_enables_it() {
        assert!(talk_with_agent_enabled(br#"{"talkWithAgent":true}"#));
        assert!(!talk_with_agent_enabled(br#"{"talkWithAgent":false}"#));
        assert!(!talk_with_agent_enabled(br#"{}"#));
        assert!(!talk_with_agent_enabled(b"not json"));
    }

    #[test]
    fn only_tabs_the_app_reads_are_asked_to_speak() {
        let all = br#"{"talkWithAgent":true,"talkReadsByDefault":true,"talkExceptions":["s_b"]}"#;
        assert!(reads_session_aloud(all, "s_a"));
        assert!(!reads_session_aloud(all, "s_b"));

        let only = br#"{"talkWithAgent":true,"talkReadsByDefault":false,"talkExceptions":["s_a"]}"#;
        assert!(reads_session_aloud(only, "s_a"));
        assert!(!reads_session_aloud(only, "s_b"));

        let none = br#"{"talkWithAgent":true,"talkReadsByDefault":false,"talkExceptions":[]}"#;
        assert!(!reads_session_aloud(none, "s_a"));

        let muted = br#"{"talkWithAgent":true,"mutedNotificationSessions":["s_a"]}"#;
        assert!(!reads_session_aloud(muted, "s_a"));
        assert!(reads_session_aloud(muted, "s_b"));

        let off = br#"{"talkWithAgent":false,"talkReadsByDefault":true}"#;
        assert!(!reads_session_aloud(off, "s_a"));
        assert!(!reads_session_aloud(b"not json", "s_a"));
    }
}
