//! Native OS mechanisms shared by the Engine, Holder and its clients.
//! No product state, terminal parsing or orchestration lives here.
pub mod ipc;
pub mod launch;
pub mod pipe;
pub mod poll;
pub mod security;
pub mod signals;

#[cfg(windows)]
pub use windows_sys;

pub fn home_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    let value = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let value = std::env::var_os("HOME");
    value
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// `std::fs::canonicalize`, except that Windows results use the ordinary
/// `C:\...` / `\\server\share` form whenever it names the same file. Verbatim
/// `\\?\` paths break Git, shells and Agents, and never compare equal to the
/// paths those tools report.
pub fn canonicalize(path: impl AsRef<std::path::Path>) -> std::io::Result<std::path::PathBuf> {
    #[cfg(windows)]
    {
        dunce::canonicalize(path)
    }
    #[cfg(not(windows))]
    {
        std::fs::canonicalize(path)
    }
}

/// The GUI and the Engine have no visible console, so Windows opens a new
/// console window for every console child unless it is created without one.
pub fn hide_console_window(command: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    command
}

pub fn executable_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.into()
    }
}

pub mod directory;

#[cfg(windows)]
pub mod process;

#[cfg(windows)]
pub mod job;

pub mod child;

pub fn curl_executable() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        std::path::PathBuf::from(
            std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()),
        )
        .join("System32/curl.exe")
    }
    #[cfg(unix)]
    {
        "/usr/bin/curl".into()
    }
}
