//! Agent descriptors: how to launch an agent, read from its manifest.
//!
//! The `agent` half of each manifest says what to run and how to talk to it —
//! binary, resume flags, environment, which keystroke approves a prompt. Like
//! the detection rules, it is data: adding an agent should not require code.
//!
//! This module turns a descriptor plus a working directory into a [`PtySpec`].

use serde::Deserialize;

use crate::pty::PtySpec;
use crate::status::Authority;

/// How an agent's status is decided. Declared per agent rather than inferred.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StatusAuthority {
    Hooks,
    Screen,
    Process,
}

impl From<StatusAuthority> for Authority {
    fn from(authority: StatusAuthority) -> Self {
        match authority {
            StatusAuthority::Hooks => Authority::HooksPrimary,
            StatusAuthority::Screen => Authority::ScreenPrimary,
            StatusAuthority::Process => Authority::ProcessOnly,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSpec {
    pub style: String,
    #[serde(default)]
    pub token: Option<String>,
}

/// The config-injection mechanisms a manifest can opt into. Each is a
/// Dirijor-implemented shim (hooks file, MCP config, notify callback): the
/// manifest names the mechanism, the daemon owns the file it points at.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct InjectionSpec {
    #[serde(default, rename = "claudeHooks")]
    pub claude_hooks: bool,
    #[serde(default, rename = "claudeMCP")]
    pub claude_mcp: bool,
    #[serde(default, rename = "codexNotify")]
    pub codex_notify: bool,
    #[serde(default, rename = "codexMCP")]
    pub codex_mcp: bool,
    /// Cursor has no `--mcp-config`. Launch a session-local `--plugin-dir`
    /// whose `mcp.json` advertises the `dirijor` stdio server.
    #[serde(default, rename = "cursorMCP")]
    pub cursor_mcp: bool,
    /// Same plugin ships hooks: Cursor `stop` → `dirijor hook Stop`.
    #[serde(default, rename = "cursorHooks")]
    pub cursor_hooks: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApproveSpec {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub submit: bool,
}

/// Display-only setup metadata. It is parsed here so user overrides can carry
/// it, but the Engine never executes the hint or opens the URL.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupSpec {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub install_hint: Option<String>,
    #[serde(default)]
    pub sign_in_hint: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDescriptor {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub short_label: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub first_class: bool,
    #[serde(default)]
    pub status_authority: Option<StatusAuthority>,
    /// The executable to run. Absent for `shell` and `generic`, whose command
    /// comes from the caller.
    #[serde(default)]
    pub binary: Option<String>,
    #[serde(default)]
    pub return_to_login_shell: bool,
    /// Swift Codable spelling: capital ID, which `rename_all = "camelCase"`
    /// would miss (`sessionIdFlag`) — and a silently-unparsed flag means no
    /// caller-minted conversation UUID and therefore no resume.
    #[serde(default, rename = "sessionIDFlag")]
    pub session_id_flag: Option<String>,
    /// Extra argv the manifest wants on every spawn, before injection args.
    #[serde(default)]
    pub spawn_args: Vec<String>,
    /// Which Dirijor-implemented config shims this agent takes.
    #[serde(default)]
    pub injection: InjectionSpec,
    #[serde(default)]
    pub resume: Option<ResumeSpec>,
    /// Environment the agent needs.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Prefixes to strip from the inherited environment.
    ///
    /// A daemon that leaks its own `CLAUDE_*` or `CODEX_*` variables into a
    /// fresh agent makes it resume somebody else's session or refuse to start.
    #[serde(default)]
    pub env_scrub_prefixes: Vec<String>,
    #[serde(default)]
    pub approve: Option<ApproveSpec>,
    #[serde(default)]
    pub setup: Option<SetupSpec>,
}

impl AgentDescriptor {
    /// The reducer authority this agent declares, defaulting to the
    /// conservative one when a manifest does not say.
    pub fn authority(&self) -> Authority {
        self.status_authority
            .map_or(Authority::ProcessOnly, Authority::from)
    }

    /// Builds the launch spec for this agent in `cwd`.
    ///
    /// `inherited` is the environment to start from — normally the daemon's.
    /// Three things happen to it, all of which have caused real bugs:
    ///
    /// - **Scrubbing.** Variables matching `env_scrub_prefixes` are dropped, so
    ///   a new agent does not inherit the identity of the session that spawned
    ///   it.
    /// - **Colour is asserted, not inherited.** An inherited `NO_COLOR` (or a
    ///   missing `TERM`) silently turns an agent's output monochrome, which
    ///   then breaks the screen rules that look for its prompt box. `TERM` and
    ///   `COLORTERM` are set explicitly and `NO_COLOR` is removed.
    /// - **The agent's own `env` is applied last**, so a manifest can override
    ///   anything above.
    pub fn spawn_spec(
        &self,
        cwd: &std::path::Path,
        inherited: impl IntoIterator<Item = (String, String)>,
        extra_args: &[String],
    ) -> Option<PtySpec> {
        let binary = self.binary.clone()?;
        let mut argv = vec![binary];
        argv.extend(extra_args.iter().cloned());

        let mut spec = PtySpec::new(argv, cwd);
        for (key, value) in inherited {
            if self.should_scrub(&key) {
                continue;
            }
            spec.env.push((key, value));
        }
        spec.env.retain(|(key, _)| key != "NO_COLOR");
        spec.env
            .retain(|(key, _)| key != "TERM" && key != "COLORTERM");
        spec.env.push(("TERM".into(), "xterm-256color".into()));
        spec.env.push(("COLORTERM".into(), "truecolor".into()));
        for (key, value) in &self.env {
            spec.env.retain(|(existing, _)| existing != key);
            spec.env.push((key.clone(), value.clone()));
        }
        if self.return_to_login_shell {
            // Keep the shell as the PTY's session leader. When the agent exits
            // (notably after Codex updates itself), the command re-enters that
            // shell and leaves a usable prompt instead of ending the session.
            // The agent binary deliberately stays bare: the fresh interactive
            // login shell re-sources nvm/mise/Homebrew config and resolves the
            // version selected *now*, not when the daemon started.
            let shell = spec
                .env
                .iter()
                .rev()
                .find(|(key, value)| key == "SHELL" && !value.is_empty())
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| "/bin/sh".to_string());
            let mut command = spec
                .argv
                .iter()
                .map(|argument| shell_quote(argument))
                .collect::<Vec<_>>()
                .join(" ");
            command.push_str(&format!("; exec {} -i -l", shell_quote(&shell)));
            spec.argv = vec![shell, "-i".into(), "-l".into(), "-c".into(), command];
        } else if let Some(first) = spec.argv.first_mut()
            && !first.contains('/')
        {
            // Bare launches still need an absolute executable. The process
            // that finally execs this argv may be a long-lived holder manager
            // whose launchd-minimal environment predates this daemon —
            // posix_spawnp searches the *caller's* PATH, not the child's.
            let path = spec
                .env
                .iter()
                .rev()
                .find(|(key, _)| key == "PATH")
                .map(|(_, value)| value.clone())
                .or_else(|| std::env::var("PATH").ok());
            if let Some(resolved) = path
                .as_deref()
                .and_then(|path| resolve_on_path(first, path))
            {
                *first = resolved;
            }
        }
        Some(spec)
    }

    /// Builds the same exact argv/environment tuple for a remote Helper.
    /// Executable lookup is deliberately left to the remote Holder against
    /// the captured remote PATH; no local filesystem probe can answer it.
    /// `return_to_login_shell` is not synthesized as a shell command: remote
    /// Agent launches remain structured and the session exits with Agent.
    pub fn remote_spawn_spec(
        &self,
        cwd: &std::path::Path,
        inherited: impl IntoIterator<Item = (String, String)>,
        extra_args: &[String],
    ) -> Option<PtySpec> {
        let binary = self.binary.clone()?;
        let mut spec = PtySpec::new(
            std::iter::once(binary)
                .chain(extra_args.iter().cloned())
                .collect(),
            cwd,
        );
        for (key, value) in inherited {
            if !self.should_scrub(&key) {
                spec.env.push((key, value));
            }
        }
        spec.env
            .retain(|(key, _)| !matches!(key.as_str(), "NO_COLOR" | "TERM" | "COLORTERM"));
        spec.env.push(("TERM".into(), "xterm-256color".into()));
        spec.env.push(("COLORTERM".into(), "truecolor".into()));
        for (key, value) in &self.env {
            spec.env.retain(|(existing, _)| existing != key);
            spec.env.push((key.clone(), value.clone()));
        }
        Some(spec)
    }

    fn should_scrub(&self, key: &str) -> bool {
        self.env_scrub_prefixes
            .iter()
            .any(|prefix| key.starts_with(prefix))
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Absolute path of `binary` searched across a colon-separated `path`, or
/// `None` when nothing executable matches (the spawn then fails with its
/// honest error instead of a misleading one).
pub(crate) fn resolve_on_path(binary: &str, path: &str) -> Option<String> {
    for dir in path.split(':').filter(|dir| !dir.is_empty()) {
        let candidate = std::path::Path::new(dir).join(binary);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = std::fs::metadata(&candidate)
                && metadata.is_file()
                && metadata.permissions().mode() & 0o111 != 0
            {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
        #[cfg(not(unix))]
        {
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

impl AgentDescriptor {
    /// The argv tail that resumes an existing conversation, if the agent can.
    pub fn resume_args(&self, agent_session_id: Option<&str>) -> Option<Vec<String>> {
        let resume = self.resume.as_ref()?;
        let token = resume.token.clone()?;
        match resume.style.as_str() {
            // `--resume <id>` when we know the id, bare `--resume` otherwise.
            "flag" => Some(match agent_session_id {
                Some(id) => vec![token, id.to_string()],
                None => vec![token],
            }),
            // The id is passed through the session-id flag instead.
            "sessionIDFlag" => {
                let flag = self.session_id_flag.clone()?;
                let id = agent_session_id?;
                Some(vec![flag, id.to_string()])
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::ManifestEngine;
    use std::path::{Path, PathBuf};

    fn manifest_dir() -> PathBuf {
        crate::detect::bundled_manifest_dir()
            .canonicalize()
            .expect("manifests")
    }

    fn descriptor(id: &str) -> AgentDescriptor {
        let (engine, _) = ManifestEngine::load_dir(&manifest_dir()).expect("load");
        engine
            .manifest(id)
            .expect("manifest")
            .agent
            .clone()
            .expect("every shipped manifest carries an agent descriptor")
    }

    #[test]
    fn authority_comes_from_the_manifest_not_from_hardcoded_ids() {
        assert_eq!(
            descriptor("claude-code").authority(),
            Authority::HooksPrimary
        );
        assert_eq!(descriptor("codex").authority(), Authority::ScreenPrimary);
        assert_eq!(descriptor("shell").authority(), Authority::ProcessOnly);
        // An agent added by dropping in a JSON file gets the right authority
        // with no code change at all.
        assert_eq!(descriptor("opencode").authority(), Authority::ScreenPrimary);
    }

    #[test]
    fn the_daemons_own_agent_variables_are_scrubbed() {
        // Inheriting CLAUDE_* from the session that spawned this one makes the
        // new agent resume somebody else's conversation.
        let claude = descriptor("claude-code");
        let inherited = [
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("CLAUDE_CODE_CHILD_SESSION".to_string(), "1".to_string()),
            ("CLAUDECODE".to_string(), "1".to_string()),
        ];
        let spec = claude
            .spawn_spec(Path::new("/tmp"), inherited, &[])
            .expect("claude has a binary");

        let keys: Vec<&str> = spec.env.iter().map(|(key, _)| key.as_str()).collect();
        assert!(keys.contains(&"PATH"), "unrelated variables survive");
        assert!(
            !keys.iter().any(|key| key.starts_with("CLAUDE_CODE_CHILD")),
            "inherited agent state must not leak: {keys:?}"
        );
        assert!(!keys.contains(&"CLAUDECODE"));
    }

    #[test]
    fn bare_binaries_resolve_to_absolute_paths_for_foreign_executors() {
        // The holder manager that execs the argv may carry a launchd-minimal
        // PATH from a previous era; posix_spawnp searches the caller's PATH,
        // so a bare name must leave the daemon already absolute. This is the
        // "every ⌘T exits 127" failure.
        assert_eq!(
            resolve_on_path("true", "/nonexistent:/usr/bin"),
            Some("/usr/bin/true".to_string())
        );
        assert_eq!(resolve_on_path("no-such-binary-anywhere", "/usr/bin"), None);

        // End to end through spawn_spec: a bare-launch agent is resolved on
        // the spec's PATH before it reaches the holder.
        let bin_dir = tempfile::tempdir().expect("temp dir");
        let stub = bin_dir.path().join("gemini");
        std::fs::write(&stub, "#!/bin/sh\n").expect("stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let gemini = descriptor("gemini");
        let inherited = [(
            "PATH".to_string(),
            bin_dir.path().to_string_lossy().into_owned(),
        )];
        let spec = gemini
            .spawn_spec(Path::new("/tmp"), inherited, &[])
            .expect("gemini has a binary");
        assert_eq!(
            spec.argv[0],
            stub.to_string_lossy(),
            "argv[0] must leave the daemon already absolute"
        );
    }

    #[test]
    fn shipped_agents_land_in_a_login_shell_when_the_agent_exits() {
        // Codex replaces its own binary when it self-updates and then exits.
        // Without the wrapper the PTY dies with it and the session is gone; the
        // wrapper leaves a usable prompt in the same tab. Dropping
        // `returnToLoginShell` from the manifests silently reverts that.
        let codex = descriptor("codex");
        assert!(codex.return_to_login_shell);
        let spec = codex
            .spawn_spec(
                Path::new("/tmp"),
                [
                    ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                    ("SHELL".to_string(), "/bin/sh".to_string()),
                ],
                &["--version".to_string()],
            )
            .expect("codex has a binary");

        assert_eq!(spec.argv[..4], ["/bin/sh", "-i", "-l", "-c"]);
        assert_eq!(
            spec.argv[4], "'codex' '--version'; exec '/bin/sh' -i -l",
            "the agent runs first, then the shell takes the PTY over"
        );
    }

    /// Sixteen of the twenty shipped manifests declare `returnToLoginShell`;
    /// only `cursor`, `gemini` and the two command-less manifests do not. The
    /// flag has been lost wholesale once already, so assert the whole set
    /// rather than a sample: a port that drops it fails here.
    #[test]
    fn the_login_shell_wrapper_is_declared_by_every_agent_that_needs_it() {
        let (engine, failed) = ManifestEngine::load_dir(&manifest_dir()).expect("load");
        assert!(failed.is_empty(), "manifests failed to decode: {failed:?}");

        let mut wrapped: Vec<&str> = engine
            .ids()
            .into_iter()
            .filter(|id| {
                engine
                    .manifest(id)
                    .and_then(|manifest| manifest.agent.as_ref())
                    .is_some_and(|agent| agent.return_to_login_shell)
            })
            .collect();
        wrapped.sort_unstable();

        assert_eq!(
            wrapped,
            [
                "aider",
                "amp",
                "antigravity",
                "claude-code",
                "codex",
                "copilot",
                "devin",
                "droid",
                "grok",
                "hermes",
                "kilo",
                "kimi",
                "kiro",
                "opencode",
                "pi",
                "qoder",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn wrapped_agent_really_accepts_shell_input_after_the_agent_finishes() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let wrapped = AgentDescriptor {
            binary: Some("/bin/sh".into()),
            return_to_login_shell: true,
            ..Default::default()
        };
        let spec = wrapped
            .spawn_spec(
                Path::new("/tmp"),
                [("SHELL".to_string(), "/bin/sh".to_string())],
                &["-c".into(), "printf 'agent-finished\\n'".into()],
            )
            .expect("spec");

        let mut child = Command::new(&spec.argv[0])
            .args(&spec.argv[1..])
            .current_dir(&spec.cwd)
            .envs(spec.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("launch wrapped agent");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(b"printf 'shell-ready\\n'\nexit\n")
            .expect("type after agent exit");
        let output = child.wait_with_output().expect("wait");
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains("agent-finished"),
            "agent did not run: {stdout:?}"
        );
        assert!(
            stdout.contains("shell-ready"),
            "the session did not accept shell input after agent exit: {stdout:?}"
        );
    }

    #[test]
    fn colour_is_asserted_rather_than_inherited() {
        // An inherited NO_COLOR turns the agent monochrome, and the screen
        // rules that look for its prompt box then never match.
        let claude = descriptor("claude-code");
        let inherited = [
            ("NO_COLOR".to_string(), "1".to_string()),
            ("TERM".to_string(), "dumb".to_string()),
        ];
        let spec = claude
            .spawn_spec(Path::new("/tmp"), inherited, &[])
            .expect("spec");

        let get = |name: &str| {
            spec.env
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(get("NO_COLOR"), None, "NO_COLOR must be removed");
        assert_eq!(get("TERM"), Some("xterm-256color"));
        assert_eq!(get("COLORTERM"), Some("truecolor"));
    }

    #[test]
    fn a_manifests_own_env_wins() {
        let claude = descriptor("claude-code");
        let spec = claude
            .spawn_spec(
                Path::new("/tmp"),
                [("CLAUDE_CODE_NO_FLICKER".to_string(), "0".to_string())],
                &[],
            )
            .expect("spec");
        let value = spec
            .env
            .iter()
            .find(|(key, _)| key == "CLAUDE_CODE_NO_FLICKER")
            .map(|(_, value)| value.as_str());
        assert_eq!(value, Some("1"), "the manifest sets this deliberately");
    }

    #[test]
    fn resume_arguments_follow_the_declared_style() {
        let claude = descriptor("claude-code");
        assert_eq!(
            claude.resume_args(Some("abc")),
            Some(vec!["--resume".to_string(), "abc".to_string()])
        );
        assert_eq!(
            claude.resume_args(None),
            Some(vec!["--resume".to_string()]),
            "claude can resume the latest session without an id"
        );

        // Gemini mints no id of its own: without `sessionIDFlag` there is no
        // caller-minted UUID to resume against, so losing that one field
        // silently costs gemini its resume entirely.
        let gemini = descriptor("gemini");
        assert_eq!(gemini.session_id_flag.as_deref(), Some("--session-id"));
        assert_eq!(
            gemini.resume_args(Some("uuid-1")),
            Some(vec!["--resume".to_string(), "uuid-1".to_string()])
        );

        // The latest-session agents: no id anywhere, so the bare token is the
        // whole resume. A manifest with no `resume` block cannot resume at all.
        for (id, token) in [
            ("opencode", "--continue"),
            ("aider", "--restore-chat-history"),
            ("codex", "resume"),
            ("cursor", "resume"),
            ("pi", "-c"),
        ] {
            assert_eq!(
                descriptor(id).resume_args(None),
                Some(vec![token.to_string()]),
                "{id} must resume"
            );
        }
    }

    #[test]
    fn an_agent_without_a_binary_has_no_spawn_spec() {
        // `shell` and `generic` take their command from the caller.
        let shell = descriptor("shell");
        assert!(shell.spawn_spec(Path::new("/tmp"), [], &[]).is_none());
    }

    #[test]
    fn every_shipped_manifest_declares_an_authority() {
        let (engine, _) = ManifestEngine::load_dir(&manifest_dir()).expect("load");
        for id in engine.ids() {
            let manifest = engine.manifest(id).expect("manifest");
            let agent = manifest
                .agent
                .as_ref()
                .unwrap_or_else(|| panic!("{id} has no agent descriptor"));
            assert!(
                agent.status_authority.is_some(),
                "{id} does not declare statusAuthority"
            );
        }
    }
}
