//! Display names for Agents the Engine recognises in a shell's foreground
//! without a launch manifest (`diri-engine/src/recognition.rs`). A manifest's
//! Agent is named by its manifest's `displayName`, which reaches clients in
//! the Agent catalog; these have no manifest, so Engine and app read their
//! names from here. The Engine's tests keep the two tables in step.

/// `(AgentKind id, display name)` for every Agent known only by name.
pub const RECOGNIZED_AGENT_NAMES: &[(&str, &str)] = &[
    ("abacusai", "Abacus.AI"),
    ("auggie", "Auggie"),
    ("codebuddy", "CodeBuddy"),
    ("codebuff", "Codebuff"),
    ("coderabbit", "CodeRabbit"),
    ("command-code", "Command Code"),
    ("crush", "Crush"),
    ("goose", "Goose"),
    ("junie", "Junie"),
    ("mcode", "MCode"),
    ("muse", "Muse Code"),
    ("openclaw", "OpenClaw"),
    ("openhands", "OpenHands"),
    ("qwen", "Qwen Code"),
    ("traecli", "TRAE CLI"),
    ("vibe", "Mistral Vibe"),
    ("warp", "Warp"),
];

/// The display name of an Agent known only by name, by its id.
pub fn recognized_agent_name(id: &str) -> Option<&'static str> {
    RECOGNIZED_AGENT_NAMES
        .iter()
        .find_map(|&(known, name)| (known == id).then_some(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_trimmed_and_found() {
        let mut ids = std::collections::HashSet::new();
        for &(id, name) in RECOGNIZED_AGENT_NAMES {
            assert!(ids.insert(id), "duplicate {id}");
            assert!(!name.is_empty() && name.trim() == name, "{id}");
            assert_eq!(recognized_agent_name(id), Some(name));
        }
        assert_eq!(recognized_agent_name("claude-code"), None);
        assert_eq!(recognized_agent_name(""), None);
    }
}
