//! The person's Lua scripts inside the app: `~/.config/diri/init.lua`.
//!
//! Nothing runs, watches or connects unless that file exists (or Reload
//! Scripts is chosen after creating it). When it does, one `diri-script`
//! thread owns the Lua state ([`diri_script::driver`]); it reaches the
//! Engine over its own control connection, never the UI thread, and the app
//! only hears back three things through [`Ui`]: the script's keys and
//! palette commands, notices to toast, and load results.
//!
//! Keys form one layer: over the shipped defaults, under the shortcuts the
//! person records in Settings. A key bound to a window command dispatches
//! that command's GPUI action directly; anything else (a Lua function, an
//! action with arguments, entering a key mode) dispatches [`RunScriptKey`],
//! which this module forwards to the script thread or handles itself.
//!
//! Key modes (`diri.keymap.mode`) are read before any binding by a
//! keystroke interceptor that exists only once a script declares a mode;
//! with no mode on, it is one relaxed atomic load per key.
//!
//! Script palette commands are also published as window actions
//! (`script.NAME`), so `dirijor do script.tidy` and Agents run them, and get
//! back what their function returned.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock, mpsc};
use std::time::Duration;

use diri_proto::SessionId;
use diri_proto::app_actions::{
    ActionRisk, AppActionSpec, AppPerformRequest, UNKNOWN_ACTION,
};
use diri_proto::control::ControlError;
use diri_script::driver::{Config, Driver, Input, Sink, Source};
use diri_script::engine::EngineClient;
use diri_script::{
    BindingTarget, COMMAND_PREFIX, GrantRequest, Host, HostError, Layer, Level, LoadError, Mode,
    Notice, Options,
};
use gpui::{App, KeyBinding, KeybindingKeystroke, Keystroke, NoAction};
use serde_json::{Value, json};

use crate::commands::{COMMANDS, ShortcutOverrides};
use crate::i18n::{t, tf};
use crate::toast::Toast;

/// A key a script bound to a function, or to an action with arguments.
#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = diri_script, no_json)]
pub struct RunScriptKey {
    pub id: u64,
}

struct Running {
    tx: mpsc::Sender<Input>,
    /// Kept alive for hot reload.
    _watcher: Option<notify::RecommendedWatcher>,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);
/// A fast "no scripts" check for hot paths such as focus changes.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// The loaded script's keys and commands.
static LAYER: RwLock<Option<Layer>> = RwLock::new(None);
/// The window, session and pane that had focus last, for callbacks'
/// context. Kept only while a script runs.
static FOCUS: Mutex<Option<Focus>> = Mutex::new(None);
/// The key mode that is on, if any.
static MODE: Mutex<Option<Mode>> = Mutex::new(None);
/// A fast "no mode is on" check for every keystroke.
static MODE_ON: AtomicBool = AtomicBool::new(false);
/// Whether the mode interceptor is installed (once a script declares a mode).
static MODE_KEYS: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug, PartialEq)]
struct Focus {
    session: String,
    pane: Option<String>,
    window: u64,
}

const SCRIPT_LOG: &str = "scripts.log";
/// Script commands published as window actions, at most.
const MAX_PUBLISHED_COMMANDS: usize = 128;
const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// What the script thread tells the UI thread.
enum Ui {
    Layer(Layer),
    Notice(Notice),
    /// `diri.keymap.enter(name)` / `diri.keymap.exit()`.
    Mode(Option<String>),
    Loaded {
        result: Result<(), String>,
        first: bool,
    },
}

/// Starts the person's `init.lua`, if there is one. Installs the handler
/// for script keys either way (it does nothing until a script binds one).
pub fn start(cx: &mut App) {
    install_key_handler(cx);
    if let Some(path) = diri_script::init_file()
        && path.is_file()
    {
        spawn(path, cx);
    }
}

fn install_key_handler(cx: &mut App) {
    cx.on_action(|key: &RunScriptKey, cx| {
        let target = layer_binding(key.id).map(|binding| binding.target);
        match target {
            Some(BindingTarget::Mode { name }) => set_mode(name.as_deref(), cx),
            _ => {
                send(Input::Binding {
                    id: key.id,
                    context: context("key"),
                });
            }
        }
    });
}

fn layer_binding(id: u64) -> Option<diri_script::Binding> {
    let guard = LAYER.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .as_ref()?
        .bindings
        .iter()
        .find(|binding| binding.id == id)
        .cloned()
}

/// Whether a script is running. Cheap: one relaxed load.
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// What a script callback receives: where the person is, and what started
/// it (`key`, `mode`, `palette`, `cli`, `agent`).
pub fn context(origin: &str) -> Value {
    let focus = FOCUS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    match focus {
        Some(focus) => json!({
            "session": focus.session,
            "pane": focus.pane,
            "window": focus.window,
            "origin": origin,
        }),
        None => json!({"origin": origin}),
    }
}

/// Reload Scripts: runs `init.lua` again, or starts it when it is new.
pub fn reload(cx: &mut App) {
    if send(Input::Reload) {
        return;
    }
    match diri_script::init_file() {
        Some(path) if path.is_file() => spawn(path, cx),
        path => show_toast(
            Toast::info(tf(
                "toast.scripts_missing",
                &[(
                    "path",
                    &path
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "~/.config/diri/init.lua".into()),
                )],
            )),
            cx,
        ),
    }
}

/// The window's selected session changed.
pub fn emit_focus(session: &SessionId, pane: Option<String>, window: u64) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    *FOCUS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Focus {
        session: session.0.clone(),
        pane,
        window,
    });
    send(Input::App {
        name: diri_script::events::FOCUS.into(),
        payload: json!({"session": session.0}),
    });
}

/// A script palette command was chosen.
pub fn run_command(id: u64) {
    send(Input::Command {
        id,
        context: context("palette"),
        reply: None,
    });
}

/// The script commands, as window actions (`script.NAME`) for the catalog
/// the app publishes. Empty without a script.
pub fn action_specs() -> Vec<AppActionSpec> {
    let guard = LAYER.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(layer) = guard.as_ref() else {
        return Vec::new();
    };
    // The Engine refuses a catalog past `MAX_ACTIONS`; a script with an
    // enormous palette must not take the built-in actions down with it.
    layer
        .commands
        .iter()
        .take(MAX_PUBLISHED_COMMANDS)
        .map(|command| AppActionSpec {
            name: format!("{COMMAND_PREFIX}{}", command.name),
            title: command.title.clone(),
            description: command
                .description
                .clone()
                .unwrap_or_else(|| format!("A command from {}", source_file(&command.source))),
            category: "Script".into(),
            // A script reads what it was given from `ctx.args`.
            args: json!({"type": "object"}),
            risk: if command.destructive {
                ActionRisk::Destructive
            } else {
                ActionRisk::Safe
            },
            shortcut: None,
        })
        .collect()
}

fn source_file(source: &Option<String>) -> String {
    source
        .as_deref()
        .and_then(|source| source.rsplit_once(':').map(|(file, _)| file))
        .unwrap_or(diri_script::INIT_FILE)
        .to_owned()
}

/// Whether `action` names a script command (`script.tidy`).
pub fn is_script_action(action: &str) -> bool {
    action.starts_with(COMMAND_PREFIX)
}

/// The title a script command shows in the capsule.
pub fn action_title(action: &str) -> Option<String> {
    let guard = LAYER.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .as_ref()?
        .command_for_action(action)
        .map(|command| command.title.clone())
}

/// Runs a script command an Agent, a script or `dirijor do` asked for.
/// The answer arrives when the command's function returns.
pub fn perform(
    request: &AppPerformRequest,
) -> Result<tokio::sync::oneshot::Receiver<Result<Value, ControlError>>, ControlError> {
    let id = {
        let guard = LAYER.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .as_ref()
            .and_then(|layer| layer.command_for_action(&request.action))
            .map(|command| command.id)
    };
    let Some(id) = id else {
        return Err(ControlError::new(
            UNKNOWN_ACTION,
            format!("no script command {:?}", request.action),
        ));
    };
    let mut context = context(if request.actor.is_some() {
        "agent"
    } else {
        "cli"
    });
    context["args"] = request.args.clone();
    if let Some(actor) = &request.actor {
        context["actor"] = json!(actor.0);
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let reply: diri_script::Reply = Box::new(move |result| {
        let _ = tx.send(result.map_err(|error| ControlError::new("script_error", error)));
    });
    if !send(Input::Command {
        id,
        context,
        reply: Some(reply),
    }) {
        return Err(ControlError::new(
            "script_error",
            "the script is not running",
        ));
    }
    Ok(rx)
}

/// Palette rows for the script's commands.
pub fn palette_actions() -> Vec<crate::palette::PaletteAction> {
    if !ACTIVE.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let guard = LAYER.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(layer) = guard.as_ref() else {
        return Vec::new();
    };
    layer
        .commands
        .iter()
        .map(|command| crate::palette::PaletteAction {
            id: format!("script:{}", command.name),
            title: command.title.clone(),
            system_image: "chevron.left.forwardslash.chevron.right",
            shortcut: None,
            detail: Some(t("palette.script").into()),
            enabled: true,
            is_default: false,
            command: crate::palette::PaletteCommand::Script { id: command.id },
            keywords: format!("script lua {}", command.name.replace(['-', '_', '.'], " ")),
        })
        .collect()
}

fn send(input: Input) -> bool {
    let running = RUNNING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    running
        .as_ref()
        .is_some_and(|running| running.tx.send(input).is_ok())
}

fn spawn(path: PathBuf, cx: &mut App) {
    let (tx, rx) = mpsc::channel();
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel::<Ui>();
    let watcher = path.parent().and_then(|dir| watch(dir, tx.clone()));
    let socket = diri_platform::home_dir()
        .map(diri_proto::paths::DirijorPaths::socket)
        .unwrap_or_else(diri_script::engine::default_socket_path);
    let driver_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("diri-script".into())
        .spawn(move || {
            let build = concat!("diri-app-script-", env!("CARGO_PKG_VERSION"));
            let host_socket = socket.clone();
            let host_ui = ui_tx.clone();
            let mut driver = Driver::new(
                Config {
                    source: Source::File(path),
                    options: Options::default(),
                    socket,
                    build: build.into(),
                    exit_when_idle: false,
                },
                Box::new(move || {
                    Box::new(AppHost {
                        client: EngineClient::new(host_socket.clone(), build),
                        ui: host_ui.clone(),
                    })
                }),
                Box::new(AppSink { ui: ui_tx }),
                driver_tx,
            );
            let _ = driver.load();
            driver.run(&rx, None);
        });
    if spawned.is_err() {
        return;
    }
    *RUNNING.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Running {
        tx,
        _watcher: watcher,
    });
    ACTIVE.store(true, Ordering::Relaxed);
    cx.spawn(async move |cx| {
        while let Some(message) = ui_rx.recv().await {
            cx.update(|cx| apply(message, cx));
        }
    })
    .detach();
}

/// Reloads when a `.lua` file beside `init.lua` changes.
fn watch(dir: &Path, tx: mpsc::Sender<Input>) -> Option<notify::RecommendedWatcher> {
    use notify::{EventKind, RecursiveMode, Watcher};
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            return;
        };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        if event
            .paths
            .iter()
            .any(|path| path.extension().is_some_and(|ext| ext == "lua"))
        {
            let _ = tx.send(Input::Reload);
        }
    })
    .ok()?;
    watcher.watch(dir, RecursiveMode::Recursive).ok()?;
    Some(watcher)
}

fn apply(message: Ui, cx: &mut App) {
    match message {
        Ui::Layer(layer) => {
            let modes = !layer.modes.is_empty();
            let commands_changed = {
                let mut guard = LAYER.write().unwrap_or_else(|poisoned| poisoned.into_inner());
                let before = guard.as_ref().map(published_commands).unwrap_or_default();
                let after = published_commands(&layer);
                *guard = Some(layer);
                before != after
            };
            // A mode that is gone, or changed, ends.
            let stale = MODE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
                .is_some_and(|mode| find_mode(&mode.name).as_ref() != Some(&mode));
            if stale {
                set_mode(None, cx);
            }
            if modes {
                install_mode_keys(cx);
            }
            crate::commands::rebind_active_keys(cx);
            if commands_changed {
                republish_actions(cx);
            }
        }
        Ui::Mode(name) => set_mode(name.as_deref(), cx),
        Ui::Notice(notice) => show_toast(toast_for(notice), cx),
        Ui::Loaded { result, first } => match result {
            Ok(()) if first => {}
            Ok(()) => show_toast(Toast::success(t("toast.scripts_reloaded")), cx),
            Err(error) => show_toast(
                Toast::error(t("toast.scripts_failed"))
                    .detail(error.lines().next().unwrap_or_default().to_owned()),
                cx,
            ),
        },
    }
}

/// What the published catalog says about the script's commands.
fn published_commands(layer: &Layer) -> Vec<(String, String, Option<String>, bool)> {
    layer
        .commands
        .iter()
        .map(|command| {
            (
                command.name.clone(),
                command.title.clone(),
                command.description.clone(),
                command.destructive,
            )
        })
        .collect()
}

/// Publishes the window's actions again, now with the script's commands.
fn republish_actions(cx: &mut App) {
    let target = cx
        .windows()
        .into_iter()
        .find_map(|window| window.downcast::<crate::root::RootView>());
    if let Some(window) = target {
        let _ = window.update(cx, |root, _, _| root.republish_app_actions());
    }
}

fn find_mode(name: &str) -> Option<Mode> {
    let guard = LAYER.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .as_ref()?
        .modes
        .iter()
        .find(|mode| mode.name == name)
        .cloned()
}

/// Enters key mode `name`, or leaves the current one.
fn set_mode(name: Option<&str>, cx: &mut App) {
    let mode = name.and_then(find_mode);
    if let Some(name) = name
        && mode.is_none()
    {
        log(Level::Warning, &format!("no key mode is named \"{name}\""));
        return;
    }
    let announce = mode
        .as_ref()
        .filter(|mode| !mode.once)
        .map(|mode| mode.description.clone().unwrap_or_else(|| mode.name.clone()));
    MODE_ON.store(mode.is_some(), Ordering::Relaxed);
    *MODE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = mode;
    if let Some(label) = announce {
        show_toast(
            Toast::ambient(tf("toast.script_mode", &[("mode", &label)])),
            cx,
        );
    }
}

/// Reads keys for the mode that is on, before any binding. Installed once,
/// the first time a script declares a mode.
fn install_mode_keys(cx: &mut App) {
    if MODE_KEYS.swap(true, Ordering::Relaxed) {
        return;
    }
    cx.intercept_keystrokes(|event, window, cx| {
        if !MODE_ON.load(Ordering::Relaxed) {
            return;
        }
        if matches!(
            event.keystroke.key.as_str(),
            "shift" | "control" | "alt" | "platform" | "function"
        ) {
            return;
        }
        let Some(mode) = MODE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return;
        };
        match mode_key(&mode, &event.keystroke) {
            ModeKey::Run(binding) => {
                cx.stop_propagation();
                if mode.once {
                    set_mode(None, cx);
                }
                run_mode_binding(binding, window, cx);
            }
            ModeKey::Leave => {
                cx.stop_propagation();
                set_mode(None, cx);
            }
            ModeKey::Swallow => {
                cx.stop_propagation();
                if mode.once {
                    set_mode(None, cx);
                }
            }
            ModeKey::PassThrough => {
                if mode.once {
                    set_mode(None, cx);
                }
            }
        }
    })
    .detach();
}

#[derive(Debug, PartialEq)]
enum ModeKey {
    Run(diri_script::Binding),
    /// Escape, unbound in the mode.
    Leave,
    /// Unbound in an exclusive mode.
    Swallow,
    PassThrough,
}

fn mode_key(mode: &Mode, keystroke: &Keystroke) -> ModeKey {
    let binding = {
        let guard = LAYER.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.as_ref().and_then(|layer| {
            layer
                .bindings
                .iter()
                .filter(|binding| binding.mode.as_deref() == Some(mode.name.as_str()))
                .find(|binding| {
                    Keystroke::parse(&binding.keys).is_ok_and(|parsed| {
                        keystroke.should_match(&KeybindingKeystroke::from_keystroke(parsed))
                    })
                })
                .cloned()
        })
    };
    match binding {
        Some(binding) => ModeKey::Run(binding),
        None if keystroke.key == "escape" && !keystroke.modifiers.modified() => ModeKey::Leave,
        None if mode.exclusive => ModeKey::Swallow,
        None => ModeKey::PassThrough,
    }
}

fn run_mode_binding(binding: diri_script::Binding, window: &mut gpui::Window, cx: &mut App) {
    match &binding.target {
        BindingTarget::Mode { name } => set_mode(name.as_deref(), cx),
        BindingTarget::Action { name, args } if args.as_object().is_none_or(|args| args.is_empty()) => {
            match COMMANDS.iter().find(|command| command.stable_id == name) {
                Some(command) => window.dispatch_action(command.id.action(), cx),
                None => {
                    send(Input::Binding {
                        id: binding.id,
                        context: context("mode"),
                    });
                }
            }
        }
        _ => {
            send(Input::Binding {
                id: binding.id,
                context: context("mode"),
            });
        }
    }
}

/// `list-keys`: every binding the window has, what it runs and where it
/// came from (`default`, `recorded`, or the script's `file:line`), plus the
/// script's key modes.
pub fn keymap_listing() -> Value {
    let overrides = crate::commands::active_overrides();
    let mut bindings: Vec<Value> = Vec::new();
    let recorded = |stable_id: &str| {
        overrides
            .keys()
            .any(|key| key == stable_id || key.starts_with(&format!("{stable_id}#")))
    };
    for command in COMMANDS {
        let source = if recorded(command.stable_id) {
            "recorded"
        } else {
            "default"
        };
        for keys in command.effective_keystrokes(&overrides) {
            bindings.push(json!({
                "keys": keys,
                "action": command.stable_id,
                "source": source,
            }));
        }
    }
    let guard = LAYER.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut modes = Vec::new();
    if let Some(layer) = guard.as_ref() {
        let (_, skipped) = key_bindings(layer, &overrides);
        for keys in &layer.unbinds {
            bindings.push(json!({"keys": keys, "action": "none", "source": diri_script::INIT_FILE}));
        }
        for binding in &layer.bindings {
            let (action, args) = match &binding.target {
                BindingTarget::Action { name, args } => (name.clone(), args.clone()),
                BindingTarget::Callback => ("lua function".to_owned(), Value::Null),
                BindingTarget::Mode { name: Some(name) } => {
                    ("enter-mode".to_owned(), json!({"name": name}))
                }
                BindingTarget::Mode { name: None } => ("exit-mode".to_owned(), Value::Null),
            };
            let shadowed = binding.mode.is_none()
                && skipped
                    .iter()
                    .any(|line| line.starts_with(&format!("key \"{}\":", binding.keys)));
            bindings.push(json!({
                "keys": binding.keys,
                "mode": binding.mode,
                "action": action,
                "args": args,
                "description": binding.description,
                "source": binding.source.clone().unwrap_or_else(|| diri_script::INIT_FILE.into()),
                "skipped": shadowed,
            }));
        }
        for mode in &layer.modes {
            modes.push(json!({
                "name": mode.name,
                "exclusive": mode.exclusive,
                "once": mode.once,
                "description": mode.description,
                "source": mode.source,
            }));
        }
    }
    json!({"bindings": bindings, "modes": modes})
}

fn toast_for(notice: Notice) -> Toast {
    let toast = if notice.logged {
        // A script error; `message` names where it happened.
        Toast::error(tf("toast.script_error", &[("what", &notice.message)]))
    } else {
        match notice.level {
            Level::Info => Toast::info(notice.message),
            Level::Success => Toast::success(notice.message),
            Level::Warning => Toast::warning(notice.message),
            Level::Error => Toast::error(notice.message),
        }
    };
    match notice.detail {
        Some(detail) => toast.detail(detail),
        None => toast,
    }
}

/// Toasts in the active window, else the first main window.
fn show_toast(toast: Toast, cx: &mut App) {
    let target = cx
        .active_window()
        .and_then(|window| window.downcast::<crate::root::RootView>())
        .or_else(|| {
            cx.windows()
                .into_iter()
                .find_map(|window| window.downcast::<crate::root::RootView>())
        });
    if let Some(window) = target {
        let _ = window.update(cx, |root, _, cx| root.show_script_toast(toast, cx));
    }
}

/// The script layer's key bindings, bound after every other binding: a
/// binding without a context ranks with the deepest context, and among
/// equals the later one wins. Keys the person recorded in Settings are
/// left to them.
pub fn bind_keys(cx: &mut App, overrides: &ShortcutOverrides) {
    let guard = LAYER.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(layer) = guard.as_ref() else {
        return;
    };
    let (bindings, skipped) = key_bindings(layer, overrides);
    drop(guard);
    for line in skipped {
        log(Level::Warning, &line);
    }
    cx.bind_keys(bindings);
}

fn key_bindings(layer: &Layer, overrides: &ShortcutOverrides) -> (Vec<KeyBinding>, Vec<String>) {
    let recorded: Vec<Keystroke> = overrides
        .values()
        .flatten()
        .filter_map(|key| Keystroke::parse(key).ok())
        .collect();
    let mut skipped = Vec::new();
    let mut usable = |keys: &str| -> bool {
        let parsed: Result<Vec<Keystroke>, _> =
            keys.split_whitespace().map(Keystroke::parse).collect();
        let Some(parsed) = parsed
            .ok()
            .filter(|parsed| {
                !parsed.is_empty()
                    && parsed
                        .iter()
                        .all(|key| diri_script::keys::known_key(&key.key))
            })
        else {
            skipped.push(format!(
                "key \"{keys}\": not a key; write it like cmd-shift-d or \"ctrl-a c\""
            ));
            return false;
        };
        if let [single] = parsed.as_slice()
            && recorded
                .iter()
                .any(|key| key.key == single.key && key.modifiers == single.modifiers)
        {
            skipped.push(format!(
                "key \"{keys}\": recorded in Settings › Shortcuts, which wins over init.lua"
            ));
            return false;
        }
        true
    };
    let mut bindings = Vec::new();
    for keys in &layer.unbinds {
        if usable(keys) {
            bindings.push(KeyBinding::new(keys, NoAction, None));
        }
    }
    for binding in &layer.bindings {
        // A mode's keys are read by the mode interceptor, not the keymap.
        if binding.mode.is_some() || !usable(&binding.keys) {
            continue;
        }
        let command = match &binding.target {
            BindingTarget::Action { name, args }
                if args.as_object().is_none_or(|args| args.is_empty()) =>
            {
                COMMANDS.iter().find(|command| command.stable_id == name)
            }
            _ => None,
        };
        let keys = binding.keys.as_str();
        bindings.push(match command {
            Some(command) => {
                match KeyBinding::load(
                    keys,
                    command.id.action(),
                    None,
                    false,
                    None,
                    &gpui::DummyKeyboardMapper,
                ) {
                    Ok(binding) => binding,
                    Err(_) => continue,
                }
            }
            None => KeyBinding::new(keys, RunScriptKey { id: binding.id }, None),
        });
    }
    (bindings, skipped)
}

/// The script log: `scripts.log` beside the Engine's logs, owner-only,
/// rolled over to `scripts.log.1` past 1 MiB.
fn log(level: Level, message: &str) {
    let Some(home) = diri_platform::home_dir() else {
        return;
    };
    let dir = diri_proto::paths::DirijorPaths::logs_dir(home);
    let path = dir.join(SCRIPT_LOG);
    let _ = std::fs::create_dir_all(&dir);
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > MAX_LOG_BYTES) {
        let _ = std::fs::rename(&path, dir.join(format!("{SCRIPT_LOG}.1")));
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    if let Ok(mut file) = options.open(&path) {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or_default();
        let _ = writeln!(file, "{seconds} {} {message}", level.label());
    }
}

struct AppHost {
    client: EngineClient,
    ui: tokio::sync::mpsc::UnboundedSender<Ui>,
}

impl Host for AppHost {
    fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, HostError> {
        self.client.request(method, params, timeout)
    }

    fn notify(&mut self, notice: Notice) {
        let _ = self.ui.send(Ui::Notice(notice));
    }

    fn log(&mut self, level: Level, message: &str) {
        log(level, message);
    }

    fn context(&self) -> &'static str {
        "app"
    }

    fn actions(&mut self) -> Result<Vec<diri_proto::app_actions::AppActionSpec>, HostError> {
        Ok(crate::app_actions::catalog())
    }

    /// `init.lua` is the person's own file: the line that asks is the grant.
    fn grant(&mut self, request: &GrantRequest) -> bool {
        log(
            Level::Info,
            &format!(
                "{} may run: {}",
                request.script,
                request.exec.join(", ")
            ),
        );
        true
    }

    fn set_mode(&mut self, name: Option<&str>) -> Result<(), HostError> {
        let _ = self.ui.send(Ui::Mode(name.map(str::to_owned)));
        Ok(())
    }
}

struct AppSink {
    ui: tokio::sync::mpsc::UnboundedSender<Ui>,
}

impl Sink for AppSink {
    fn layer(&mut self, layer: Layer) {
        let _ = self.ui.send(Ui::Layer(layer));
    }

    fn loaded(&mut self, result: &Result<(), LoadError>, first: bool) {
        match result {
            Ok(()) => log(Level::Info, "init.lua loaded"),
            Err(error) => log(Level::Error, &format!("init.lua: {error}")),
        }
        let _ = self.ui.send(Ui::Loaded {
            result: result.clone().map_err(|error| error.message),
            first,
        });
    }
}

#[cfg(test)]
mod tests;
