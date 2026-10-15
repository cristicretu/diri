//! "Talk with your agent": the closing paragraph Diri asks an Agent for, and
//! reads aloud when the turn ends.
//!
//! The Engine adds [`SPOKEN_INSTRUCTION`] to every prompt while the setting
//! is on; the app reads the paragraph that starts with [`SPOKEN_MARKER`].

/// Starts the paragraph the app reads aloud.
pub const SPOKEN_MARKER: &str = "Spoken:";

/// Context added to each prompt while the setting is on.
pub const SPOKEN_INSTRUCTION: &str = "The developer has Diri read the end of each of your turns aloud and may be away from the screen. End every reply with a final paragraph that starts with \"Spoken:\" followed by two to four short sentences meant to be heard, not read: what you did, the result, and anything you need from the developer. No code, commands, file names, paths, links or Markdown in that paragraph. Write it in the language of your reply, but keep the word \"Spoken:\" as is.";

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
