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
    /// What the person answers a grant request.
    grant: bool,
    grants: Vec<GrantRequest>,
    modes: Vec<Option<String>>,
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

    fn grant(&mut self, request: &GrantRequest) -> bool {
        let mut record = self.0.borrow_mut();
        record.grants.push(request.clone());
        record.grant
    }

    fn set_mode(&mut self, name: Option<&str>) -> Result<(), HostError> {
        self.0.borrow_mut().modes.push(name.map(str::to_owned));
        Ok(())
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

    runtime.run_binding(layer.bindings[1].id, &json!({"origin": "key"}));
    assert_eq!(runtime.eval("pressed").unwrap(), "true");
    runtime.run_command(layer.commands[0].id, &json!({}), None);
    assert_eq!(runtime.eval("said").unwrap(), "true");
    runtime.run_command(layer.commands[1].id, &json!({}), None);
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
            runtime.run_command(command.id, &json!({"origin": "palette"}), None);
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

fn errors(record: &Rc<RefCell<Record>>) -> Vec<String> {
    record
        .borrow()
        .logs
        .iter()
        .filter(|(level, _)| *level == Level::Error)
        .map(|(_, line)| line.clone())
        .collect()
}

#[test]
fn sleep_and_wait_suspend_the_callback_not_the_thread() {
    let (runtime, record) = runtime_with(Options {
        callback_budget: Duration::from_millis(50),
        ..Options::default()
    });
    runtime
        .load_source(
            "steps = {}\n\
             diri.on('session.created', function(e)\n\
               table.insert(steps, 'created ' .. e.session.id)\n\
               local status = diri.wait('session.status', {session = e.session.id, to = 'idle'})\n\
               table.insert(steps, 'idle ' .. status.session.id)\n\
               diri.sleep(10)\n\
               table.insert(steps, 'slept')\n\
               local none = diri.wait('session.exited', nil, 20)\n\
               table.insert(steps, 'timeout ' .. tostring(none))\n\
             end)\n\
             diri.on('focus', function(e)\n\
               local t = diri.wait('session.title', function(p) return p.to == 'b' end)\n\
               table.insert(steps, 'title ' .. t.to)\n\
             end)",
            "init.lua",
        )
        .unwrap();
    let steps = |runtime: &Runtime| runtime.eval("table.concat(steps, ', ')").unwrap();
    runtime.emit("session.created", &json!({"session": {"id": "a"}}));
    runtime.emit("focus", &json!({"session": "a"}));
    assert!(runtime.has_pending_work());
    assert!(
        runtime.wants_engine_events(),
        "a wait needs the event stream"
    );
    // Another session, or another status: still waiting.
    runtime.emit(
        "session.status",
        &json!({"session": {"id": "b"}, "to": "idle"}),
    );
    runtime.emit(
        "session.status",
        &json!({"session": {"id": "a"}, "to": "working"}),
    );
    runtime.emit("session.title", &json!({"to": "a"}));
    assert_eq!(steps(&runtime), "\"created a\"");
    assert_eq!(
        runtime.emit(
            "session.status",
            &json!({"session": {"id": "a"}, "to": "idle"})
        ),
        1
    );
    runtime.emit("session.title", &json!({"to": "b"}));
    assert_eq!(steps(&runtime), "\"created a, idle a, title b\"");

    // The sleep is a timer; waiting is never counted against the budget.
    let due = runtime.next_timer().expect("the sleep is due");
    std::thread::sleep(Duration::from_millis(60));
    runtime.run_timers(due.max(Instant::now()));
    assert_eq!(steps(&runtime), "\"created a, idle a, title b, slept\"");
    let due = runtime.next_timer().expect("the wait's timeout");
    runtime.run_timers(due);
    assert_eq!(
        steps(&runtime),
        "\"created a, idle a, title b, slept, timeout nil\""
    );
    assert!(!runtime.has_pending_work() || runtime.next_timer().is_none());
    assert!(errors(&record).is_empty(), "{:?}", errors(&record));
}

#[test]
fn waits_work_only_in_the_runtimes_own_coroutines() {
    let (runtime, record) = runtime();
    let error = runtime
        .eval("coroutine.wrap(function() diri.sleep(1) end)()")
        .unwrap_err();
    assert!(error.contains("not in a coroutine of your own"), "{error}");
    assert!(
        runtime
            .eval("diri.wait('nope')")
            .unwrap_err()
            .contains("unknown event")
    );
    assert!(
        runtime
            .eval("diri.sleep(-1)")
            .unwrap_err()
            .contains("milliseconds")
    );
    // A plain yield from a callback is an error, not a silent hang.
    runtime
        .load_source(
            "diri.on('focus', function() coroutine.yield() end)",
            "init.lua",
        )
        .unwrap();
    runtime.emit("focus", &json!({}));
    assert!(
        errors(&record)[0].contains("coroutine.yield"),
        "{:?}",
        errors(&record)
    );
    assert!(!runtime.has_pending_work() || runtime.next_timer().is_none());

    // The top level of a script can wait too: `dirijor script` runs on.
    let (runtime, _) = runtime_with(Options::default());
    runtime
        .load_source("diri.sleep(0) done = true", "run.lua")
        .unwrap();
    assert_eq!(runtime.eval("done").unwrap(), "nil");
    runtime.run_timers(Instant::now());
    assert_eq!(runtime.eval("done").unwrap(), "true");
}

#[test]
fn keys_and_commands_get_their_context_and_commands_answer() {
    let (runtime, record) = runtime();
    runtime
        .load_source(
            "diri.keymap.set('cmd-shift-l', function(ctx) seen = ctx.session .. ' ' .. ctx.origin end)\n\
             diri.command{name = 'count', desc = 'Counts', destructive = true,\n\
               run = function(ctx) diri.sleep(0) return {n = ctx.args.n + 1, origin = ctx.origin} end}",
            "init.lua",
        )
        .unwrap();
    let layer = runtime.layer();
    assert_eq!(layer.bindings[0].source.as_deref(), Some("init.lua:1"));
    assert_eq!(layer.commands[0].source.as_deref(), Some("init.lua:2"));
    assert!(layer.commands[0].destructive);
    assert_eq!(layer.commands[0].description.as_deref(), Some("Counts"));
    assert_eq!(
        layer
            .command_for_action("script.count")
            .map(|command| command.id),
        Some(layer.commands[0].id)
    );
    runtime.run_binding(
        layer.bindings[0].id,
        &json!({"session": "s1", "pane": "p1", "window": 1, "origin": "key"}),
    );
    assert_eq!(runtime.eval("seen").unwrap(), "\"s1 key\"");

    let (tx, rx) = std::sync::mpsc::channel();
    runtime.run_command(
        layer.commands[0].id,
        &json!({"origin": "cli", "args": {"n": 41}}),
        Some(Box::new(move |result| {
            let _ = tx.send(result);
        })),
    );
    assert!(rx.try_recv().is_err(), "answers when the function returns");
    runtime.run_timers(Instant::now());
    assert_eq!(
        rx.try_recv().unwrap(),
        Ok(json!({"n": 42, "origin": "cli"}))
    );

    // A script runs its own command directly, not through the window.
    runtime
        .eval("diri.command{name = 'double', run = function(ctx) return ctx.args.n * 2, ctx.origin end}")
        .unwrap();
    let requests = record.borrow().requests.len();
    assert_eq!(
        runtime
            .eval("diri.action('script.double', {n = 21})")
            .unwrap(),
        "42"
    );
    assert_eq!(record.borrow().requests.len(), requests, "no app.perform");
}

#[test]
fn modes_bind_keys_that_apply_only_inside_them() {
    let (runtime, record) = runtime();
    runtime
        .load_source(
            "diri.keymap.mode('resize', {exclusive = true, desc = 'Resize panes'})\n\
             diri.keymap.mode('go', {once = true})\n\
             diri.keymap.set('cmd-r', {mode = 'resize'})\n\
             diri.keymap.set('resize/h', 'pane-shrink-width')\n\
             diri.keymap.set('resize/enter', {mode = false})\n\
             diri.keymap.set('go/t', function() went = true end)",
            "init.lua",
        )
        .unwrap();
    let layer = runtime.layer();
    assert_eq!(layer.modes.len(), 2);
    assert!(layer.modes[0].exclusive && !layer.modes[0].once);
    assert!(layer.modes[1].once);
    let find = |keys: &str| layer.bindings.iter().find(|b| b.keys == keys).unwrap();
    assert_eq!(
        find("cmd-r").target,
        BindingTarget::Mode {
            name: Some("resize".into())
        }
    );
    assert_eq!(find("cmd-r").mode, None);
    assert_eq!(find("h").mode.as_deref(), Some("resize"));
    assert_eq!(find("enter").target, BindingTarget::Mode { name: None });
    assert_eq!(find("t").mode.as_deref(), Some("go"));

    for (code, needle) in [
        (
            "diri.keymap.set('nope/h', 'toggle-sidebar')",
            "no mode is named \"nope\"",
        ),
        (
            "diri.keymap.set('cmd-x', {mode = 'nope'})",
            "no mode is named",
        ),
        (
            "diri.keymap.set('resize/ctrl-a c', 'toggle-sidebar')",
            "one keystroke",
        ),
        ("diri.keymap.mode('bad name')", "letters, digits"),
        (
            "diri.command{name = 'm', run = {mode = 'go'}}",
            "cannot enter a mode",
        ),
    ] {
        let error = runtime.eval(code).unwrap_err();
        assert!(error.contains(needle), "{code}: {error}");
    }
    runtime
        .eval("diri.keymap.enter('resize') diri.keymap.exit()")
        .unwrap();
    assert_eq!(record.borrow().modes, vec![Some("resize".to_owned()), None]);
    // A key name that never fires is a warning where it was written.
    runtime
        .eval("diri.keymap.set('cmd-shift-jay', 'toggle-sidebar')")
        .unwrap();
    assert!(
        record
            .borrow()
            .logs
            .iter()
            .any(|(level, line)| *level == Level::Warning && line.contains("unknown key name"))
    );
}

#[test]
fn args_and_session_id_reach_the_script() {
    let (runtime, _) = runtime_with(Options {
        args: json!({"branch": "main", "count": 3}),
        session_id: Some("s_42".into()),
        ..Options::default()
    });
    assert_eq!(
        runtime.eval("diri.args.branch .. diri.args.count").unwrap(),
        "\"main3\""
    );
    assert_eq!(runtime.eval("diri.session_id").unwrap(), "\"s_42\"");
    let (runtime, _) = runtime_with(Options::default());
    assert_eq!(
        runtime
            .eval("next(diri.args) == nil and diri.session_id == nil")
            .unwrap(),
        "true"
    );
}

#[test]
fn a_check_runs_the_top_level_until_it_would_reach_outside() {
    let (runtime, record) = runtime_with(Options {
        check: true,
        ..Options::default()
    });
    runtime
        .load_source(
            "diri.keymap.mode('resize')\n\
             diri.keymap.set('resize/h', 'pane-shrink-width')\n\
             diri.on('focus', function() end)\n\
             diri.command{name = 'x', run = function() end}\n\
             local ok = diri.require_grant{exec = {'git'}}\n\
             diri.session.list()\n\
             error('never reached')",
            "deploy.lua",
        )
        .expect("a check stops at the first call, it does not fail there");
    let summary = runtime.summary();
    assert_eq!(summary.stopped_at.as_deref(), Some("deploy.lua:6"));
    assert_eq!(
        (
            summary.keys,
            summary.modes,
            summary.handlers,
            summary.commands
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(summary.grants_requested, vec!["git".to_owned()]);
    assert!(
        record
            .borrow()
            .requests
            .iter()
            .all(|(method, _)| method == "app.actions.list")
    );
    assert!(record.borrow().grants.is_empty(), "a check never asks");

    // Bad parameters are still found before the stop.
    let (runtime, _) = runtime_with(Options {
        check: true,
        ..Options::default()
    });
    let error = runtime
        .load_source("diri.session.spawn{cwd = 1}", "bad.lua")
        .unwrap_err();
    assert!(error.message.contains("session.spawn"), "{error}");
}

#[test]
fn exec_is_denied_until_granted() {
    let (runtime, record) = runtime();
    let error = runtime.eval("diri.exec{'echo', 'hi'}").unwrap_err();
    assert!(error.contains("\"echo\" is not granted"), "{error}");
    runtime
        .load_source(
            "granted = diri.require_grant{exec = {'echo'}}",
            "deploy.lua",
        )
        .unwrap();
    assert_eq!(runtime.eval("granted").unwrap(), "false");
    let request = record.borrow().grants[0].clone();
    assert_eq!(request.exec, vec!["echo".to_owned()]);
    assert_eq!(request.script, "deploy.lua");
    assert_eq!(request.digest.len(), 64);
    assert!(runtime.eval("diri.exec{'echo', 'hi'}").is_err());
    assert!(
        runtime
            .eval("diri.require_grant{fs = {'/'}}")
            .unwrap_err()
            .contains("no \"fs\" grants")
    );
}

#[cfg(unix)]
#[test]
fn granted_processes_run_with_a_timeout_and_streamed_output() {
    let (runtime, record) = runtime();
    record.borrow_mut().grant = true;
    runtime.eval("diri.require_grant{exec = {'sh'}}").unwrap();
    // Outside a coroutine with a waker (the REPL): runs to the end.
    let out = runtime
        .eval(
            "lines = {}\n\
             local r = diri.exec({'sh', '-c', 'echo one; echo two >&2; printf three; exit 3'},\n\
               {on_output = function(line, stream) table.insert(lines, stream .. ':' .. line) end})\n\
             table.sort(lines)\n\
             return r.code, r.ok, r.stdout, r.stderr, table.concat(lines, ',')",
        )
        .unwrap();
    assert_eq!(
        out,
        "3\tfalse\t\"one\\nthree\"\t\"two\\n\"\t\"stderr:two,stdout:one,stdout:three\""
    );
    let started = Instant::now();
    let out = runtime
        .eval("local r = diri.exec({'sh', '-c', 'sleep 30'}, {timeout = 0.2}) return r.timed_out, r.ok")
        .unwrap();
    assert_eq!(out, "true\tfalse");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(
        runtime
            .eval("diri.exec({'sh'}, {cwd = 'relative'})")
            .unwrap_err()
            .contains("absolute")
    );
}

#[cfg(unix)]
#[test]
fn exec_in_a_callback_suspends_it_until_the_process_ends() {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let tx = std::sync::Mutex::new(tx);
    let (runtime, record) = runtime_with(Options {
        waker: Some(Waker(std::sync::Arc::new(move || {
            let _ = tx.lock().unwrap().send(());
        }))),
        ..Options::default()
    });
    record.borrow_mut().grant = true;
    runtime
        .load_source(
            "diri.require_grant{exec = {'sh'}}\n\
             diri.on('focus', function()\n\
               local r = diri.exec{'sh', '-c', 'echo done'}\n\
               result = r.stdout\n\
             end)",
            "init.lua",
        )
        .unwrap();
    runtime.emit("focus", &json!({}));
    assert_eq!(runtime.eval("result").unwrap(), "nil", "the callback waits");
    assert!(runtime.has_pending_work());
    let deadline = Instant::now() + Duration::from_secs(10);
    while runtime.eval("result").unwrap() == "nil" {
        assert!(Instant::now() < deadline, "the process never finished");
        let _ = rx.recv_timeout(Duration::from_millis(100));
        runtime.pump();
    }
    assert_eq!(runtime.eval("result").unwrap(), "\"done\\n\"");
    assert!(runtime.next_timer().is_none(), "no process or wait is left");
}
