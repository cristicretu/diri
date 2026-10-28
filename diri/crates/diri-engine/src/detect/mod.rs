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
pub(crate) use regions::{bottom_non_empty, prompt_box_body};

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
    /// The rule's `stale_after_submit` flag: right after a submit this screen
    /// can still be the previous turn's composer, so it cannot end the new one.
    pub stale_after_submit: bool,
    /// The rule's `turn_started` flag: proof the submitted turn is running.
    pub turn_started: bool,
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

    /// Resolves frontend verbs from the same manifest that owns launch and
    /// status behavior. Unknown Agents get a conservative, lifecycle-only
    /// answer rather than being guessed from their command text.
    pub fn session_capabilities(
        &self,
        record: &diri_proto::SessionRecord,
    ) -> diri_proto::SessionCapabilities {
        self.manifest(record.effective_kind().id())
            .and_then(|manifest| manifest.agent.as_ref())
            .map_or_else(
                || diri_proto::SessionCapabilities {
                    resume: false,
                    fork: false,
                    archive: !record.is_archived(),
                    send_text: !record.is_archived()
                        && !matches!(record.status, diri_proto::SessionStatus::Exited(_)),
                    quick_approve: false,
                    reliable_completion: false,
                },
                |agent| {
                    let mut capabilities = agent.session_capabilities(
                        record.resumability,
                        &record.status,
                        record.is_archived(),
                        record.agent_session_id.as_deref(),
                    );
                    // An Agent started by hand in a shell has no conversation
                    // Diri knows: forking it would pick whichever was latest.
                    if record.kind == diri_proto::AgentKind::SHELL {
                        capabilities.fork = false;
                    }
                    capabilities
                },
            )
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
            stale_after_submit: rule.has_flag("stale_after_submit"),
            turn_started: rule.has_flag("turn_started"),
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

    /// Synthetic examples based on aider.json, not captured user sessions.
    #[test]
    fn aider_unanswered_and_wrapped_confirmations_match_permission() {
        let engine = engine();
        for lines in [
            vec!["Create new file? (Y)es/(N)o [Yes]:"],
            vec![
                "░█ Waiting for openai/model",
                "Add files? (Y)es/(N)o/(A)ll/(S)kip-all",
                "[Yes]:",
            ],
        ] {
            let snapshot = ScreenSnapshot::from_lines(lines.clone());
            let observation = engine
                .evaluate(&snapshot, "aider")
                .expect("synthetic pending Aider confirmation should match");
            assert_eq!(
                (observation.state, observation.matched_rule_id.as_str()),
                (ManifestState::BlockedPermission, "confirm-prompt"),
                "{lines:#?}"
            );
            assert_eq!(observation.prompt_excerpt, Some(lines.join("\n")));
        }
    }

    /// Synthetic examples based on aider.json, not captured user sessions.
    #[test]
    fn aider_answered_confirmation_yields_to_working_spinner() {
        let engine = engine();
        for spinner in [
            "░█ Waiting for openai/model",
            "  █░ Generating commit message with model",
        ] {
            for lines in [
                vec![spinner],
                vec!["Create new file? (Y)es/(N)o [Yes]: y", spinner],
            ] {
                let observation = engine
                    .evaluate(&ScreenSnapshot::from_lines(lines.clone()), "aider")
                    .expect("synthetic Aider spinner should match");
                assert_eq!(
                    (observation.state, observation.matched_rule_id.as_str()),
                    (ManifestState::Working, "working-spinner"),
                    "{lines:#?}"
                );
                assert_eq!(observation.prompt_excerpt, None);
            }
        }
    }

    /// Synthetic examples based on aider.json, not captured user sessions.
    #[test]
    fn aider_bottom_row_prompts_match_idle() {
        let engine = engine();
        for prompt in [">", "ask>  "] {
            let observation = engine
                .evaluate(
                    &ScreenSnapshot::from_lines(["Response complete.", prompt]),
                    "aider",
                )
                .expect("synthetic Aider bottom-row prompt should match");
            assert_eq!(
                (observation.state, observation.matched_rule_id.as_str()),
                (ManifestState::Idle, "idle-prompt"),
                "{prompt:?}"
            );
        }
    }

    /// Synthetic examples based on aider.json, not captured user sessions.
    #[test]
    fn aider_arrows_and_quoted_prompt_above_last_row_do_not_match_idle() {
        let engine = engine();
        for lines in [
            vec!["->"],
            vec!["-->"],
            vec![">", "Response continues here."],
        ] {
            assert_eq!(
                engine.evaluate(&ScreenSnapshot::from_lines(lines.clone()), "aider"),
                None,
                "synthetic non-prompt screen must not match: {lines:#?}"
            );
        }
    }

    #[test]
    fn claude_live_work_outranks_its_visible_input_box() {
        let engine = engine();
        for (title, activity) in [
            ("◐ Working", ""),
            ("◑ Working", ""),
            ("◒ Working", ""),
            ("◓ Working", ""),
            ("✳ Project", "⏵ processing · esc to interrupt"),
            ("✳ Project", "✻ Thinking… (12s · ↓ 100 tokens)"),
            ("✳ Project", "✻ Waiting for 2 background agents to finish"),
            ("✳ Project", "✻ Working… · 2 MCP tasks still running"),
        ] {
            let snapshot = ScreenSnapshot {
                lines: vec![
                    activity.into(),
                    "──────────".into(),
                    "❯".into(),
                    "──────────".into(),
                ],
                osc_title: Some(title.into()),
                ..Default::default()
            };
            let observation = engine
                .evaluate(&snapshot, "claude-code")
                .expect("Claude rule");
            assert_eq!(
                observation.state,
                ManifestState::Working,
                "title={title}, activity={activity}"
            );
        }
    }

    #[test]
    fn claude_work_indicators_do_not_hide_blockers_or_match_user_prompt_text() {
        let engine = engine();
        let mut blocked = ScreenSnapshot::from_lines([
            "✻ Working… · 2 MCP tasks still running",
            "Do you want to proceed?",
            "❯ 1. Yes",
            "2. No",
            "esc to cancel",
        ]);
        blocked.osc_title = Some("◐ Working".into());
        assert_eq!(
            engine.evaluate(&blocked, "claude-code").unwrap().state,
            ManifestState::BlockedPermission
        );
        for text in [
            "❯ ✻ Waiting for 2 background agents to finish",
            "❯ ✻ Working… · 2 MCP tasks still running",
            "❯ ⏵ processing · esc to interrupt",
            "1 background shell · ↓ to view",
        ] {
            let mut idle = ScreenSnapshot::from_lines([text]);
            idle.osc_title = Some("✳ Project".into());
            assert_eq!(
                engine.evaluate(&idle, "claude-code").unwrap().state,
                ManifestState::Idle,
                "{text}"
            );
        }
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
        // shipped broken once already. Spelling out all twenty-three ids means a
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
                "cline",
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
                "maki",
                "opencode",
                "pi",
                "qoder",
                "shell",
                "whipcode",
            ]
        );

        // Every id but the two command-less ones detects state from the
        // screen, and the rules are the substance of that. Counting them is
        // what catches a manifest that survives as a stub.
        let rules: usize = engine
            .ids()
            .into_iter()
            .map(|id| engine.manifest(id).expect("manifest").rules.len())
            .sum();
        assert_eq!(rules, 135, "the shipped ruleset lost rules");

        for id in engine.ids() {
            let expected_empty = matches!(id, "shell" | "generic");
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
    /// as a bare shell. Decode all twenty-three the way the client will.
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
    fn cline_and_maki_carry_official_setup_guidance() {
        let engine = engine();
        for (id, expected_url) in [
            (
                "cline",
                "https://docs.cline.bot/getting-started/installing-cline",
            ),
            ("maki", "https://github.com/tontinton/maki#installation"),
        ] {
            let setup = engine
                .raw_agent(id)
                .and_then(|agent| agent.get("setup"))
                .and_then(serde_json::Value::as_object)
                .unwrap_or_else(|| panic!("{id} has no setup metadata"));
            assert_eq!(
                setup.get("url").and_then(serde_json::Value::as_str),
                Some(expected_url)
            );
            for field in ["installHint", "signInHint"] {
                assert!(
                    setup
                        .get(field)
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|hint| !hint.trim().is_empty()),
                    "{id} has no {field}"
                );
            }
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
    fn cline_rules_cover_permission_question_working_and_idle() {
        let engine = engine();
        let cases = [
            (
                vec![
                    "Approve tool call?",
                    "run_commands",
                    "[y] approve  [n] deny",
                ],
                ManifestState::BlockedPermission,
                "tool-permission",
            ),
            (
                vec![
                    "Which approach should I use?",
                    "❯ Keep the API",
                    "  Type a response...",
                    "↑/↓ navigate, Enter to select, 1-2 to pick",
                ],
                ManifestState::BlockedQuestion,
                "follow-up-question",
            ),
            (
                vec!["⠋ streaming a response", "❯ Ask anything..."],
                ManifestState::Working,
                "streaming-spinner",
            ),
            (
                vec!["❯ Ask anything...", "● Act (Tab)"],
                ManifestState::Idle,
                "idle-input-placeholder",
            ),
        ];

        for (lines, state, rule) in cases {
            let observation = engine
                .evaluate(&ScreenSnapshot::from_lines(lines), "cline")
                .expect("Cline screen should match");
            assert_eq!(observation.state, state);
            assert_eq!(observation.matched_rule_id, rule);
        }
    }

    #[test]
    fn maki_rules_cover_permission_plan_working_and_idle() {
        let engine = engine();
        let cases = [
            (
                vec!["Permission required", "y allow    n deny"],
                ManifestState::BlockedPermission,
                "permission-prompt",
            ),
            (
                vec!["Plan complete", "space toggle parallel", "enter confirm"],
                ManifestState::BlockedQuestion,
                "plan-complete-form",
            ),
            (
                vec![" ⠋ ⠙ [BUILD] model"],
                ManifestState::Working,
                "status-bar-spinner-working",
            ),
            (
                vec![" [PLAN] model"],
                ManifestState::Idle,
                "status-bar-idle",
            ),
        ];

        for (lines, state, rule) in cases {
            let observation = engine
                .evaluate(&ScreenSnapshot::from_lines(lines), "maki")
                .expect("Maki screen should match");
            assert_eq!(observation.state, state);
            assert_eq!(observation.matched_rule_id, rule);
        }
    }

    /// Screens captured from Gemini CLI 0.62 running inside the Engine. Its
    /// footer (edit-mode hint, composer bars, workspace row) is seven lines
    /// tall, so the spinner sits eighth from the bottom: a six-line window
    /// read every streamed turn as idle.
    #[test]
    fn gemini_rules_match_the_screens_gemini_cli_draws() {
        let footer = [
            " ────────────────────────────────────────────────────────",
            "  Shift+Tab to accept edits",
            " ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄",
            "  >   Type your message or @path/to/file",
            " ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀",
            "  workspace (/directory)        sandbox          /model",
            "  ~/project                     no sandbox       gemini-2.5-flash",
        ];
        let with_footer = |lines: &[&'static str]| {
            lines
                .iter()
                .chain(footer.iter())
                .copied()
                .collect::<Vec<_>>()
        };
        let trust_dialog = vec![
            " ╭──────────────────────────────────────────────────────╮",
            " │ Do you trust the files in this folder?               │",
            " │                                                      │",
            " │ Trusting a folder allows Gemini CLI to load its      │",
            " │ local configurations, including custom commands.     │",
            " │                                                      │",
            " │ ● 1. Trust folder (project)                          │",
            " │   2. Trust parent folder (work)                      │",
            " │   3. Don't trust                                     │",
            " │                                                      │",
            " ╰──────────────────────────────────────────────────────╯",
        ];
        let cases = [
            (
                with_footer(&[
                    " > SLOW stream something",
                    " ✦ slow part 0 slow part 1",
                    "  ⠼ Thinking... (esc to cancel, 1s)        ? for shortcuts",
                ]),
                ManifestState::Working,
                "working-cancel-timer",
            ),
            (
                trust_dialog.clone(),
                ManifestState::BlockedQuestion,
                "folder-trust-dialog",
            ),
            (
                // Accepting trust restarts Gemini below the old dialog, which
                // stays on screen; the fresh composer must win.
                trust_dialog
                    .iter()
                    .copied()
                    .chain([
                        "  Gemini CLI is restarting to apply the trust changes...",
                        "  Gemini CLI v0.62.0",
                        " Tips for getting started:",
                        " 1. Create GEMINI.md files to customize your interactions",
                    ])
                    .chain(footer)
                    .collect(),
                ManifestState::Idle,
                "idle-placeholder",
            ),
            (
                vec![
                    " > RUNCMD for me",
                    "╭──────────────────────────────────────────────────────╮",
                    "│ ? Shell  touch diri-e2e-file                         │",
                    "│ Allow execution of [Shell]?                          │",
                    "│                                                      │",
                    "│ ● 1. Allow once                                      │",
                    "│   2. Allow for this session                          │",
                    "│   3. No, suggest changes (esc)                       │",
                    "╰──────────────────────────────────────────────────────╯",
                ],
                ManifestState::BlockedPermission,
                "confirm-dialog",
            ),
            (
                with_footer(&[" > Reply with PINEAPPLE", " ✦ PINEAPPLE"]),
                ManifestState::Idle,
                "idle-placeholder",
            ),
        ];

        let engine = engine();
        for (lines, state, rule) in cases {
            let observation = engine
                .evaluate(&ScreenSnapshot::from_lines(lines), "gemini")
                .expect("Gemini screen should match");
            assert_eq!(
                (observation.state, observation.matched_rule_id.as_str()),
                (state, rule)
            );
        }
    }

    /// Gemini CLI 0.62 signed out, captured by `tests/gemini_real.rs`: the
    /// auth picker replaces the composer, and the browser confirmation it
    /// leads to carries "Esc to cancel", which the working rule read as a
    /// turn in progress.
    #[test]
    fn gemini_sign_in_screens_need_input() {
        let engine = engine();
        for text in [
            include_str!("../../tests/fixtures/gemini_screens/auth.txt"),
            include_str!("../../tests/fixtures/gemini_screens/auth_browser.txt"),
        ] {
            let observation = engine
                .evaluate(
                    &ScreenSnapshot::from_lines(text.lines().map(str::to_owned)),
                    "gemini",
                )
                .expect("Gemini sign-in screen should match");
            assert_eq!(
                (observation.state, observation.matched_rule_id.as_str()),
                (ManifestState::BlockedQuestion, "auth-dialog"),
                "{text}"
            );
        }
    }

    /// Visible grids captured from Antigravity CLI (agy) 1.2.17 via
    /// `tests/antigravity_real.rs`; the onboarding pages from a PTY run of
    /// the same build. Only the temporary project path is normalized. Before
    /// these rules agy had no idle rule, so its "Signing in..." spinner left
    /// every session working until it went stale; login, trust, onboarding
    /// and `ask_question` matched nothing; and the permission rule wanted
    /// "edit command" and "do you want to proceed?", which 1.2 no longer
    /// draws, so no approval ever read as blocked.
    #[test]
    fn antigravity_rules_match_the_screens_agy_draws() {
        let engine = engine();
        let cases = [
            (
                "login",
                Some((ManifestState::BlockedQuestion, "login-method-dialog")),
            ),
            (
                "onboarding_color",
                Some((ManifestState::BlockedQuestion, "onboarding-dialog")),
            ),
            (
                "onboarding_terms",
                Some((ManifestState::BlockedQuestion, "onboarding-dialog")),
            ),
            (
                "trust",
                Some((ManifestState::BlockedQuestion, "workspace-trust-dialog")),
            ),
            ("fresh", Some((ManifestState::Idle, "idle-composer"))),
            ("done", Some((ManifestState::Idle, "idle-composer"))),
            ("idle", Some((ManifestState::Idle, "idle-composer"))),
            ("denied", Some((ManifestState::Idle, "idle-composer"))),
            ("ctrl_c_once", Some((ManifestState::Idle, "idle-composer"))),
            (
                "working",
                Some((ManifestState::Working, "working-spinner-verb")),
            ),
            (
                "permission_command",
                Some((
                    ManifestState::BlockedPermission,
                    "blocked-permission-request",
                )),
            ),
            (
                "permission_file",
                Some((
                    ManifestState::BlockedPermission,
                    "blocked-permission-request",
                )),
            ),
            (
                "question",
                Some((ManifestState::BlockedQuestion, "blocked-ask-question")),
            ),
            // Typing a slash command swaps the footer for "esc to cancel":
            // neither work nor a finished turn, so the status stays put.
            ("slash_menu", None),
        ];
        for (name, expected) in cases {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/antigravity_screens")
                .join(format!("{name}.txt"));
            let text = std::fs::read_to_string(&path).expect("fixture screen");
            let actual = engine
                .evaluate(
                    &ScreenSnapshot::from_lines(text.lines().map(str::to_owned)),
                    "antigravity",
                )
                .map(|observation| (observation.state, observation.matched_rule_id));
            assert_eq!(
                actual.as_ref().map(|(state, rule)| (*state, rule.as_str())),
                expected,
                "{name}:\n{text}"
            );
        }
    }

    /// Screens captured from Pi 0.99.2 (and 0.73.1, whose loader sits above
    /// the composer instead of in its border) driven by tests/pi_real.rs.
    #[test]
    fn pi_rules_match_the_screens_pi_draws() {
        let rule = "─".repeat(100);
        let rule = rule.as_str();
        let footer = [
            "/private/var/folders/T/.tmpELAZcH/project",
            "↑10 ↓5 0.0%/128k (auto)                                       fake-model",
        ];
        let header = [
            " ▀▀█  v0.99.2",
            " █▀ █ escape interrupt · ctrl+c/ctrl+d clear/exit · / commands · ! bash · ctrl+o more",
            " Warning: fd not found. Offline mode enabled, skipping download.",
        ];
        let screen = |body: &[&str]| {
            header
                .iter()
                .chain(body)
                .chain(footer.iter())
                .map(|line| (*line).to_owned())
                .collect::<Vec<_>>()
        };
        let border_working = format!("── ⠙ Working {}", "─".repeat(86));
        let border_retry = format!(
            "── ⠼ Retrying (1/3) in 2s... (escape to cancel) {}",
            "─".repeat(40)
        );
        let cases = [
            (
                screen(&[
                    " Reply with the word PINEAPPLE please.",
                    &border_working,
                    rule,
                ]),
                ManifestState::Working,
                "working-spinner",
            ),
            (
                screen(&[" SLOW stream something", &border_retry, rule]),
                ManifestState::Working,
                "working-spinner",
            ),
            (
                // 0.73: the loader is its own line above the composer.
                screen(&[" Reply with PINEAPPLE", " ⠙ Working...", rule, rule]),
                ManifestState::Working,
                "working-spinner",
            ),
            (
                screen(&[" Reply with PINEAPPLE", " PINEAPPLE", rule, rule]),
                ManifestState::Idle,
                "idle-composer",
            ),
            (
                // A draft in the composer is still idle.
                screen(&[" PINEAPPLE", rule, "half a thought", rule]),
                ManifestState::Idle,
                "idle-composer",
            ),
            (
                vec![
                    rule.to_owned(),
                    " Trust project folder?".into(),
                    " /private/var/folders/T/.tmpfi3BJz/project".into(),
                    " This allows pi to load .pi settings and resources, install missing project packages, and execute".into(),
                    " project extensions.".into(),
                    " → Trust".into(),
                    "   Trust parent folder (/private/var/folders/T/.tmpfi3BJz)".into(),
                    "   Trust (this session only)".into(),
                    "   Do not trust".into(),
                    "   Do not trust (this session only)".into(),
                    " ↑↓ navigate  enter select  escape/ctrl+c cancel".into(),
                    rule.to_owned(),
                ],
                ManifestState::BlockedQuestion,
                "question-dialog",
            ),
            (
                // An extension's ctx.ui.select, e.g. a permission gate, with
                // the footer still below it.
                screen(&[
                    rule,
                    " ⚠️ Dangerous command:",
                    "   rm -rf build",
                    " Allow?",
                    " → Yes",
                    "   No",
                    " ↑↓ navigate  enter select  escape/ctrl+c cancel",
                    rule,
                ]),
                ManifestState::BlockedQuestion,
                "question-dialog",
            ),
            (
                screen(&[
                    rule,
                    " Name this session",
                    " > ",
                    " enter submit  escape/ctrl+c cancel",
                    rule,
                ]),
                ManifestState::BlockedQuestion,
                "question-dialog",
            ),
        ];

        let engine = engine();
        for (lines, state, rule_id) in cases {
            let observation = engine
                .evaluate(&ScreenSnapshot::from_lines(lines.clone()), "pi")
                .unwrap_or_else(|| panic!("Pi screen should match: {lines:#?}"));
            assert_eq!(
                (observation.state, observation.matched_rule_id.as_str()),
                (state, rule_id),
                "{lines:#?}"
            );
        }
    }

    /// Visible grids captured from Droid 0.233.0 via `tests/droid_real.rs`.
    /// Only the temporary project path is normalized in the fixture screens.
    /// Before these rules Droid had no idle rule, so every finished turn read
    /// as unknown; its trust dialog matched nothing and stayed "starting";
    /// and the approval options sit inside a box border the old line regex
    /// could not get past, so a permission never read as blocked.
    #[test]
    fn droid_rules_match_the_screens_droid_draws() {
        let engine = engine();
        let cases = [
            (
                include_str!("../../tests/fixtures/droid_screens/trust.txt"),
                ManifestState::BlockedQuestion,
                "workspace-trust-dialog",
            ),
            (
                include_str!("../../tests/fixtures/droid_screens/idle.txt"),
                ManifestState::Idle,
                "idle-composer",
            ),
            (
                include_str!("../../tests/fixtures/droid_screens/idle_no_model.txt"),
                ManifestState::Idle,
                "idle-composer",
            ),
            (
                include_str!("../../tests/fixtures/droid_screens/done.txt"),
                ManifestState::Idle,
                "idle-composer",
            ),
            (
                include_str!("../../tests/fixtures/droid_screens/working_stream.txt"),
                ManifestState::Working,
                "working-spinner-stop",
            ),
            (
                include_str!("../../tests/fixtures/droid_screens/working_execute.txt"),
                ManifestState::Working,
                "working-spinner-stop",
            ),
            (
                include_str!("../../tests/fixtures/droid_screens/permission.txt"),
                ManifestState::BlockedPermission,
                "blocked-approval-panel",
            ),
            (
                include_str!("../../tests/fixtures/droid_screens/ask_user.txt"),
                ManifestState::BlockedQuestion,
                "blocked-ask-user",
            ),
        ];
        for (text, state, rule_id) in cases {
            let actual = engine
                .evaluate(
                    &ScreenSnapshot::from_lines(text.lines().map(str::to_owned)),
                    "droid",
                )
                .expect("captured Droid screen must match");
            assert_eq!(
                (actual.state, actual.matched_rule_id.as_str()),
                (state, rule_id),
                "{text}"
            );
        }
    }

    /// Visible grids captured from Kimi Code 2.1.1 via `tests/kimi_real.rs`.
    /// Only temporary paths/session IDs are normalized in the fixture screens.
    #[test]
    fn kimi_rules_match_the_screens_kimi_draws() {
        let engine = engine();
        let cases = [
            (
                include_str!("../../tests/fixtures/kimi_screens/trust.txt"),
                ManifestState::BlockedQuestion,
                "workspace-trust-dialog",
            ),
            (
                include_str!("../../tests/fixtures/kimi_screens/login.txt"),
                ManifestState::BlockedQuestion,
                "login-platform-dialog",
            ),
            (
                include_str!("../../tests/fixtures/kimi_screens/no_model.txt"),
                ManifestState::Idle,
                "idle-composer",
            ),
            (
                include_str!("../../tests/fixtures/kimi_screens/idle.txt"),
                ManifestState::Idle,
                "idle-composer",
            ),
            (
                include_str!("../../tests/fixtures/kimi_screens/working.txt"),
                ManifestState::Working,
                "working-spinner-verb",
            ),
            (
                include_str!("../../tests/fixtures/kimi_screens/permission.txt"),
                ManifestState::BlockedPermission,
                "blocked-approval-panel",
            ),
        ];
        for (text, state, rule_id) in cases {
            let observe = |text: &str| {
                engine
                    .evaluate(
                        &ScreenSnapshot::from_lines(text.lines().map(str::to_owned)),
                        "kimi",
                    )
                    .expect("captured Kimi screen must match")
            };
            let actual = observe(text);
            assert_eq!(
                (actual.state, actual.matched_rule_id.as_str()),
                (state, rule_id),
                "{text}"
            );
            // The permanent model footer can say `kimi-k2.5 thinking` even
            // while idle or choosing a dialog. It is not a working signal.
            let actual = observe(&text.replace("fake-model thinking", "kimi-k2.5 thinking"));
            assert_eq!(
                (actual.state, actual.matched_rule_id.as_str()),
                (state, rule_id)
            );
        }
    }

    /// Screens captured from OpenCode 1.18 driven through the Engine
    /// (`tests/opencode_real.rs`). OpenCode draws no idle marker beyond its
    /// footer, so without the footer rule no screen read as idle and every
    /// session sat Starting, then Working, forever.
    #[test]
    fn opencode_rules_match_the_screens_opencode_draws() {
        let composer = [
            "   ┃",
            "   ┃  Build · Fake Model Fake",
            "   ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀",
        ];
        let with_composer = |lines: &[&'static str], footer: &'static str| {
            lines
                .iter()
                .chain(composer.iter())
                .copied()
                .chain([footer])
                .collect::<Vec<_>>()
        };
        let cases = [
            (
                // First run, no provider configured: the home screen.
                vec![
                    "                                ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▄ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀",
                    "              ┃",
                    "              ┃  Ask anything... \"Fix a TODO in the codebase\"",
                    "              ┃",
                    "              ┃  Build · Big Pickle OpenCode Zen",
                    "              ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀",
                    "                                                    tab agents  ctrl+p commands",
                    "                       ● Tip Run /connect to add an AI provider and start coding",
                    "   ~/project                                                               1.17.9",
                ],
                ManifestState::Idle,
                "idle-footer",
            ),
            (
                with_composer(
                    &["   ┃  SLOW stream something", "      ▣  Build · Fake Model"],
                    "    ■⬝⬝⬝⬝⬝⬝⬝  esc interrupt                         tab agents  ctrl+p commands",
                ),
                ManifestState::Working,
                "working-interrupt",
            ),
            (
                // After a turn the token count replaces "tab agents".
                with_composer(
                    &[
                        "      part 7 slow part 8 slow part 9 SLOWDONE",
                        "      ▣  Build · Fake Model · 6.7s",
                    ],
                    "                                                         15  ctrl+p commands",
                ),
                ManifestState::Idle,
                "idle-footer",
            ),
            (
                vec![
                    "   ┃  RUNCMD for me",
                    "      $ touch diri-e2e-file",
                    "      ▣  Build · Fake Model",
                    "   ┃",
                    "   ┃  △ Permission required",
                    "   ┃    # Create the e2e marker file",
                    "   ┃",
                    "   ┃  $ touch diri-e2e-file",
                    "   ┃",
                    "   ┃   Allow once   Allow always   Reject     ctrl+f fullscreen  ⇆ select  enter confirm",
                ],
                ManifestState::BlockedPermission,
                "blocked-permission",
            ),
        ];

        let engine = engine();
        for (lines, state, rule) in cases {
            let observation = engine
                .evaluate(&ScreenSnapshot::from_lines(lines), "opencode")
                .expect("OpenCode screen should match");
            assert_eq!(
                (observation.state, observation.matched_rule_id.as_str()),
                (state, rule)
            );
        }
    }

    /// Captured from whipcode v1.0.0 at 120x36 against
    /// `tests/fixtures/fake_openai_api.py` (see tests/whipcode_real.rs); the
    /// left column is whipcode's own sidebar, the cwd footer is shortened.
    #[test]
    fn whipcode_rules_match_the_screens_whipcode_draws() {
        let composer = |placeholder: &'static str, footer: &'static str| {
            vec![
                "    [2] Context                        0%",
                "                                            ┃",
                placeholder,
                "                                            ┃",
                "    [3] LSP                             0   ┃  Off · fake-model  fake",
                "                                            ┃",
                footer,
            ]
        };
        let idle_footer = " ~/project                                                    ctrl+r repl  ctrl+p commands";
        let busy_footer = " ⬝⬝⬝■■■⬝⬝ esc interrupt                                        ctrl+r repl  ctrl+p commands";
        let idle_input = "                                            ┃  Ask whipcode anything… (/ commands, tab completes)";
        let busy_input =
            "                                            ┃  busy — enter steers the running turn";
        let with = |head: &[&'static str], tail: Vec<&'static str>| {
            head.iter().copied().chain(tail).collect::<Vec<_>>()
        };
        let permission = [
            "    blocked root       In[1] print(shell…",
            "                                            ⚒ rlm_exec {\"code\": \"print(shell.run(command=\\\"touch diri-e2e-file\\\"))\"}",
            "                                            ⚠ Allow bash?",
            "                                              touch diri-e2e-file",
            "                                              always: bash touch",
            "                                              agent src5sni7oyp4ak4vn3qa · permission 09791b3401fcf6355f4da264ba5faa1a",
        ];
        let cases = [
            (
                with(&["    idle root"], composer(idle_input, idle_footer)),
                ManifestState::Idle,
                "idle-footer",
            ),
            (
                // Typed-but-unsent text hides the placeholder; the footer still reads idle.
                with(
                    &["    idle root"],
                    composer(
                        "                                            ┃  fix the build",
                        idle_footer,
                    ),
                ),
                ManifestState::Idle,
                "idle-footer",
            ),
            (
                with(
                    &[
                        "    running root",
                        "                                            ● slow part 0 slow part 1",
                    ],
                    composer(busy_input, busy_footer),
                ),
                ManifestState::Working,
                "footer-interrupt",
            ),
            (
                with(
                    &["    running root"],
                    composer(
                        busy_input,
                        " ■■■⬝⬝⬝⬝⬝ esc again to interrupt                               ctrl+r repl  ctrl+p commands",
                    ),
                ),
                ManifestState::Working,
                "footer-interrupt",
            ),
            (
                // The leader chord replaces the footer for two seconds.
                with(
                    &["    running root"],
                    composer(
                        busy_input,
                        " ctrl+x r repl b sidebar 1·2·3 panels s stop t themes m model",
                    ),
                ),
                ManifestState::Working,
                "busy-placeholder",
            ),
            (
                with(
                    &permission,
                    with(
                        &[
                            "                                              ❯ allow once (a)    allow always for this tree (t)    reject (r)",
                        ],
                        composer(busy_input, busy_footer),
                    ),
                ),
                ManifestState::BlockedPermission,
                "permission-dialog",
            ),
            (
                // After 'r' the options give way to the reason field.
                with(
                    &permission,
                    with(
                        &[
                            "                                              reject with message: █",
                            "                                              enter sends · esc back",
                        ],
                        composer(busy_input, busy_footer),
                    ),
                ),
                ManifestState::BlockedPermission,
                "permission-dialog",
            ),
            (
                with(
                    &[
                        "    running root       In[1] print(user.…",
                        "                                                Question                                                        esc",
                        "                                                Which database should I use?",
                        "                                                1 Postgres                                               relational",
                        "                                                2 SQLite                                                   embedded",
                        "                                                ↑↓ select  enter answer  esc dismiss  1-2 jump",
                    ],
                    composer(busy_input, busy_footer),
                ),
                ManifestState::BlockedQuestion,
                "question-dialog",
            ),
            (
                with(
                    &[
                        "                                                     Connect a provider                                     esc",
                        "                                                     Search",
                        "                                                     Popular",
                        "                                                     OpenRouter",
                        "                                                     Custom endpoint",
                        "                                                     enter select  ctrl+e manage",
                    ],
                    composer(
                        "                                            ┃  Connect a provider to get started · /connect",
                        " ~/project                                                     /connect providers  ctrl+p commands",
                    ),
                ),
                ManifestState::BlockedQuestion,
                "provider-picker",
            ),
            (
                // The picker dismissed: its hint remains in the composer, which is ready.
                composer(
                    "                                            ┃  Connect a provider to get started · /connect",
                    " ~/project                                                     /connect providers  ctrl+p commands",
                ),
                ManifestState::Idle,
                "idle-footer",
            ),
            (
                // An answered permission leaves only the tool row and its result.
                with(
                    &[
                        "                                               ⚙ rlm_exec {\"code\": \"print(shell.run(command=\\\"touch diri-e2e-file\\\"))…",
                        "                                               ↳ Error: Permission denied: the user rejected this action",
                        "                                               The command ran. DONECMD",
                    ],
                    composer(idle_input, idle_footer),
                ),
                ManifestState::Idle,
                "idle-footer",
            ),
        ];

        let engine = engine();
        for (lines, state, rule) in cases {
            let observation = engine
                .evaluate(&ScreenSnapshot::from_lines(lines.clone()), "whipcode")
                .unwrap_or_else(|| panic!("WhipCode screen should match: {lines:#?}"));
            assert_eq!(
                (observation.state, observation.matched_rule_id.as_str()),
                (state, rule),
                "{lines:#?}"
            );
        }
    }

    /// Captured from official Grok Build 1.0.46 via tests/grok_real.rs.
    /// The first-run login screen emits idle OSC metadata despite requiring login.
    #[test]
    fn grok_rules_match_the_screens_grok_draws() {
        let engine = engine();
        let login = [
            "         error sending request for url (https://auth.x.ai/.well-known/openid-configuration)",
            "                                   Login with Grok              l",
            "                                   Quit                         q",
            "  ╭──────────────────────────────────────────────────────────────────────────────────────────────╮",
            "  │ ❯ Type a message...                                                                          │",
            "  ╰──────────────────────────────────────────────────────────────────────────── Grok 4.6 (high) ─╯",
            "                                                                                Grok Build  1.0.46",
        ];
        let mut snapshot = ScreenSnapshot::from_lines(login);
        // Login must outrank both ways Grok can advertise idle.
        snapshot.osc_title = Some("grok".into());
        snapshot.osc_progress_state = Some(0);
        let observation = engine.evaluate(&snapshot, "grok").unwrap();
        assert_eq!(observation.state, ManifestState::BlockedQuestion);
        assert_eq!(observation.matched_rule_id, "blocked-login");

        // A quoted login instruction in conversation is not the active menu.
        let mut stale = login.to_vec();
        stale.extend(["later conversation output"; 9]);
        snapshot.lines = stale.into_iter().map(str::to_owned).collect();
        assert_eq!(
            engine.evaluate(&snapshot, "grok").unwrap().state,
            ManifestState::Idle
        );
        let working = ScreenSnapshot::from_lines([
            "     ⠸ Waiting for response… 0.0s                                                   0.0s ⇣15 [stop]",
            "  ╭──────────────────────────────────────────────────────────────────────────────────────────────╮",
            "  │ ❯                                                                                            │",
            "  ╰───────────────────────────────────────────────────────────────────────────────── fake-model ─╯",
            "  Shift+Tab:mode  │  Ctrl+c:cancel  │  Ctrl+x:shortcuts",
        ]);
        assert_eq!(
            engine.evaluate(&working, "grok").unwrap().state,
            ManifestState::Working
        );
        let question = ScreenSnapshot::from_lines([
            "  ┃  Pick a test color",
            "  ┃  1 (○) Blue   Use blue                                                   █",
            "  ┃  2 (○) Green  Use green                                                  █",
            "  ┃  z (○) Type your answer here",
            "  ┃  ↑/↓ navigate · y copy                                    Enter:submit",
            "  Tab:next answer  │  Esc:scrollback  │  Shift+x:dismiss",
        ]);
        assert_eq!(
            engine.evaluate(&question, "grok").unwrap().state,
            ManifestState::BlockedQuestion
        );
        let permission = ScreenSnapshot::from_lines([
            "  ┃  Create the e2e marker file",
            "  ┃  touch diri-e2e-file",
            "  ┃  1 (○) Yes, and don't ask again for anything (always-approve mode)",
            "  ┃  2 (○) Always allow: touch diri-e2e-file",
            "  ┃  3 (●) Yes, proceed",
            "  ┃  4 (○) No, reject (type to add feedback)",
            "  ┃  5 (○) Never allow: touch diri-e2e-file",
            "  1/5:select  │  Tab:next option  │  ←/→:scope  │  e:edit pattern  │  Ctrl+o:always-approve  │",
        ]);
        assert_eq!(
            engine.evaluate(&permission, "grok").unwrap().state,
            ManifestState::BlockedPermission
        );
    }

    /// Unedited visible rows captured by the real Copilot 1.0.90 Engine harness.
    #[test]
    fn copilot_rules_match_the_screens_copilot_draws() {
        let engine = engine();
        let cases = [
            (
                include_str!("../../tests/fixtures/copilot_screens/first-run.txt"),
                ManifestState::Idle,
                "idle-footer",
            ),
            (
                include_str!("../../tests/fixtures/copilot_screens/idle.txt"),
                ManifestState::Idle,
                "idle-footer",
            ),
            (
                include_str!("../../tests/fixtures/copilot_screens/working-edit.txt"),
                ManifestState::Working,
                "working-cancel-hint",
            ),
            (
                include_str!("../../tests/fixtures/copilot_screens/working-stream.txt"),
                ManifestState::Working,
                "working-cancel-hint",
            ),
            (
                include_str!("../../tests/fixtures/copilot_screens/permission.txt"),
                ManifestState::BlockedPermission,
                "blocked-confirm-dialog",
            ),
            (
                include_str!("../../tests/fixtures/copilot_screens/trust.txt"),
                ManifestState::BlockedPermission,
                "blocked-confirm-dialog",
            ),
            (
                include_str!("../../tests/fixtures/copilot_screens/login.txt"),
                ManifestState::BlockedQuestion,
                "login-account-picker",
            ),
        ];
        for (screen, state, rule) in cases {
            let observation = engine
                .evaluate(&ScreenSnapshot::from_lines(screen.lines()), "copilot")
                .expect("Copilot screen should match");
            assert_eq!(
                (observation.state, observation.matched_rule_id.as_str()),
                (state, rule)
            );
        }
        // Old permission/working text in a transcript must not override a live footer.
        let stale = format!(
            "{}\n{}",
            include_str!("../../tests/fixtures/copilot_screens/permission.txt"),
            include_str!("../../tests/fixtures/copilot_screens/idle.txt")
        );
        assert_eq!(
            engine
                .evaluate(&ScreenSnapshot::from_lines(stale.lines()), "copilot")
                .unwrap()
                .state,
            ManifestState::Idle
        );
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

    fn cursor_snapshot(lines: &[&str], osc_title: Option<&str>) -> ScreenSnapshot {
        ScreenSnapshot {
            lines: lines.iter().map(|line| (*line).to_owned()).collect(),
            osc_title: osc_title.map(str::to_owned),
            ..ScreenSnapshot::default()
        }
    }

    #[test]
    fn codex_queued_follow_up_question_keeps_session_working() {
        use crate::status::{Authority, StatusReducer, StatusSignal};
        use diri_proto::SessionStatus;
        use std::time::{Duration, SystemTime};

        let engine = engine();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let mut reducer = StatusReducer::new(Authority::ScreenPrimary, now);
        for (index, activity) in [
            "• Working (27m 01s • esc to interrupt)",
            "• Reviewing changes (28m 02s • esc to interrupt)",
        ]
        .into_iter()
        .enumerate()
        {
            let snapshot = ScreenSnapshot {
                lines: vec![
                    activity.into(),
                    "• Queued follow-up inputs".into(),
                    "  ? 1 question".into(),
                    "    ⌥ + ↑ to answer".into(),
                    "› Ask Codex to do anything".into(),
                    "gpt-6-astra high · ~/project".into(),
                ],
                osc_title: Some("Action Required | project".into()),
                content_seq: index as u64 + 1,
                ..ScreenSnapshot::default()
            };
            let observation = engine.evaluate(&snapshot, "codex").expect("queue rule");
            assert_eq!(observation.state, ManifestState::Working, "{observation:?}");
            let outcome = reducer.reduce(
                StatusSignal::Screen(observation),
                now + Duration::from_secs(index as u64),
            );
            assert_eq!(reducer.status(), &SessionStatus::Working);
            assert!(outcome.needs_input.is_none(), "queued input is nonblocking");
            assert!(!outcome.turn_completed);
        }
    }

    #[test]
    fn codex_completed_turn_with_queued_question_settles_idle() {
        use crate::status::{Authority, StatusReducer, StatusSignal};
        use diri_proto::SessionStatus;
        use std::time::{Duration, SystemTime};

        let engine = engine();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let mut reducer = StatusReducer::new(Authority::ScreenPrimary, now);
        let mut snapshot = ScreenSnapshot {
            lines: vec![
                "• Working (27s • esc to interrupt)".into(),
                "• Queued follow-up inputs".into(),
                "  ? 1 question".into(),
                "    ⌥ + ↑ to answer".into(),
                "› Ask Codex to do anything".into(),
                "gpt-6-astra high · ~/project".into(),
            ],
            osc_title: Some("Action Required | project".into()),
            content_seq: 1,
            ..ScreenSnapshot::default()
        };
        reducer.reduce(
            StatusSignal::Screen(engine.evaluate(&snapshot, "codex").unwrap()),
            now + Duration::from_secs(5),
        );
        assert_eq!(reducer.status(), &SessionStatus::Working);

        snapshot.lines[0] = "─ Worked for 3m 48s ─────────────────".into();
        snapshot.content_seq += 1;
        reducer.reduce(
            StatusSignal::Screen(engine.evaluate(&snapshot, "codex").unwrap()),
            now + Duration::from_secs(6),
        );
        let outcome = reducer.reduce(StatusSignal::Tick, now + Duration::from_secs(7));
        assert_eq!(reducer.status(), &SessionStatus::Idle);
        assert!(outcome.turn_completed);
        assert!(outcome.needs_input.is_none());
    }

    #[test]
    fn codex_completed_turn_with_stale_spinner_settles_idle() {
        use crate::status::{Authority, StatusReducer, StatusSignal};
        use diri_proto::SessionStatus;
        use std::time::{Duration, SystemTime};

        let engine = engine();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let mut reducer = StatusReducer::new(Authority::ScreenPrimary, now);
        let mut snapshot = cursor_snapshot(
            &[
                "• Working (27s • esc to interrupt)",
                "› Ask Codex to do anything",
            ],
            Some("⠋ Working | project"),
        );
        snapshot.content_seq = 1;
        reducer.reduce(
            StatusSignal::Screen(engine.evaluate(&snapshot, "codex").unwrap()),
            now + Duration::from_secs(5),
        );
        assert_eq!(reducer.status(), &SessionStatus::Working);
        snapshot.lines[0] = "─ Worked for 3m 48s ─────────────────".into();
        snapshot.content_seq += 1;
        reducer.reduce(
            StatusSignal::Screen(engine.evaluate(&snapshot, "codex").unwrap()),
            now + Duration::from_secs(6),
        );
        let outcome = reducer.reduce(StatusSignal::Tick, now + Duration::from_secs(7));
        assert_eq!(reducer.status(), &SessionStatus::Idle);
        assert!(outcome.turn_completed);

        // A new turn can start with the previous completion still visible.
        snapshot
            .lines
            .insert(1, "• Reviewing (1s • esc to interrupt)".into());
        snapshot.content_seq += 1;
        reducer.reduce(
            StatusSignal::Screen(engine.evaluate(&snapshot, "codex").unwrap()),
            now + Duration::from_secs(8),
        );
        assert_eq!(reducer.status(), &SessionStatus::Working);
    }

    /// Codex's startup update chooser, as `update_prompt.rs` draws it. Its
    /// selected row starts with `›`, the same mark as the composer, so it read
    /// as an idle input box; Enter there updates Codex and exits it.
    #[test]
    fn codex_update_chooser_is_a_question_not_an_idle_composer() {
        let engine = engine();
        let chooser = [
            "  ✨ Update available! 0.158.0 -> 0.159.0",
            "",
            "  Release notes: https://github.com/openai/codex/releases/latest",
            "",
            "› 1. Update now (runs `npm install -g @openai/codex`)",
            "  2. Skip",
            "  3. Skip until next version",
            "",
            "  Press enter to continue",
        ];
        let observation = engine
            .evaluate(&cursor_snapshot(&chooser, None), "codex")
            .expect("chooser rule");
        assert_eq!(observation.state, ManifestState::BlockedQuestion);

        // Once the chat is up, the in-history update banner is not a blocker.
        let chat = [
            "✨ Update available! 0.158.0 -> 0.159.0",
            "Run npm install -g @openai/codex to update.",
            "",
            "› Ask Codex to do anything",
        ];
        assert_eq!(
            engine
                .evaluate(&cursor_snapshot(&chat, None), "codex")
                .unwrap()
                .state,
            ManifestState::Idle
        );
    }

    #[test]
    fn codex_completion_requires_a_recent_prompt_and_preserves_blockers() {
        let engine = engine();
        let completed = "─ Worked for 3m 48s ─────────────────";
        let mut snapshot = cursor_snapshot(&[completed], Some("⠋ Working | project"));
        assert_eq!(
            engine.evaluate(&snapshot, "codex").unwrap().state,
            ManifestState::Working
        );
        snapshot
            .lines
            .extend((0..12).map(|i| format!("Output {i}")));
        snapshot.lines.push("› Ask Codex to do anything".into());
        assert_eq!(
            engine.evaluate(&snapshot, "codex").unwrap().state,
            ManifestState::Working
        );

        for (footer, expected) in [
            ("Enter to submit answer", ManifestState::BlockedQuestion),
            (
                "Press enter to confirm or esc to cancel",
                ManifestState::BlockedPermission,
            ),
        ] {
            let snapshot = cursor_snapshot(
                &[completed, "• Working (1s • esc to interrupt)", "›", footer],
                Some("⠋ Working | project"),
            );
            assert_eq!(engine.evaluate(&snapshot, "codex").unwrap().state, expected);
        }
        let snapshot = cursor_snapshot(&[completed, "›"], Some("Action Required | project"));
        assert_eq!(
            engine.evaluate(&snapshot, "codex").unwrap().state,
            ManifestState::BlockedPermission
        );
    }

    #[test]
    fn codex_queue_override_requires_a_complete_recent_footer() {
        let engine = engine();
        let footer = [
            "• Queued follow-up inputs",
            "  ? 1 question",
            "    ⌥ + ↑ to answer",
            "› Ask Codex to do anything",
        ];
        for missing in 0..footer.len() {
            let snapshot = ScreenSnapshot {
                lines: footer
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| *index != missing)
                    .map(|(_, line)| (*line).into())
                    .collect(),
                osc_title: Some("Action Required | project".into()),
                ..ScreenSnapshot::default()
            };
            let observation = engine.evaluate(&snapshot, "codex").unwrap();
            assert_eq!(observation.state, ManifestState::BlockedPermission);
        }

        let mut snapshot = ScreenSnapshot {
            lines: footer.iter().map(|line| (*line).into()).collect(),
            osc_title: Some("Action Required | project".into()),
            ..ScreenSnapshot::default()
        };
        snapshot
            .lines
            .extend((0..12).map(|i| format!("Output {i}")));
        let observation = engine.evaluate(&snapshot, "codex").unwrap();
        assert_eq!(observation.state, ManifestState::BlockedPermission);
    }

    #[test]
    fn codex_action_required_redraws_keep_the_same_notification_identity() {
        use crate::status::{Authority, StatusReducer, StatusSignal};
        use std::time::{Duration, SystemTime};

        let engine = engine();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let mut reducer = StatusReducer::new(Authority::ScreenPrimary, now);
        let mut first = None;
        for seconds in 0..20 {
            // Generic action-required attention must retain its identity
            // across redraws when no nonblocking queue footer is visible.
            let snapshot = ScreenSnapshot {
                lines: vec![
                    format!("• Ran tool {seconds}"),
                    format!("• Working (27m {seconds:02}s • esc to interrupt)"),
                    "› Ask Codex to do anything".into(),
                ],
                osc_title: Some("Action Required | project".into()),
                content_seq: seconds + 1,
                ..ScreenSnapshot::default()
            };
            let observation = engine.evaluate(&snapshot, "codex").expect("title rule");
            let outcome = reducer.reduce(
                StatusSignal::Screen(observation),
                now + Duration::from_secs(seconds),
            );
            assert!(
                !outcome.turn_completed,
                "an action-required title is not completion"
            );
            let mut detail = outcome.needs_input.expect("action-required attention");
            // Notification identity includes the summary and kind, excluding
            // repaint timestamps. The rest of the detail must stay stable too.
            detail.occurred_at = now.into();
            if let Some(first) = &first {
                assert_eq!(&detail, first, "redraw must not create a new alert");
            } else {
                first = Some(detail);
            }
        }
    }

    #[test]
    fn codex_visible_prompts_outrank_the_generic_action_required_title() {
        let engine = engine();
        for (footer, expected, rule) in [
            (
                "Enter to submit answer",
                ManifestState::BlockedQuestion,
                "submit-answer",
            ),
            (
                "Press enter to confirm or esc to cancel",
                ManifestState::BlockedPermission,
                "confirm-prompt",
            ),
        ] {
            let snapshot = cursor_snapshot(
                &[
                    "• Queued follow-up inputs",
                    "  ? 2 questions",
                    "    ⌥ + ↑ to answer",
                    "╭────────────────────╮",
                    "│ Which option?      │",
                    "╰────────────────────╯",
                    footer,
                ],
                Some("Action Required | project"),
            );
            let observation = engine.evaluate(&snapshot, "codex").expect("prompt rule");
            assert_eq!(observation.state, expected);
            assert_eq!(observation.matched_rule_id, rule);
            assert_eq!(
                observation.prompt_excerpt.as_deref().map(str::trim),
                Some("Which option?")
            );
        }
    }

    /// Real 100x30 screens from cursor-agent 2026.10.01-14929f9 through
    /// tests/cursor_real.rs, default zen UI (OSC status indicators off).
    #[test]
    fn cursor_rules_match_the_screens_cursor_cli_draws() {
        let engine = engine();
        let cases = [
            (
                include_str!("../../tests/fixtures/cursor_screens/login.txt"),
                ManifestState::BlockedQuestion,
                "login-dialog",
            ),
            (
                include_str!("../../tests/fixtures/cursor_screens/browser-login.txt"),
                ManifestState::BlockedQuestion,
                "login-dialog",
            ),
            (
                include_str!("../../tests/fixtures/cursor_screens/trust.txt"),
                ManifestState::BlockedQuestion,
                "workspace-trust-dialog",
            ),
            (
                include_str!("../../tests/fixtures/cursor_screens/working.txt"),
                ManifestState::Working,
                "working-status-line",
            ),
            (
                include_str!("../../tests/fixtures/cursor_screens/permission.txt"),
                ManifestState::BlockedPermission,
                "confirm-dialog",
            ),
            (
                include_str!("../../tests/fixtures/cursor_screens/idle.txt"),
                ManifestState::Idle,
                "idle-follow-up-placeholder",
            ),
            // The answered trust box stays above the composer; it must not stick.
            (
                include_str!("../../tests/fixtures/cursor_screens/trusted.txt"),
                ManifestState::Idle,
                "idle-follow-up-placeholder",
            ),
        ];
        for (screen, state, rule) in cases {
            let lines: Vec<_> = screen.lines().collect();
            let observation = engine
                .evaluate(&cursor_snapshot(&lines, None), "cursor")
                .expect(screen);
            assert_eq!(observation.state, state, "{screen}");
            assert_eq!(observation.matched_rule_id, rule, "{screen}");
        }
    }

    #[test]
    fn cursor_spinner_frames_include_braille_blank() {
        let engine = engine();
        // spinner-definitions.ts in 2026.09.28-64d2043. U+2800 is not whitespace.
        for frame in ["⠀⠞", "⠠⠜", "⠰⠰", "⠘⠤", "⠘⠆", "⠘⠣", "⠰⠳", "⠠⠛"]
        {
            let screen = include_str!("../../tests/fixtures/cursor_screens/working.txt")
                .replace("⠠⠜", frame);
            let lines: Vec<_> = screen.lines().collect();
            let observation = engine
                .evaluate(&cursor_snapshot(&lines, None), "cursor")
                .unwrap();
            assert_eq!(observation.state, ManifestState::Working, "{frame}");
        }
    }

    #[test]
    fn cursor_osc_title_keeps_tool_turns_working_until_ready() {
        let engine = engine();
        let grep = cursor_snapshot(
            &["Add a follow-up", "Grep AgentKind", "→"],
            Some("Cursor Integration Fix - \u{23f3} Working ..."),
        );
        let observation = engine.evaluate(&grep, "cursor").expect("match");
        assert_eq!(observation.state, ManifestState::Working);
        assert_eq!(observation.matched_rule_id, "working-osc-title");

        let ready = cursor_snapshot(
            &["Add a follow-up"],
            Some("Cursor Integration Fix - \u{2705} Ready"),
        );
        let observation = engine.evaluate(&ready, "cursor").expect("match");
        assert_eq!(observation.state, ManifestState::Idle);
        assert_eq!(observation.matched_rule_id, "idle-osc-title");

        let grep_without_osc = cursor_snapshot(&["Grep AgentKind", "Add a follow-up"], None);
        let observation = engine.evaluate(&grep_without_osc, "cursor").expect("match");
        assert_eq!(observation.state, ManifestState::Working);
        assert_eq!(observation.matched_rule_id, "working-status-line");

        let leftover_follow_up = cursor_snapshot(
            &["Grep AgentKind already finished", "Add a follow-up"],
            None,
        );
        let observation = engine
            .evaluate(&leftover_follow_up, "cursor")
            .expect("grep outranks leftover follow-up chrome");
        assert_eq!(observation.state, ManifestState::Working);

        let idle_prompt = cursor_snapshot(&["\u{2192} Add a follow-up"], None);
        let observation = engine.evaluate(&idle_prompt, "cursor").expect("match");
        assert_eq!(observation.state, ManifestState::Idle);
        assert_eq!(observation.matched_rule_id, "idle-prompt-arrow");
    }

    #[test]
    fn cursor_permission_prompts_from_2026_10_01_still_block() {
        let engine = engine();
        for prompt in [
            "Run this command?",
            "Run this command outside the sandbox?",
            "Read this file?",
            "Allow this web search?",
            "Allow this web fetch?",
            "Proceed with this edit?",
        ] {
            let observation = engine
                .evaluate(
                    &cursor_snapshot(&[prompt, "→ Run (once) (y)"], None),
                    "cursor",
                )
                .expect(prompt);
            assert_eq!(
                observation.state,
                ManifestState::BlockedPermission,
                "{prompt}"
            );
            assert_eq!(observation.matched_rule_id, "confirm-dialog", "{prompt}");
        }
        // The same words inside a prompt or reply are not a dialog.
        let prose = engine
            .evaluate(
                &cursor_snapshot(
                    &[
                        "can you read this file? and allow this web fetch?",
                        "⠠⠜ Generating",
                    ],
                    None,
                ),
                "cursor",
            )
            .expect("working");
        assert_eq!(prose.state, ManifestState::Working);
    }

    /// Composer chrome may be the previous turn's right after Enter; the
    /// status line proves the new one started. The reducer reads these flags.
    #[test]
    fn cursor_rules_flag_submit_chrome_and_turn_start() {
        let engine = engine();
        let evaluate = |lines: &[&str]| {
            engine
                .evaluate(&cursor_snapshot(lines, None), "cursor")
                .expect("match")
        };
        let arrow = evaluate(&["→ Add a follow-up"]);
        assert!(arrow.stale_after_submit && !arrow.turn_started);
        let placeholder = evaluate(&["→ Plan, search, build anything"]);
        assert!(placeholder.stale_after_submit);
        let working = evaluate(&["⠠⠜ Generating", "→ Add a follow-up"]);
        assert!(working.turn_started && !working.stale_after_submit);
        let ready = engine
            .evaluate(
                &cursor_snapshot(&["→ Add a follow-up"], Some("Chat - Ready")),
                "cursor",
            )
            .expect("ready");
        assert_eq!(ready.matched_rule_id, "idle-osc-title");
        assert!(!ready.stale_after_submit);
    }

    #[test]
    fn an_unknown_manifest_id_is_none_rather_than_a_panic() {
        let engine = engine();
        let snapshot = ScreenSnapshot::from_lines(["anything"]);
        assert!(engine.evaluate(&snapshot, "no-such-agent").is_none());
    }

    #[test]
    fn first_class_agents_ship_verified_official_setup_links() {
        let engine = engine();
        let mut first_class_count = 0;
        for id in engine.ids() {
            let descriptor: diri_proto::AgentDescriptor =
                serde_json::from_value(engine.raw_agent(id).expect("bundled descriptor").clone())
                    .expect("client descriptor");
            if !descriptor.first_class {
                continue;
            }
            first_class_count += 1;
            let setup = descriptor.setup.expect("first-class setup metadata");
            let url = setup
                .url
                .as_deref()
                .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
                .unwrap_or_else(|| panic!("{id} needs a verified HTTP(S) setup URL"));
            let authority = url
                .split_once("://")
                .expect("checked scheme")
                .1
                .split(['/', '?', '#'])
                .next()
                .unwrap_or_default();
            assert!(!authority.is_empty(), "{id} setup URL has no host");
            assert!(
                setup
                    .install_hint
                    .as_deref()
                    .is_some_and(|hint| !hint.trim().is_empty()),
                "{id} needs an install hint"
            );
            assert!(
                setup
                    .sign_in_hint
                    .as_deref()
                    .is_some_and(|hint| !hint.trim().is_empty()),
                "{id} needs a sign-in hint"
            );
            if id == "amp" {
                assert_eq!(
                    setup.sign_in_hint.as_deref(),
                    Some("Sign in at ampcode.com, then run amp."),
                    "Amp guidance must stay within its official manual"
                );
            }
        }
        assert!(
            first_class_count >= 17,
            "the bundled first-class roster unexpectedly shrank"
        );
    }

    /// An install command is typed into a user's shell, so the bundled set is
    /// pinned verbatim: changing one is a reviewed edit of this list, checked
    /// against the vendor's own install page, never a drive-by manifest tweak.
    #[test]
    fn bundled_install_commands_are_the_reviewed_vendor_installers() {
        let engine = engine();
        let mut bundled = Vec::new();
        for id in engine.ids() {
            let descriptor: diri_proto::AgentDescriptor =
                serde_json::from_value(engine.raw_agent(id).expect("bundled descriptor").clone())
                    .expect("client descriptor");
            let Some(setup) = descriptor.setup else {
                continue;
            };
            if let Some(command) = setup.install_command {
                bundled.push((id.to_owned(), command, setup.install_requirement));
            } else {
                assert_eq!(
                    setup.install_requirement, None,
                    "{id} names a requirement for an installer it does not ship"
                );
            }
        }
        bundled.sort();
        let reviewed = [
            (
                "claude-code",
                "curl -fsSL https://claude.ai/install.sh | bash",
                None,
            ),
            (
                "codex",
                "curl -fsSL https://chatgpt.com/codex/install.sh | sh",
                None,
            ),
            (
                "gemini",
                "npm install -g @google/gemini-cli",
                Some("Node.js"),
            ),
            (
                "opencode",
                "curl -fsSL https://opencode.ai/install | bash",
                None,
            ),
            (
                "whipcode",
                "curl -fsSL https://raw.githubusercontent.com/context-labs/whip/main/install.sh | sh",
                Some("Python 3"),
            ),
        ]
        .map(|(id, command, requirement)| {
            (
                id.to_owned(),
                command.to_owned(),
                requirement.map(str::to_owned),
            )
        });
        assert_eq!(bundled, reviewed);
    }
}
