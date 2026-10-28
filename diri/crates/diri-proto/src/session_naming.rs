//! Agents naming their own session after the task.
//!
//! Agents are told once, at startup, through the channel Diri already
//! injects (the `dirijor` MCP server's instructions, and Codex's developer
//! instructions), and name the session with the `name_session` tool. Nothing
//! runs per session: the cost is a few hundred bytes of context.
//!
//! Precedence: the user's name and a name Diri or a parent gave always win.
//! An Agent's own name sits above every automatic title (the first prompt,
//! the terminal's title, the provider's generated summary), so those never
//! replace it.

use std::path::Path;

/// The app preference (`prefs.json`) that switches this off. Missing or
/// unreadable means on.
pub const PREF_KEY: &str = "agentsNameSessions";

/// The longest name an Agent may give, in characters.
pub const MAX_AGENT_NAME_CHARS: usize = 48;
/// The most words an Agent's name may have.
pub const MAX_AGENT_NAME_WORDS: usize = 4;
/// The longest name passed on from the user, in characters.
pub const MAX_USER_NAME_CHARS: usize = 128;

/// What an Agent is told at startup. Kept short: Claude Code keeps only the
/// first 2048 characters of all the server's instructions; the tool's own
/// description carries the remaining rules.
pub const INSTRUCTION: &str = "Name this session: while handling the first request that shows \
     the task (not a greeting), call Diri's name_session tool unasked with 2-4 lowercase \
     hyphenated words naming it, e.g. fix-login-redirect. Keep a name that still fits; rename \
     when the task changes. If it fails, carry on.";

/// Whether the app's preferences allow it. A missing key, file, or a file
/// that does not parse means on, the default.
pub fn enabled(prefs: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(prefs)
        .ok()
        .and_then(|prefs| prefs.get(PREF_KEY)?.as_bool())
        .unwrap_or(true)
}

/// [`enabled`] for the preferences file under `home`.
pub fn enabled_for_home(home: impl AsRef<Path>) -> bool {
    std::fs::read(crate::paths::DirijorPaths::prefs_file(home))
        .map(|prefs| enabled(&prefs))
        .unwrap_or(true)
}

/// [`enabled`] for the current user (`$HOME`).
pub fn enabled_for_current_user() -> bool {
    std::env::var_os("HOME").is_none_or(enabled_for_home)
}

/// Why an Agent's name was not used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidName {
    Empty,
    TooLong,
    TooManyWords,
    Generic,
}

impl InvalidName {
    pub fn message(self) -> &'static str {
        match self {
            Self::Empty => "the name has no letters or digits",
            Self::TooLong => "the name is too long; use 2-4 short words",
            Self::TooManyWords => "use at most 4 words",
            Self::Generic => "name the task, not the agent or the session",
        }
    }
}

/// Labels that say nothing about the work.
const GENERIC: &[&str] = &[
    "agent",
    "session",
    "terminal",
    "shell",
    "task",
    "untitled",
    "new-session",
    "new-task",
    "claude",
    "claude-code",
    "codex",
    "cursor",
    "gemini",
    "work",
    "chat",
    "conversation",
];

/// An Agent's name in the shape every Agent name has: lowercase words of
/// letters and digits joined by single hyphens. Spaces, underscores and
/// punctuation become hyphens, so `Fix Login_redirect!` is
/// `fix-login-redirect`. Letters outside ASCII are kept.
pub fn normalize_agent_name(name: &str) -> Result<String, InvalidName> {
    let mut out = String::with_capacity(name.len());
    let mut pending_hyphen = false;
    for c in name.chars() {
        if c.is_alphanumeric() {
            if pending_hyphen && !out.is_empty() {
                out.push('-');
            }
            pending_hyphen = false;
            out.extend(c.to_lowercase());
        } else if c == '\'' || c == '\u{2019}' {
            // "don't" is one word.
        } else {
            pending_hyphen = true;
        }
    }
    if out.is_empty() {
        return Err(InvalidName::Empty);
    }
    if out.chars().count() > MAX_AGENT_NAME_CHARS {
        return Err(InvalidName::TooLong);
    }
    if out.split('-').count() > MAX_AGENT_NAME_WORDS {
        return Err(InvalidName::TooManyWords);
    }
    if GENERIC.contains(&out.as_str()) {
        return Err(InvalidName::Generic);
    }
    Ok(out)
}

/// A name the user asked the Agent for, kept as they wrote it: only
/// surrounding space is trimmed, control characters dropped, and the length
/// capped.
pub fn normalize_user_name(name: &str) -> Result<String, InvalidName> {
    let cleaned: String = name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_USER_NAME_CHARS)
        .collect();
    let cleaned = cleaned.trim().to_owned();
    if cleaned.is_empty() {
        return Err(InvalidName::Empty);
    }
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_names_are_lowercase_hyphenated_words() {
        assert_eq!(
            normalize_agent_name("Fix Login_redirect!").as_deref(),
            Ok("fix-login-redirect")
        );
        assert_eq!(
            normalize_agent_name("  checkout--flaky  tests ").as_deref(),
            Ok("checkout-flaky-tests")
        );
        assert_eq!(
            normalize_agent_name("don't-panic").as_deref(),
            Ok("dont-panic")
        );
        assert_eq!(
            normalize_agent_name("修复-登录").as_deref(),
            Ok("修复-登录")
        );
        assert_eq!(normalize_agent_name("refactor").as_deref(), Ok("refactor"));
    }

    #[test]
    fn agent_names_reject_empty_long_wordy_and_generic_labels() {
        assert_eq!(normalize_agent_name(" -- "), Err(InvalidName::Empty));
        assert_eq!(
            normalize_agent_name("one two three four five"),
            Err(InvalidName::TooManyWords)
        );
        assert_eq!(
            normalize_agent_name(&"a".repeat(MAX_AGENT_NAME_CHARS + 1)),
            Err(InvalidName::TooLong)
        );
        for generic in ["Claude", "session", "Claude Code", "untitled"] {
            assert_eq!(
                normalize_agent_name(generic),
                Err(InvalidName::Generic),
                "{generic}"
            );
        }
    }

    #[test]
    fn user_names_keep_their_wording() {
        assert_eq!(
            normalize_user_name("  Release prep: v2 \n").as_deref(),
            Ok("Release prep: v2")
        );
        assert_eq!(normalize_user_name(" \t"), Err(InvalidName::Empty));
        assert_eq!(
            normalize_user_name(&"x".repeat(500)).map(|name| name.len()),
            Ok(MAX_USER_NAME_CHARS)
        );
    }

    #[test]
    fn the_setting_defaults_on() {
        assert!(enabled(b"{}"));
        assert!(enabled(b"not json"));
        assert!(enabled(br#"{"agentsNameSessions":true}"#));
        assert!(!enabled(br#"{"agentsNameSessions":false}"#));
        let home = tempfile::tempdir().unwrap();
        assert!(enabled_for_home(home.path()));
    }

    #[test]
    fn the_instruction_states_when_how_and_the_fallback() {
        for phrase in [
            "first request",
            "not a greeting",
            "name_session",
            "2-4 lowercase",
            "unasked",
            "still fits",
            "task changes",
            "carry on",
        ] {
            assert!(INSTRUCTION.contains(phrase), "{phrase}");
        }
    }
}
