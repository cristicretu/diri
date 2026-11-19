//! Opt-in: real Agent CLIs driven through a private Engine, recording what
//! message navigation (`diri-term`'s `messages` module) relies on.
//!
//! `record_message_fixtures` writes the screens under
//! `diri-term/tests/fixtures/agent_messages` (as `.ansi`, with a styled
//! `.txt` description beside each). The measurements print the figures each
//! `Notches` model is built from: `measure_burst_curve` the lines one burst of
//! wheel notches moves and when it is drawn, `measure_wheel_response` how
//! bursts close together speed up, and `measure_claude_page_keys` how far
//! Claude Code's PageUp/PageDown move and that they leave a draft alone.
//! Re-run them when an Agent changes how it draws or scrolls its transcript.
//!
//! Nothing here needs a provider account. Each CLI talks to
//! `fixtures/fake_agent_api.py` (OpenAI chat and Responses, Anthropic
//! Messages) on a free 127.0.0.1 port; the same server is the CLIs' HTTP(S)
//! proxy and refuses every other host. HOME, every XDG directory and the
//! project live in a temporary directory removed on drop; the developer's
//! own Agent configuration is never read or written. `claude`, `codex` and
//! `opencode` must be on PATH, and `python3` for the scripted API:
//!
//! ```sh
//! DIRI_AGENTS=claude-code,codex,opencode DIRI_DUMP_DIR=/tmp/screens \
//!   cargo test -p diri-engine --test agent_messages_real -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Codex starts a detached app-server daemon in that HOME on first launch.
//! On drop the fixture ends every session first (a live Codex would relaunch
//! the daemon), stops the daemon with its own managed binary's
//! `app-server daemon stop`, then kills and reports anything still running
//! from the temporary directory; one left running outlives its deleted HOME
//! until reboot.
//!
//! `DIRI_INLINE=1` runs Codex with `--no-alt-screen`, recording its inline
//! history instead. The tests set process-wide environment the Engine hands
//! to its children, so they must run with `--test-threads=1`.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_proto::ControlMessage;
use diri_proto::grid::{GridCell, GridRowCodec, TermColor, TermStyle};
use serde_json::{Value, json};

#[path = "support/composer.rs"]
mod composer;

#[path = "support/teardown.rs"]
mod teardown;

struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next: u64,
}

impl Client {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next += 1;
        let message = ControlMessage::Request {
            id: self.next,
            method: method.into(),
            params: Some(params),
        };
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        self.writer.write_all(&bytes).unwrap();
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).unwrap();
            if let ControlMessage::Response { id, result, .. } =
                serde_json::from_str::<ControlMessage>(&line).unwrap()
                && id == self.next
            {
                return result.map_err(|error| format!("{error:?}"));
            }
        }
    }

    fn screen(&mut self, id: &str) -> String {
        self.call("session.read_screen", json!({ "sessionID": id }))
            .map(|result| result["text"].as_str().unwrap_or_default().to_string())
            .unwrap_or_default()
    }

    fn wait_screen(&mut self, id: &str, within: Duration, done: impl Fn(&str) -> bool) -> String {
        let start = Instant::now();
        loop {
            let screen = self.screen(id);
            if done(&screen) || start.elapsed() > within {
                return screen;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Every retained row, history first, as message navigation reads it.
    fn history(&mut self, id: &str) -> Vec<Vec<GridCell>> {
        let mut rows = Vec::new();
        loop {
            let page = self
                .call(
                    "session.read_scrollback_cells",
                    json!({ "sessionID": id, "firstRow": rows.len(), "maxRows": 256 }),
                )
                .unwrap();
            let count = page["rowCount"].as_i64().unwrap() as usize;
            let payload: Vec<u8> = {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD
                    .decode(page["payload"].as_str().unwrap())
                    .unwrap()
            };
            rows.extend(GridRowCodec::decode_rows(&payload, count).unwrap());
            if count == 0 || rows.len() as i64 >= page["totalRows"].as_i64().unwrap() {
                return rows;
            }
        }
    }
}

/// One line per row: its text, then the column, colors and style of its first
/// glyph, the row's own background, and whether it soft-wraps.
fn describe(rows: &[Vec<GridCell>]) -> String {
    let mut out = String::new();
    for (index, row) in rows.iter().enumerate() {
        let text: String = row
            .iter()
            .filter(|cell| !cell.style.contains(TermStyle::WIDE_SPACER))
            .map(|cell| {
                char::from_u32(cell.scalar)
                    .filter(|c| *c != '\0')
                    .unwrap_or(' ')
            })
            .collect();
        let first = row
            .iter()
            .position(|cell| cell.scalar != 32 && cell.scalar != 0);
        let color = |color: TermColor| match color {
            TermColor::Default => "d".to_string(),
            TermColor::DefaultInverted => "-".to_string(),
            TermColor::Ansi(n) => format!("a{n}"),
            TermColor::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        };
        let style = first.map(|col| {
            let cell = row[col];
            format!(
                "c{col} fg={} bg={} st={:x} bg0={}{}",
                color(cell.fg),
                color(cell.bg),
                cell.style.bits(),
                color(row[0].bg),
                if row.last().unwrap().style.contains(TermStyle::SOFT_WRAP) {
                    " WRAP"
                } else {
                    ""
                }
            )
        });
        out.push_str(&format!(
            "{index:5} |{}| {}\n",
            text.trim_end(),
            style.unwrap_or_default()
        ));
    }
    out
}

struct Fixture {
    temp: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    api: Child,
    registry: Option<Arc<Mutex<Registry>>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Ends every session and waits for its exit, also after a test
        // panicked before its own `session.kill`: a live Codex relaunches
        // the daemon stopped below.
        if let Some(registry) = &self.registry {
            let mut registry = registry.lock().unwrap_or_else(|poison| poison.into_inner());
            for record in registry.records() {
                if let Err(error) = registry.terminate(&record.id.0, Duration::from_millis(500)) {
                    eprintln!("teardown: session {} did not end: {error}", record.id.0);
                }
            }
        }
        // Codex's daemon is detached and outlives every tab; stop it before
        // its HOME disappears, with the managed binary that runs it (the
        // test's own PATH may hold another `codex`, or none). Absent when
        // Codex never ran. CODEX_HOME would aim the stop at the developer's
        // own daemon.
        let codex = self
            .home
            .join(".codex/packages/app-server-daemon/current/bin/codex");
        if codex.exists() {
            teardown::stop_daemon(
                &codex,
                &["app-server", "daemon", "stop"],
                &self.home,
                "CODEX_HOME",
            );
        }
        let _ = self.api.kill();
        let _ = self.api.wait();
        teardown::sweep(self.temp.path());
        teardown::remove_tree(self.temp.path());
    }
}

fn fixture() -> Fixture {
    // macOS limits a Unix socket path to 103 bytes; Holders append a session ID.
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.canonicalize().unwrap();
    for dir in [".config", ".local/share", ".local/state", ".cache"] {
        std::fs::create_dir_all(home.join(dir)).unwrap();
    }
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let api = Command::new("python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_agent_api.py"))
        .arg(port.to_string())
        .arg(temp.path().join("api.log"))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "fake API never listened");
        std::thread::sleep(Duration::from_millis(50));
    }
    let proxy = format!("http://127.0.0.1:{port}");
    // SAFETY: --test-threads=1, and set before the Engine spawns anything.
    unsafe {
        // A caller running inside Diri can carry its live socket, session,
        // CLI, recovery paths and MCP token; an Agent can carry its own
        // config/credential directory and provider overrides. Keep only
        // this suite's selectors before building the isolated environment.
        for (name, _) in std::env::vars_os() {
            let Some(key) = name.to_str() else { continue };
            if key.starts_with("DIRIJOR_")
                || key.starts_with("CLAUDE_")
                || key.starts_with("ANTHROPIC_")
                || (key.starts_with("DIRI_")
                    && !matches!(
                        key,
                        "DIRI_AGENTS" | "DIRI_DUMP_DIR" | "DIRI_INLINE" | "DIRI_DELIVERY_TASK_DIR"
                    ))
            {
                std::env::remove_var(name);
            }
        }
        std::env::remove_var("CLAUDECODE");
        // Without CLAUDE_CONFIG_DIR, Claude uses the fixture HOME/.claude
        // and its onboarding seed in HOME/.claude.json.
        std::env::set_var("HOME", &home);
        std::env::set_var("ZDOTDIR", &home);
        std::env::set_var(
            "SHELL",
            if cfg!(target_os = "macos") {
                "/bin/zsh"
            } else {
                "/bin/bash"
            },
        );
        std::env::set_var("DIRIJOR_APP_SUPPORT", temp.path());
        std::env::set_var("DIRIJOR_SOCKET", temp.path().join("daemon.sock"));
        std::env::set_var("DIRIJOR_AGENT_LAUNCHER", "0");
        std::env::set_var("DIRI_HOLDER_IDLE_SECONDS", "1");
        std::env::set_var("DIRI_TELEMETRY", "off");
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        std::env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        std::env::set_var("XDG_CACHE_HOME", home.join(".cache"));
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            std::env::set_var(name, &proxy);
        }
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("no_proxy", "127.0.0.1,localhost");
        for name in [
            "OPENCODE_DISABLE_AUTOUPDATE",
            "OPENCODE_DISABLE_MODELS_FETCH",
            "OPENCODE_DISABLE_DEFAULT_PLUGINS",
            "OPENCODE_DISABLE_LSP_DOWNLOAD",
            "OPENCODE_DISABLE_SHARE",
            "DISABLE_AUTOUPDATER",
            "DISABLE_TELEMETRY",
            "DISABLE_ERROR_REPORTING",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
        ] {
            std::env::set_var(name, "1");
        }
        std::env::set_var("ANTHROPIC_BASE_URL", &proxy);
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-fake-key-for-diri-e2e-0000");
        for name in [
            "OPENAI_API_KEY",
            "CODEX_HOME",
            "OPENCODE_CONFIG",
            "OPENCODE_CONFIG_CONTENT",
        ] {
            std::env::remove_var(name);
        }
    }
    // Wrappers add launch flags the manifests do not pass.
    let bin = temp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let wrap = |name: &str, flag: &str| {
        let Ok(real) = which(name) else {
            return;
        };
        let wrapper = bin.join(name);
        std::fs::write(
            &wrapper,
            format!("#!/bin/sh\nexec '{}' {flag} \"$@\"\n", real.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    // OpenCode 2 defaults to a shared background service; a private server
    // keeps it inside the temporary HOME.
    wrap("opencode", "--standalone");
    if std::env::var_os("DIRI_INLINE").is_some() {
        wrap("codex", "--no-alt-screen");
    }
    // Agents launch through an interactive login shell, whose system profile
    // puts Homebrew first; the rc file runs after it.
    for rc in [".zshrc", ".bashrc"] {
        std::fs::write(
            home.join(rc),
            format!("export PATH=\"{}:$PATH\"\n", bin.display()),
        )
        .unwrap();
    }
    // OpenCode: the fake provider as its model.
    std::fs::create_dir_all(home.join(".config/opencode")).unwrap();
    std::fs::write(
        home.join(".config/opencode/opencode.json"),
        json!({
            "$schema": "https://opencode.ai/config.json",
            "model": "fake/fake-model",
            "small_model": "fake/fake-model",
            "autoupdate": false,
            "share": "disabled",
            "provider": { "fake": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Fake",
                "options": { "baseURL": format!("{proxy}/v1"), "apiKey": "fake" },
                "models": { "fake-model": { "name": "Fake Model" } },
            }}
        })
        .to_string(),
    )
    .unwrap();
    // Codex: a trusted project and a Responses provider.
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(
        home.join(".codex/config.toml"),
        format!(
            "model = \"fake-model\"\nmodel_provider = \"fake\"\ncheck_for_update_on_startup = false\n\
             [model_providers.fake]\nname = \"Fake\"\nbase_url = \"{proxy}/v1\"\nwire_api = \"responses\"\n\
             [projects.\"{}\"]\ntrust_level = \"trusted\"\n",
            project.display()
        ),
    )
    .unwrap();
    // Claude Code: onboarding done, the fake key approved, the project trusted.
    std::fs::write(
        home.join(".claude.json"),
        json!({
            "hasCompletedOnboarding": true,
            "theme": "dark",
            "customApiKeyResponses": { "approved": ["ey-for-diri-e2e-0000"], "rejected": [] },
            "projects": { project.display().to_string(): {
                "hasTrustDialogAccepted": true,
                "hasCompletedProjectOnboarding": true,
            }},
        })
        .to_string(),
    )
    .unwrap();
    Fixture {
        temp,
        home,
        project,
        api,
        registry: None,
    }
}

fn which(name: &str) -> Result<PathBuf, ()> {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|path| path.is_file())
        .ok_or(())
}

/// Claude Code owns API retry. Interrupt only its private Anthropic socket,
/// then check that the actual reply is drawn in the still-live PTY without
/// another submit. Use `--test-threads=1` as for the other real CLI tests.
#[test]
#[ignore = "needs Claude Code and python3 on PATH; includes a 30-second outage"]
fn claude_finishes_after_a_model_connection_cut_without_manual_continue() {
    let mut fixture = fixture();
    let cut = fixture.temp.path().join("api.cut");
    let log = fixture.temp.path().join("api.log");
    let (mut client, registry) = start_with_registry(&mut fixture);
    let record = client
        .call(
            "session.spawn",
            json!({
                "kind": {"claude-code":{}}, "cwd": fixture.project,
                "initialCols": 120, "initialRows": 40
            }),
        )
        .unwrap();
    let id = record["id"].as_str().unwrap();
    let screen = client.wait_screen(id, Duration::from_secs(40), |s| s.contains("❯"));
    assert!(screen.contains("❯"), "Claude startup: {screen}");
    let pid = registry.lock().unwrap().get(id).unwrap().child_pid();
    std::fs::write(&cut, []).unwrap();
    client
        .call(
            "session.send_text",
            json!({
                "sessionID": id, "text": "Show LINES 3", "submit": true
            }),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let events = std::fs::read_to_string(&log).unwrap_or_default();
        if events.contains("CUT model request last_user='Show LINES 3'") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "model never attempted the test request: {}",
            client.screen(id)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let cut_ends = Instant::now() + Duration::from_secs(30);
    let mut showed_retry = false;
    let mut during = String::new();
    while Instant::now() < cut_ends {
        during = client.screen(id);
        let lower = during.to_lowercase();
        showed_retry |= lower.contains("retry") || lower.contains("connection error");
        std::thread::sleep(Duration::from_millis(250));
    }
    std::fs::remove_file(&cut).unwrap();
    let screen = client.wait_screen(id, Duration::from_secs(60), |s| {
        s.contains("Reply line 3 for this turn.")
    });
    assert!(
        screen.contains("Reply line 3 for this turn."),
        "Claude recovery: {screen}"
    );
    assert!(
        showed_retry,
        "no retry or connection error visible during the outage: {during}"
    );
    let registry = registry.lock().unwrap();
    let session = registry.get(id).unwrap();
    assert_eq!(session.child_pid(), pid);
    assert!(!session.view().exited);
}

/// A user quitting Claude keeps the same tab and gets a working login shell.
#[test]
#[ignore = "needs Claude Code and python3 on PATH"]
fn claude_exit_returns_to_a_live_shell_in_the_original_session() {
    let mut fixture = fixture();
    let (mut client, registry) = start_with_registry(&mut fixture);
    let record = client.call("session.spawn", json!({
        "kind": {"claude-code":{}}, "cwd": fixture.project, "initialCols": 100, "initialRows": 30
    })).unwrap();
    let id = record["id"].as_str().unwrap();
    assert!(
        client
            .wait_screen(id, Duration::from_secs(40), |s| s.contains("❯"))
            .contains("❯")
    );
    let pid = registry.lock().unwrap().get(id).unwrap().child_pid();
    client
        .call(
            "session.send_text",
            json!({"sessionID": id, "text": "/exit", "submit": true}),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while registry.lock().unwrap().get(id).unwrap().agent_exit() != Some(0) {
        assert!(
            Instant::now() < deadline,
            "Claude did not exit cleanly: {}",
            client.screen(id)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    {
        let mut registry = registry.lock().unwrap();
        let changes = registry.changed_since(&mut Default::default());
        let (_, published) = changes.iter().find(|(key, _)| key == id).unwrap();
        assert!(!matches!(
            published.status,
            diri_proto::SessionStatus::Exited(_)
        ));
        let session = registry.get(id).unwrap();
        assert_eq!(session.child_pid(), pid);
        session
            .write_input(b"printf 'returned-%s\\n' shell\r")
            .unwrap();
    }
    let screen = client.wait_screen(id, Duration::from_secs(5), |s| s.contains("returned-shell"));
    assert!(screen.contains("returned-shell"), "login shell: {screen}");
}

/// Starts a private Engine in the fixture, which ends its sessions on drop.
fn start_with_registry(fixture: &mut Fixture) -> (Client, Arc<Mutex<Registry>>) {
    start_engine(fixture, None)
}

fn start_engine(
    fixture: &mut Fixture,
    holder: Option<diri_engine::session::HolderConfig>,
) -> (Client, Arc<Mutex<Registry>>) {
    let temp = fixture.temp.path();
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .unwrap();
    let (engine, _) = ManifestEngine::load_dir(&dir).unwrap();
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(engine),
        temp.join("state.json"),
    )));
    fixture.registry = Some(Arc::clone(&registry));
    let shared = Arc::clone(&registry);
    let mut server = ControlServer::new(Arc::clone(&registry), temp.join("daemon.sock"))
        .with_logs_dir(temp.join("logs"));
    if let Some(config) = holder {
        // The daemon creates this before the first Holder opens its log.
        diri_platform::private::create_dir_all(&temp.join("logs")).unwrap();
        server = server.with_holder(config);
    }
    let server = Arc::new(server);
    let listener = server.bind().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let server = Arc::clone(&server);
            std::thread::spawn(move || {
                let _ = server.serve(stream);
            });
        }
    });
    let stream = UnixStream::connect(temp.join("daemon.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .unwrap();
    (
        Client {
            writer: stream.try_clone().unwrap(),
            reader: BufReader::new(stream),
            next: 0,
        },
        shared,
    )
}

/// The screen as text with SGR colors, replayable through a headless
/// terminal of the same size. Soft-wrapped rows wrap again on replay.
fn ansi(rows: &[Vec<GridCell>], scrub: &[(&str, &str)]) -> String {
    fn color(out: &mut String, color: TermColor, background: bool) {
        let base = if background { 40 } else { 30 };
        match color {
            TermColor::Default | TermColor::DefaultInverted => {
                out.push_str(&format!(";{}", base + 9))
            }
            TermColor::Ansi(n) if n < 8 => out.push_str(&format!(";{}", base + n as i32)),
            TermColor::Ansi(n) if n < 16 => out.push_str(&format!(";{}", base + 60 + n as i32 - 8)),
            TermColor::Ansi(n) => out.push_str(&format!(";{};5;{n}", base + 8)),
            TermColor::Rgb(r, g, b) => out.push_str(&format!(";{};2;{r};{g};{b}", base + 8)),
        }
    }
    let mut out = String::new();
    for (index, row) in rows.iter().enumerate() {
        let wraps = row
            .last()
            .is_some_and(|cell| cell.style.contains(TermStyle::SOFT_WRAP));
        let len = if wraps {
            row.len()
        } else {
            row.iter()
                .rposition(|cell| {
                    !(matches!(cell.scalar, 0 | 32)
                        && matches!(cell.bg, TermColor::Default | TermColor::DefaultInverted)
                        && !cell.style.contains(TermStyle::INVERSE))
                })
                .map_or(0, |col| col + 1)
        };
        let mut text = String::new();
        let mut last = None;
        for cell in &row[..len] {
            if cell.style.contains(TermStyle::WIDE_SPACER) {
                continue;
            }
            let visual = cell.style.bits() & 0xff;
            let key = (cell.fg, cell.bg, visual);
            if last != Some(key) {
                text.push_str("\x1b[0");
                for (bit, code) in [(0, 1), (1, 4), (3, 7), (5, 2), (6, 3), (7, 9)] {
                    if visual & (1 << bit) != 0 {
                        text.push_str(&format!(";{code}"));
                    }
                }
                color(&mut text, cell.fg, false);
                color(&mut text, cell.bg, true);
                text.push('m');
                last = Some(key);
            }
            text.push(
                char::from_u32(cell.scalar)
                    .filter(|c| *c != '\0')
                    .unwrap_or(' '),
            );
        }
        for (from, to) in scrub {
            text = text.replace(from, to);
        }
        out.push_str(&text);
        out.push_str("\x1b[0m");
        if !wraps && index + 1 < rows.len() {
            out.push_str("\r\n");
        }
    }
    out
}

/// Records the screens the detector's unit tests replay. Opt-in; writes to
/// `DIRI_DUMP_DIR`.
#[test]
#[ignore = "fixture recorder: needs real Agent CLIs on PATH"]
fn record_message_fixtures() {
    let agents = std::env::var("DIRI_AGENTS").unwrap_or_else(|_| "codex".into());
    let out = PathBuf::from(std::env::var("DIRI_DUMP_DIR").unwrap());
    let mut fixture = fixture();
    let (mut client, registry) = start_with_registry(&mut fixture);
    let project = fixture.project.display().to_string();
    let temp = fixture
        .temp
        .path()
        .canonicalize()
        .unwrap()
        .display()
        .to_string();
    let private = format!("/private{temp}");
    let scrub = [
        (project.as_str(), "~/project"),
        (private.as_str(), "~"),
        (temp.as_str(), "~"),
    ];
    let inline = if std::env::var_os("DIRI_INLINE").is_some() {
        "-inline"
    } else {
        ""
    };
    for agent in agents.split(',') {
        let record = client
            .call(
                "session.spawn",
                json!({ "kind": { agent: {} }, "cwd": fixture.project, "initialCols": 100, "initialRows": 30 }),
            )
            .unwrap();
        let id = record["id"].as_str().unwrap().to_string();
        client.wait_screen(&id, Duration::from_secs(30), |screen| {
            screen.trim().len() > 40
        });
        std::thread::sleep(Duration::from_secs(4));
        for prompt in [
            "First question about SEARCH, LINES 30",
            "RUNCMD please run the marker command",
            "Third question, a long one that should wrap across more than one terminal row because it keeps going and going with more words, LINES 3",
            "1. Fix the search, then 2. run the checks FINAL LINES 12",
        ] {
            client
                .call(
                    "session.send_text",
                    json!({ "sessionID": id, "text": prompt, "submit": true }),
                )
                .unwrap();
            std::thread::sleep(Duration::from_secs(6));
        }
        client
            .call(
                "session.send_text",
                json!({ "sessionID": id, "text": "unsent draft", "submit": false }),
            )
            .unwrap();
        std::thread::sleep(Duration::from_secs(1));
        let write = |client: &mut Client, name: &str| {
            let rows = client.history(&id);
            std::fs::write(
                out.join(format!("{agent}{inline}-{name}.ansi")),
                ansi(&rows, &scrub),
            )
            .unwrap();
            std::fs::write(
                out.join(format!("{agent}{inline}-{name}.txt")),
                describe(&rows),
            )
            .unwrap();
        };
        write(&mut client, "live");
        if inline.is_empty() {
            let mut ticks = 0;
            for (stop, name) in [(2, "up2"), (7, "up7"), (16, "up16"), (300, "top")] {
                while ticks < stop {
                    {
                        let registry = registry.lock().unwrap();
                        registry.get(&id).unwrap().scroll(true, 1, 50, 8).unwrap();
                    }
                    std::thread::sleep(Duration::from_millis(40));
                    ticks += 1;
                }
                std::thread::sleep(Duration::from_millis(300));
                write(&mut client, name);
            }
        }
        let _ = client.call("session.kill", json!({ "sessionID": id }));
    }
}

/// Prints, per Agent, how many transcript lines wheel notches move and when
/// the redraws land: bursts of two notches at shrinking spacing (Claude Code
/// speeds up bursts closer than about 50 ms), then single bursts of growing
/// size, each with its redraws timed from the moment it was sent.
#[test]
#[ignore = "measurement: needs real Agent CLIs on PATH"]
fn measure_wheel_response() {
    let agents = std::env::var("DIRI_AGENTS").unwrap_or_else(|_| "claude-code".into());
    let mut fixture = fixture();
    let (mut client, registry) = start_with_registry(&mut fixture);
    for agent in agents.split(',') {
        let record = client
            .call(
                "session.spawn",
                json!({ "kind": { agent: {} }, "cwd": fixture.project, "initialCols": 100, "initialRows": 40 }),
            )
            .unwrap();
        let id = record["id"].as_str().unwrap().to_string();
        client.wait_screen(&id, Duration::from_secs(30), |screen| {
            screen.trim().len() > 40
        });
        std::thread::sleep(Duration::from_secs(4));
        for prompt in ["Alpha message LINES 120", "Bravo message LINES 120"] {
            client
                .call(
                    "session.send_text",
                    json!({ "sessionID": id, "text": prompt, "submit": true }),
                )
                .unwrap();
            std::thread::sleep(Duration::from_secs(6));
        }
        let wheel = |up: bool, ticks: usize| {
            let registry = registry.lock().unwrap();
            registry
                .get(&id)
                .unwrap()
                .scroll(up, ticks, 50, 10)
                .unwrap();
        };
        // Reply lines are a blank row apart: where the transcript stands.
        let position = |client: &mut Client| -> i64 {
            client
                .screen(&id)
                .lines()
                .enumerate()
                .find_map(|(row, line)| {
                    let rest = line.trim().strip_prefix("Reply line ")?;
                    let n: i64 = rest.split(' ').next()?.parse().ok()?;
                    Some(n * 2 - row as i64)
                })
                .unwrap_or(-1)
        };
        for spacing in [10u64, 25, 50, 100, 200] {
            let mut last = position(&mut client);
            let mut moves = Vec::new();
            for _ in 0..6 {
                wheel(true, 2);
                std::thread::sleep(Duration::from_millis(spacing));
                let now = position(&mut client);
                moves.push(last - now);
                last = now;
            }
            println!("{agent}: two notches every {spacing} ms moved {moves:?} lines");
            wheel(false, 60);
            std::thread::sleep(Duration::from_millis(600));
        }
        for (up, ticks) in [
            (true, 1),
            (true, 2),
            (true, 4),
            (true, 6),
            (true, 8),
            (false, 4),
            (false, 8),
        ] {
            let start = position(&mut client);
            let sent = Instant::now();
            wheel(up, ticks);
            let mut redraws = Vec::new();
            let mut last = start;
            while sent.elapsed() < Duration::from_millis(600) {
                let now = position(&mut client);
                if now != last {
                    redraws.push((sent.elapsed().as_millis(), (start - now).abs()));
                    last = now;
                }
                std::thread::sleep(Duration::from_millis(3));
            }
            let way = if up { "up" } else { "down" };
            println!("{agent}: {ticks} notches {way}: (ms, lines) {redraws:?}");
            std::thread::sleep(Duration::from_millis(300));
        }
        let _ = client.call("session.kill", json!({ "sessionID": id }));
    }
}

/// Prints the lines one burst of `n` notches moves after a pause, each way,
/// and when its last redraw lands: the curve a jump plans its steps with.
#[test]
#[ignore = "measurement: needs real Agent CLIs on PATH"]
fn measure_burst_curve() {
    let agents = std::env::var("DIRI_AGENTS").unwrap_or_else(|_| "claude-code".into());
    let mut fixture = fixture();
    let (mut client, registry) = start_with_registry(&mut fixture);
    for agent in agents.split(',') {
        let record = client
            .call(
                "session.spawn",
                json!({ "kind": { agent: {} }, "cwd": fixture.project, "initialCols": 100, "initialRows": 40 }),
            )
            .unwrap();
        let id = record["id"].as_str().unwrap().to_string();
        client.wait_screen(&id, Duration::from_secs(30), |screen| {
            screen.trim().len() > 40
        });
        std::thread::sleep(Duration::from_secs(4));
        for prompt in ["Alpha message LINES 200", "Bravo message LINES 200"] {
            client
                .call(
                    "session.send_text",
                    json!({ "sessionID": id, "text": prompt, "submit": true }),
                )
                .unwrap();
            std::thread::sleep(Duration::from_secs(8));
        }
        let wheel = |up: bool, ticks: usize| {
            let registry = registry.lock().unwrap();
            registry
                .get(&id)
                .unwrap()
                .scroll(up, ticks, 50, 10)
                .unwrap();
        };
        let position = |client: &mut Client| -> i64 {
            client
                .screen(&id)
                .lines()
                .enumerate()
                .find_map(|(row, line)| {
                    let rest = line.trim().strip_prefix("Reply line ")?;
                    let n: i64 = rest.split(' ').next()?.parse().ok()?;
                    Some(n * 2 - row as i64)
                })
                .unwrap_or(-1)
        };
        // Start well inside the transcript.
        wheel(true, 30);
        std::thread::sleep(Duration::from_millis(400));
        wheel(true, 30);
        std::thread::sleep(Duration::from_millis(400));
        let mut last_up = true;
        for n in [1usize, 1, 2, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24] {
            for up in [true, false, false, true] {
                let start = position(&mut client);
                let sent = Instant::now();
                wheel(up, n);
                let mut last_change = None;
                let mut last = start;
                while sent.elapsed() < Duration::from_millis(250) {
                    let now = position(&mut client);
                    if now != last {
                        last_change = Some(sent.elapsed().as_millis());
                        last = now;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                let reversal = if up != last_up { " (turned)" } else { "" };
                last_up = up;
                println!(
                    "{agent} n={n:2} {}{reversal}: {} lines, settled at {:?} ms",
                    if up { "up  " } else { "down" },
                    (start - last).abs(),
                    last_change
                );
                std::thread::sleep(Duration::from_millis(150));
            }
        }
        let _ = client.call("session.kill", json!({ "sessionID": id }));
    }
}

/// Prints how far Claude Code's PageUp/PageDown move its transcript, how
/// soon they are drawn, and that they leave a draft in the composer alone,
/// also while the conversation still fits on screen.
#[test]
#[ignore = "measurement: needs real Agent CLIs on PATH"]
fn measure_claude_page_keys() {
    let mut fixture = fixture();
    let (mut client, registry) = start_with_registry(&mut fixture);
    let record = client
        .call(
            "session.spawn",
            json!({ "kind": { "claude-code": {} }, "cwd": fixture.project, "initialCols": 100, "initialRows": 40 }),
        )
        .unwrap();
    let id = record["id"].as_str().unwrap().to_string();
    client.wait_screen(&id, Duration::from_secs(30), |screen| {
        screen.trim().len() > 40
    });
    std::thread::sleep(Duration::from_secs(4));
    let key = |bytes: &[u8]| {
        let registry = registry.lock().unwrap();
        registry.get(&id).unwrap().write_input(bytes).unwrap();
    };
    // The composer: the rows between the last two full-width rules.
    let draft = |client: &mut Client| {
        let screen = client.screen(&id);
        let lines: Vec<&str> = screen.lines().collect();
        let rules: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.chars().filter(|ch| *ch == '─').count() > 40)
            .map(|(row, _)| row)
            .collect();
        match rules.as_slice() {
            [.., upper, lower] => lines[upper + 1..*lower].join(" / "),
            _ => String::from("(no composer)"),
        }
    };
    // A short conversation that fits, with a draft.
    client
        .call(
            "session.send_text",
            json!({ "sessionID": id, "text": "Short one LINES 2", "submit": true }),
        )
        .unwrap();
    std::thread::sleep(Duration::from_secs(5));
    client
        .call(
            "session.send_text",
            json!({ "sessionID": id, "text": "keep this draft", "submit": false }),
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let before = draft(&mut client);
    let screen = client.screen(&id);
    for bytes in [&b"\x1b[5~"[..], b"\x1b[6~", b"\x1b[5~"] {
        key(bytes);
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("fits: draft before {before:?}");
    println!("fits: draft after  {:?}", draft(&mut client));
    let after = client.screen(&id);
    for (row, (a, b)) in screen.lines().zip(after.lines()).enumerate() {
        if a != b {
            println!("fits: row {row} changed {a:?} -> {b:?}");
        }
    }
    // Clear the draft, then a long conversation.
    key(b"\x15");
    std::thread::sleep(Duration::from_millis(300));
    for prompt in ["Alpha message LINES 200", "Bravo message LINES 200"] {
        client
            .call(
                "session.send_text",
                json!({ "sessionID": id, "text": prompt, "submit": true }),
            )
            .unwrap();
        std::thread::sleep(Duration::from_secs(8));
    }
    client
        .call(
            "session.send_text",
            json!({ "sessionID": id, "text": "keep this draft", "submit": false }),
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let position = |client: &mut Client| -> i64 {
        client
            .screen(&id)
            .lines()
            .enumerate()
            .find_map(|(row, line)| {
                let rest = line.trim().strip_prefix("Reply line ")?;
                let n: i64 = rest.split(' ').next()?.parse().ok()?;
                Some(n * 2 - row as i64)
            })
            .unwrap_or(-1)
    };
    for (name, bytes, gap) in [
        ("pageup", &b"\x1b[5~"[..], 150u64),
        ("pageup", b"\x1b[5~", 150),
        ("pagedown", b"\x1b[6~", 150),
        ("pageup", b"\x1b[5~", 5),
        ("pageup", b"\x1b[5~", 5),
        ("pageup", b"\x1b[5~", 5),
        ("pagedown", b"\x1b[6~", 5),
    ] {
        let start = position(&mut client);
        let sent = Instant::now();
        key(bytes);
        let mut last = start;
        let mut landed = None;
        while sent.elapsed() < Duration::from_millis(120) {
            let now = position(&mut client);
            if now != last {
                landed = Some(sent.elapsed().as_millis());
                last = now;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        println!(
            "{name}: {} lines, drawn by {landed:?} ms",
            (start - last).abs()
        );
        std::thread::sleep(Duration::from_millis(gap));
    }
    println!("long: draft after {:?}", draft(&mut client));
    // Cursor keys move within a draft and must not reach it from a jump.
    println!("long: draft before keys {:?}", draft(&mut client));
    let _ = client.call("session.kill", json!({ "sessionID": id }));
}

/// C2/C3 with a real composer and a replayed in-flight model request. As in
/// the GUI profile, paste and Ctrl+M are separate terminal input frames. Each
/// probe segment is cleared without submitting. Optionally point DIRI_DELIVERY_TASK_DIR
/// at the local C2 task folder to replay its real prompt and follow-ups.
/// Run only with --test-threads=1.
#[test]
#[ignore = "needs Claude Code and python3 on PATH; includes 150 seconds of live input probes"]
fn claude_queues_followups_while_a_model_request_is_in_flight() {
    use composer::Composer;

    let mut fixture = fixture();
    let (prompt, followups) = match std::env::var_os("DIRI_DELIVERY_TASK_DIR") {
        Some(dir) => {
            let load = |name: &str| {
                std::fs::read_to_string(Path::new(&dir).join(name))
                    .unwrap()
                    .trim()
                    .to_owned()
            };
            (
                load("prompt.md"),
                ["followups/F1.md", "followups/F2.md", "followups/F3.md"].map(load),
            )
        }
        None => (
            "Respond when ready.".into(),
            ["First follow-up", "Second follow-up", "Third follow-up"].map(str::to_owned),
        ),
    };
    std::fs::write(
        fixture.temp.path().join("api.messages"),
        serde_json::to_vec(&json!({"prompt":prompt,"followups":followups})).unwrap(),
    )
    .unwrap();
    let hold = fixture.temp.path().join("api.hold");
    let capture = fixture.temp.path().join("api.delivery");
    std::fs::write(&hold, []).unwrap();
    std::fs::write(&capture, []).unwrap();
    let holder = diri_engine::session::HolderConfig {
        holders_dir: fixture.temp.path().join("holders"),
        executable: env!("CARGO_BIN_EXE_diri-holder").into(),
    };
    let (mut client, _) = start_engine(&mut fixture, Some(holder));
    let record = client
        .call(
            "session.spawn",
            json!({
                "kind": {"claude-code": {}},
                "cwd": fixture.project,
                "initialCols": 140,
                "initialRows": 45,
            }),
        )
        .unwrap();
    let id = record["id"].as_str().unwrap();
    let data = UnixStream::connect(fixture.temp.path().join("daemon.sock")).unwrap();
    let mut composer = Composer::attach(data, id);
    assert!(
        composer.wait(Duration::from_secs(40), |rows| rows
            .iter()
            .any(|r| r.contains('❯'))),
        "Claude startup: {:?}",
        composer.rows
    );
    let start = Instant::now();
    composer.paste(&prompt);
    std::thread::sleep(Duration::from_millis(500));
    composer.key("m", true);
    while std::fs::metadata(&capture).unwrap().len() == 0 {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "initial turn never reached the replay server: {}",
            client.screen(id)
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut probes = 0;
    let mut missed = Vec::new();
    let mut max_echo = Duration::ZERO;
    for (followup, at) in followups.iter().zip([30, 90, 150]) {
        let deadline = start + Duration::from_secs(at);
        let mut typed = String::new();
        let mut letters = ["a", "d", "e", "m", "a", "r", "k", "q", "z"]
            .into_iter()
            .cycle();
        while Instant::now() + Duration::from_millis(3500) < deadline {
            // ADEMark's native lag instrument accumulates "ademarkqz"
            // through each segment and clears it before the next follow-up.
            let ch = letters.next().unwrap();
            typed.push_str(ch);
            let posted = Instant::now();
            composer.key(ch, false);
            let has_echo = |rows: &[String]| {
                rows.iter().any(|row| {
                    row.trim()
                        .strip_prefix('❯')
                        .is_some_and(|draft| draft.trim() == typed)
                })
            };
            let echoed = composer.wait(Duration::from_secs(2), has_echo);
            let elapsed = posted.elapsed();
            probes += 1;
            if echoed && elapsed <= Duration::from_secs(2) {
                max_echo = max_echo.max(elapsed);
            } else {
                missed.push(posted.duration_since(start));
                eprintln!(
                    "no echo for probe {probes} at {:?} (sampled after {elapsed:?})",
                    posted.duration_since(start)
                );
                // Match later probes against the actual composer after a
                // missing key, so one miss does not mask all subsequent echoes.
                if let Some(draft) = composer
                    .rows
                    .iter()
                    .rev()
                    .find_map(|r| r.trim().strip_prefix('❯'))
                {
                    typed = draft
                        .trim()
                        .chars()
                        .take_while(|c| "ademarkqz".contains(*c))
                        .collect();
                }
            }
            std::thread::sleep(Duration::from_secs(2).saturating_sub(elapsed));
        }
        composer.key("u", true);
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        let sent = Instant::now();
        composer.paste(followup);
        std::thread::sleep(Duration::from_millis(500));
        composer.key("m", true);
        // The last rows contain the active composer and its footer. Count
        // only an appearance in the thread above those rows as acceptance.
        let prefix: String = followup.chars().take(24).collect();
        let in_thread =
            |rows: &[String]| rows.iter().rev().skip(5).any(|row| row.contains(&prefix));
        assert!(
            composer.wait(Duration::from_secs(5), in_thread),
            "follow-up at {at}s did not appear in the thread: {:?}",
            composer.rows
        );
        eprintln!(
            "follow-up at {at}s appeared above the composer after {:?}",
            sent.elapsed()
        );
        std::thread::sleep(Duration::from_millis(1500).saturating_sub(sent.elapsed()));
    }
    std::fs::remove_file(hold).unwrap();
    let waited = Instant::now();
    loop {
        let counts: Vec<[u64; 3]> = std::fs::read_to_string(&capture)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            counts.iter().flatten().all(|&n| n <= 1),
            "duplicate in model history: {counts:?}"
        );
        if counts.contains(&[1, 1, 1]) {
            eprintln!("model request marker counts in capture order: {counts:?}");
            break;
        }
        assert!(
            waited.elapsed() < Duration::from_secs(60),
            "follow-ups missing from capture: {counts:?}; {}",
            client.screen(id)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!(
        "3/3 follow-ups in model user history exactly once, {probes} probes, missed {missed:?}, maximum observed echo {max_echo:?}"
    );
    assert!(
        missed.is_empty(),
        "composer probes missed the two-second deadline"
    );
}
