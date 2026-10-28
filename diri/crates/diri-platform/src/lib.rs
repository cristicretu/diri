//! Native OS mechanisms shared by the Engine, Holder and its clients.
//! No product state, terminal parsing or orchestration lives here.
pub mod ipc;
pub mod launch;
pub mod pipe;
pub mod poll;
pub mod private;
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

/// The `curl` every HTTPS request goes through, resolved once per process.
pub fn curl_executable() -> &'static std::path::Path {
    static CURL: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    CURL.get_or_init(|| {
        #[cfg(windows)]
        {
            std::path::PathBuf::from(
                std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()),
            )
            .join("System32/curl.exe")
        }
        #[cfg(not(windows))]
        {
            system_tool("curl", option_env!("DIRI_CURL"))
        }
    })
}

/// The `gzip` telemetry compresses with, resolved once per process.
pub fn gzip_executable() -> &'static std::path::Path {
    static GZIP: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    GZIP.get_or_init(|| system_tool("gzip", option_env!("DIRI_GZIP")))
}

/// A path pinned at build time (the Nix package sets `DIRI_CURL`/`DIRI_GZIP`
/// to store paths), else `/usr/bin/<name>`, else `<name>` on `PATH` for
/// systems without `/usr/bin` tools such as NixOS and Guix.
fn system_tool(name: &str, pinned: Option<&str>) -> std::path::PathBuf {
    if let Some(pinned) = pinned.filter(|path| !path.is_empty()) {
        return pinned.into();
    }
    let system = std::path::Path::new("/usr/bin").join(name);
    if system.exists() { system } else { name.into() }
}

#[cfg(test)]
mod tests {
    use super::system_tool;

    #[test]
    fn system_tool_prefers_the_build_time_path() {
        assert_eq!(
            system_tool("curl", Some("/nix/store/x-curl/bin/curl")),
            std::path::Path::new("/nix/store/x-curl/bin/curl"),
        );
    }

    #[test]
    fn system_tool_falls_back_to_path_without_usr_bin() {
        assert_eq!(
            system_tool("diri-no-such-tool", Some("")),
            std::path::Path::new("diri-no-such-tool"),
        );
    }
}
