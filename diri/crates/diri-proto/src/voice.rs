//! "Talk with your agent": how Diri asks an Agent for a spoken summary of
//! its turn, and how that summary reaches the reader.
//!
//! Agents with Diri's MCP server call its [`SPEAK_TOOL`]; the Engine keeps
//! the text for that session's turn ([`SpeakParams`]) until the app collects
//! it when the turn ends ([`SpokenParams`]). Agents without it end their
//! reply with a paragraph starting [`SPOKEN_MARKER`], which the app reads
//! from the transcript or the terminal.

use serde::{Deserialize, Serialize};

/// Diri's MCP tool an Agent calls with its spoken summary.
pub const SPEAK_TOOL: &str = "speak";

pub const SPEAK_TOOL_DESCRIPTION: &str = "The developer has Diri read each of your turns aloud and may be away from the screen. At the end of every turn, after your work and before your final message, call this once with two to four short sentences meant to be heard, not read: what you did, the result, and anything you need from the developer. No code, commands, file names, paths, links or Markdown. Use the language of your reply.";

/// Longest summary kept; a spoken summary is a few sentences.
pub const MAX_SPOKEN_CHARS: usize = 2000;

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
/// developer instructions (its launch) while the setting is on. Both have
/// Diri's MCP server; the paragraph covers a session where it failed.
pub const SPOKEN_INSTRUCTION: &str = "The developer has Diri read each of your turns aloud and may be away from the screen. At the end of every turn, call Diri's speak tool once with two to four short sentences meant to be heard, not read: what you did, the result, and anything you need from the developer. No code, commands, file names, paths, links or Markdown. If the speak tool is unavailable, end your reply with a final paragraph starting \"Spoken:\" with those sentences instead.";

/// The short form, added to a spoken answer for agents Diri has no other
/// way to ask.
pub const SPOKEN_REQUEST: &str = "End your reply with a short paragraph starting \"Spoken:\" that says, in plain words for listening, what you did and what you need from me.";

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
