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
[ "$1" = mcp ] || exit 2
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$0.log"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  [ -z "$id" ] && continue
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
    assert!(error.message.contains("permissions grant"));
}
