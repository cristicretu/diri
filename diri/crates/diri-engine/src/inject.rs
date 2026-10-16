//! Spawn-time config injection: what turns a bare agent CLI into a
//! Dirijor-connected session.
//!
//! Ported from the local half of `InjectionBuilder`. The daemon writes two
//! files at startup — a Claude hooks settings file and a Claude MCP config —
//! whose contents reference `$DIRIJOR_CLI` / the CLI's sibling `dirijor-mcp`,
//! then appends per-launch flags (`--settings`, `--mcp-config`, Codex `-c`
//! overrides) for whichever mechanisms the agent's manifest opted into. This
//! is what makes a Claude session hook-driven rather than screen-detected,
//! and what gives every agent the `dirijor` MCP tools.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::agent::InjectionSpec;

/// Environment variable names shared with every hook and MCP shim.
pub const SESSION_ID_ENV: &str = "DIRIJOR_SESSION_ID";
pub const SOCKET_ENV: &str = "DIRIJOR_SOCKET";
pub const CLI_ENV: &str = "DIRIJOR_CLI";
/// The session's bearer token for the Engine-served MCP endpoint. Set only
/// in that session's PTY environment; config files name it, never hold it.
pub const MCP_TOKEN_ENV: &str = "DIRIJOR_MCP_TOKEN";
/// The Claude `--mcp-config` that points at the Engine-served endpoint.
pub const CLAUDE_MCP_HTTP_FILE: &str = "claude-mcp-http.json";

/// A random v4 UUID in the lowercase-hex form Claude accepts as
/// `--session-id`. Minting it ourselves is what makes resume possible later
/// without the agent ever reporting an id.
pub fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("random");
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant
    let h = |range: std::ops::Range<usize>| {
        bytes[range]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    format!(
        "{}-{}-{}-{}-{}",
        h(0..4),
        h(4..6),
        h(6..8),
        h(8..10),
        h(10..16)
    )
}

/// The static hooks file injected into every Claude session via `--settings`.
/// Commands read `$DIRIJOR_CLI` / `$DIRIJOR_SOCKET` from the PTY env, so the
/// file content is identical for all sessions and safe to write once.
pub fn write_claude_hooks_file(inject_dir: &Path) -> io::Result<()> {
    const EVENTS: [&str; 11] = [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "PostToolUseFailure",
        "PermissionRequest",
        "Notification",
        "Stop",
        "SubagentStart",
        "SubagentStop",
        "SessionEnd",
    ];
    let mut hooks = serde_json::Map::new();
    for event in EVENTS {
        hooks.insert(
            event.to_string(),
            json!([{
                "hooks": [{
                    "type": "command",
                    "command": format!("\"${CLI_ENV}\" hook {event}"),
                    "timeout": 10,
                }]
            }]),
        );
    }
    write_atomic(
        &inject_dir.join("claude-hooks.json"),
        &serde_json::to_vec(&json!({ "hooks": hooks }))?,
    )
}

/// The Claude `--mcp-config` file: the `dirijor` stdio server backed by the
/// CLI's sibling `dirijor-mcp` proxy (or the CLI itself as a fallback).
pub fn write_claude_mcp_file(inject_dir: &Path, cli_path: &Path) -> io::Result<()> {
    let (command, args) = mcp_launch(cli_path);
    write_atomic(
        &inject_dir.join("claude-mcp.json"),
        &serde_json::to_vec_pretty(&json!({
            "mcpServers": {
                "dirijor": { "type": "stdio", "command": command, "args": args }
            }
        }))?,
    )
}

/// The Claude `--mcp-config` for the Engine-served Streamable HTTP endpoint.
/// Claude Code expands `${DIRIJOR_MCP_TOKEN}` from its own environment, so
/// this one file serves every session and holds no secret.
pub fn write_claude_mcp_http_file(inject_dir: &Path, url: &str) -> io::Result<()> {
    write_atomic(
        &inject_dir.join(CLAUDE_MCP_HTTP_FILE),
        &serde_json::to_vec_pretty(&json!({
            "mcpServers": {
                "dirijor": {
                    "type": "http",
                    "url": url,
                    "headers": { "Authorization": format!("Bearer ${{{MCP_TOKEN_ENV}}}") },
                }
            }
        }))?,
    )
}

fn mcp_launch(cli_path: &Path) -> (String, Vec<String>) {
    let proxy = cli_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(diri_platform::executable_name("dirijor-mcp"));
    if is_executable(&proxy) {
        (proxy.to_string_lossy().into_owned(), Vec::new())
    } else {
        (
            cli_path.to_string_lossy().into_owned(),
            vec!["mcp-stdio".into()],
        )
    }
}

/// Folder of the Claude Code plugin that carries Diri's skills.
pub const CLAUDE_SKILLS_PLUGIN_DIR: &str = "claude-skills-plugin";

/// Writes `<inject>/claude-skills-plugin/`: a Claude Code plugin named `diri`
/// whose `skills/<name>/SKILL.md` load as native skills (`diri:scheduling`,
/// …). Static like the hooks and MCP files, so one copy serves every session.
/// Staged and swapped whole, so an Engine upgrade never leaves a skill from an
/// older build behind.
pub fn write_claude_skills_plugin(inject_dir: &Path) -> io::Result<PathBuf> {
    let plugin_dir = inject_dir.join(CLAUDE_SKILLS_PLUGIN_DIR);
    let staging = inject_dir.join(format!(
        ".{CLAUDE_SKILLS_PLUGIN_DIR}.{}.tmp",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(staging.join(".claude-plugin"))?;
    write_atomic(
        &staging.join(".claude-plugin/plugin.json"),
        &serde_json::to_vec_pretty(&json!({
            "name": "diri",
            "version": "0.1.0",
            "description": "How to use Diri from inside a Diri session: scheduling, notes, and coordinating other agents.",
            "author": { "name": "Diri" },
        }))?,
    )?;
    for skill in diri_proto::skills::ALL {
        let folder = staging.join("skills").join(skill.name);
        std::fs::create_dir_all(&folder)?;
        write_atomic(&folder.join("SKILL.md"), skill.skill_md().as_bytes())?;
    }
    let backup = inject_dir.join(format!(
        ".{CLAUDE_SKILLS_PLUGIN_DIR}.{}.old",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&backup);
    if plugin_dir.exists() {
        std::fs::rename(&plugin_dir, &backup)?;
    }
    match std::fs::rename(&staging, &plugin_dir) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&backup);
            Ok(plugin_dir)
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            if backup.exists() {
                let _ = std::fs::rename(&backup, &plugin_dir);
            }
            Err(error)
        }
    }
}

/// Session identity needed to bake Cursor's per-launch plugin (MCP env + hooks).
pub struct CursorInject<'a> {
    pub session_id: &'a str,
    pub socket_path: &'a Path,
}

/// Per-launch injection arguments for the mechanisms a manifest opted into.
pub fn injection_args(
    injection: &InjectionSpec,
    inject_dir: &Path,
    cli_path: &Path,
) -> Vec<String> {
    injection_args_with_cursor(injection, inject_dir, cli_path, None)
}

/// Like [`injection_args`], and when `cursor` is set also writes/appends the
/// session-local Cursor `--plugin-dir` (MCP + stop hook).
pub fn injection_args_with_cursor(
    injection: &InjectionSpec,
    inject_dir: &Path,
    cli_path: &Path,
    cursor: Option<CursorInject<'_>>,
) -> Vec<String> {
    injection_args_full(injection, inject_dir, cli_path, cursor, None).argv
}

/// Per-launch injection plus whether the Agent was pointed at the
/// Engine-served MCP endpoint, in which case the caller must put the
/// session's token in [`MCP_TOKEN_ENV`].
pub struct Injected {
    pub argv: Vec<String>,
    pub mcp_http: bool,
}

/// Like [`injection_args_with_cursor`]; with `mcp_http_url`, Agents whose
/// CLI speaks MCP over HTTP (Claude Code, Codex) reach the Engine directly
/// instead of starting a `dirijor-mcp` process. Without it, or when the
/// HTTP config is missing, they keep the stdio server.
pub fn injection_args_full(
    injection: &InjectionSpec,
    inject_dir: &Path,
    cli_path: &Path,
    cursor: Option<CursorInject<'_>>,
    mcp_http_url: Option<&str>,
) -> Injected {
    let mut argv = Vec::new();
    let mut mcp_http = false;
    if injection.claude_hooks {
        let hooks = inject_dir.join("claude-hooks.json");
        if hooks.exists() {
            argv.push("--settings".into());
            argv.push(hooks.to_string_lossy().into_owned());
        }
    }
    if injection.claude_mcp {
        let http = inject_dir.join(CLAUDE_MCP_HTTP_FILE);
        let stdio = inject_dir.join("claude-mcp.json");
        let config = if mcp_http_url.is_some() && http.exists() {
            mcp_http = true;
            Some(http)
        } else {
            stdio.exists().then_some(stdio)
        };
        if let Some(config) = config {
            argv.push("--mcp-config".into());
            argv.push(config.to_string_lossy().into_owned());
            // The skills explain those tools, so they travel together.
            let skills = inject_dir.join(CLAUDE_SKILLS_PLUGIN_DIR);
            if skills.join(".claude-plugin/plugin.json").exists() {
                argv.push("--plugin-dir".into());
                argv.push(skills.to_string_lossy().into_owned());
            }
        }
    }
    if injection.codex_notify {
        argv.push("-c".into());
        argv.push(format!(
            "notify=[{}, \"notify\"]",
            toml_string(&cli_path.to_string_lossy())
        ));
    }
    if injection.codex_mcp {
        if let Some(url) = mcp_http_url {
            // Codex reads the bearer token from the named variable itself.
            mcp_http = true;
            argv.push("-c".into());
            argv.push(format!("mcp_servers.dirijor.url={}", toml_string(url)));
            argv.push("-c".into());
            argv.push(format!(
                "mcp_servers.dirijor.bearer_token_env_var={}",
                toml_string(MCP_TOKEN_ENV)
            ));
        } else {
            let (command, args) = mcp_launch(cli_path);
            let encoded_args = args
                .iter()
                .map(|arg| toml_string(arg))
                .collect::<Vec<_>>()
                .join(",");
            argv.push("-c".into());
            argv.push(format!(
                "mcp_servers.dirijor.command={}",
                toml_string(&command)
            ));
            argv.push("-c".into());
            argv.push(format!("mcp_servers.dirijor.args=[{encoded_args}]"));
            // Codex starts stdio MCP servers with a scrubbed environment, so
            // the bridge would not know which session is calling and refuse
            // every write. Name it explicitly.
            if let Some(identity) = cursor.as_ref() {
                argv.push("-c".into());
                argv.push(format!(
                    "mcp_servers.dirijor.env={{DIRIJOR_SESSION_ID={}, DIRIJOR_SOCKET={}}}",
                    toml_string(identity.session_id),
                    toml_string(&identity.socket_path.to_string_lossy())
                ));
            }
        }
    }
    // Cursor stays on stdio: cursor-agent 2025.09.12 sends no configured
    // headers to HTTP MCP servers (it falls into OAuth discovery instead).
    if (injection.cursor_mcp || injection.cursor_hooks)
        && let Some(cursor) = cursor
        && let Ok(plugin_dir) = write_cursor_plugin(
            inject_dir,
            cli_path,
            cursor.session_id,
            cursor.socket_path,
            injection.cursor_mcp,
            injection.cursor_hooks,
        )
    {
        argv.push("--plugin-dir".into());
        argv.push(plugin_dir.to_string_lossy().into_owned());
    }
    Injected { argv, mcp_http }
}

/// Writes `<inject>/cursor-plugin/<session>/` with plugin manifest, optional
/// `mcp.json`, and optional `hooks/hooks.json`. Returns the plugin directory.
///
/// Always stages into a fresh temp dir and replaces the live plugin tree so a
/// resume that disables MCP or hooks cannot leave the previous files active.
pub fn write_cursor_plugin(
    inject_dir: &Path,
    cli_path: &Path,
    session_id: &str,
    socket_path: &Path,
    mcp: bool,
    hooks: bool,
) -> io::Result<PathBuf> {
    let plugin_root = inject_dir.join("cursor-plugin");
    std::fs::create_dir_all(&plugin_root)?;
    let plugin_dir = plugin_root.join(session_id);
    let staging = plugin_root.join(format!(".{session_id}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(staging.join(".cursor-plugin"))?;
    write_atomic(
        &staging.join(".cursor-plugin/plugin.json"),
        &serde_json::to_vec_pretty(&json!({
            "name": "dirijor",
            "version": "0.1.0",
            "description": "Diri agent orchestration for Cursor sessions",
        }))?,
    )?;

    if mcp {
        let (target, target_args) = mcp_launch(cli_path);
        // Cursor splits `command` on spaces before spawning it. Keep the
        // App Support path in argv, where JSON preserves it as one argument.
        #[cfg(unix)]
        let (command, args, environment) = (
            "/usr/bin/env".to_owned(),
            std::iter::once(target)
                .chain(target_args)
                .collect::<Vec<_>>(),
            json!({ "DIRIJOR_SESSION_ID": session_id, "DIRIJOR_SOCKET": socket_path.to_string_lossy(), "DIRIJOR_CLI": cli_path.to_string_lossy() }),
        );
        #[cfg(windows)]
        let (command, args, environment) = {
            let binary = Path::new(&target);
            let directory = binary
                .parent()
                .ok_or_else(|| io::Error::other("MCP executable has no directory"))?;
            let path = std::env::var_os("PATH").unwrap_or_default();
            let path = std::env::join_paths(
                std::iter::once(directory.to_path_buf()).chain(std::env::split_paths(&path)),
            )
            .map_err(io::Error::other)?;
            (
                binary
                    .file_name()
                    .ok_or_else(|| io::Error::other("MCP executable has no filename"))?
                    .to_string_lossy()
                    .into_owned(),
                target_args,
                json!({ "PATH": path.to_string_lossy(), "DIRIJOR_SESSION_ID": session_id, "DIRIJOR_SOCKET": socket_path.to_string_lossy(), "DIRIJOR_CLI": cli_path.to_string_lossy() }),
            )
        };
        write_atomic(
            &staging.join("mcp.json"),
            &serde_json::to_vec_pretty(&json!({
                "mcpServers": {
                    "dirijor": {
                        "type": "stdio",
                        "command": command,
                        "args": args,
                        "env": environment
                    }
                }
            }))?,
        )?;
    }

    if hooks {
        std::fs::create_dir_all(staging.join("hooks"))?;
        // Absolute CLI path: Cursor does not expand $DIRIJOR_CLI in hook cmds.
        let quoted = shell_single_quote(&cli_path.to_string_lossy());
        write_atomic(
            &staging.join("hooks/hooks.json"),
            &serde_json::to_vec_pretty(&json!({
                "version": 1,
                "hooks": {
                    "stop": [{ "command": format!("{quoted} hook Stop") }],
                    "beforeSubmitPrompt": [{
                        "command": format!("{quoted} hook UserPromptSubmit")
                    }],
                }
            }))?,
        )?;
    }

    let backup = plugin_root.join(format!(".{session_id}.{}.old", std::process::id()));
    let _ = std::fs::remove_dir_all(&backup);
    if plugin_dir.exists() {
        std::fs::rename(&plugin_dir, &backup)?;
    }
    match std::fs::rename(&staging, &plugin_dir) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&backup);
            Ok(plugin_dir)
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            if backup.exists() {
                let _ = std::fs::rename(&backup, &plugin_dir);
            }
            Err(error)
        }
    }
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Claude Code's project-directory slug. Native Windows paths also encode the
/// drive colon, backslashes, spaces and other non-ASCII-alphanumeric characters.
pub fn claude_project_slug(cwd: &str) -> String {
    #[cfg(windows)]
    {
        // Match the vendor's UTF-16 /[^a-zA-Z0-9]/g replacement, including two
        // replacements for a non-BMP character. Never join a raw drive path
        // beneath the transcript root: Path::join would replace that root.
        cwd.encode_utf16()
            .map(|unit| match unit {
                0x30..=0x39 | 0x41..=0x5a | 0x61..=0x7a => char::from_u32(unit.into()).unwrap(),
                _ => '-',
            })
            .collect()
    }
    #[cfg(not(windows))]
    cwd.replace(['/', '.'], "-")
}

/// `~/.claude/projects/<slug>/<uuid>.jsonl` — predictable only for Claude,
/// because only Claude lets the caller choose the session UUID *and* derives
/// its jsonl path from the cwd.
pub fn claude_transcript_path(home: &Path, cwd: &str, session_uuid: &str) -> PathBuf {
    home.join(".claude/projects")
        .join(claude_project_slug(cwd))
        .join(format!("{session_uuid}.jsonl"))
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_v4_and_unique() {
        let a = uuid_v4();
        let b = uuid_v4();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'4', "version nibble: {a}");
        assert!(
            matches!(a.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "variant nibble: {a}"
        );
    }

    #[test]
    fn the_hooks_file_matches_the_swift_shape() {
        let temp = tempfile::tempdir().expect("temp");
        write_claude_hooks_file(temp.path()).expect("write");
        let parsed: serde_json::Value = serde_json::from_slice(
            &std::fs::read(temp.path().join("claude-hooks.json")).expect("read"),
        )
        .expect("parse");
        let stop = &parsed["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(stop["type"], "command");
        assert_eq!(stop["command"], "\"$DIRIJOR_CLI\" hook Stop");
        assert_eq!(stop["timeout"], 10);
        assert!(parsed["hooks"]["SubagentStop"].is_array());
    }

    #[test]
    fn the_mcp_file_prefers_the_sibling_proxy() {
        let temp = tempfile::tempdir().expect("temp");
        let cli = temp.path().join("bin/dirijor");
        std::fs::create_dir_all(cli.parent().unwrap()).expect("mkdir");
        std::fs::write(&cli, "#!/bin/sh\n").expect("cli");

        // No proxy: fall back to `dirijor mcp-stdio`.
        write_claude_mcp_file(temp.path(), &cli).expect("write");
        let parsed: serde_json::Value = serde_json::from_slice(
            &std::fs::read(temp.path().join("claude-mcp.json")).expect("read"),
        )
        .expect("parse");
        assert_eq!(parsed["mcpServers"]["dirijor"]["args"][0], "mcp-stdio");

        // With an executable sibling, it becomes the command.
        let proxy = temp
            .path()
            .join("bin")
            .join(diri_platform::executable_name("dirijor-mcp"));
        std::fs::write(&proxy, "#!/bin/sh\n").expect("proxy");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&proxy, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        write_claude_mcp_file(temp.path(), &cli).expect("write");
        let parsed: serde_json::Value = serde_json::from_slice(
            &std::fs::read(temp.path().join("claude-mcp.json")).expect("read"),
        )
        .expect("parse");
        assert_eq!(
            parsed["mcpServers"]["dirijor"]["command"],
            proxy.to_string_lossy().as_ref()
        );
        assert_eq!(
            parsed["mcpServers"]["dirijor"]["args"]
                .as_array()
                .map(Vec::len),
            Some(0)
        );
    }

    /// Writes the real plugin to `DIRI_SKILLS_OUT` so it can be checked with
    /// `claude plugin validate` and loaded with `claude --plugin-dir`.
    #[test]
    #[ignore = "writes the Claude skills plugin to DIRI_SKILLS_OUT"]
    fn write_skills_plugin_for_inspection() {
        let out = std::env::var_os("DIRI_SKILLS_OUT").expect("set DIRI_SKILLS_OUT");
        let dir = write_claude_skills_plugin(Path::new(&out)).expect("write");
        eprintln!("{}", dir.display());
    }

    #[test]
    fn the_skills_plugin_holds_every_diri_skill_and_replaces_stale_ones() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = write_claude_skills_plugin(temp.path()).expect("write");
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join(".claude-plugin/plugin.json")).expect("manifest"),
        )
        .expect("json");
        assert_eq!(manifest["name"], "diri", "skills load as diri:<name>");
        for skill in diri_proto::skills::ALL {
            let md = std::fs::read_to_string(dir.join("skills").join(skill.name).join("SKILL.md"))
                .expect("skill file");
            assert!(md.starts_with(&format!("---\nname: {}\n", skill.name)));
            assert!(md.contains(skill.markdown));
        }
        // A skill an older build wrote must not survive an upgrade.
        std::fs::create_dir_all(dir.join("skills/retired")).expect("stale");
        write_claude_skills_plugin(temp.path()).expect("rewrite");
        assert!(!dir.join("skills/retired").exists());
        let leftovers: Vec<_> = std::fs::read_dir(temp.path())
            .expect("list")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");
    }

    #[test]
    fn injection_args_cover_all_four_mechanisms() {
        let temp = tempfile::tempdir().expect("temp");
        let cli = temp.path().join("dirijor");
        write_claude_hooks_file(temp.path()).expect("hooks");
        write_claude_mcp_file(temp.path(), &cli).expect("mcp");

        let claude = InjectionSpec {
            claude_hooks: true,
            claude_mcp: true,
            ..Default::default()
        };
        let args = injection_args(&claude, temp.path(), &cli);
        assert_eq!(args[0], "--settings");
        assert!(args[1].ends_with("claude-hooks.json"));
        assert_eq!(args[2], "--mcp-config");
        assert!(args[3].ends_with("claude-mcp.json"));
        // Without the skills plugin on disk, no dangling --plugin-dir.
        assert_eq!(args.len(), 4, "{args:?}");
        write_claude_skills_plugin(temp.path()).expect("skills");
        let args = injection_args(&claude, temp.path(), &cli);
        assert_eq!(args[4], "--plugin-dir");
        assert!(args[5].ends_with(CLAUDE_SKILLS_PLUGIN_DIR));

        let codex = InjectionSpec {
            codex_notify: true,
            codex_mcp: true,
            ..Default::default()
        };
        let args = injection_args(&codex, temp.path(), &cli);
        assert_eq!(args[0], "-c");
        assert!(args[1].starts_with("notify=["), "{args:?}");
        assert!(
            args.iter()
                .any(|arg| arg.starts_with("mcp_servers.dirijor.command=")),
            "{args:?}"
        );
    }

    #[test]
    fn http_capable_agents_reach_the_engine_and_others_keep_stdio() {
        let temp = tempfile::tempdir().expect("temp");
        let cli = temp.path().join("dirijor");
        let url = "http://127.0.0.1:4545/mcp";
        write_claude_mcp_file(temp.path(), &cli).expect("stdio mcp");
        let claude = InjectionSpec {
            claude_mcp: true,
            ..Default::default()
        };
        let codex = InjectionSpec {
            codex_mcp: true,
            ..Default::default()
        };

        // Endpoint up but the Claude HTTP config never written: stdio.
        // Codex on stdio scrubs its MCP server's environment: the session
        // identity has to travel in the server's own env table.
        let identity = CursorInject {
            session_id: "s_1",
            socket_path: Path::new("/tmp/d.sock"),
        };
        let stdio = injection_args_full(&codex, temp.path(), &cli, Some(identity), None);
        assert!(
            stdio.argv.contains(
                &"mcp_servers.dirijor.env={DIRIJOR_SESSION_ID=\"s_1\", DIRIJOR_SOCKET=\"/tmp/d.sock\"}"
                    .to_owned()
            ),
            "{:?}",
            stdio.argv
        );

        let injected = injection_args_full(&claude, temp.path(), &cli, None, Some(url));
        assert!(!injected.mcp_http);
        assert!(injected.argv[1].ends_with("claude-mcp.json"));

        write_claude_mcp_http_file(temp.path(), url).expect("http mcp");
        let config: serde_json::Value = serde_json::from_slice(
            &std::fs::read(temp.path().join(CLAUDE_MCP_HTTP_FILE)).expect("read"),
        )
        .expect("json");
        assert_eq!(
            config["mcpServers"]["dirijor"],
            json!({
                "type": "http",
                "url": url,
                "headers": {"Authorization": "Bearer ${DIRIJOR_MCP_TOKEN}"},
            })
        );
        let injected = injection_args_full(&claude, temp.path(), &cli, None, Some(url));
        assert!(injected.mcp_http);
        assert_eq!(injected.argv[0], "--mcp-config");
        assert!(injected.argv[1].ends_with(CLAUDE_MCP_HTTP_FILE));
        // No endpoint: the stdio config, even with the HTTP file on disk.
        let injected = injection_args_full(&claude, temp.path(), &cli, None, None);
        assert!(!injected.mcp_http);
        assert!(injected.argv[1].ends_with("claude-mcp.json"));

        let injected = injection_args_full(&codex, temp.path(), &cli, None, Some(url));
        assert!(injected.mcp_http);
        assert_eq!(
            injected.argv,
            [
                "-c",
                "mcp_servers.dirijor.url=\"http://127.0.0.1:4545/mcp\"",
                "-c",
                "mcp_servers.dirijor.bearer_token_env_var=\"DIRIJOR_MCP_TOKEN\"",
            ]
        );
        let injected = injection_args_full(&codex, temp.path(), &cli, None, None);
        assert!(!injected.mcp_http);
        assert!(
            injected
                .argv
                .iter()
                .any(|arg| arg.starts_with("mcp_servers.dirijor.command=")),
            "{:?}",
            injected.argv
        );

        // Cursor cannot send our header over HTTP: it stays on stdio.
        let cursor = InjectionSpec {
            cursor_mcp: true,
            ..Default::default()
        };
        let socket = temp.path().join("d.sock");
        let injected = injection_args_full(
            &cursor,
            temp.path(),
            &cli,
            Some(CursorInject {
                session_id: "s_c",
                socket_path: &socket,
            }),
            Some(url),
        );
        assert!(!injected.mcp_http);
        let mcp: serde_json::Value = serde_json::from_slice(
            &std::fs::read(Path::new(&injected.argv[1]).join("mcp.json")).expect("mcp.json"),
        )
        .expect("json");
        assert_eq!(mcp["mcpServers"]["dirijor"]["type"], "stdio");
    }

    /// A launch kept on stdio (endpoint down, or a CLI below the verified
    /// HTTP minimum) must get exactly what it got before the HTTP endpoint
    /// existed. The expected values are spelled out as main produced them.
    #[test]
    fn gated_out_launches_get_main_s_stdio_injection_byte_for_byte() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = temp.path();
        let cli = dir.join("bin/dirijor");
        write_claude_hooks_file(dir).expect("hooks");
        write_claude_mcp_file(dir, &cli).expect("mcp");
        write_claude_skills_plugin(dir).expect("skills");
        // The HTTP config sitting beside it must not change anything.
        write_claude_mcp_http_file(dir, "http://127.0.0.1:1/mcp").expect("http");
        let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
        let cli_text = cli.to_string_lossy().into_owned();

        // main's exact expression for the stdio config (no sibling
        // dirijor-mcp, so the CLI's `mcp-stdio` subcommand).
        assert_eq!(
            std::fs::read(dir.join("claude-mcp.json")).unwrap(),
            serde_json::to_vec_pretty(&json!({
                "mcpServers": {
                    "dirijor": { "type": "stdio", "command": cli_text, "args": ["mcp-stdio"] }
                }
            }))
            .unwrap()
        );

        let claude = InjectionSpec {
            claude_hooks: true,
            claude_mcp: true,
            ..Default::default()
        };
        let injected = injection_args_full(&claude, dir, &cli, None, None);
        assert!(!injected.mcp_http);
        assert_eq!(
            injected.argv,
            [
                "--settings".to_owned(),
                path("claude-hooks.json"),
                "--mcp-config".to_owned(),
                path("claude-mcp.json"),
                "--plugin-dir".to_owned(),
                path(CLAUDE_SKILLS_PLUGIN_DIR),
            ]
        );
        assert_eq!(injected.argv, injection_args(&claude, dir, &cli));

        let codex = InjectionSpec {
            codex_notify: true,
            codex_mcp: true,
            ..Default::default()
        };
        let injected = injection_args_full(&codex, dir, &cli, None, None);
        assert!(!injected.mcp_http);
        assert_eq!(
            injected.argv,
            [
                "-c".to_owned(),
                format!("notify=[{}, \"notify\"]", toml_string(&cli_text)),
                "-c".to_owned(),
                format!("mcp_servers.dirijor.command={}", toml_string(&cli_text)),
                "-c".to_owned(),
                "mcp_servers.dirijor.args=[\"mcp-stdio\"]".to_owned(),
            ]
        );
        assert_eq!(injected.argv, injection_args(&codex, dir, &cli));
    }

    #[test]
    fn cursor_plugin_is_launch_scoped_with_baked_env_and_stop_hook() {
        let temp = tempfile::tempdir().expect("temp");
        let cli = temp.path().join("Application Support/dirijor");
        std::fs::create_dir_all(cli.parent().unwrap()).expect("mkdir");
        std::fs::write(&cli, b"#!/bin/sh\n").expect("cli");
        let proxy = cli.with_file_name(diri_platform::executable_name("dirijor-mcp"));
        std::fs::write(&proxy, b"#!/bin/sh\n").expect("proxy");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&cli).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&cli, perms).unwrap();
            std::fs::set_permissions(&proxy, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let socket = temp.path().join("d.sock");
        let cursor = InjectionSpec {
            cursor_mcp: true,
            cursor_hooks: true,
            ..Default::default()
        };
        let args = injection_args_with_cursor(
            &cursor,
            temp.path(),
            &cli,
            Some(CursorInject {
                session_id: "s_test",
                socket_path: &socket,
            }),
        );
        assert_eq!(args[0], "--plugin-dir");
        assert!(
            Path::new(&args[1]).ends_with("cursor-plugin/s_test"),
            "{args:?}"
        );

        let plugin = Path::new(&args[1]);
        let mcp: serde_json::Value =
            serde_json::from_slice(&std::fs::read(plugin.join("mcp.json")).expect("mcp.json"))
                .expect("json");
        #[cfg(unix)]
        {
            assert_eq!(mcp["mcpServers"]["dirijor"]["command"], "/usr/bin/env");
            assert_eq!(
                mcp["mcpServers"]["dirijor"]["args"][0],
                proxy.to_string_lossy().as_ref()
            );
        }
        // Cursor splits `command` on spaces: the bare file name runs from the
        // proxy's directory, which leads the session's PATH.
        #[cfg(windows)]
        {
            assert_eq!(
                mcp["mcpServers"]["dirijor"]["command"],
                proxy.file_name().unwrap().to_string_lossy().as_ref()
            );
            let path = mcp["mcpServers"]["dirijor"]["env"]["PATH"]
                .as_str()
                .expect("PATH");
            assert_eq!(
                std::env::split_paths(path).next().as_deref(),
                proxy.parent()
            );
        }
        assert_eq!(
            mcp["mcpServers"]["dirijor"]["env"]["DIRIJOR_SESSION_ID"],
            "s_test"
        );
        assert_eq!(
            mcp["mcpServers"]["dirijor"]["env"]["DIRIJOR_SOCKET"],
            socket.to_string_lossy().as_ref()
        );

        let hooks: serde_json::Value =
            serde_json::from_slice(&std::fs::read(plugin.join("hooks/hooks.json")).expect("hooks"))
                .expect("json");
        let stop = hooks["hooks"]["stop"][0]["command"].as_str().expect("cmd");
        assert!(stop.contains("hook Stop"), "{stop}");
        assert!(stop.contains("dirijor"), "{stop}");
    }

    #[test]
    fn cursor_plugin_rewrite_drops_disabled_components() {
        let temp = tempfile::tempdir().expect("temp");
        let cli = temp.path().join("bin/dirijor");
        std::fs::create_dir_all(cli.parent().unwrap()).expect("mkdir");
        std::fs::write(&cli, b"#!/bin/sh\n").expect("cli");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let socket = temp.path().join("d.sock");
        let both = InjectionSpec {
            cursor_mcp: true,
            cursor_hooks: true,
            ..Default::default()
        };
        let args = injection_args_with_cursor(
            &both,
            temp.path(),
            &cli,
            Some(CursorInject {
                session_id: "s_reuse",
                socket_path: &socket,
            }),
        );
        let plugin = Path::new(&args[1]);
        assert!(plugin.join("mcp.json").is_file());
        assert!(plugin.join("hooks/hooks.json").is_file());

        let hooks_only = InjectionSpec {
            cursor_mcp: false,
            cursor_hooks: true,
            ..Default::default()
        };
        let _ = injection_args_with_cursor(
            &hooks_only,
            temp.path(),
            &cli,
            Some(CursorInject {
                session_id: "s_reuse",
                socket_path: &socket,
            }),
        );
        assert!(
            !plugin.join("mcp.json").exists(),
            "disabled mcp.json must not survive a rewrite"
        );
        assert!(plugin.join("hooks/hooks.json").is_file());
    }

    #[test]
    fn the_transcript_slug_matches_claudes_rule() {
        assert_eq!(
            claude_project_slug("/Users/giga/.claude/worktrees/x"),
            "-Users-giga--claude-worktrees-x"
        );
        let path = claude_transcript_path(Path::new("/Users/giga"), "/tmp/repo", "abc-123");
        assert_eq!(
            path,
            Path::new("/Users/giga/.claude/projects/-tmp-repo/abc-123.jsonl")
        );
    }
}
