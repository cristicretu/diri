//! Diri's scripting layer: Lua 5.4 scripts that drive the app and the
//! Engine through the same API agents and the `dirijor` CLI use.
//!
//! - [`Runtime`] is one sandboxed Lua state. It installs the `diri` table:
//!   `diri.on`, `diri.call`, `diri.action`, `diri.keymap`, `diri.command`,
//!   `diri.notify`, timers, coroutine waits (`diri.sleep`, `diri.wait`),
//!   granted processes (`diri.exec`), and one function per Engine method generated
//!   from the method catalog's JSON Schemas ([`api`]).
//! - A [`Host`] is what the runtime talks to: the Engine socket, plus the
//!   app's toasts and log when it runs inside Diri, or stdout when it runs
//!   from `dirijor script`.
//! - [`driver`] runs a runtime on its own thread: Engine events, key
//!   presses, palette commands, timers and reloads all arrive on one channel,
//!   so callbacks never run on the UI thread or near a PTY.
//!
//! Design and the full API: `diri/docs/scripting.md`.

pub mod api;
pub mod driver;
pub mod engine;
pub mod events;
pub mod keys;
mod runtime;

pub use runtime::{
    Binding, BindingTarget, COMMAND_PREFIX, Command, GrantRequest, Host, HostError, Layer, Level,
    LoadError, Mode, Notice, Options, Reply, Runtime, Summary, Waker,
};

/// The file Diri loads at launch, if it exists.
pub const INIT_FILE: &str = "init.lua";

/// Where `init.lua` lives: `$DIRI_CONFIG_DIR`, else `$XDG_CONFIG_HOME/diri`,
/// else `~/.config/diri` (on every platform, so a dotfiles repo works the
/// same on macOS and Linux).
pub fn config_dir() -> Option<std::path::PathBuf> {
    let from_env = |name: &str| {
        std::env::var_os(name)
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    if let Some(dir) = from_env("DIRI_CONFIG_DIR") {
        return Some(dir);
    }
    if let Some(dir) = from_env("XDG_CONFIG_HOME") {
        return Some(dir.join("diri"));
    }
    diri_platform::home_dir().map(|home| home.join(".config").join("diri"))
}

/// `config_dir()/init.lua`.
pub fn init_file() -> Option<std::path::PathBuf> {
    config_dir().map(|dir| dir.join(INIT_FILE))
}
