//! One-time move of Engine files that earlier Linux builds kept beside the
//! control socket.
//!
//! Up to 0.9.2 the Engine derived `agents.json`, `accounts.json` and
//! `remote-bindings/` from the socket's directory. On macOS that is the one
//! App Support root, but on Linux it is `$XDG_RUNTIME_DIR/diri`: a tmpfs the
//! session manager empties at logout, so Agent settings and remote session
//! bindings did not survive a reboot. `hosts.json` was read from there too,
//! while the app writes it to the config directory, so every host the user
//! added was "unknown" to the Engine.
//!
//! The move runs under the daemon lock, before the stores open. It never
//! overwrites a file at the destination, and never follows a symlink.

use std::fs;
use std::io;
use std::path::Path;

const CONFIG_FILES: [&str; 2] = ["agents.json", "accounts.json"];
pub const REMOTE_BINDINGS_DIR_NAME: &str = "remote-bindings";

/// What [`adopt_runtime_files`] moved, for one startup log line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Moved {
    pub files: usize,
    pub failed: usize,
}

/// Moves files from the legacy `runtime_dir` into `config_dir` (Agent and
/// account configuration) and `state_dir` (remote session bindings). A no-op
/// when the directories coincide, as they do on macOS.
pub fn adopt_runtime_files(runtime_dir: &Path, config_dir: &Path, state_dir: &Path) -> Moved {
    let mut moved = Moved::default();
    if runtime_dir != config_dir {
        for name in CONFIG_FILES {
            tally(
                &mut moved,
                move_file(&runtime_dir.join(name), &config_dir.join(name)),
            );
        }
    }
    let legacy = runtime_dir.join(REMOTE_BINDINGS_DIR_NAME);
    let current = state_dir.join(REMOTE_BINDINGS_DIR_NAME);
    if legacy != current
        && let Ok(entries) = fs::read_dir(&legacy)
    {
        if let Err(error) = create_private_dir(&current) {
            eprintln!("diri-engine: cannot create {}: {error}", current.display());
            moved.failed += 1;
            return moved;
        }
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            tally(&mut moved, move_file(&entry.path(), &current.join(&name)));
        }
        let _ = fs::remove_dir(&legacy);
    }
    moved
}

fn tally(moved: &mut Moved, result: io::Result<bool>) {
    match result {
        Ok(true) => moved.files += 1,
        Ok(false) => {}
        Err(_) => moved.failed += 1,
    }
}

/// Copies `from` to `to` owner-only, then removes `from`. Copy rather than
/// rename: the runtime directory is usually a different filesystem.
/// `Ok(false)` when there was nothing to move or `to` already exists.
fn move_file(from: &Path, to: &Path) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(from) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || fs::symlink_metadata(to).is_ok() {
        return Ok(false);
    }
    let bytes = fs::read(from)?;
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt as _;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(to)?
    };
    #[cfg(not(unix))]
    let mut file = diri_platform::security::create_private(to)?;
    io::Write::write_all(&mut file, &bytes)?;
    file.sync_all()?;
    fs::remove_file(from)?;
    Ok(true)
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    diri_platform::security::owner_only(path, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    #[cfg(unix)]
    #[test]
    fn moves_linux_runtime_files_into_config_and_state() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("run/diri");
        let config = root.path().join("config/diri");
        let state = root.path().join("state/diri");
        for dir in [&runtime, &config, &state] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(runtime.join("agents.json"), b"{\"agents\":1}").unwrap();
        fs::write(runtime.join("accounts.json"), b"{\"accounts\":1}").unwrap();
        fs::create_dir_all(runtime.join(REMOTE_BINDINGS_DIR_NAME)).unwrap();
        fs::write(
            runtime.join(REMOTE_BINDINGS_DIR_NAME).join("s_1.json"),
            b"{}",
        )
        .unwrap();
        fs::write(
            runtime.join(REMOTE_BINDINGS_DIR_NAME).join(".tmp-abc"),
            b"partial",
        )
        .unwrap();

        let moved = adopt_runtime_files(&runtime, &config, &state);

        assert_eq!(
            moved,
            Moved {
                files: 3,
                failed: 0
            }
        );
        assert_eq!(
            fs::read(config.join("agents.json")).unwrap(),
            b"{\"agents\":1}"
        );
        assert!(config.join("accounts.json").is_file());
        assert!(
            state
                .join(REMOTE_BINDINGS_DIR_NAME)
                .join("s_1.json")
                .is_file()
        );
        assert!(!runtime.join("agents.json").exists());
        let mode = fs::metadata(config.join("agents.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn never_overwrites_a_newer_file_or_follows_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("run");
        let config = root.path().join("config");
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&config).unwrap();
        fs::write(runtime.join("agents.json"), b"old").unwrap();
        fs::write(config.join("agents.json"), b"new").unwrap();
        std::os::unix::fs::symlink(root.path().join("elsewhere"), runtime.join("accounts.json"))
            .unwrap();

        let moved = adopt_runtime_files(&runtime, &config, &config);

        assert_eq!(moved, Moved::default());
        assert_eq!(fs::read(config.join("agents.json")).unwrap(), b"new");
        assert!(runtime.join("agents.json").exists());
        assert!(!config.join("accounts.json").exists());
    }

    #[test]
    fn single_root_layouts_are_left_alone() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("agents.json"), b"x").unwrap();
        fs::create_dir_all(root.path().join(REMOTE_BINDINGS_DIR_NAME)).unwrap();
        fs::write(
            root.path().join(REMOTE_BINDINGS_DIR_NAME).join("s_1.json"),
            b"{}",
        )
        .unwrap();

        let moved = adopt_runtime_files(root.path(), root.path(), root.path());

        assert_eq!(moved, Moved::default());
        assert!(root.path().join("agents.json").is_file());
        assert!(
            root.path()
                .join(REMOTE_BINDINGS_DIR_NAME)
                .join("s_1.json")
                .is_file()
        );
    }
}
