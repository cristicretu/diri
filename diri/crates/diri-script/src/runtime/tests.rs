use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::*;

#[derive(Default)]
struct Record {
    requests: Vec<(String, Value)>,
    notices: Vec<Notice>,
    logs: Vec<(Level, String)>,
    /// How long each request takes, to check the budget excludes it.
    delay: Duration,
}

struct FakeHost(Rc<RefCell<Record>>);

impl Host for FakeHost {
    fn request(
        &mut self,
        method: &str,
        params: Value,
        _timeout: Duration,
    ) -> Result<Value, HostError> {
        let delay = self.0.borrow().delay;
        std::thread::sleep(delay);
        self.0
            .borrow_mut()
            .requests
            .push((method.to_owned(), params.clone()));
        match method {
            "session.list" => Ok(json!({"sessions": [{"id": "a"}], "projects": []})),
            "app.perform" => Ok(json!({"action": params["action"], "result": {"done": true}})),
            "app.actions.list" => Ok(json!({"connected": true, "actions": [
                {"name": "split-pane-right", "title": "Split Right", "args": {"type": "object"}}
            ]})),
            "session.kill" => Err(HostError::new("not_found", "no session zz")),
            _ => Ok(json!({})),
        }
    }

    fn notify(&mut self, notice: Notice) {
        self.0.borrow_mut().notices.push(notice);
    }

    fn log(&mut self, level: Level, message: &str) {
        self.0.borrow_mut().logs.push((level, message.to_owned()));
    }

    fn context(&self) -> &'static str {
        "cli"
    }
}

fn runtime_with(options: Options) -> (Runtime, Rc<RefCell<Record>>) {
    let record = Rc::new(RefCell::new(Record::default()));
    let runtime = Runtime::new(Box::new(FakeHost(Rc::clone(&record))), options).unwrap();
    (runtime, record)
}

fn runtime() -> (Runtime, Rc<RefCell<Record>>) {
    runtime_with(Options::default())
}

#[test]
fn the_sandbox_has_no_files_processes_or_bytecode() {
    let (runtime, _) = runtime();
    for absent in [
        "io",
        "debug",
        "package",
        "dofile",
        "loadfile",
        "os.execute",
        "os.getenv",
        "os.remove",
        "os.exit",
        "string.dump",
    ] {
        assert_eq!(
            runtime.eval(&format!("{absent} == nil")).unwrap(),
            "true",
            "{absent} must not exist"
        );
    }
    assert_eq!(runtime.eval("load('return 1 + 1')()").unwrap(), "2");
    let binary = runtime
        .eval("load('\\27Lua')")
        .expect("load answers nil, error");
    assert!(binary.starts_with("nil"), "{binary}");
    assert!(
        runtime
            .eval("require('anything')")
            .unwrap_err()
            .contains("only works in a script loaded from a file")
    );
    assert_eq!(runtime.eval("type(os.time())").unwrap(), "\"number\"");
    assert_eq!(
        runtime.eval("os.date('!%Y-%m-%d', 0)").unwrap(),
        "\"1970-01-01\""
    );
}

#[test]
fn require_loads_only_plain_names_beside_the_script() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("keys.lua"), "return {answer = 42}").unwrap();
    std::fs::create_dir(root.path().join("lib")).unwrap();
    std::fs::write(root.path().join("lib/util.lua"), "return 'util'").unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("evil.lua"), "return 'evil'").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        outside.path().join("evil.lua"),
        root.path().join("link.lua"),
    )
    .unwrap();

    let (runtime, _) = runtime_with(Options {
        root: Some(root.path().to_path_buf()),
        ..Options::default()
    });
    assert_eq!(runtime.eval("require('keys').answer").unwrap(), "42");
    assert_eq!(runtime.eval("require('lib.util')").unwrap(), "\"util\"");
    for name in ["../evil", "/etc/passwd", ".hidden", "a b"] {
        let error = runtime.eval(&format!("require('{name}')")).unwrap_err();
        assert!(error.contains("plain module names"), "{name}: {error}");
    }
    #[cfg(unix)]
    {
        let error = runtime.eval("require('link')").unwrap_err();
        assert!(error.contains("outside the script directory"), "{error}");
    }
}

#[test]
fn a_runaway_callback_is_stopped_and_reported_but_engine_time_is_not_counted() {
    let (runtime, record) = runtime_with(Options {
        callback_budget: Duration::from_millis(50),
        ..Options::default()
    });
    runtime
        .load_source(
            "diri.on('focus', function() while true do end end)\n\
             diri.on('session.created', function() diri.session.list() diri.session.list() end)",
            "init.lua",
        )
        .unwrap();

    let started = Instant::now();
    assert_eq!(runtime.emit("focus", &json!({"session": "a"})), 1);
    assert!(started.elapsed() < Duration::from_secs(2));
    {
        let record = record.borrow();
        assert_eq!(record.notices.len(), 1);
        assert_eq!(record.notices[0].level, Level::Error);
        let detail = record.notices[0].detail.as_deref().unwrap();
        assert!(detail.contains("stopped after 50 ms"), "{detail}");
    }

    // Two 40 ms Engine round trips fit a 50 ms budget: waiting is not CPU.
    record.borrow_mut().delay = Duration::from_millis(40);
    runtime.emit("session.created", &json!({}));
    let record = record.borrow();
    assert_eq!(record.notices.len(), 1, "{:?}", record.notices);
    assert_eq!(
        record
            .requests
            .iter()
            .filter(|(method, _)| method == "session.list")
            .count(),
        2
    );
}

#[test]
fn a_load_that_runs_forever_fails_instead_of_hanging() {
    let (runtime, _) = runtime_with(Options {
        load_budget: Duration::from_millis(50),
        ..Options::default()
    });
    let error = runtime
        .load_source("while true do end", "init.lua")
        .unwrap_err();
    assert!(error.message.contains("stopped after 50 ms"), "{error}");
}

#[test]
fn memory_is_capped() {
    let (runtime, _) = runtime_with(Options {
        memory_limit: 8 * 1024 * 1024,
        ..Options::default()
    });
    let error = runtime
        .eval("local t = {} for i = 1, 1e7 do t[i] = ('x'):rep(64) .. i end")
        .unwrap_err();
    assert!(error.contains("memory"), "{error}");
}

#[test]
fn calls_are_checked_against_the_schema_and_generated_per_method() {
    let (runtime, record) = runtime();
    assert_eq!(
        runtime.eval("diri.session.list().sessions[1].id").unwrap(),
        "\"a\""
    );
    assert_eq!(
        runtime
            .eval("diri.call('session.list').sessions[1].id")
            .unwrap(),
        "\"a\""
    );
    // Whole numbers from arithmetic still pass an integer schema.
    runtime
        .eval("diri.session.resize({sessionID = 'a', cols = 80 * 1.0, rows = 24})")
        .unwrap();
    assert_eq!(
        record.borrow().requests.last().unwrap(),
        &(
            "session.resize".to_owned(),
            json!({"sessionID": "a", "cols": 80, "rows": 24})
        )
    );

    let sent = record.borrow().requests.len();
    let error = runtime.eval("diri.session.spawn({cwd = 3})").unwrap_err();
    assert!(
        error.contains("session.spawn: missing required argument: kind"),
        "{error}"
    );
    let error = runtime.eval("diri.call('daemon.shutdown')").unwrap_err();
    assert!(error.contains("not available to scripts"), "{error}");
    let error = runtime.eval("diri.call('session.nope')").unwrap_err();
    assert!(error.contains("unknown Engine method"), "{error}");
    assert_eq!(
        record.borrow().requests.len(),
        sent,
        "nothing invalid was sent"
    );
    assert_eq!(runtime.eval("diri.daemon").unwrap(), "nil");

    let error = runtime
        .eval("diri.session.kill({sessionID = 'zz'})")
        .unwrap_err();
    assert!(error.contains("not_found: no session zz"), "{error}");
    assert!(
        runtime
            .eval("diri.methods['session.spawn']")
            .unwrap()
            .contains("Start an Agent")
    );
}

#[test]
fn actions_run_through_the_engine() {
    let (runtime, record) = runtime();
    assert_eq!(
        runtime
            .eval("diri.action('select-tab', {index = 2}).done")
            .unwrap(),
        "true"
    );
    runtime.eval("diri.actions.split_pane_right()").unwrap();
    let requests = &record.borrow().requests;
    assert_eq!(
        requests[0],
        (
            "app.perform".into(),
            json!({"action": "select-tab", "args": {"index": 2}})
        )
    );
    assert_eq!(requests[1].1["action"], "split-pane-right");
}

#[test]
fn handlers_receive_their_event_and_unknown_events_are_refused() {
    let (runtime, record) = runtime();
    runtime
        .load_source(
            "seen = {}\n\
             local id = diri.on('session.status', function(e) seen[#seen + 1] = e.to end)\n\
             diri.on('session.status', function(e) diri.log('status', e.session.id) end)\n\
             off_id = id",
            "init.lua",
        )
        .unwrap();
    assert!(runtime.wants_engine_events());
    assert_eq!(
        runtime.emit(
            "session.status",
            &json!({"session": {"id": "a"}, "from": "idle", "to": "working"})
        ),
        2
    );
    assert_eq!(runtime.eval("seen[1]").unwrap(), "\"working\"");
    assert_eq!(record.borrow().logs.last().unwrap().1, "status\ta");
    runtime.eval("diri.off(off_id)").unwrap();
    assert_eq!(runtime.emit("session.status", &json!({"to": "idle"})), 1);

    let error = runtime
        .eval("diri.on('child_exited', function() end)")
        .unwrap_err();
    assert!(error.contains("unknown event \"child_exited\""), "{error}");
    assert!(error.contains("session.exited"), "{error}");
}

#[test]
fn keys_and_commands_form_a_layer() {
    let (runtime, record) = runtime();
    runtime
        .load_source(
            "diri.keymap.set('cmd-shift-d', 'split-pane-right', {desc = 'split'})\n\
             diri.keymap.set('cmd-shift-d', 'split-pane-below')\n\
             diri.keymap.set('ctrl-a c', function() pressed = true end)\n\
             diri.keymap.set('cmd-1', {action = 'select-tab', args = {index = 1}})\n\
             diri.keymap.del('cmd-k')\n\
             diri.command{name = 'say-hi', title = 'Say Hi', run = function() said = true end}\n\
             diri.command{name = 'two', run = 'select-tab', args = {index = 2}}",
            "init.lua",
        )
        .unwrap();
    let layer = runtime
        .take_layer_change()
        .expect("the load changed the layer");
    assert!(runtime.take_layer_change().is_none());
    assert_eq!(layer.unbinds, vec!["cmd-k".to_owned()]);
    let keys: Vec<&str> = layer.bindings.iter().map(|b| b.keys.as_str()).collect();
    assert_eq!(
        keys,
        vec!["cmd-shift-d", "ctrl-a c", "cmd-1"],
        "newest wins"
    );
    assert_eq!(
        layer.bindings[0].target,
        BindingTarget::Action {
            name: "split-pane-below".into(),
            args: json!({})
        }
    );
    assert_eq!(layer.bindings[1].target, BindingTarget::Callback);
    assert_eq!(
        layer.bindings[2].target,
        BindingTarget::Action {
            name: "select-tab".into(),
            args: json!({"index": 1})
        }
    );
    assert_eq!(layer.commands[0].title, "Say Hi");
    assert_eq!(layer.commands[1].title, "two");

    runtime.run_binding(layer.bindings[1].id);
    assert_eq!(runtime.eval("pressed").unwrap(), "true");
    runtime.run_command(layer.commands[0].id);
    assert_eq!(runtime.eval("said").unwrap(), "true");
    runtime.run_command(layer.commands[1].id);
    assert_eq!(
        record.borrow().requests.last().unwrap().1,
        json!({"action": "select-tab", "args": {"index": 2}})
    );
    // `split-pane-below` is not in the fake catalog: a warning, not an error.
    assert!(
        record
            .borrow()
            .logs
            .iter()
            .any(|(level, line)| *level == Level::Warning && line.contains("split-pane-below"))
    );
}

#[test]
fn timers_fire_repeat_and_cancel() {
    let (runtime, _) = runtime();
    runtime
        .load_source(
            "once, many = 0, 0\n\
             diri.after(0, function() once = once + 1 end)\n\
             ticker = diri.every(0.5, function() many = many + 1 end)",
            "init.lua",
        )
        .unwrap();
    let now = Instant::now();
    runtime.run_timers(now);
    assert_eq!(runtime.eval("once").unwrap(), "1");
    assert_eq!(runtime.eval("many").unwrap(), "0");
    runtime.run_timers(now + Duration::from_secs(1));
    runtime.run_timers(now + Duration::from_secs(1));
    assert_eq!(runtime.eval("once").unwrap(), "1");
    assert_eq!(runtime.eval("many").unwrap(), "1", "rescheduled from now");
    runtime.run_timers(now + Duration::from_secs(2));
    assert_eq!(runtime.eval("many").unwrap(), "2");
    runtime.eval("diri.off(ticker)").unwrap();
    assert!(runtime.next_timer().is_none());
    assert!(!runtime.has_pending_work());
}

#[test]
fn the_same_error_toasts_once_but_is_always_logged() {
    let (runtime, record) = runtime();
    runtime
        .load_source("diri.on('focus', function() error('boom') end)", "init.lua")
        .unwrap();
    for _ in 0..3 {
        runtime.emit("focus", &json!({}));
    }
    let record = record.borrow();
    assert_eq!(record.notices.len(), 1);
    assert!(
        record.notices[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("init.lua:1: boom")
    );
    assert_eq!(
        record
            .logs
            .iter()
            .filter(|(level, _)| *level == Level::Error)
            .count(),
        3
    );
}

#[test]
fn syntax_errors_name_the_file_and_line() {
    let (runtime, _) = runtime();
    let error = runtime
        .load_source("diri.on('focus', function()\n  end end", "init.lua")
        .unwrap_err();
    assert!(error.message.contains("init.lua:2"), "{error}");
}

#[test]
fn values_round_trip_through_json() {
    let (runtime, record) = runtime();
    assert_eq!(
        runtime
            .eval("diri.json.decode(diri.json.encode({a = {1, 2}, b = 'x'})).a[2]")
            .unwrap(),
        "2"
    );
    runtime
        .eval("diri.notify('hi', {level = 'success', detail = 'more'})")
        .unwrap();
    assert_eq!(
        record.borrow().notices[0],
        Notice {
            level: Level::Success,
            message: "hi".into(),
            detail: Some("more".into()),
            logged: false,
        }
    );
    assert_eq!(runtime.eval("diri.context").unwrap(), "\"cli\"");
    assert!(runtime.eval("diri.notify('x', {level = 'loud'})").is_err());
}

#[test]
fn the_documented_examples_load_and_handle_events() {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/scripting/examples");
    let mut loaded = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "lua") {
            continue;
        }
        let (runtime, record) = runtime_with(Options {
            root: Some(dir.clone()),
            ..Options::default()
        });
        runtime
            .load_file(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let session = json!({"id": "a", "title": "fix-auth", "cwd": "/tmp/project"});
        runtime.emit("focus", &json!({"session": "a"}));
        runtime.emit(
            "session.status",
            &json!({"session": session, "kind": "claude-code", "from": "working", "to": "idle"}),
        );
        runtime.emit(
            "session.exited",
            &json!({"session": session, "kind": "shell", "code": 1}),
        );
        runtime.emit(
            "session.created",
            &json!({"session": session, "kind": "shell"}),
        );
        runtime.run_timers(Instant::now() + Duration::from_secs(5));
        for command in runtime.layer().commands {
            runtime.run_command(command.id);
        }
        let record = record.borrow();
        let errors: Vec<&String> = record
            .logs
            .iter()
            .filter(|(level, _)| *level == Level::Error)
            .map(|(_, line)| line)
            .collect();
        assert!(errors.is_empty(), "{}: {errors:?}", path.display());
        loaded += 1;
    }
    assert!(loaded >= 4);
}
