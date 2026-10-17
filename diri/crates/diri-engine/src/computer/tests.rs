use super::*;

fn caller<'a>(session_id: &'a str, badge: &'a str) -> Caller<'a> {
    Caller { session_id, badge }
}

#[test]
fn badges_are_stable_unique_and_fit_the_driver_cap() {
    let mut sessions = Sessions::default();
    let a = sessions.label_for(&caller("a", "Codex · fix login"));
    assert_eq!(a, "Codex · fix login");
    assert_eq!(sessions.label_for(&caller("a", "renamed later")), a);
    let b = sessions.label_for(&caller("b", "Codex · fix login"));
    assert_eq!(b, "Codex · fix login 2");

    let long = sessions.label_for(&caller("c", "Claude · a very long tab title that goes on"));
    assert_eq!(long.chars().count(), MAX_LABEL_CHARS - NAME_TAG_LEN);
    assert!(long.ends_with('…'));
    let long_twin = sessions.label_for(&caller("d", "Claude · a very long tab title that goes on"));
    assert!(
        long_twin.chars().count() <= MAX_LABEL_CHARS - NAME_TAG_LEN,
        "{long_twin}"
    );
    assert!(long_twin.ends_with(" 2"), "{long_twin}");

    assert_eq!(sessions.label_for(&caller("e", "  ")), "Diri agent");
}

#[test]
fn an_app_belongs_to_one_session_until_it_goes_quiet_or_away() {
    let mut sessions = Sessions::default();
    let now = Instant::now();
    let live = |_: &str| true;
    sessions.lease("a", "A", &[10], now, &live).unwrap();
    // The holder renews freely; others are told who holds it.
    sessions
        .lease("a", "A", &[10], now + Duration::from_secs(5), &live)
        .unwrap();
    let busy = sessions
        .lease("b", "B", &[10, 11], now + Duration::from_secs(6), &live)
        .unwrap_err();
    assert_eq!(busy.code, APP_BUSY);
    assert!(busy.message.contains("\"A\""), "{}", busy.message);
    assert!(busy.message.contains("creates_new_application_instance"));
    // A refused call takes nothing, not even the free pid it also named.
    assert!(!sessions.leases.contains_key(&11));

    // Renewal restarts the holder's clock and never touches someone else's.
    let later = now + Duration::from_secs(5) + LEASE_IDLE - Duration::from_secs(1);
    sessions.renew("a", &[10], later);
    sessions.renew("b", &[10], later + Duration::from_secs(100));
    assert!(
        sessions
            .lease(
                "b",
                "B",
                &[10],
                later + LEASE_IDLE - Duration::from_secs(1),
                &live
            )
            .is_err()
    );

    // A quiet holder loses it.
    sessions
        .lease("b", "B", &[10], later + LEASE_IDLE, &live)
        .unwrap();
    assert_eq!(sessions.leases[&10].session_id, "b");

    // An ended holder loses it at once.
    sessions.lease("c", "C", &[20], now, &live).unwrap();
    sessions
        .lease("d", "D", &[20], now, &|id: &str| id != "c")
        .unwrap();

    sessions.forget_session("d");
    assert!(!sessions.leases.contains_key(&20));
    assert!(!sessions.labels.contains_key("d"));
}

#[test]
fn tags_are_invisible_and_fit_the_badge() {
    let mut sessions = Sessions::default();
    let label = sessions.label_for(&caller("a", "Claude · a very long tab title that goes on"));
    let name = tagged(&label, 0b1110_0100);
    assert_eq!(name.chars().count(), MAX_LABEL_CHARS);
    assert_eq!(visible(&name), label);
    assert_ne!(tagged(&label, 1), tagged(&label, 2));
}

#[test]
fn every_nested_pid_is_leased() {
    let args = json!({"steps":[{"tool":"click","args":{"pid":1,"x":3}},
                                {"tool":"type_text","args":{"pid":2,"window_id":9}},
                                {"tool":"click","args":{"pid":1}}]});
    assert_eq!(mentioned_pids(&args), vec![1, 2]);
    assert!(mentioned_pids(&json!({"pid":"not a number"})).is_empty());
}

#[test]
fn screenshots_default_to_model_size_and_stay_bounded() {
    let mut args = Map::new();
    bound_screenshot("get_window_state", &mut args);
    assert_eq!(args["max_image_dimension"], DEFAULT_IMAGE_DIMENSION);
    args.insert("max_image_dimension".into(), json!(0));
    bound_screenshot("get_window_state", &mut args);
    assert_eq!(args["max_image_dimension"], MAX_IMAGE_DIMENSION);
    let mut click = Map::new();
    bound_screenshot("click", &mut click);
    assert!(click.is_empty());
}

#[test]
fn oversized_results_lose_images_not_text() {
    let big = "A".repeat(MAX_RESULT_BYTES);
    let result = json!({"content":[{"type":"text","text":"tree"},
                                   {"type":"image","data":big,"mimeType":"image/png"}]});
    let fitted = fit_result(result).unwrap();
    let content = fitted["mcpToolResult"]["content"].as_array().unwrap();
    assert_eq!(content[0]["text"], "tree");
    assert!(content.iter().all(|block| block["type"] != "image"));
    assert!(
        content.last().unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("max_image_dimension")
    );

    let text_only = json!({"content":[{"type":"text","text":"A".repeat(MAX_RESULT_BYTES)}]});
    assert_eq!(fit_result(text_only).unwrap_err().code, DRIVER_ERROR);
}

#[test]
fn summaries_end_at_the_first_sentence() {
    assert_eq!(
        first_sentence("Click a target pid. More words."),
        "Click a target pid"
    );
    assert_eq!(
        first_sentence("Press a key combination — e.g. cmd+c"),
        "Press a key combination — e.g"
    );
    assert_eq!(first_sentence("One line\nnext"), "One line");
}

/// A stand-in for `cua-driver mcp`: answers the handshake and a three-tool
/// catalog, and echoes each call's request line back as structured content.
const FAKE_DRIVER: &str = r#"#!/bin/sh
case "$1" in
  --version) printf 'cua-driver %s\n' "$(cat "$0.version" 2>/dev/null || echo 0.36.0)"; exit 0 ;;
  call)
    [ "$2" = check_permissions ] || exit 2
    cat "$0.grants" 2>/dev/null || printf '{"accessibility":true,"screen_recording":true}\n'
    exit 0 ;;
  mcp) ;;
  *) exit 2 ;;
esac
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$0.log"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  [ -z "$id" ] && continue
  if [ -e "$0.ungranted" ]; then
    case "$line" in
      *'"name":"click"'*)
        printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"permissions_pending: macOS Accessibility or Screen Recording permission is still pending"}],"isError":true}}\n' "$id"
        continue ;;
    esac
  fi
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"launch_app","description":"Launch a macOS app in the background. More.","inputSchema":{"type":"object","properties":{"session":{"type":"string"},"bundle_id":{"type":"string"}}}},{"name":"click","description":"Click a target pid.","inputSchema":{"type":"object"}},{"name":"get_window_state","description":"Snapshot a window.","inputSchema":{"type":"object"}},{"name":"set_config","description":"Driver-wide.","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"name":"start_session"'*)
      # The first name asked for belongs to an earlier pipe.
      if [ ! -e "$0.taken" ]; then
        : > "$0.taken"
        printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"session is not available to this transport"}],"isError":true,"structuredContent":{"code":"session_unavailable"}}}\n' "$id"
      else
        printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"active"}],"structuredContent":{"active":true}}}\n' "$id"
      fi ;;
    *'"name":"launch_app"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"launched"}],"structuredContent":{"pid":4242,"echo":%s}}}\n' "$id" "$line" ;;
    *'"x":-1'*'"name":"click"'*)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"no such point"}}\n' "$id" ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"ok"}],"structuredContent":{"echo":%s}}}\n' "$id" "$line" ;;
  esac
done
"#;

fn fake_driver(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join("cua-driver");
    std::fs::write(&path, FAKE_DRIVER).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn agents_share_one_driver_with_their_own_badges_and_apps() {
    let temp = tempfile::tempdir().unwrap();
    let computer = ComputerUse::with_binary(Some(fake_driver(temp.path())));
    let live = |_: &str| true;
    let codex = caller("s-codex", "Codex · checkout flow");
    let claude = caller("s-claude", "Claude · settings page");

    let catalog = computer.act(&codex, "tools", json!({}), &live).unwrap();
    let names: Vec<&str> = catalog["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["launch_app", "click", "get_window_state"]);
    assert_eq!(
        catalog["tools"][0]["summary"],
        "Launch a macOS app in the background"
    );
    let one = computer
        .act(&codex, "tools", json!({"name":"launch_app"}), &live)
        .unwrap();
    assert!(one["args"]["properties"].get("session").is_none());
    assert!(one["args"]["properties"].get("bundle_id").is_some());

    // The badge travels on every call, whatever the Agent passed.
    let launched = computer
        .act(
            &codex,
            "launch_app",
            json!({"bundle_id":"com.apple.TextEdit","session":"spoofed"}),
            &live,
        )
        .unwrap();
    let echo = &launched["mcpToolResult"]["structuredContent"]["echo"];
    let codex_name = echo["params"]["arguments"]["session"].as_str().unwrap();
    assert_eq!(visible(codex_name), "Codex · checkout flow");

    // The launcher owns the new pid; the other Agent may look but not act.
    let busy = computer
        .act(&claude, "click", json!({"pid":4242,"x":1,"y":2}), &live)
        .unwrap_err();
    assert_eq!(busy.code, APP_BUSY);
    let looked = computer
        .act(
            &claude,
            "get_window_state",
            json!({"pid":4242,"window_id":1}),
            &live,
        )
        .unwrap();
    let echo = &looked["mcpToolResult"]["structuredContent"]["echo"];
    assert_eq!(
        visible(echo["params"]["arguments"]["session"].as_str().unwrap()),
        "Claude · settings page"
    );
    assert_eq!(
        echo["params"]["arguments"]["max_image_dimension"],
        DEFAULT_IMAGE_DIMENSION
    );
    computer
        .act(&codex, "click", json!({"pid":4242,"x":1,"y":2}), &live)
        .unwrap();

    // Each badge's driver session starts once, with Diri's quick cursor,
    // under a name no earlier pipe holds: a taken name is retried with
    // another invisible tag.
    let log = std::fs::read_to_string(temp.path().join("cua-driver.log")).unwrap();
    let starts: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|message| message["params"]["name"] == "start_session")
        .collect();
    assert_eq!(starts.len(), 3, "{starts:?}");
    let first = starts[0]["params"]["arguments"]["session"]
        .as_str()
        .unwrap();
    let second = starts[1]["params"]["arguments"]["session"]
        .as_str()
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(visible(first), visible(second));
    assert_eq!(
        second, codex_name,
        "the call runs under the name that started"
    );
    assert_eq!(
        starts[0]["params"]["arguments"]["cursor_motion"]["glide_duration_ms"],
        CURSOR_GLIDE_MS
    );

    // Driver-wide tools stay with the person; unknown tools say so.
    assert_eq!(
        computer
            .act(&codex, "set_config", json!({}), &live)
            .unwrap_err()
            .code,
        UNKNOWN_TOOL
    );
    assert_eq!(
        computer
            .act(&codex, "rm_rf", json!({}), &live)
            .unwrap_err()
            .code,
        UNKNOWN_TOOL
    );
    // Long batches are split so other Agents' actions interleave.
    let steps: Vec<Value> = (0..=MAX_BATCH_STEPS)
        .map(|_| json!({"click": {"pid": 4242}}))
        .collect();
    assert_eq!(
        computer
            .act(&codex, "run_steps", json!({"steps": steps}), &live)
            .unwrap_err()
            .code,
        BATCH_TOO_LONG
    );
    // A driver error reaches the Agent as a driver error.
    let failed = computer
        .act(&codex, "click", json!({"pid":4242,"x":-1}), &live)
        .unwrap_err();
    assert_eq!(failed.code, DRIVER_ERROR);
    assert!(failed.message.contains("no such point"));
}

#[test]
fn concurrent_agents_are_answered_by_one_pipe() {
    let temp = tempfile::tempdir().unwrap();
    let computer = ComputerUse::with_binary(Some(fake_driver(temp.path())));
    let handles: Vec<_> = (0..10)
        .map(|n| {
            let computer = computer.clone();
            std::thread::spawn(move || {
                let id = format!("s{n}");
                let badge = format!("Agent {n}");
                computer
                    .act(
                        &caller(&id, &badge),
                        "get_window_state",
                        json!({"pid": 100 + n, "window_id": n}),
                        &|_: &str| true,
                    )
                    .map(|result| {
                        visible(
                            result["mcpToolResult"]["structuredContent"]["echo"]["params"]
                                ["arguments"]["session"]
                                .as_str()
                                .unwrap(),
                        )
                    })
            })
        })
        .collect();
    for (n, handle) in handles.into_iter().enumerate() {
        assert_eq!(handle.join().unwrap().unwrap(), format!("Agent {n}"));
    }
    let children = computer.lock().incarnation;
    assert_eq!(children, 1, "ten Agents, one driver process");
}

#[test]
fn a_driver_that_dies_is_relaunched_on_the_next_call() {
    let temp = tempfile::tempdir().unwrap();
    let computer = ComputerUse::with_binary(Some(fake_driver(temp.path())));
    let live = |_: &str| true;
    let agent = caller("s", "Agent");
    let name_on = |result: Value| {
        result["mcpToolResult"]["structuredContent"]["echo"]["params"]["arguments"]["session"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let read = json!({"pid":1,"window_id":1});
    let before = name_on(
        computer
            .act(&agent, "get_window_state", read.clone(), &live)
            .unwrap(),
    );
    {
        let mut inner = computer.lock();
        let child = inner.child.as_mut().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }
    let after = name_on(
        computer
            .act(&agent, "get_window_state", read, &live)
            .unwrap(),
    );
    assert_eq!(computer.lock().incarnation, 2);
    // The old pipe's names died with it; the badge reads the same.
    assert_ne!(before, after);
    assert_eq!(visible(&before), visible(&after));
}

#[test]
fn a_missing_driver_is_reported_as_unavailable() {
    let temp = tempfile::tempdir().unwrap();
    let computer = ComputerUse::with_binary(Some(temp.path().join("missing")));
    let error = computer
        .act(&caller("s", "Agent"), "tools", json!({}), &|_: &str| true)
        .unwrap_err();
    assert_eq!(error.code, UNAVAILABLE);
    // Agents are pointed at Diri's setup, never at a terminal installer.
    assert!(error.message.contains("Settings › Agents › Computer use"));
    assert!(!error.message.contains("curl"));
    let status = computer.status(true);
    assert!(!status.installed());
    assert!(!status.ready());
}

#[test]
fn a_missing_grant_points_the_agent_at_setup_once_per_spell() {
    let temp = tempfile::tempdir().unwrap();
    let driver = fake_driver(temp.path());
    std::fs::write(driver.with_extension("ungranted"), "").unwrap();
    let computer = ComputerUse::with_binary(Some(driver));
    let error = computer
        .act(
            &caller("s", "Agent"),
            "click",
            json!({"pid":1,"x":1,"y":1}),
            &|_: &str| true,
        )
        .unwrap_err();
    assert_eq!(error.code, PERMISSIONS);
    assert!(error.message.contains("Settings › Agents › Computer use"));
    // A refused call holds no app.
    let other = computer
        .act(
            &caller("t", "Other"),
            "click",
            json!({"pid":1,"x":1,"y":1}),
            &|_: &str| true,
        )
        .unwrap_err();
    assert_eq!(other.code, PERMISSIONS);

    assert!(computer.should_nudge("s"));
    assert!(
        !computer.should_nudge("s"),
        "a retry is not a second prompt"
    );
    assert!(computer.should_nudge("t"));
}

#[test]
fn a_driver_without_grants_is_not_asked_to_act() {
    let temp = tempfile::tempdir().unwrap();
    let driver = fake_driver(temp.path());
    let grants = driver.with_extension("grants");
    std::fs::write(
        &grants,
        r#"{"accessibility":true,"screen_recording":false}"#,
    )
    .unwrap();
    let computer = ComputerUse::with_binary(Some(driver.clone()));
    let live = |_: &str| true;
    let error = computer
        .act(
            &caller("s", "Agent"),
            "click",
            json!({"pid":7,"x":1,"y":1}),
            &live,
        )
        .unwrap_err();
    assert_eq!(error.code, PERMISSIONS);
    let log = std::fs::read_to_string(driver.with_extension("log")).unwrap_or_default();
    assert!(
        !log.contains(r#""name":"click""#),
        "refused before the driver: {log}"
    );

    // Granted in System Settings: the very next call goes through.
    std::fs::write(&grants, r#"{"accessibility":true,"screen_recording":true}"#).unwrap();
    computer
        .act(
            &caller("s", "Agent"),
            "click",
            json!({"pid":7,"x":1,"y":1}),
            &live,
        )
        .unwrap();
}

#[test]
fn status_reads_version_bundle_and_grants() {
    let temp = tempfile::tempdir().unwrap();
    let macos = temp.path().join("CuaDriver.app/Contents/MacOS");
    std::fs::create_dir_all(&macos).unwrap();
    let driver = fake_driver(&macos);
    let computer = ComputerUse::with_binary(Some(driver.clone()));
    let grants = driver.with_extension("grants");
    std::fs::write(
        &grants,
        r#"{"accessibility":false,"screen_recording":false}"#,
    )
    .unwrap();

    let status = computer.status(false);
    assert!(status.supported);
    assert_eq!(status.version.as_deref(), Some("0.36.0"));
    assert!(!status.outdated);
    assert_eq!(
        status.app_path.as_deref().map(Path::new),
        Some(
            std::fs::canonicalize(temp.path().join("CuaDriver.app"))
                .unwrap()
                .as_path()
        )
    );
    assert_eq!(status.accessibility, Some(false));
    assert_eq!(status.screen_recording, Some(false));
    assert!(!status.ready());

    std::fs::write(
        driver.with_extension("grants"),
        r#"{"accessibility":true,"screen_recording":true,"source":{"pid":1}}"#,
    )
    .unwrap();
    assert!(computer.status(false).ready());

    std::fs::write(driver.with_extension("version"), "0.35.2").unwrap();
    let old = computer.status(false);
    assert!(old.outdated);
    assert!(!old.ready(), "an outdated driver is offered an update");
}

#[test]
fn install_runs_in_the_background_and_reports_the_result() {
    use std::os::unix::fs::PermissionsExt as _;
    let temp = tempfile::tempdir().unwrap();
    let driver = temp.path().join("bin/cua-driver");
    // Stands in for cua.ai's script: checks Diri's pins, then "installs".
    let installer = temp.path().join("install.sh");
    std::fs::write(
        &installer,
        format!(
            "echo \"==> installing $CUA_DRIVER_RS_VERSION telemetry=$CUA_DRIVER_RS_TELEMETRY_ENABLED args=$*\"\n\
             [ \"$CUA_DRIVER_RS_NO_MODIFY_PATH\" = 1 ] || exit 3\n\
             mkdir -p {bin}\n\
             cat > {driver} <<'FAKE'\n{FAKE_DRIVER}FAKE\n\
             chmod +x {driver}\n",
            bin = temp.path().join("bin").display(),
            driver = driver.display(),
        ),
    )
    .unwrap();
    let computer = ComputerUse::with_driver(Some(driver.clone()), Some(installer.clone()));
    assert!(!computer.status(false).installed());

    let started = computer
        .setup(diri_proto::ComputerSetupAction::Install)
        .unwrap();
    let done = wait_for(|| {
        let status = computer.status(false);
        (!status.installing).then_some(status)
    });
    assert!(started.installing || done.installed());
    assert_eq!(done.install_error, None, "{:?}", done.install_log);
    assert_eq!(done.version.as_deref(), Some(TESTED_VERSION));
    let log = done.install_log.unwrap();
    assert!(
        log.contains(&format!(
            "installing {TESTED_VERSION} telemetry=0 args=--no-modify-path"
        )),
        "{log}"
    );

    // A failing installer leaves its words for the person, and the log.
    std::fs::write(
        &installer,
        "echo 'error: /Applications is not writable. Re-run'; exit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&installer, std::fs::Permissions::from_mode(0o644)).unwrap();
    computer
        .setup(diri_proto::ComputerSetupAction::Install)
        .unwrap();
    let failed = wait_for(|| {
        let status = computer.status(false);
        (!status.installing).then_some(status)
    });
    assert!(failed.install_error.unwrap().contains("administrator"));
    assert!(failed.install_log.unwrap().contains("not writable"));
}

fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn installer_failures_read_as_what_to_do() {
    assert!(
        setup::install_failure("curl: (6) Could not resolve host: github.com")
            .contains("connection")
    );
    assert_eq!(
        setup::install_failure(
            "==> x\nerror: downloaded CuaDriver.app failed signature verification\n"
        ),
        "The installer stopped: downloaded CuaDriver.app failed signature verification"
    );
    assert!(setup::install_failure("").contains("Show the log"));
}

#[test]
fn versions_compare_numerically() {
    assert!(setup::older_than("0.9.0", "0.36.0"));
    assert!(setup::older_than("0.35.9", "0.36.0"));
    assert!(!setup::older_than("0.36.0", "0.36.0"));
    assert!(!setup::older_than("0.36.1-nightly.2", "0.36.0"));
    assert!(!setup::older_than("1.0.0", "0.36.0"));
}

#[test]
fn grants_parse_from_either_shape() {
    assert_eq!(
        setup::parse_grants(br#"{"accessibility":true,"screen_recording":false}"#),
        Some((true, false))
    );
    assert_eq!(
        setup::parse_grants(
            br#"{"structuredContent":{"accessibility":false,"screen_recording":true}}"#
        ),
        Some((false, true))
    );
    assert_eq!(setup::parse_grants(b"permissions_pending: retry"), None);
}

#[test]
fn the_app_bundle_is_found_through_the_cli_symlink() {
    let temp = tempfile::tempdir().unwrap();
    let macos = temp.path().join("CuaDriver.app/Contents/MacOS");
    std::fs::create_dir_all(&macos).unwrap();
    let binary = fake_driver(&macos);
    let link = temp.path().join("cua-driver");
    std::os::unix::fs::symlink(&binary, &link).unwrap();
    let app = std::fs::canonicalize(temp.path().join("CuaDriver.app")).unwrap();
    assert_eq!(setup::app_bundle(&link), Some(app.clone()));
    assert_eq!(setup::app_bundle(&binary), Some(app));
    assert_eq!(
        setup::app_bundle(&temp.path().join("cua-driver-elsewhere")),
        None
    );
}

/// Against the real Cua Driver on this Mac. With `DIRI_CUA_DRIVER_SOCKET`
/// pointing at a daemon started without grants, expects them missing.
/// `cargo test -p diri-engine --lib real_driver_status -- --ignored --nocapture`
#[test]
#[ignore = "needs Cua Driver installed"]
fn real_driver_status() {
    let computer = ComputerUse::new();
    let status = computer.status(true);
    eprintln!("{status:#?}");
    assert!(status.installed());
    assert!(
        status
            .app_path
            .as_deref()
            .is_some_and(|app| app.ends_with("CuaDriver.app"))
    );
    let expect_granted = setup::socket_override().is_none();
    assert_eq!(status.accessibility, Some(expect_granted));
    if !expect_granted {
        let error = computer
            .act(
                &caller("real", "Diri test"),
                "get_window_state",
                json!({"pid": finder_pid(), "include_screenshot": false}),
                &|_: &str| true,
            )
            .unwrap_err();
        eprintln!("{error:?}");
        assert_eq!(error.code, PERMISSIONS);
    }
    computer.shutdown();
}

fn finder_pid() -> i64 {
    let output = std::process::Command::new("pgrep")
        .args(["-x", "Finder"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap()
}
