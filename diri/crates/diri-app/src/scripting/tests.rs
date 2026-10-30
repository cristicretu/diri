use std::path::Path;
use std::sync::Mutex;

use diri_script::{Binding, Command};
use diri_proto::app_actions::ActionRisk;
use gpui::{Action as _, TestAppContext};

use super::*;

/// The module's statics are process-wide; tests that set them take turns.
static STATICS: Mutex<()> = Mutex::new(());

fn binding(id: u64, keys: &str, target: BindingTarget) -> Binding {
    Binding {
        id,
        keys: keys.into(),
        target,
        description: None,
        mode: None,
        source: Some(format!("init.lua:{id}")),
    }
}

fn action(name: &str, args: Value) -> BindingTarget {
    BindingTarget::Action {
        name: name.into(),
        args,
    }
}

#[test]
fn script_keys_sit_over_defaults_and_under_recorded_shortcuts() {
    let layer = Layer {
        bindings: vec![
            binding(1, "cmd-shift-j", action("split-pane-right", json!({}))),
            binding(2, "ctrl-a c", BindingTarget::Callback),
            binding(3, "cmd-1", action("select-tab", json!({"index": 1}))),
            binding(4, "cmd-shift-jay", BindingTarget::Callback),
            binding(5, "cmd-shift-y", BindingTarget::Callback),
        ],
        unbinds: vec!["cmd-k".into()],
        commands: Vec::new(),
        modes: Vec::new(),
    };
    let mut overrides = ShortcutOverrides::new();
    overrides.insert("open-launcher".into(), Some("cmd-shift-y".into()));

    let (bindings, skipped) = key_bindings(&layer, &overrides);
    let names: Vec<&str> = bindings
        .iter()
        .map(|binding| binding.action().name())
        .collect();
    let split = crate::commands::command(crate::commands::CommandId::SplitPaneRight)
        .id
        .action();
    assert_eq!(
        names,
        vec![
            NoAction.name(),
            split.name(),
            RunScriptKey { id: 2 }.name(),
            RunScriptKey { id: 3 }.name(),
        ],
        "a command with no arguments dispatches directly; the rest go to the script"
    );
    assert_eq!(bindings[2].keystrokes().len(), 2, "a key sequence");
    assert!(
        bindings.iter().all(|binding| binding.predicate().is_none()),
        "no context: they outrank context-bound defaults"
    );
    assert_eq!(skipped.len(), 2, "{skipped:?}");
    assert!(skipped[0].contains("not a key"));
    assert!(skipped[1].contains("recorded in Settings"));
}

#[gpui::test]
fn a_script_key_and_command_reach_the_script_thread(cx: &mut TestAppContext) {
    let _statics = STATICS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (tx, rx) = mpsc::channel();
    *RUNNING.lock().unwrap() = Some(Running {
        tx,
        _watcher: None,
    });
    ACTIVE.store(true, Ordering::Relaxed);
    *LAYER.write().unwrap() = Some(Layer {
        bindings: vec![binding(7, "cmd-shift-j", BindingTarget::Callback)],
        unbinds: Vec::new(),
        commands: vec![Command {
            id: 9,
            name: "say-hi".into(),
            title: "Say Hi".into(),
            target: BindingTarget::Callback,
            description: None,
            destructive: true,
            source: Some("init.lua:4".into()),
        }],
        modes: Vec::new(),
    });
    cx.update(|cx| {
        install_key_handler(cx);
        crate::commands::rebind_keys(cx, &ShortcutOverrides::new());
    });
    let window = cx.add_empty_window();
    emit_focus(&SessionId::new("s-1"), Some("p-1".into()), 3);
    match rx.try_recv() {
        Ok(Input::App { name, payload }) => {
            assert_eq!(name, "focus");
            assert_eq!(payload, json!({"session": "s-1"}));
        }
        _ => panic!("focus reaches the script"),
    }
    // A script's keys are bound as written: `cmd` is Super off macOS, not the
    // Ctrl that shipped bindings move to there.
    window.simulate_keystrokes("cmd-shift-j");
    match rx.try_recv() {
        Ok(Input::Binding { id: 7, context }) => assert_eq!(
            context,
            json!({"session": "s-1", "pane": "p-1", "window": 3, "origin": "key"})
        ),
        _ => panic!("the key reaches the script with its context"),
    }

    let rows = palette_actions();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "Say Hi");
    assert_eq!(
        rows[0].command,
        crate::palette::PaletteCommand::Script { id: 9 }
    );
    run_command(9);
    assert!(matches!(
        rx.try_recv(),
        Ok(Input::Command { id: 9, reply: None, .. })
    ));

    // Published as `script.say-hi`, and run by name with an answer.
    let specs = action_specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name, "script.say-hi");
    assert_eq!(specs[0].risk, ActionRisk::Destructive);
    assert_eq!(action_title("script.say-hi").as_deref(), Some("Say Hi"));
    let answer = perform(&AppPerformRequest {
        request_id: 1,
        action: "script.say-hi".into(),
        args: json!({"n": 1}),
        actor: Some(SessionId::new("agent-1")),
    })
    .unwrap();
    match rx.try_recv() {
        Ok(Input::Command {
            id: 9,
            context,
            reply: Some(reply),
        }) => {
            assert_eq!(context["origin"], "agent");
            assert_eq!(context["actor"], "agent-1");
            assert_eq!(context["args"], json!({"n": 1}));
            reply(Ok(json!({"said": true})));
        }
        _ => panic!("the command reaches the script with a reply"),
    }
    assert_eq!(answer.blocking_recv().unwrap(), Ok(json!({"said": true})));
    assert_eq!(
        perform(&AppPerformRequest {
            request_id: 2,
            action: "script.gone".into(),
            args: json!({}),
            actor: None,
        })
        .unwrap_err()
        .code,
        "unknown_action"
    );

    let listing = keymap_listing();
    let script_key = listing["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|binding| binding["keys"] == "cmd-shift-j" && binding["source"] == "init.lua:7")
        .unwrap();
    assert_eq!(script_key["action"], "lua function");
    assert!(
        listing["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|binding| binding["source"] == "default" && binding["action"] == "toggle-sidebar"),
        "the shipped defaults are listed"
    );

    *RUNNING.lock().unwrap() = None;
    *LAYER.write().unwrap() = None;
    *FOCUS.lock().unwrap() = None;
    ACTIVE.store(false, Ordering::Relaxed);
}

#[gpui::test]
fn a_mode_reads_keys_first_and_escape_leaves_it(cx: &mut TestAppContext) {
    let _statics = STATICS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (tx, rx) = mpsc::channel();
    *RUNNING.lock().unwrap() = Some(Running {
        tx,
        _watcher: None,
    });
    ACTIVE.store(true, Ordering::Relaxed);
    let mode = |name: &str, exclusive: bool, once: bool| Mode {
        name: name.into(),
        exclusive,
        once,
        description: None,
        source: None,
    };
    let in_mode = |id: u64, mode: &str, keys: &str, target: BindingTarget| Binding {
        mode: Some(mode.into()),
        ..binding(id, keys, target)
    };
    *LAYER.write().unwrap() = Some(Layer {
        bindings: vec![
            binding(1, "cmd-shift-r", BindingTarget::Mode { name: Some("resize".into()) }),
            binding(2, "cmd-shift-g", BindingTarget::Mode { name: Some("go".into()) }),
            in_mode(3, "resize", "h", BindingTarget::Callback),
            in_mode(4, "resize", "q", BindingTarget::Mode { name: None }),
            in_mode(5, "go", "t", BindingTarget::Callback),
        ],
        unbinds: Vec::new(),
        commands: Vec::new(),
        modes: vec![mode("resize", true, false), mode("go", false, true)],
    });
    cx.update(|cx| {
        install_key_handler(cx);
        install_mode_keys(cx);
        crate::commands::rebind_keys(cx, &ShortcutOverrides::new());
    });
    let window = cx.add_empty_window();
    let current = || {
        MODE.lock()
            .unwrap()
            .as_ref()
            .map(|mode| mode.name.clone())
    };

    window.simulate_keystrokes(&crate::commands::test_chords("cmd-shift-r"));
    assert_eq!(current().as_deref(), Some("resize"), "entered without the script thread");
    assert!(rx.try_recv().is_err());
    window.simulate_keystrokes(&crate::commands::test_chords("h"));
    match rx.try_recv() {
        Ok(Input::Binding { id: 3, context }) => assert_eq!(context["origin"], "mode"),
        _ => panic!("a mode key reaches the script"),
    }
    // Exclusive: an unbound key is swallowed and the mode stays.
    window.simulate_keystrokes(&crate::commands::test_chords("cmd-shift-j"));
    assert!(rx.try_recv().is_err());
    assert_eq!(current().as_deref(), Some("resize"));
    window.simulate_keystrokes(&crate::commands::test_chords("q"));
    assert_eq!(current(), None, "a binding can leave the mode");

    window.simulate_keystrokes(&crate::commands::test_chords("cmd-shift-r"));
    window.simulate_keystrokes(&crate::commands::test_chords("escape"));
    assert_eq!(current(), None, "escape leaves a mode that does not bind it");

    // Once: the next key, bound or not, ends the mode.
    window.simulate_keystrokes(&crate::commands::test_chords("cmd-shift-g"));
    window.simulate_keystrokes(&crate::commands::test_chords("t"));
    assert!(matches!(rx.try_recv(), Ok(Input::Binding { id: 5, .. })));
    assert_eq!(current(), None);

    // A mode's keys never enter the normal keymap.
    let (bindings, _) = key_bindings(
        LAYER.read().unwrap().as_ref().unwrap(),
        &ShortcutOverrides::new(),
    );
    assert_eq!(bindings.len(), 2);

    *RUNNING.lock().unwrap() = None;
    *LAYER.write().unwrap() = None;
    *MODE.lock().unwrap() = None;
    MODE_ON.store(false, Ordering::Relaxed);
    ACTIVE.store(false, Ordering::Relaxed);
}

#[test]
fn script_errors_and_notices_become_toasts() {
    let error = toast_for(Notice {
        level: Level::Error,
        message: "diri.on(\"focus\")".into(),
        detail: Some("init.lua:3: boom".into()),
        logged: true,
    });
    assert_eq!(error.message, "Script error in diri.on(\"focus\")");
    assert_eq!(error.detail.as_deref(), Some("init.lua:3: boom"));
    let note = toast_for(Notice {
        level: Level::Success,
        message: "Deployed".into(),
        detail: None,
        logged: false,
    });
    assert_eq!(note.tone, crate::toast::ToastTone::Success);
}

/// `docs/scripting-api.md` and the editor stubs are generated from the
/// Engine's method catalog and this build's action catalog. Regenerate:
/// `DIRI_UPDATE_DOCS=1 cargo test -p diri-app scripting_reference`.
#[test]
fn scripting_reference_matches_the_catalogs() {
    let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
    let methods = diri_script::api::methods();
    let actions = crate::app_actions::catalog();
    let files = [
        (
            docs.join("scripting-api.md"),
            diri_script::api::reference_markdown(methods, &actions),
        ),
        (
            docs.join("scripting/types/diri.lua"),
            diri_script::api::lua_definitions(methods, &actions),
        ),
    ];
    for (path, generated) in files {
        if std::env::var_os("DIRI_UPDATE_DOCS").is_some() {
            std::fs::write(&path, &generated).unwrap();
            continue;
        }
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            current == generated,
            "{} is stale. Run: DIRI_UPDATE_DOCS=1 cargo test -p diri-app scripting_reference",
            path.display()
        );
    }
}
