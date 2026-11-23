use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::*;

fn app(source: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    (dir, source.to_owned())
}

fn ctx() -> Value {
    plain_context(1_700_000_000_000)
}

const COUNTER: &str = r##"
app = { title = "Counter", icon = "🧮", accent = "#7c3aed" }

function init(ctx) return { count = 0 } end

function render(state, data, ctx)
  return ui.col{ gap = 8, padding = 12,
    ui.text("Count " .. state.count, { size = 24, weight = "bold" }),
    ui.row{ ui.button{ id = "inc", label = "+1", on = "inc" }, ui.spacer() },
    ui.sparkline({ 1, 4, 2, 6, 3 }, { width = 80, height = 20 }),
  }
end

function on_event(state, event)
  if event.type == "action" and event.name == "inc" then
    state.count = state.count + 1
    if state.count == 2 then
      return state, { action.open("https://example.com/two") }
    end
  end
  return state
end
"##;

#[test]
fn render_builds_a_valid_tree_and_reads_meta() {
    let (dir, source) = app(COUNTER);
    let app = App { source: &source, dir: dir.path() };
    let rendered = render(app, &Value::Null, &json!({}), &ctx(), INTERACTIVE).unwrap();
    let meta = rendered.meta.unwrap();
    assert_eq!(meta["title"], "Counter");
    assert_eq!(meta["icon"], "🧮");
    let root = &rendered.root;
    assert_eq!(root[0]["type"], "box");
    assert_eq!(root[0]["direction"], "column");
    assert_eq!(root[0]["style"]["padding"], 12);
    assert_eq!(root[0]["children"][0]["text"], "Count 0");
    assert_eq!(root[0]["children"][1]["children"][0]["action"]["kind"], "app");
    assert_eq!(root[0]["children"][2]["type"], "canvas");
    // The JSON is exactly the surface format: it parses and validates.
    let nodes: Vec<diri_proto::surface::Node> = serde_json::from_value(root.clone()).unwrap();
    let mut doc = diri_proto::surface::SurfaceDoc::new("a", "A", Default::default());
    doc.root = nodes;
    doc.validate().expect("a Lua render is a valid surface");
}

#[test]
fn events_update_state_and_may_ask_for_gated_actions() {
    let (dir, source) = app(COUNTER);
    let app = App { source: &source, dir: dir.path() };
    let event = json!({"type": "action", "name": "inc"});
    let one = handle_event(app, &json!({"count": 0}), &event, &json!({}), &ctx(), INTERACTIVE).unwrap();
    assert_eq!(one.state, json!({"count": 1}));
    assert!(one.actions.is_empty());
    let two = handle_event(app, &one.state, &event, &json!({}), &ctx(), INTERACTIVE).unwrap();
    assert_eq!(two.actions, vec![json!({"kind": "open_url", "url": "https://example.com/two"})]);
}

#[test]
fn empty_lists_stay_lists() {
    let (dir, source) = app("function render() return ui.row{} end");
    let app = App { source: &source, dir: dir.path() };
    let rendered = render(app, &Value::Null, &json!({}), &ctx(), INTERACTIVE).unwrap();
    assert_eq!(rendered.root[0]["children"], json!([]));
}

#[test]
fn a_looping_app_stops_within_its_budget() {
    for source in [
        "function render() while true do end end",
        "while true do end",
        "function render() local s = 'x' while true do s = s .. 'x' end end",
    ] {
        let (dir, source) = app(source);
        let started = Instant::now();
        let error = render(
            App { source: &source, dir: dir.path() },
            &Value::Null,
            &json!({}),
            &ctx(),
            INTERACTIVE,
        )
        .unwrap_err();
        assert!(matches!(error.code, "budget" | "memory"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }
}

#[test]
fn memory_is_capped() {
    let (dir, source) = app(
        "function render() local t = {} for i = 1, 1e9 do t[i] = string.rep('x', 1024) end end",
    );
    let error = render(
        App { source: &source, dir: dir.path() },
        &Value::Null,
        &json!({}),
        &ctx(),
        Limits {
            memory: 8 * 1024 * 1024,
            ..INTERACTIVE
        },
    )
    .unwrap_err();
    assert_eq!(error.code, "memory", "{error}");
}

#[test]
fn the_sandbox_has_no_way_out() {
    for probe in [
        "return os",
        "return io",
        "return load",
        "return require",
        "return dofile",
        "return loadfile",
        "return string.dump",
        "return debug",
        "return package",
        "return collectgarbage",
    ] {
        let source = format!("function render() local x = (function() {probe} end)() return ui.text(tostring(x)) end");
        let (dir, source) = app(&source);
        let rendered = render(
            App { source: &source, dir: dir.path() },
            &Value::Null,
            &json!({}),
            &ctx(),
            INTERACTIVE,
        )
        .unwrap();
        assert_eq!(rendered.root[0]["text"], "nil", "{probe} is reachable");
    }
}

#[test]
fn sync_needs_approval_for_exec_and_fetch() {
    let source = r#"
function sync(ctx)
  local r = ctx.exec({ "echo", "hello" })
  return { said = r.stdout }
end
"#;
    let (dir, source) = app(source);
    let app = App { source: &source, dir: dir.path() };
    let refused = sync(app, &ctx(), SyncCaps { approved: false }, sync_limits(Duration::from_secs(5))).unwrap();
    assert!(refused.needs_approval);
    assert_eq!(refused.data, None, "nothing changes without approval");
    let allowed = sync(app, &ctx(), SyncCaps { approved: true }, sync_limits(Duration::from_secs(5))).unwrap();
    assert!(!allowed.needs_approval);
    assert_eq!(allowed.data, Some(json!({"said": "hello\n"})));
}

#[test]
fn sync_processes_get_a_clean_environment() {
    let source = r#"
function sync(ctx)
  return { env = ctx.exec({ "/usr/bin/env" }).stdout }
end
"#;
    // SAFETY: tests in this binary do not read this variable concurrently.
    unsafe { std::env::set_var("DIRIJOR_SESSION_ID", "leak-me") };
    let (dir, source) = app(source);
    let synced = sync(
        App { source: &source, dir: dir.path() },
        &ctx(),
        SyncCaps { approved: true },
        sync_limits(Duration::from_secs(5)),
    )
    .unwrap();
    let env = synced.data.unwrap()["env"].as_str().unwrap().to_owned();
    assert!(!env.contains("DIRIJOR_SESSION_ID"), "{env}");
    assert!(env.contains("DIRI_APP_DIR="));
}

#[test]
fn sync_reads_only_its_own_folder() {
    let source = r#"
function sync(ctx)
  local ok, err = pcall(ctx.read, "../secret.txt")
  return { own = ctx.read("notes.txt"), escaped = ok, err = tostring(err), missing = ctx.read("none.txt") }
end
"#;
    let (dir, source) = app(source);
    std::fs::write(dir.path().join("notes.txt"), "mine").unwrap();
    let synced = sync(
        App { source: &source, dir: dir.path() },
        &ctx(),
        SyncCaps { approved: false },
        sync_limits(Duration::from_secs(5)),
    )
    .unwrap();
    let data = synced.data.unwrap();
    assert_eq!(data["own"], "mine");
    assert_eq!(data["escaped"], false);
    assert_eq!(data["missing"], "");
}

#[test]
fn fetch_is_https_only() {
    let source = r#"
function sync(ctx)
  local ok, err = pcall(ctx.fetch, "http://example.com/")
  local ok2, err2 = pcall(ctx.fetch, "file:///etc/passwd")
  return { ok = ok, ok2 = ok2 }
end
"#;
    let (dir, source) = app(source);
    let synced = sync(
        App { source: &source, dir: dir.path() },
        &ctx(),
        SyncCaps { approved: true },
        sync_limits(Duration::from_secs(5)),
    )
    .unwrap();
    assert_eq!(synced.data, Some(json!({"ok": false, "ok2": false})));
}

#[test]
fn sync_time_covers_the_processes_it_starts() {
    let source = r#"
function sync(ctx)
  local r = ctx.exec({ "sleep", "30" })
  return { timed_out = r.timed_out }
end
"#;
    let (dir, source) = app(source);
    let started = Instant::now();
    let synced = sync(
        App { source: &source, dir: dir.path() },
        &ctx(),
        SyncCaps { approved: true },
        sync_limits(Duration::from_millis(300)),
    );
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    match synced {
        Ok(synced) => assert_eq!(synced.data, Some(json!({"timed_out": true}))),
        Err(error) => assert_eq!(error.code, "budget", "{error}"),
    }
}

#[test]
fn digest_binds_approval_to_the_exact_source() {
    assert_eq!(digest("a"), digest("a"));
    assert_ne!(digest("a"), digest("a "));
    assert_eq!(digest("").len(), 64);
}

#[test]
fn errors_name_the_line() {
    let (dir, source) = app("function render()\n  return nil + 1\nend");
    let error = render(
        App { source: &source, dir: dir.path() },
        &Value::Null,
        &json!({}),
        &ctx(),
        INTERACTIVE,
    )
    .unwrap_err();
    assert_eq!(error.code, "lua");
    assert!(error.message.contains("app.lua:2"), "{}", error.message);
}
