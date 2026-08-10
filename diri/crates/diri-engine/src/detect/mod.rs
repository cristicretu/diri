//! Screen-driven status detection.
//!
//! An agent's status — working, idle, waiting on you — is inferred from what it
//! painted on its terminal, using per-agent rules that live in JSON manifests.
//! The rules and Agent launch descriptors are Rust-workspace resources under
//! `crates/diri-engine/manifests`. Adding an agent remains a data-only change,
//! without coupling the authoritative Engine to another implementation.
//!
//! The one behavioral difference worth knowing: Swift compiled these patterns
//! with `NSRegularExpression` (ICU), while this uses the `regex` crate, which
//! has no backreferences or lookaround. Every bundled pattern is checked
//! against that restriction by a test, so an incompatible rule fails loudly at
//! development time rather than silently never matching in production.

mod manifest;
mod redact;
mod regions;

pub use manifest::{Manifest, ManifestState, RegionKind, StatusModel};
pub use redact::redact;

use std::collections::HashMap;
use std::path::Path;

pub use diri_terminal_state::ScreenSnapshot;

use manifest::Rule;

/// Source-tree location of the Rust-owned built-in Agent catalog. Release
/// packaging copies this directory next to `dirijord-rs`; this fallback keeps
/// tests and loose development binaries independent of application packaging.
#[must_use]
pub fn bundled_manifest_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

/// The engine's verdict for one snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenObservation {
    pub state: ManifestState,
    pub matched_rule_id: String,
    pub priority: i64,
    pub content_seq: u64,
    pub prompt_excerpt: Option<String>,
    pub options: Option<Vec<String>>,
}

/// Immutable manifest storage, built once and shared across sessions.
pub struct ManifestEngine {
    manifests: HashMap<String, Manifest>,
    /// Each manifest's `agent` object verbatim, for wire surfaces that hand
    /// the descriptor to clients (`agent.readiness` is the agent catalog).
    raw_agents: HashMap<String, serde_json::Value>,
}

impl ManifestEngine {
    pub fn new(manifests: Vec<Manifest>) -> Self {
        Self {
            manifests: manifests
                .into_iter()
                .map(|manifest| (manifest.id.clone(), manifest))
                .collect(),
            raw_agents: HashMap::new(),
        }
    }

    /// The manifest's `agent` JSON exactly as shipped.
    pub fn raw_agent(&self, id: &str) -> Option<&serde_json::Value> {
        self.raw_agents.get(id)
    }

    /// Loads every `*.json` in `dir`, later ids replacing earlier ones.
    ///
    /// Decoding is best-effort per file, matching the Swift loader: one broken
    /// override must not take out detection for every other agent. The count of
    /// files that failed is returned so a caller can surface it.
    pub fn load_dir(dir: &Path) -> std::io::Result<(Self, Vec<String>)> {
        let mut manifests = Vec::new();
        let mut failed = Vec::new();
        let mut entries: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        entries.sort();

        let mut raw_agents = HashMap::new();
        for path in entries {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let bytes = std::fs::read(&path).ok();
            match bytes
                .as_deref()
                .and_then(|bytes| serde_json::from_slice::<Manifest>(bytes).ok())
            {
                Some(manifest) => {
                    if let Some(raw) = bytes
                        .as_deref()
                        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok())
                        .and_then(|mut value| value.get_mut("agent").map(serde_json::Value::take))
                    {
                        raw_agents.insert(manifest.id.clone(), raw);
                    }
                    manifests.push(manifest);
                }
                None => failed.push(name),
            }
        }
        let mut engine = Self::new(manifests);
        engine.raw_agents = raw_agents;
        Ok((engine, failed))
    }

    /// Loads several directories in order, later dirs overriding earlier ones
    /// by manifest id — base catalog first, user overrides second.
    pub fn load_dirs(dirs: &[&Path]) -> std::io::Result<(Self, Vec<String>)> {
        let mut merged: Option<Self> = None;
        let mut all_failed = Vec::new();
        for dir in dirs {
            if !dir.is_dir() {
                continue;
            }
            let (engine, failed) = Self::load_dir(dir)?;
            all_failed.extend(failed);
            match &mut merged {
                None => merged = Some(engine),
                Some(base) => {
                    base.manifests.extend(engine.manifests);
                    base.raw_agents.extend(engine.raw_agents);
                }
            }
        }
        Ok((merged.unwrap_or_else(|| Self::new(Vec::new())), all_failed))
    }

    pub fn manifest(&self, id: &str) -> Option<&Manifest> {
        self.manifests.get(id)
    }

    pub fn ids(&self) -> Vec<&str> {
        self.manifests.keys().map(String::as_str).collect()
    }

    /// Evaluates `snapshot` against the manifest for `manifest_id`.
    ///
    /// Rules are pre-sorted by descending priority at load, so the first match
    /// is the highest-priority match: take it and stop rather than scoring
    /// every rule on a path that runs several times a second per session.
    pub fn evaluate(
        &self,
        snapshot: &ScreenSnapshot,
        manifest_id: &str,
    ) -> Option<ScreenObservation> {
        let manifest = self.manifests.get(manifest_id)?;

        // Region text is shared across rules — five of claude's ten read
        // `whole_recent` — so extract and join each region at most once per
        // snapshot, along with the case-folded copy that `contains` predicates
        // search (folding ~12KB per predicate per evaluation dwarfed the
        // search itself). Keyed by region and line count, since
        // `bottom_non_empty_lines` varies by count.
        let mut cache: HashMap<(RegionKind, usize), (Vec<String>, String, String)> = HashMap::new();

        let mut winner: Option<&Rule> = None;
        for rule in &manifest.rules {
            let key = (rule.region, rule.region_lines);
            let entry = cache.entry(key).or_insert_with(|| {
                let lines = regions::extract(rule.region, rule.region_lines, snapshot);
                let text = lines.join("\n");
                let text_lower = text.to_lowercase();
                (lines, text, text_lower)
            });
            let context = manifest::PredicateContext {
                text: &entry.1,
                text_lower: &entry.2,
                lines: &entry.0,
                progress_state: snapshot.osc_progress_state,
            };
            if rule.when.evaluate(&context) {
                winner = Some(rule);
                break;
            }
        }

        let rule = winner?;

        // Capture region: explicit, or the prompt box for blockers.
        let capture = match (&rule.capture, rule.is_blocker()) {
            (Some(capture), _) => Some((capture.region, capture.region_lines, capture.max_chars)),
            (None, true) => Some((RegionKind::PromptBoxBody, 5, 400)),
            (None, false) => None,
        };

        let mut excerpt = None;
        let mut options = None;
        if let Some((region, region_lines, max_chars)) = capture {
            let lines = regions::extract(region, region_lines, snapshot);
            let joined = lines.join("\n");
            if !joined.is_empty() {
                let redacted = redact(&joined);
                excerpt = Some(redacted.chars().take(max_chars).collect());
            }
            if rule.is_blocker() {
                let found = regions::numbered_options(&lines);
                if !found.is_empty() {
                    options = Some(found);
                }
            }
        }

        Some(ScreenObservation {
            state: rule.state,
            matched_rule_id: rule.id.clone(),
            priority: rule.priority,
            content_seq: snapshot.content_seq,
            prompt_excerpt: excerpt,
            options,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact Rust-owned catalog shipped next to the Engine.
    pub(crate) fn manifest_dir() -> std::path::PathBuf {
        bundled_manifest_dir()
            .canonicalize()
            .expect("manifests directory")
    }

    fn engine() -> ManifestEngine {
        let (engine, failed) = ManifestEngine::load_dir(&manifest_dir()).expect("load");
        assert!(failed.is_empty(), "manifests failed to decode: {failed:?}");
        engine
    }

    /// Every manifest decoding is also the proof that every pattern in them
    /// compiles under the `regex` crate — the one real risk in moving off ICU,
    /// since `regex` has no backreferences or lookaround. A pattern that needed
    /// either would fail this test rather than silently never match.
    #[test]
    fn every_bundled_manifest_decodes() {
        let dir = manifest_dir();
        let on_disk = std::fs::read_dir(&dir)
            .expect("read manifests")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .count();

        let engine = engine();
        assert_eq!(
            engine.ids().len(),
            on_disk,
            "every manifest file must load; loaded {:?}",
            engine.ids()
        );

        // The whole catalog, by name. A shrunken catalog does not error: a
        // missing agent just spawns as a bare login shell, which is how this
        // shipped broken once already. Spelling out all twenty ids means a
        // dropped manifest fails here instead of in someone's terminal.
        let mut ids = engine.ids();
        ids.sort_unstable();
        assert_eq!(
            ids,
            [
                "aider",
                "amp",
                "antigravity",
                "claude-code",
                "codex",
                "copilot",
                "cursor",
                "devin",
                "droid",
                "gemini",
                "generic",
                "grok",
                "hermes",
                "kilo",
                "kimi",
                "kiro",
                "opencode",
                "pi",
                "qoder",
                "shell",
            ]
        );

        // Every id but the two command-less ones detects state from the
        // screen, and the rules are the substance of that. Counting them is
        // what catches a manifest that survives as a stub: `pi` alone ships
        // zero rules, deliberately, because it is process-only.
        let rules: usize = engine
            .ids()
            .into_iter()
            .map(|id| engine.manifest(id).expect("manifest").rules.len())
            .sum();
        assert_eq!(rules, 85, "the shipped ruleset lost rules");

        for id in engine.ids() {
            let expected_empty = matches!(id, "shell" | "generic" | "pi");
            assert_eq!(
                engine.manifest(id).expect("manifest").rules.is_empty(),
                expected_empty,
                "{id}: unexpected rule coverage"
            );
        }
    }

    /// `agent.readiness` hands the raw `agent` object to the client, which
    /// decodes it as `diri_proto::AgentDescriptor`. That type needs `id` and
    /// `displayName`, and a single manifest missing either fails the *whole*
    /// response — leaving the client with no catalog and every agent spawning
    /// as a bare shell. Decode all twenty the way the client will.
    #[test]
    fn every_shipped_descriptor_decodes_the_way_the_client_decodes_it() {
        let engine = engine();
        for id in engine.ids() {
            let raw = engine
                .raw_agent(id)
                .unwrap_or_else(|| panic!("{id} carries no agent object"));
            let descriptor: diri_proto::AgentDescriptor = serde_json::from_value(raw.clone())
                .unwrap_or_else(|error| panic!("{id} is not a client descriptor: {error}"));
            assert_eq!(descriptor.id, id, "{id} declares a mismatched agent id");
            assert!(
                !descriptor.display_name.is_empty(),
                "{id} has no display name"
            );
        }
    }

    #[test]
    fn rules_are_sorted_by_descending_priority() {
        let engine = engine();
        for id in engine.ids() {
            let rules = &engine.manifest(id).expect("manifest").rules;
            for pair in rules.windows(2) {
                assert!(
                    pair[0].priority >= pair[1].priority,
                    "{id}: {} ({}) came before {} ({})",
                    pair[0].id,
                    pair[0].priority,
                    pair[1].id,
                    pair[1].priority
                );
            }
        }
    }

    #[test]
    fn a_claude_permission_prompt_is_a_visible_blocker() {
        let engine = engine();
        let snapshot = ScreenSnapshot::from_lines([
            "│ Bash command                    │",
            "│ rm -rf build                    │",
            "│ Do you want to proceed?         │",
            "│ ❯ 1. Yes                        │",
            "│   2. No, and tell Claude        │",
            "│ esc to cancel                   │",
        ]);

        let observation = engine
            .evaluate(&snapshot, "claude-code")
            .expect("a rule should match");
        assert_eq!(observation.state, ManifestState::BlockedPermission);
        let options = observation.options.expect("numbered options");
        assert_eq!(options[0], "Yes");
        assert!(options[1].starts_with("No"));
    }

    #[test]
    fn the_transcript_viewer_holds_state_instead_of_transitioning() {
        let engine = engine();
        let snapshot = ScreenSnapshot::from_lines([
            "Showing detailed transcript (ctrl+r to toggle)",
            "❯ 1. Yes",
        ]);

        let observation = engine.evaluate(&snapshot, "claude-code").expect("match");
        assert_eq!(
            observation.state,
            ManifestState::Skip,
            "the viewer outranks the option list underneath it"
        );
    }

    #[test]
    fn an_unrecognized_screen_produces_no_verdict() {
        let engine = engine();
        let snapshot = ScreenSnapshot::from_lines(["just some ordinary output"]);
        // claude's manifest has no catch-all, so nothing should match.
        let observation = engine.evaluate(&snapshot, "claude-code");
        assert!(
            observation.is_none() || observation.unwrap().state != ManifestState::BlockedPermission,
            "ordinary output must not read as a blocker"
        );
    }

    #[test]
    fn an_unknown_manifest_id_is_none_rather_than_a_panic() {
        let engine = engine();
        let snapshot = ScreenSnapshot::from_lines(["anything"]);
        assert!(engine.evaluate(&snapshot, "no-such-agent").is_none());
    }
}
