//! Message jumps driven through a terminal pane, a real Engine and a PTY.
//!
//! The scripted inline Agent always runs. The real Agents are opt-in: set
//! `DIRI_REAL_AGENTS=1` with `claude`, `codex` and `opencode` on PATH and
//! `python3` for the scripted model API in
//! `diri-engine/tests/fixtures/fake_agent_api.py`. Each runs with a private
//! HOME in a temporary directory, removed on drop, and the API server is also
//! their HTTP(S) proxy, refusing every other host: no request leaves the
//! machine and no account is used.
//!
//! ```sh
//! DIRI_REAL_AGENTS=1 cargo test -p diri-app --bin diri messages_tests -- --ignored --test-threads=1
//! ```
use super::*;
use diri_proto::grid::GridCell;
use gpui::{Entity, TestAppContext, VisualTestContext};

fn row_text(cells: &[GridCell]) -> String {
    cells
        .iter()
        .filter(|cell| cell.scalar != 0)
        .map(|cell| char::from_u32(cell.scalar).unwrap_or(' '))
        .collect::<String>()
        .trim_end()
        .to_owned()
}

struct Harness<'a> {
    pane: Entity<TerminalPane>,
    cx: &'a mut VisualTestContext,
    id: SessionId,
}

impl Harness<'_> {
    fn open<'cx>(
        cx: &'cx mut TestAppContext,
        fixture: &crate::workspace_fixture::LiveWorkspace,
    ) -> Harness<'cx> {
        // A real Engine wakes this view from Tokio, as it does in the app.
        cx.background_executor.allow_parking();
        let runtime = Arc::clone(&fixture.services.store);
        let tokio = Arc::clone(&fixture.services.tokio);
        let id = SessionId::new("build");
        let selected = id.clone();
        let (pane, cx) = cx.add_window_view(move |window, cx| {
            crate::commands::bind_keys(cx, &Default::default());
            TerminalPane::new_fixed(runtime, tokio, selected, window, cx)
        });
        pane.update_in(cx, |pane, window, cx| {
            window.activate_window();
            pane.focus(window, cx);
        });
        let mut harness = Harness { pane, cx, id };
        harness.wait("a live attachment", Duration::from_secs(10), |pane, id| {
            pane.residents
                .get(id)
                .is_some_and(|resident| resident.attachment_state == AttachmentState::Live)
        });
        harness
    }

    /// Runs the view, its timers and the Engine until `done`.
    fn wait(
        &mut self,
        what: &str,
        within: Duration,
        done: impl Fn(&TerminalPane, &SessionId) -> bool,
    ) {
        let deadline = Instant::now() + within;
        loop {
            self.cx.run_until_parked();
            if self.pane.read_with(self.cx, |pane, _| done(pane, &self.id)) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; screen:\n{}",
                self.screen().join("\n")
            );
            self.cx.executor().advance_clock(Duration::from_millis(4));
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn settle(&mut self, time: Duration) {
        let until = Instant::now() + time;
        while Instant::now() < until {
            self.cx.run_until_parked();
            self.cx.executor().advance_clock(Duration::from_millis(4));
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn screen(&mut self) -> Vec<String> {
        let id = self.id.clone();
        self.pane.read_with(self.cx, |pane, _| {
            let buffer = pane.residents[&id].element.buffer();
            let buffer = buffer.read().unwrap();
            (0..usize::from(buffer.rows))
                .map(|row| row_text(buffer.row(row).unwrap_or_default()))
                .collect()
        })
    }

    /// The rows the message band covers, as the view shows them.
    fn flashed(&mut self) -> Option<Vec<String>> {
        let id = self.id.clone();
        self.pane.read_with(self.cx, |pane, _| {
            let element = &pane.residents[&id].element;
            let rows = element.message_flash_rows()?;
            let viewport = element.viewport();
            let buffer = element.buffer();
            let buffer = buffer.read().unwrap();
            let first = viewport.window_row_for_absolute(rows.start)?;
            Some(
                (0..rows.end - rows.start)
                    .map(|offset| {
                        let row = usize::try_from(first + offset).unwrap_or(usize::MAX);
                        if row < usize::from(buffer.rows) {
                            row_text(&viewport.window_row(&buffer, row))
                        } else {
                            String::new()
                        }
                    })
                    .collect(),
            )
        })
    }

    /// Presses a jump shortcut and returns the marked message's text, or the
    /// notice shown instead.
    fn jump(&mut self, keys: &str) -> Result<String, String> {
        let before = self
            .pane
            .read_with(self.cx, |pane, _| pane.qol.feedback_generation);
        let id = self.id.clone();
        self.pane.update(self.cx, |pane, _| {
            pane.residents[&id].element.clear_message_flash();
        });
        self.cx.simulate_keystrokes(keys);
        self.wait("the jump", Duration::from_secs(15), |pane, _| {
            pane.qol.message_jump.is_none() && !pane.qol.busy
        });
        let id = self.id.clone();
        assert!(
            !self
                .pane
                .read_with(self.cx, |pane, _| pane.residents[&id].element.frame_held()),
            "a finished jump shows the live screen again"
        );
        // History rows arrive with the scrollback fetch that follows.
        for _ in 0..200 {
            if let Some(rows) = self.flashed()
                && rows.iter().any(|row| !row.trim().is_empty())
            {
                return Ok(rows.join("\n"));
            }
            let notice = self.pane.read_with(self.cx, |pane, _| {
                (pane.qol.feedback_generation != before)
                    .then(|| pane.qol.feedback.clone())
                    .flatten()
            });
            if let Some(notice) = notice {
                return Err(notice);
            }
            self.settle(Duration::from_millis(10));
        }
        Err(format!(
            "no mark and no notice; screen:\n{}",
            self.screen().join("\n")
        ))
    }
}

/// An inline Agent prints its transcript into the terminal history, where a
/// jump reads retained rows and scrolls Diri's own view.
#[gpui::test]
fn inline_agent_jumps_through_history_and_the_live_grid(cx: &mut TestAppContext) {
    let fixture = crate::workspace_fixture::LiveWorkspace::start_with_agent_script(
        "stty raw -echo; printf '› First prompt\\r\\n'; i=0; while [ $i -lt 1500 ]; do printf '• response line %s\\r\\n' $i; i=$((i + 1)); done; printf '\\r\\n› Second prompt\\r\\n● second reply\\r\\n\\r\\n› unfinished draft'; printf ready > ready; cat > received",
        ProtoAgentKind::CODEX,
    );
    let received = fixture.directory.path().join("build/received");
    let mut harness = Harness::open(cx, &fixture);
    harness.settle(Duration::from_millis(200));
    // The latest message is on screen; Previous goes before it, deep into
    // history, past the Find capture's recent tail.
    assert_eq!(
        harness.jump("cmd-shift-up").as_deref(),
        Ok("› First prompt")
    );
    let (offset, mark) = harness.pane.read_with(harness.cx, |pane, _| {
        (
            pane.residents[&SessionId::new("build")]
                .element
                .view_offset(),
            pane.qol.message_mark.as_ref().map(|mark| mark.row),
        )
    });
    assert!(offset > 1000, "the view is reading history ({offset})");
    assert_eq!(mark, Some(0));
    assert_eq!(
        harness.jump("cmd-shift-up"),
        Err("No earlier messages".into())
    );
    assert_eq!(
        harness.jump("cmd-shift-down").as_deref(),
        Ok("› Second prompt")
    );
    let offset = harness.pane.read_with(harness.cx, |pane, _| {
        pane.residents[&SessionId::new("build")]
            .element
            .view_offset()
    });
    assert_eq!(offset, 0, "a message on the live grid is marked at live");
    // The draft in the composer is not a stop.
    assert_eq!(
        harness.jump("cmd-shift-down"),
        Err("No later messages".into())
    );
    assert_eq!(
        harness.jump("cmd-shift-up").as_deref(),
        Ok("› First prompt")
    );
    // Ordinary wheel scrolling still moves the view after a jump.
    harness.pane.update_in(harness.cx, |pane, _, _| {
        let resident = &pane.residents[&SessionId::new("build")];
        assert!(resident.element.view_offset() > 0);
        resident.element.scroll_to_live(40);
    });
    assert!(
        std::fs::read(&received).unwrap_or_default().is_empty(),
        "jumping sent bytes to the PTY"
    );
    fixture.verify_process_identity();
}

/// A real Agent under a private HOME, talking to the scripted model API.
struct RealAgent {
    _home: tempfile::TempDir,
    api: std::process::Child,
    script: String,
    kind: ProtoAgentKind,
}

impl Drop for RealAgent {
    fn drop(&mut self) {
        let _ = self.api.kill();
        let _ = self.api.wait();
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn find_on_path(name: &str) -> Option<std::path::PathBuf> {
    std::env::var("PATH")
        .ok()?
        .split(':')
        .map(|dir| std::path::Path::new(dir).join(name))
        .find(|path| path.is_file())
}

impl RealAgent {
    fn prepare(agent: &str, args: &str) -> Option<Self> {
        if std::env::var_os("DIRI_REAL_AGENTS").is_none() {
            eprintln!("DIRI_REAL_AGENTS unset; skipping {agent}");
            return None;
        }
        let binary = match agent {
            "claude-code" => "claude",
            other => other,
        };
        let executable = find_on_path(binary).unwrap_or_else(|| panic!("{binary} is not on PATH"));
        let home = tempfile::tempdir().unwrap();
        let root = home.path().canonicalize().unwrap();
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        for dir in [
            ".config/opencode",
            ".local/share",
            ".local/state",
            ".cache",
            ".codex",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let api = std::process::Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../diri-engine/tests/fixtures/fake_agent_api.py"
            ))
            .arg(port.to_string())
            .arg(root.join("api.log"))
            .spawn()
            .expect("python3 for the scripted model API");
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "the model API never listened");
            std::thread::sleep(Duration::from_millis(50));
        }
        let proxy = format!("http://127.0.0.1:{port}");
        std::fs::write(
            root.join(".config/opencode/opencode.json"),
            serde_json::json!({
                "model": "fake/fake-model", "small_model": "fake/fake-model",
                "autoupdate": false, "share": "disabled",
                "provider": { "fake": {
                    "npm": "@ai-sdk/openai-compatible", "name": "Fake",
                    "options": { "baseURL": format!("{proxy}/v1"), "apiKey": "fake" },
                    "models": { "fake-model": { "name": "Fake Model" } },
                }}
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            root.join(".codex/config.toml"),
            format!(
                "model = \"fake-model\"\nmodel_provider = \"fake\"\ncheck_for_update_on_startup = false\n\
                 [model_providers.fake]\nname = \"Fake\"\nbase_url = \"{proxy}/v1\"\nwire_api = \"responses\"\n\
                 [projects.\"{}\"]\ntrust_level = \"trusted\"\n",
                project.display()
            ),
        )
        .unwrap();
        std::fs::write(
            root.join(".claude.json"),
            serde_json::json!({
                "hasCompletedOnboarding": true,
                "theme": "dark",
                "customApiKeyResponses": { "approved": ["ey-for-diri-e2e-0000"], "rejected": [] },
                "projects": { project.display().to_string(): {
                    "hasTrustDialogAccepted": true, "hasCompletedProjectOnboarding": true,
                }},
            })
            .to_string(),
        )
        .unwrap();
        let mut environment = vec![
            ("HOME".to_owned(), root.display().to_string()),
            ("PATH".to_owned(), std::env::var("PATH").unwrap_or_default()),
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("COLORTERM".to_owned(), "truecolor".to_owned()),
            ("LANG".to_owned(), "en_US.UTF-8".to_owned()),
            (
                "XDG_CONFIG_HOME".to_owned(),
                root.join(".config").display().to_string(),
            ),
            (
                "XDG_DATA_HOME".to_owned(),
                root.join(".local/share").display().to_string(),
            ),
            (
                "XDG_STATE_HOME".to_owned(),
                root.join(".local/state").display().to_string(),
            ),
            (
                "XDG_CACHE_HOME".to_owned(),
                root.join(".cache").display().to_string(),
            ),
            ("NO_PROXY".to_owned(), "127.0.0.1,localhost".to_owned()),
            ("no_proxy".to_owned(), "127.0.0.1,localhost".to_owned()),
            ("ANTHROPIC_BASE_URL".to_owned(), proxy.clone()),
            (
                "ANTHROPIC_API_KEY".to_owned(),
                "sk-ant-fake-key-for-diri-e2e-0000".to_owned(),
            ),
            // As the claude-code manifest launches it in Diri.
            ("CLAUDE_CODE_NO_FLICKER".to_owned(), "1".to_owned()),
        ];
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            environment.push((name.to_owned(), proxy.clone()));
        }
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
            environment.push((name.to_owned(), "1".to_owned()));
        }
        let exports: String = environment
            .iter()
            .map(|(name, value)| format!("export {name}={}; ", shell_quote(value)))
            .collect();
        // The second pane of the fixture idles instead of starting another Agent.
        let script = format!(
            "{exports}printf ready > ready; case \"$PWD\" in */review) exec cat ;; esac; cd {}; exec {} {args}",
            shell_quote(&project.display().to_string()),
            shell_quote(&executable.display().to_string()),
        );
        let kind = match agent {
            "claude-code" => ProtoAgentKind::CLAUDE_CODE,
            "codex" => ProtoAgentKind::CODEX,
            _ => ProtoAgentKind::new("opencode"),
        };
        Some(Self {
            _home: home,
            api,
            script,
            kind,
        })
    }
}

/// Sends four messages with long replies, then walks back to the first and
/// forward to the latest, checking the view stays usable after each jump.
fn walk_real_agent(cx: &mut TestAppContext, agent: &str, args: &str, ready: &str) {
    let Some(real) = RealAgent::prepare(agent, args) else {
        return;
    };
    let fixture = crate::workspace_fixture::LiveWorkspace::start_with_agent_script(
        &real.script,
        real.kind.clone(),
    );
    let client = Arc::clone(fixture.services.store.client());
    let tokio = Arc::clone(&fixture.services.tokio);
    let mut harness = Harness::open(cx, &fixture);
    harness.wait(
        &format!("{agent} to start"),
        Duration::from_secs(60),
        |pane, id| {
            let buffer = pane.residents[id].element.buffer();
            let buffer = buffer.read().unwrap();
            (0..usize::from(buffer.rows))
                .any(|row| row_text(buffer.row(row).unwrap_or_default()).contains(ready))
        },
    );
    harness.settle(Duration::from_secs(3));
    let prompts = [
        ("Alpha question about SEARCH, LINES 40", 40),
        ("Bravo question about ROUTING, LINES 35", 35),
        (
            "Charlie question, which is long enough to wrap across two rows of this terminal when the agent draws it, LINES 30",
            30,
        ),
        ("Delta question FINAL LINES 45", 45),
    ];
    for (prompt, lines) in prompts {
        let id = SessionId::new("build");
        tokio
            .block_on(client.send_text(&id, prompt.to_owned(), true))
            .expect("send the prompt");
        let last = format!("Reply line {lines} for this turn.");
        harness.wait(
            &format!("the reply to {prompt}"),
            Duration::from_secs(60),
            |pane, id| {
                let buffer = pane.residents[id].element.buffer();
                let buffer = buffer.read().unwrap();
                (0..usize::from(buffer.rows))
                    .any(|row| row_text(buffer.row(row).unwrap_or_default()).contains(&last))
            },
        );
        harness.settle(Duration::from_secs(2));
    }
    let started = Instant::now();
    for (prompt, _) in prompts.iter().rev() {
        let marked = harness
            .jump("cmd-shift-up")
            .unwrap_or_else(|notice| panic!("{agent}: {notice}"));
        let words: String = prompt.chars().take(20).collect();
        if !marked.contains(&words) {
            let id = harness.id.clone();
            let flash = harness.pane.read_with(harness.cx, |pane, _| {
                (
                    pane.residents[&id].element.message_flash_rows(),
                    pane.qol.message_mark.as_ref().map(|mark| mark.row),
                    pane.residents[&id].element.view_offset(),
                )
            });
            panic!(
                "{agent}: expected {prompt:?}, marked {marked:?}; flash {flash:?}; screen:\n{}",
                harness
                    .screen()
                    .iter()
                    .enumerate()
                    .map(|(i, r)| format!("{i:3} {r}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }
    assert_eq!(
        harness.jump("cmd-shift-up"),
        Err("No earlier messages".into()),
        "{agent}"
    );
    for (prompt, _) in prompts.iter().skip(1) {
        let marked = harness
            .jump("cmd-shift-down")
            .unwrap_or_else(|notice| panic!("{agent}: {notice}"));
        let words: String = prompt.chars().take(20).collect();
        assert!(
            marked.contains(&words),
            "{agent}: expected {prompt:?}, marked {marked:?}"
        );
    }
    // Past the latest message the view returns to the latest output.
    let end = harness.jump("cmd-shift-down");
    assert!(
        matches!(
            end.as_ref().err().map(String::as_str),
            Some("Back to the latest output" | "No later messages")
        ),
        "{agent}: {end:?}"
    );
    assert_eq!(
        harness.jump("cmd-shift-down"),
        Err("No later messages".into()),
        "{agent}"
    );
    eprintln!("{agent}: 10 jumps in {:?}", started.elapsed());
    // After jumping, the wheel still moves the view: the Agent's own when it
    // is full screen, Diri's scrollback when it is inline.
    let offset = |harness: &mut Harness| {
        let id = harness.id.clone();
        harness.pane.read_with(harness.cx, |pane, _| {
            pane.residents[&id].element.view_offset()
        })
    };
    let before = (harness.screen(), offset(&mut harness));
    harness.pane.update_in(harness.cx, |pane, window, cx| {
        let event = gpui::ScrollWheelEvent {
            position: gpui::point(px(200.0), px(120.0)),
            delta: gpui::ScrollDelta::Lines(gpui::point(0.0, 3.0)),
            ..Default::default()
        };
        pane.handle_scroll(&event, window, cx);
    });
    harness.settle(Duration::from_millis(400));
    assert_ne!(
        (harness.screen(), offset(&mut harness)),
        before,
        "{agent}: the wheel no longer scrolls"
    );
    // And typing reaches the composer.
    harness.cx.simulate_input("typed after jumping");
    harness.wait("the typed text", Duration::from_secs(10), |pane, id| {
        let buffer = pane.residents[id].element.buffer();
        let buffer = buffer.read().unwrap();
        (0..usize::from(buffer.rows)).any(|row| {
            row_text(buffer.row(row).unwrap_or_default()).contains("typed after jumping")
        })
    });
}

#[gpui::test]
#[ignore = "needs DIRI_REAL_AGENTS=1, claude and python3"]
fn claude_code_message_jumps(cx: &mut TestAppContext) {
    walk_real_agent(cx, "claude-code", "", "❯");
}

#[gpui::test]
#[ignore = "needs DIRI_REAL_AGENTS=1, codex and python3"]
fn codex_message_jumps(cx: &mut TestAppContext) {
    walk_real_agent(cx, "codex", "", "›");
}

#[gpui::test]
#[ignore = "needs DIRI_REAL_AGENTS=1, codex and python3"]
fn codex_inline_message_jumps(cx: &mut TestAppContext) {
    walk_real_agent(cx, "codex", "--no-alt-screen", "›");
}

#[gpui::test]
#[ignore = "needs DIRI_REAL_AGENTS=1, opencode and python3"]
fn opencode_message_jumps(cx: &mut TestAppContext) {
    walk_real_agent(cx, "opencode", "--standalone", "ctrl+p commands");
}
