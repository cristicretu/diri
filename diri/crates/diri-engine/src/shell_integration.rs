//! Automatic OSC 133 shell integration for local terminal tabs.
//!
//! A terminal tab runs the user's login shell (`$SHELL -l`). For zsh and bash
//! the Engine adds prompt and command marks (OSC 133 A/B/C/D) without any
//! setup on the user's side, the way Ghostty and kitty do, while the shell
//! still starts exactly as before: same argv, same login semantics, the
//! user's own startup files in their usual order.
//!
//! - **zsh**: `ZDOTDIR` points at a directory holding only a `.zshenv` shim.
//!   The shim restores `ZDOTDIR` (or unsets it) before anything of the
//!   user's runs, sources their `.zshenv`, and loads the hooks; zsh then
//!   reads `.zprofile`, `.zshrc` and `.zlogin` from the user's own
//!   `ZDOTDIR`. A `ZDOTDIR` the shell inherited travels in
//!   `DIRI_ZSH_ZDOTDIR` for the shim to put back.
//! - **bash**: `PROMPT_COMMAND` (imported from the environment) sources the
//!   integration at the first prompt, after `/etc/profile`, the profile and
//!   `.bashrc`. The script then un-exports `PROMPT_COMMAND` and chains itself
//!   around whatever those files put there, joining bash-preexec when it is
//!   loaded and otherwise chaining any existing `DEBUG` trap.
//!
//! Neither variable reaches the programs the shell runs. Other shells (fish 4
//! emits the marks itself), agent wrappers, caller-supplied argv and remote
//! hosts are left alone.

use std::io;
use std::path::{Path, PathBuf};

use crate::pty::PtySpec;

/// Directory under the injection directory the scripts are written to.
const DIRECTORY: &str = "shell-integration";
const ZSH_DIRECTORY: &str = "zsh";
const BASH_SCRIPT: &str = "diri.bash";

const ZSHENV: &str = include_str!("shell_integration/zshenv");
const ZSH: &str = include_str!("shell_integration/diri.zsh");
const BASH: &str = include_str!("shell_integration/diri.bash");

/// Where the shim finds a `ZDOTDIR` the shell inherited.
const ZSH_ORIGINAL_ZDOTDIR: &str = "DIRI_ZSH_ZDOTDIR";

/// The installed scripts.
#[derive(Clone, Debug)]
pub struct ShellIntegration {
    root: PathBuf,
}

impl ShellIntegration {
    /// Writes the scripts under `inject_dir`, replacing older copies
    /// atomically, so a shell starting meanwhile reads a whole file.
    pub fn install(inject_dir: &Path) -> io::Result<Self> {
        let root = inject_dir.join(DIRECTORY);
        let zsh = root.join(ZSH_DIRECTORY);
        create_private_dir(&root)?;
        create_private_dir(&zsh)?;
        write_private(&zsh.join(".zshenv"), ZSHENV.as_bytes())?;
        write_private(&zsh.join("diri.zsh"), ZSH.as_bytes())?;
        write_private(&root.join(BASH_SCRIPT), BASH.as_bytes())?;
        Ok(Self { root })
    }

    /// Adds the integration to a terminal tab's login shell. Anything that is
    /// not exactly `[zsh|bash, -l]` is left untouched, as is a shell whose
    /// script is missing or whose path the bootstrap could not quote.
    pub fn apply(&self, spec: &mut PtySpec) {
        let [shell, flag] = spec.argv.as_slice() else {
            return;
        };
        if flag != "-l" {
            return;
        }
        match Path::new(shell).file_name().and_then(|name| name.to_str()) {
            Some("zsh") => self.apply_zsh(spec),
            Some("bash") => self.apply_bash(spec),
            _ => {}
        }
    }

    fn apply_zsh(&self, spec: &mut PtySpec) {
        let shim = self.root.join(ZSH_DIRECTORY);
        if !shim.join(".zshenv").is_file() {
            return;
        }
        let shim = shim.to_string_lossy().into_owned();
        let original = last_value(&spec.env, "ZDOTDIR")
            // An Engine started from a Diri zsh never sees the shim's value
            // (it is restored before the user's files run); be safe anyway.
            .filter(|value| *value != shim)
            .map(str::to_owned);
        spec.env
            .retain(|(key, _)| key != "ZDOTDIR" && key != ZSH_ORIGINAL_ZDOTDIR);
        if let Some(original) = original {
            spec.env.push((ZSH_ORIGINAL_ZDOTDIR.into(), original));
        }
        spec.env.push(("ZDOTDIR".into(), shim));
    }

    fn apply_bash(&self, spec: &mut PtySpec) {
        let script = self.root.join(BASH_SCRIPT);
        if !script.is_file() {
            return;
        }
        let Some(boot) = bash_bootstrap(&script) else {
            return;
        };
        let command = match last_value(&spec.env, "PROMPT_COMMAND") {
            Some(inherited) if !inherited.trim().is_empty() => format!("{boot}\n{inherited}"),
            _ => boot,
        };
        spec.env.retain(|(key, _)| key != "PROMPT_COMMAND");
        spec.env.push(("PROMPT_COMMAND".into(), command));
    }
}

/// The `PROMPT_COMMAND` that loads the bash integration. `diri.bash`
/// rebuilds this exact text from its own path to replace it, so the two must
/// change together. The `eval` installs the DEBUG trap at the top level:
/// bash's `.` puts back a DEBUG trap that existed before it.
fn bash_bootstrap(script: &Path) -> Option<String> {
    let path = script.to_str()?;
    if path.contains('\'') || path.contains('\n') {
        return None;
    }
    Some(format!(
        ". '{path}'; builtin eval \"${{__diri_trap_install-}}\""
    ))
}

fn last_value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter()
        .rev()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Replaces `path` atomically with an owner-only file, unless it already
/// holds `contents`.
pub(crate) fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    if std::fs::read(path).is_ok_and(|current| current == contents) {
        return Ok(());
    }
    let staging = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("script"),
        std::process::id()
    ));
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&staging)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    std::fs::rename(&staging, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&staging);
    })
}

#[cfg(test)]
mod tests;
