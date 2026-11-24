//! What the composer's toolbar shows before the Agent has answered once:
//! the model, effort and permission mode the CLI will use, read from the
//! CLI's own settings, and the models its picker offers.
//!
//! - Claude Code: `~/.claude/settings.json`, then the project's
//!   `.claude/settings.json` and `.claude/settings.local.json` (later wins).
//! - Codex: `~/.codex/config.toml` and the model catalog Codex caches in
//!   `~/.codex/models_cache.json`.
//!
//! Read once per lens; the transcript and the live screen take over as
//! soon as they say anything.

use std::path::Path;

use diri_proto::AgentKind;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentSetup {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    pub models: Vec<ModelChoice>,
    /// Effort levels the CLI accepts, lowest first.
    pub efforts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelChoice {
    /// What `/model <id>` takes, or the catalog slug.
    pub id: String,
    pub label: String,
    /// The provider's own one-line description (Codex), if any.
    pub detail: String,
}

/// Claude Code 2.1's `/model` picker, in its order.
const CLAUDE_MODELS: &[(&str, &str)] = &[
    ("opus", "Opus 5.5"),
    ("claude-fable-5-2", "Fable 5.2"),
    ("sonnet", "Sonnet 5.5"),
    ("haiku", "Haiku 5.5"),
    ("default", "Default"),
];

/// `/effort <level>` in Claude Code 2.1.
const CLAUDE_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

pub fn read(kind: &AgentKind, cwd: &Path, home: &Path) -> AgentSetup {
    if kind == &AgentKind::CODEX {
        codex(home)
    } else {
        claude(cwd, home)
    }
}

fn claude(cwd: &Path, home: &Path) -> AgentSetup {
    let mut setup = AgentSetup {
        models: CLAUDE_MODELS
            .iter()
            .map(|(id, label)| ModelChoice {
                id: (*id).into(),
                label: (*label).into(),
                detail: String::new(),
            })
            .collect(),
        efforts: CLAUDE_EFFORTS.iter().map(|e| (*e).into()).collect(),
        ..AgentSetup::default()
    };
    for file in [
        home.join(".claude/settings.json"),
        cwd.join(".claude/settings.json"),
        cwd.join(".claude/settings.local.json"),
    ] {
        let Some(json) = std::fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        else {
            continue;
        };
        if let Some(model) = json.get("model").and_then(|v| v.as_str()) {
            setup.model = Some(model.to_owned());
        }
        if let Some(effort) = json.get("effortLevel").and_then(|v| v.as_str()) {
            setup.effort = Some(effort.to_owned());
        }
        if let Some(mode) = json.pointer("/permissions/defaultMode").and_then(|v| v.as_str()) {
            setup.mode = Some(mode.to_owned());
        }
    }
    setup.mode.get_or_insert_with(|| "default".into());
    setup
}

fn codex(home: &Path) -> AgentSetup {
    let mut setup = AgentSetup::default();
    if let Some(config) = std::fs::read_to_string(home.join(".codex/config.toml"))
        .ok()
        .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
    {
        setup.model = config.get("model").and_then(|v| v.as_str()).map(str::to_owned);
        setup.effort = config
            .get("model_reasoning_effort")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
    }
    let catalog = std::fs::read(home.join(".codex/models_cache.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    if let Some(models) = catalog.as_ref().and_then(|c| c.get("models")).and_then(|m| m.as_array()) {
        for model in models.iter().take(40) {
            if model.get("visibility").and_then(|v| v.as_str()) == Some("hide") {
                continue;
            }
            let Some(slug) = model.get("slug").and_then(|v| v.as_str()) else {
                continue;
            };
            let label = model.get("display_name").and_then(|v| v.as_str()).unwrap_or(slug);
            setup.models.push(ModelChoice {
                id: slug.into(),
                label: label.into(),
                detail: model.get("description").and_then(|v| v.as_str()).unwrap_or_default().into(),
            });
            if Some(slug) == setup.model.as_deref() {
                setup.efforts = model
                    .get("supported_reasoning_levels")
                    .and_then(|l| l.as_array())
                    .map(|levels| {
                        levels
                            .iter()
                            .filter_map(|l| l.get("effort")?.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
            }
        }
    }
    if setup.efforts.is_empty() {
        setup.efforts = ["low", "medium", "high"].iter().map(|e| (*e).into()).collect();
    }
    setup
}

/// One installed Agent's section in the model picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentModels {
    pub kind: AgentKind,
    pub label: &'static str,
    pub models: Vec<ModelChoice>,
}

/// The Agents the picker offers, in this order, when installed.
pub const PICKER_AGENTS: &[&str] = &[AgentKind::CLAUDE_CODE_ID, AgentKind::CODEX_ID, "antigravity"];

pub fn agent_label(kind: &AgentKind) -> &'static str {
    match kind.id() {
        AgentKind::CLAUDE_CODE_ID => "Claude Code",
        AgentKind::CODEX_ID => "Codex",
        "antigravity" => "Antigravity",
        _ => "Agent",
    }
}

/// The models an installed Agent offers. Claude's list is its picker's;
/// Codex's comes from its cached catalog; Antigravity lists its own with
/// `agy models` (a network round trip, so callers run this off the UI
/// thread and cache it).
pub fn models_for(kind: &AgentKind, home: &Path, binary: Option<&Path>) -> Vec<ModelChoice> {
    match kind.id() {
        AgentKind::CODEX_ID => codex(home).models,
        "antigravity" => binary.map(antigravity_models).unwrap_or_default(),
        _ => claude(home, home).models,
    }
}

/// `agy models` prints `id<TAB>label` per line after a status line.
pub fn parse_antigravity_models(output: &str) -> Vec<ModelChoice> {
    output
        .lines()
        .filter_map(|line| {
            let (id, label) = line.split_once('\t')?;
            let id = id.trim();
            (!id.is_empty() && !id.contains(' ')).then(|| ModelChoice {
                id: id.into(),
                label: label.trim().into(),
                detail: String::new(),
            })
        })
        .take(40)
        .collect()
}

fn antigravity_models(binary: &Path) -> Vec<ModelChoice> {
    use std::io::Read as _;
    let Ok(mut child) = std::process::Command::new(binary)
        .arg("models")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < std::time::Duration::from_secs(20) => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Vec::new();
            }
        }
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    parse_antigravity_models(&out)
}

impl AgentSetup {
    /// The label for a model id, alias or full provider id.
    pub fn label_for(&self, model: &str) -> String {
        self.models
            .iter()
            .find(|choice| choice.id == model)
            .map(|choice| choice.label.clone())
            .unwrap_or_else(|| super::model::model_display(model))
    }
}

/// The mode Claude's footer shows: `⏵⏵ auto mode on`, `⏸ plan mode on`,
/// `⏵⏵ accept edits on`, `bypass permissions on`. `None` when the footer
/// is not on screen (a picker, a scrolled view).
pub fn screen_mode(rows: &[String]) -> Option<&'static str> {
    rows.iter().rev().take(8).find_map(|row| {
        let row = row.to_lowercase();
        if row.contains("plan mode on") {
            Some("plan")
        } else if row.contains("accept edits on") {
            Some("acceptEdits")
        } else if row.contains("auto mode on") {
            Some("auto")
        } else if row.contains("bypass permissions on") {
            Some("bypassPermissions")
        } else if row.contains("? for shortcuts") || row.contains("shift+tab to cycle") {
            Some("default")
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_settings_layer_home_then_project() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(
            home.path().join(".claude/settings.json"),
            r#"{"model":"opus","effortLevel":"high","permissions":{"defaultMode":"auto"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(project.path().join(".claude")).unwrap();
        std::fs::write(project.path().join(".claude/settings.local.json"), r#"{"model":"sonnet"}"#).unwrap();
        let setup = read(&AgentKind::CLAUDE_CODE, project.path(), home.path());
        assert_eq!(setup.model.as_deref(), Some("sonnet"));
        assert_eq!(setup.effort.as_deref(), Some("high"));
        assert_eq!(setup.mode.as_deref(), Some("auto"));
        assert_eq!(setup.label_for("sonnet"), "Sonnet 5.5");
        assert_eq!(setup.label_for("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(setup.efforts, ["low", "medium", "high", "xhigh", "max"]);
    }

    #[test]
    fn a_fresh_claude_still_has_a_model_list_and_a_mode() {
        let empty = tempfile::tempdir().unwrap();
        let setup = read(&AgentKind::CLAUDE_CODE, empty.path(), empty.path());
        assert_eq!(setup.models.len(), 5);
        assert_eq!(setup.model, None);
        assert_eq!(setup.mode.as_deref(), Some("default"));
    }

    #[test]
    fn codex_reads_its_config_and_listed_models() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".codex")).unwrap();
        std::fs::write(
            home.path().join(".codex/config.toml"),
            "model = \"gpt-6.2-astra\"\nmodel_reasoning_effort = \"medium\"\n",
        )
        .unwrap();
        std::fs::write(
            home.path().join(".codex/models_cache.json"),
            r#"{"models":[
                {"slug":"gpt-6.2-sol","display_name":"GPT-6.2-Sol","description":"Fast","visibility":"list"},
                {"slug":"gpt-6.2-astra","display_name":"GPT-6.2-Astra","visibility":"list",
                 "supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"max"}]},
                {"slug":"codex-auto-review","display_name":"Review","visibility":"hide"}]}"#,
        )
        .unwrap();
        let setup = read(&AgentKind::CODEX, home.path(), home.path());
        assert_eq!(setup.label_for("gpt-6.2-astra"), "GPT-6.2-Astra");
        assert_eq!(setup.models.len(), 2);
        assert_eq!(setup.effort.as_deref(), Some("medium"));
        assert_eq!(setup.efforts, ["low", "medium", "max"]);
    }

    #[test]
    fn antigravity_lists_its_models_from_agy_models() {
        let output = "Fetching available models...\ngemini-4-neon-high\tGemini 4 Neon (High)\ngemini-3.8-flash-medium\tGemini 3.8 Flash (Medium)\n";
        let models = parse_antigravity_models(output);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gemini-4-neon-high");
        assert_eq!(models[1].label, "Gemini 3.8 Flash (Medium)");
    }

    #[test]
    fn the_footer_names_the_mode() {
        let rows = |line: &str| vec!["❯ ".to_owned(), String::new(), line.to_owned()];
        assert_eq!(screen_mode(&rows("  ⏵⏵ auto mode on (shift+tab to cycle) · ← 1 agent")), Some("auto"));
        assert_eq!(screen_mode(&rows("  ⏸ plan mode on (shift+tab to cycle)")), Some("plan"));
        assert_eq!(screen_mode(&rows("  ? for shortcuts")), Some("default"));
        assert_eq!(screen_mode(&rows("  s to use this session only · Esc to cancel")), None);
    }
}
