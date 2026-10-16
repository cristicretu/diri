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

pub const SPEAK_TOOL_DESCRIPTION: &str = "The developer has Diri read each of your turns aloud and may be away from the screen. At the end of every turn, after your work and before your final message, call this once with a spoken summary meant to be heard, not read, that lets the developer follow without the screen: what you did and the result, each option or decision you are offering (named briefly, one by one), and what you need from them. Usually three to eight sentences; fewer when there is little to say. No code, commands, file names, paths, links or Markdown. Use the language of your reply.";

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
pub const SPOKEN_INSTRUCTION: &str = "The developer has Diri read each of your turns aloud and may be away from the screen. At the end of every turn, call Diri's speak tool once with a spoken summary meant to be heard, not read, that lets the developer follow without the screen: what you did and the result, each option or decision you are offering (named briefly, one by one), and what you need from them. Usually three to eight sentences; fewer when there is little to say. No code, commands, file names, paths, links or Markdown. If the speak tool is unavailable, skip it; do not add the summary to your reply.";

/// Whether the app's preferences file has the setting on. A missing or
/// unreadable file means off.
pub fn talk_with_agent_enabled(prefs: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(prefs)
        .ok()
        .and_then(|prefs| prefs.get("talkWithAgent")?.as_bool())
        .unwrap_or(false)
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
}
