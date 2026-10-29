use std::path::Path;
use std::sync::Mutex;

use diri_script::{Binding, Command};
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
        }],
    });
    cx.update(|cx| {
        install_key_handler(cx);
        crate::commands::rebind_keys(cx, &ShortcutOverrides::new());
    });
    let window = cx.add_empty_window();
    window.simulate_keystrokes(&crate::commands::test_chords("cmd-shift-j"));
    assert!(matches!(rx.try_recv(), Ok(Input::Binding(7))));

    let rows = palette_actions();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "Say Hi");
    assert_eq!(
        rows[0].command,
        crate::palette::PaletteCommand::Script { id: 9 }
    );
    run_command(9);
    assert!(matches!(rx.try_recv(), Ok(Input::Command(9))));

    emit_focus(&SessionId::new("s-1"));
    match rx.try_recv() {
        Ok(Input::App { name, payload }) => {
            assert_eq!(name, "focus");
            assert_eq!(payload, json!({"session": "s-1"}));
        }
        _ => panic!("focus reaches the script"),
    }

    *RUNNING.lock().unwrap() = None;
    *LAYER.write().unwrap() = None;
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
