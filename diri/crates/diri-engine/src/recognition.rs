//! Coding CLIs a shell recognises in its foreground without launching them.
//!
//! The launch manifests under `manifests/` are what Diri can start, resume
//! and read the screen of. Plenty more Agents are typed at a prompt by hand,
//! so this catalog only names them: a shell whose foreground job is one of
//! these takes the Agent's identity in the sidebar, and its status follows
//! the terminal's output (see `StatusReducer::lend_to_activity`). An entry
//! whose id is also a manifest's id is read with that manifest's screen rules
//! instead; those entries add executable names the manifest's `binary` misses.
//!
//! Nothing here is resolved per session or per sample. A job is named once,
//! after it has held the foreground for a moment, and only a name that some
//! entry could mean costs anything more than a table scan.
//!
//! # Choosing names
//!
//! A name matches the program's exact base name, never a substring. A name a
//! common tool also uses (`cb`, `cmd`, `cbc`, `qoder`) only counts when the
//! program runs out of the Agent's own npm package; npm installs are where
//! those short aliases come from. Names a package check cannot back up are
//! left out rather than guessed: `cr` (CodeRabbit, a native binary), `oz`
//! (Warp, native) and `bob` (also the Neovim version manager).

/// One way a program's name can identify an Agent.
#[derive(Clone, Copy, Debug)]
pub enum Alias {
    /// The program's base name, exactly.
    Exact(&'static str),
    /// The name, only while the program runs from this npm package.
    Packaged(&'static str, &'static str),
    /// A versioned executable a launcher execs: `muse-bin-1.4.2`.
    Prefix(&'static str),
}

/// An Agent a shell can recognise by name.
#[derive(Debug)]
pub struct RecognizedAgent {
    /// An `AgentKind` id: a manifest's when the Agent has one.
    pub id: &'static str,
    pub aliases: &'static [Alias],
}

use Alias::{Exact, Packaged, Prefix};

/// Every Agent recognised by name. Sources for each alias are the CLI's own
/// install docs or its npm package's `bin` map (`npm view <package> bin`).
pub const CATALOG: &[RecognizedAgent] = &[
    // Executables a launch manifest's single `binary` does not name.
    RecognizedAgent {
        id: "aider",
        // pipx/uv install `aider-chat`'s entry point under its package name.
        aliases: &[Exact("aider-chat")],
    },
    RecognizedAgent {
        id: "grok",
        aliases: &[Exact("grok-build")],
    },
    RecognizedAgent {
        id: "kilo",
        // @kilocode/cli: bin { kilo, kilocode }.
        aliases: &[Exact("kilocode")],
    },
    RecognizedAgent {
        id: "kimi",
        aliases: &[Exact("kimi-cli"), Exact("kimi-code")],
    },
    RecognizedAgent {
        id: "kiro",
        // kiro-cli hands an interactive chat to its `kiro-cli-chat` child.
        aliases: &[Exact("kiro-cli-chat")],
    },
    RecognizedAgent {
        id: "qoder",
        // @qoder-ai/qodercli: bin { qodercli, qoder }. The Qoder IDE ships a
        // `qoder` command of its own, so only the npm one counts.
        aliases: &[Packaged("qoder", "@qoder-ai/qodercli")],
    },
    // Recognition only: no launch manifest, status from output activity.
    RecognizedAgent {
        id: "openclaw",
        aliases: &[Exact("openclaw")],
    },
    RecognizedAgent {
        id: "goose",
        // Block's goose. pressly/goose (SQL migrations) shares the name but
        // finishes before a job is named, or names the tab for its run.
        aliases: &[Exact("goose")],
    },
    RecognizedAgent {
        id: "qwen",
        // @qwen-code/qwen-code: bin { qwen }.
        aliases: &[Exact("qwen")],
    },
    RecognizedAgent {
        id: "crush",
        // @charmland/crush: bin { crush }; also Homebrew's charmbracelet tap.
        aliases: &[Exact("crush")],
    },
    RecognizedAgent {
        id: "vibe",
        // Mistral Vibe, installed with uv: a Python entry point named `vibe`.
        aliases: &[Exact("vibe")],
    },
    RecognizedAgent {
        id: "openhands",
        aliases: &[Exact("openhands")],
    },
    RecognizedAgent {
        id: "auggie",
        // @augmentcode/auggie: bin { auggie }.
        aliases: &[Exact("auggie")],
    },
    RecognizedAgent {
        id: "codebuff",
        // codebuff: bin { codebuff, cb }.
        aliases: &[Exact("codebuff"), Packaged("cb", "codebuff")],
    },
    RecognizedAgent {
        id: "command-code",
        // command-code: bin { command-code, commandcode, cmd, cmdc }.
        aliases: &[
            Exact("command-code"),
            Exact("commandcode"),
            Packaged("cmd", "command-code"),
            Packaged("cmdc", "command-code"),
        ],
    },
    RecognizedAgent {
        id: "coderabbit",
        aliases: &[Exact("coderabbit")],
    },
    RecognizedAgent {
        id: "muse",
        // The `muse` launcher execs a versioned `muse-bin-<version>`.
        aliases: &[Exact("muse"), Prefix("muse-bin-")],
    },
    RecognizedAgent {
        id: "junie",
        // @jetbrains/junie-cli: bin { junie }.
        aliases: &[Exact("junie")],
    },
    RecognizedAgent {
        id: "codebuddy",
        // @tencent-ai/codebuddy-code: bin { codebuddy, codebuddy-code, cbc }.
        // `cbc` is also COIN-OR's solver.
        aliases: &[
            Exact("codebuddy"),
            Exact("codebuddy-code"),
            Packaged("cbc", "@tencent-ai/codebuddy-code"),
        ],
    },
    RecognizedAgent {
        id: "warp",
        // Warp's agent CLI. Its short `oz` is a native binary nothing can
        // tell from any other `oz`, so it is not matched.
        aliases: &[Exact("oz-preview"), Exact("warp-cli")],
    },
    RecognizedAgent {
        id: "abacusai",
        aliases: &[Exact("abacusai")],
    },
    RecognizedAgent {
        id: "mcode",
        // mcode: bin { mcode }.
        aliases: &[Exact("mcode")],
    },
    RecognizedAgent {
        id: "traecli",
        aliases: &[Exact("traecli")],
    },
];

/// The catalog id the program `name` is, if any. `package` is asked only
/// when a name is ambiguous without it, and at most once.
pub fn recognize(name: &str, package: impl FnOnce() -> Option<String>) -> Option<&'static str> {
    let mut package = Some(package);
    let mut resolved: Option<Option<String>> = None;
    for agent in CATALOG {
        for alias in agent.aliases {
            let matched = match *alias {
                Exact(exact) => name == exact,
                Prefix(prefix) => name.len() > prefix.len() && name.starts_with(prefix),
                Packaged(exact, wanted) => {
                    name == exact
                        && resolved
                            .get_or_insert_with(|| package.take().and_then(|ask| ask()))
                            .as_deref()
                            == Some(wanted)
                }
            };
            if matched {
                return Some(agent.id);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn no_package() -> Option<String> {
        panic!("an unambiguous name never asks for its package")
    }

    #[test]
    fn exact_names_are_recognised_without_asking_for_a_package() {
        assert_eq!(recognize("goose", no_package), Some("goose"));
        assert_eq!(recognize("codebuddy-code", no_package), Some("codebuddy"));
        assert_eq!(recognize("kimi-cli", no_package), Some("kimi"));
        assert_eq!(recognize("aider-chat", no_package), Some("aider"));
        assert_eq!(recognize("muse-bin-1.4.2", no_package), Some("muse"));
    }

    #[test]
    fn names_are_never_matched_by_substring() {
        for name in [
            "goose2",
            "xgoose",
            "Goose",
            "muse-bin-",
            "muse-binary",
            "qwen3",
            "",
        ] {
            assert_eq!(recognize(name, || None), None, "{name}");
        }
    }

    #[test]
    fn short_aliases_need_the_agents_npm_package() {
        let package = |name: &'static str| move || Some(name.to_owned());
        assert_eq!(recognize("cb", package("codebuff")), Some("codebuff"));
        assert_eq!(
            recognize("cmd", package("command-code")),
            Some("command-code")
        );
        assert_eq!(
            recognize("cbc", package("@tencent-ai/codebuddy-code")),
            Some("codebuddy")
        );
        assert_eq!(
            recognize("qoder", package("@qoder-ai/qodercli")),
            Some("qoder")
        );

        // A native `cb` (a clipboard helper), another package's `cmd`, the
        // Qoder IDE's launcher, COIN-OR's `cbc`.
        assert_eq!(recognize("cb", || None), None);
        assert_eq!(recognize("cmd", package("some-other-cli")), None);
        assert_eq!(recognize("qoder", || None), None);
        assert_eq!(recognize("cbc", || None), None);
    }

    #[test]
    fn generic_short_names_are_not_in_the_catalog() {
        for name in ["cr", "oz", "bob", "cc", "sh", "pi", "cmd", "cb", "cbc"] {
            let exact = CATALOG
                .iter()
                .flat_map(|agent| agent.aliases)
                .any(|alias| matches!(alias, Exact(exact) if *exact == name));
            assert!(!exact, "{name} must not match by name alone");
        }
    }

    #[test]
    fn ids_and_names_are_unique() {
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        for agent in CATALOG {
            assert!(ids.insert(agent.id), "duplicate id {}", agent.id);
            for alias in agent.aliases {
                let name = match alias {
                    Exact(name) | Packaged(name, _) | Prefix(name) => *name,
                };
                assert!(names.insert(name), "duplicate name {name}");
            }
        }
    }

    #[test]
    fn entries_that_add_names_to_a_manifest_name_a_real_one() {
        let (engine, _) =
            crate::detect::ManifestEngine::load_dir(&crate::detect::bundled_manifest_dir())
                .unwrap();
        for id in ["aider", "grok", "kilo", "kimi", "kiro", "qoder"] {
            assert!(CATALOG.iter().any(|agent| agent.id == id), "{id}");
            assert!(engine.manifest(id).is_some(), "{id} has no manifest");
        }
    }

    /// The catalog only adds names: every manifest `binary` is recognised by
    /// the manifest itself, and a catalog name never shadows one.
    #[test]
    fn the_catalog_never_claims_a_manifest_binary() {
        let (engine, _) =
            crate::detect::ManifestEngine::load_dir(&crate::detect::bundled_manifest_dir())
                .unwrap();
        for id in engine.ids() {
            let Some(binary) = engine
                .manifest(id)
                .and_then(|manifest| manifest.agent.as_ref()?.binary.clone())
            else {
                continue;
            };
            assert_eq!(recognize(&binary, || None), None, "{binary}");
        }
    }
}
