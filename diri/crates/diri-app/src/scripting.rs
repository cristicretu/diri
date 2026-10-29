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
//! action with arguments) dispatches [`RunScriptKey`], which this module
//! forwards to the script thread.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock, mpsc};
use std::time::Duration;

use diri_proto::SessionId;
use diri_script::driver::{Config, Driver, Input, Sink, Source};
use diri_script::engine::EngineClient;
use diri_script::{BindingTarget, Host, HostError, Layer, Level, LoadError, Notice, Options};
use gpui::{App, KeyBinding, Keystroke, NoAction};
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

const SCRIPT_LOG: &str = "scripts.log";
const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// What the script thread tells the UI thread.
enum Ui {
    Layer(Layer),
    Notice(Notice),
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
    cx.on_action(|key: &RunScriptKey, _| {
        send(Input::Binding(key.id));
    });
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
pub fn emit_focus(session: &SessionId) {
    if ACTIVE.load(Ordering::Relaxed) {
        send(Input::App {
            name: diri_script::events::FOCUS.into(),
            payload: json!({"session": session.0}),
        });
    }
}

/// A script palette command was chosen.
pub fn run_command(id: u64) {
    send(Input::Command(id));
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
            *LAYER.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(layer);
            crate::commands::rebind_active_keys(cx);
        }
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
            .filter(|parsed| !parsed.is_empty() && parsed.iter().all(|key| known_key(&key.key)))
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
        if !usable(&binding.keys) {
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

/// GPUI accepts any word as a key name; one that is not a key would never
/// fire, so it is reported instead.
fn known_key(key: &str) -> bool {
    const NAMED: &[&str] = &[
        "space", "enter", "escape", "backspace", "tab", "delete", "insert", "home", "end",
        "pageup", "pagedown", "left", "right", "up", "down",
    ];
    key.chars().count() == 1
        || NAMED.contains(&key)
        || key
            .strip_prefix('f')
            .and_then(|number| number.parse::<u8>().ok())
            .is_some_and(|number| (1..=24).contains(&number))
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
