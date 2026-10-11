//! `claude` typed in a Diri terminal follows the Claude account selected in
//! the bottom-left menu, including in terminals opened before the switch.
//!
//! A switch relaunches Claude tabs with the new login store, but a terminal's
//! shell keeps the environment it started with, so `claude` typed there used
//! Claude's default store. Terminal shells instead get a `claude` function
//! that reads a one-line file naming the selected store on every launch.
//!
//! The function is defined through the shell's own startup hooks, which Diri
//! hands back unchanged before the user's files run: zsh through a `ZDOTDIR`
//! whose `.zshenv` restores the user's `ZDOTDIR`, fish through an
//! `XDG_DATA_DIRS` entry whose `vendor_conf.d` file restores the variable.
//! Other shells are left alone. A `CLAUDE_SECURESTORAGE_CONFIG_DIR` or
//! `CLAUDE_CONFIG_DIR` already set in the shell wins, as does a `claude` the
//! user defines in their own startup files.
use diri_proto::AgentAccountCatalog;
use std::{
    fs,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

/// Names the pointer file inside a terminal's environment.
const POINTER_VARIABLE: &str = "DIRI_CLAUDE_LOGIN";
/// The user's `ZDOTDIR`; present only when they had one.
const ZDOTDIR_VARIABLE: &str = "DIRI_ZDOTDIR";
/// The user's `XDG_DATA_DIRS`; empty when they had none.
const XDG_VARIABLE: &str = "DIRI_XDG_DATA_DIRS";
/// fish's own fallback when `XDG_DATA_DIRS` is unset.
const XDG_DEFAULT: &str = "/usr/local/share:/usr/share";

const ZSHENV: &str = r#"# Written by Diri on every Engine start; edits are overwritten.
# Give ZDOTDIR back first: zsh finds .zprofile, .zshrc and .zlogin through it.
if [[ -n "${DIRI_ZDOTDIR+x}" ]]; then
  ZDOTDIR="$DIRI_ZDOTDIR"
  unset DIRI_ZDOTDIR
else
  unset ZDOTDIR
fi
if [[ -r "${ZDOTDIR-$HOME}/.zshenv" ]]; then
  builtin source -- "${ZDOTDIR-$HOME}/.zshenv"
fi
if [[ -o interactive && -n "${DIRI_CLAUDE_LOGIN-}" ]]; then
  claude() {
    if [[ -n "${CLAUDE_SECURESTORAGE_CONFIG_DIR-}${CLAUDE_CONFIG_DIR-}" ]]; then
      command claude "$@"
      return
    fi
    local __diri_store
    __diri_store="$(command cat -- "$DIRI_CLAUDE_LOGIN" 2>/dev/null)"
    if [[ "$__diri_store" == /* ]]; then
      CLAUDE_SECURESTORAGE_CONFIG_DIR="$__diri_store" command claude "$@"
    else
      command claude "$@"
    fi
  }
fi
"#;

const FISH: &str = r#"# Written by Diri on every Engine start; edits are overwritten.
if set -q DIRI_XDG_DATA_DIRS
    if test -n "$DIRI_XDG_DATA_DIRS"
        set -gx XDG_DATA_DIRS "$DIRI_XDG_DATA_DIRS"
    else
        set -e XDG_DATA_DIRS
    end
    set -e DIRI_XDG_DATA_DIRS
end
if status is-interactive; and test -n "$DIRI_CLAUDE_LOGIN"
    function claude --description 'Claude Code on the account selected in Diri'
        if test -n "$CLAUDE_SECURESTORAGE_CONFIG_DIR$CLAUDE_CONFIG_DIR"
            command claude $argv
            return $status
        end
        set -l store (command cat -- "$DIRI_CLAUDE_LOGIN" 2>/dev/null)
        if string match -q -- '/*' "$store"
            env CLAUDE_SECURESTORAGE_CONFIG_DIR="$store" claude $argv
        else
            command claude $argv
        end
    end
end
"#;

/// The integration files and the pointer, under one owner-only directory.
pub struct ShellAccounts {
    root: PathBuf,
}

impl ShellAccounts {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn pointer(&self) -> PathBuf {
        self.root.join("claude-login")
    }

    /// Rewrite the startup files and the pointer from `catalog`.
    pub fn install(&self, catalog: &AgentAccountCatalog) -> io::Result<()> {
        private_directory(&self.root)?;
        private_directory(&self.root.join("zsh"))?;
        let vendor = self.root.join("fish").join("vendor_conf.d");
        private_directory(&self.root.join("fish"))?;
        private_directory(&vendor)?;
        replace(&self.root.join("zsh").join(".zshenv"), ZSHENV.as_bytes())?;
        replace(&vendor.join("diri-accounts.fish"), FISH.as_bytes())?;
        self.select(catalog)
    }

    /// Point `claude` at the selected local Claude Code login store. An
    /// empty file is Claude's default store, as before any switch.
    pub fn select(&self, catalog: &AgentAccountCatalog) -> io::Result<()> {
        private_directory(&self.root)?;
        replace(&self.pointer(), selected_store(catalog).as_bytes())
    }

    #[cfg(test)]
    pub(crate) fn selection(&self) -> String {
        fs::read_to_string(self.pointer()).unwrap_or_default()
    }

    /// Route a local terminal's login shell through the integration. Does
    /// nothing for shells other than zsh and fish, or before `install`.
    pub fn apply(&self, argv: &[String], env: &mut Vec<(String, String)>) {
        let Some(shell) = argv
            .first()
            .and_then(|shell| Path::new(shell).file_name())
            .and_then(|name| name.to_str())
        else {
            return;
        };
        if !matches!(shell, "zsh" | "fish") || !self.pointer().is_file() {
            return;
        }
        let current = |key: &str| {
            env.iter()
                .rev()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        let mut set = vec![(
            POINTER_VARIABLE.to_owned(),
            self.pointer().to_string_lossy().into_owned(),
        )];
        if shell == "zsh" {
            if let Some(user) = current("ZDOTDIR") {
                set.push((ZDOTDIR_VARIABLE.to_owned(), user));
            }
            set.push((
                "ZDOTDIR".to_owned(),
                self.root.join("zsh").to_string_lossy().into_owned(),
            ));
        } else {
            let user = current("XDG_DATA_DIRS").filter(|value| !value.is_empty());
            let fallback = user.clone().unwrap_or_else(|| XDG_DEFAULT.to_owned());
            set.push((XDG_VARIABLE.to_owned(), user.unwrap_or_default()));
            set.push((
                "XDG_DATA_DIRS".to_owned(),
                format!("{}:{fallback}", self.root.display()),
            ));
        }
        env.retain(|(name, _)| {
            !matches!(
                name.as_str(),
                POINTER_VARIABLE | ZDOTDIR_VARIABLE | XDG_VARIABLE
            ) && !set.iter().any(|(key, _)| key == name)
        });
        env.extend(set);
    }
}

fn selected_store(catalog: &AgentAccountCatalog) -> String {
    catalog
        .profiles
        .iter()
        .find(|p| p.is_default && p.agent == "claude-code" && p.host.is_none())
        .and_then(|p| p.login_store.clone())
        .filter(|store| Path::new(store).is_absolute() && !store.contains('\n'))
        .unwrap_or_default()
}

fn private_directory(path: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::other("not a directory owned by this user"));
    }
    if metadata.mode() & 0o077 != 0 {
        fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    }
    Ok(())
}

/// Atomic owner-only replacement, so a shell never sources half a file.
fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if fs::read(path).is_ok_and(|current| current == bytes)
        && fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.mode() & 0o077 == 0)
    {
        return Ok(());
    }
    let temporary = path.with_file_name(format!(".diri-{}.tmp", crate::inject::uuid_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::AgentAccountProfile;
    use std::process::{Command, Stdio};

    fn claude(id: &str, store: &str, is_default: bool) -> AgentAccountProfile {
        AgentAccountProfile {
            id: id.into(),
            label: id.into(),
            agent: "claude-code".into(),
            host: None,
            config_home: "~/.claude".into(),
            is_default,
            login_store: Some(store.into()),
        }
    }

    fn catalog(profiles: Vec<AgentAccountProfile>) -> AgentAccountCatalog {
        AgentAccountCatalog { profiles }
    }

    /// A fake `claude` on PATH that prints the store it was launched with.
    fn fake_claude(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let claude = bin.join("claude");
        fs::write(
            &claude,
            "#!/bin/sh\nprintf 'store=[%s] args=[%s]\\n' \"$CLAUDE_SECURESTORAGE_CONFIG_DIR\" \"$*\"\n",
        )
        .unwrap();
        fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    /// Run `lines` in an interactive login shell started the way Diri starts
    /// a terminal, with HOME and the user's startup files under `home`.
    fn terminal(
        shell: &str,
        shell_accounts: &ShellAccounts,
        home: &Path,
        bin: &Path,
        extra: &[(&str, &str)],
        lines: &str,
    ) -> String {
        let argv = vec![shell.to_owned(), "-i".into(), "-l".into()];
        let mut env = vec![
            ("HOME".to_owned(), home.to_string_lossy().into_owned()),
            (
                "PATH".to_owned(),
                format!("{}:/usr/bin:/bin", bin.display()),
            ),
            ("TERM".to_owned(), "dumb".to_owned()),
        ];
        env.extend(
            extra
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
        );
        shell_accounts.apply(&argv, &mut env);
        let mut child = Command::new(shell)
            .args(&argv[1..])
            .env_clear()
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{lines}\nexit\n").as_bytes())
            .unwrap();
        String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
    }

    fn shells() -> Vec<String> {
        [
            "/bin/zsh",
            "/opt/homebrew/bin/fish",
            "/usr/bin/fish",
            "/usr/local/bin/fish",
        ]
        .into_iter()
        .filter(|shell| Path::new(shell).exists())
        .map(str::to_owned)
        .collect()
    }

    #[test]
    fn an_open_terminal_follows_each_switch_without_restarting() {
        for shell in shells() {
            let temp = tempfile::tempdir().unwrap();
            let bin = fake_claude(temp.path());
            let accounts = ShellAccounts::new(temp.path().join("shell-integration"));
            accounts.install(&catalog(Vec::new())).unwrap();

            // The same shell process: one launch before, one after a switch,
            // one after switching on to another account.
            let pointer = accounts.pointer();
            let switch = |store: &str| format!("printf '%s' '{store}' > '{}'", pointer.display());
            let out = terminal(
                &shell,
                &accounts,
                temp.path(),
                &bin,
                &[],
                &format!(
                    "claude one\n{}\nclaude two 'a b'\n{}\nclaude three",
                    switch("/slots/work"),
                    switch("/slots/home"),
                ),
            );
            assert!(out.contains("store=[] args=[one]"), "{shell}: {out}");
            assert!(
                out.contains("store=[/slots/work] args=[two a b]"),
                "{shell}: {out}"
            );
            assert!(
                out.contains("store=[/slots/home] args=[three]"),
                "{shell}: {out}"
            );
        }
    }

    #[test]
    fn the_selected_store_is_the_default_local_claude_profile() {
        let temp = tempfile::tempdir().unwrap();
        let accounts = ShellAccounts::new(temp.path().join("s"));
        let mut remote = claude("remote", "/slots/remote", true);
        remote.host = Some("forge".into());
        accounts
            .install(&catalog(vec![
                claude("work", "/slots/work", false),
                claude("home", "/slots/home", true),
                remote,
            ]))
            .unwrap();
        assert_eq!(
            fs::read_to_string(accounts.pointer()).unwrap(),
            "/slots/home"
        );
        accounts.select(&catalog(Vec::new())).unwrap();
        assert_eq!(fs::read_to_string(accounts.pointer()).unwrap(), "");
        let mode = |path: &Path| fs::metadata(path).unwrap().mode() & 0o777;
        assert_eq!(mode(&temp.path().join("s")), 0o700);
        assert_eq!(mode(&accounts.pointer()), 0o600);
    }

    #[test]
    fn a_login_chosen_in_the_shell_wins_and_startup_files_still_run() {
        for shell in shells() {
            let temp = tempfile::tempdir().unwrap();
            let bin = fake_claude(temp.path());
            let accounts = ShellAccounts::new(temp.path().join("shell-integration"));
            accounts
                .install(&catalog(vec![claude("work", "/slots/work", true)]))
                .unwrap();
            // The user's own startup files, in their own ZDOTDIR for zsh.
            let zdotdir = temp.path().join("zdot");
            fs::create_dir(&zdotdir).unwrap();
            fs::write(zdotdir.join(".zshrc"), "export RAN_RC=zshrc\n").unwrap();
            let fish_config = temp.path().join(".config/fish");
            fs::create_dir_all(&fish_config).unwrap();
            fs::write(fish_config.join("config.fish"), "set -gx RAN_RC fish\n").unwrap();

            let (report, own) = if shell.ends_with("zsh") {
                (
                    "echo rc=$RAN_RC zdotdir=$ZDOTDIR xdg=${XDG_DATA_DIRS-unset}.",
                    "export CLAUDE_SECURESTORAGE_CONFIG_DIR=/mine",
                )
            } else {
                (
                    "echo rc=$RAN_RC xdg=(set -q XDG_DATA_DIRS; and echo set; or echo unset).",
                    "set -gx CLAUDE_SECURESTORAGE_CONFIG_DIR /mine",
                )
            };
            let out = terminal(
                &shell,
                &accounts,
                temp.path(),
                &bin,
                &[
                    ("ZDOTDIR", &zdotdir.to_string_lossy()),
                    (
                        "XDG_CONFIG_HOME",
                        &temp.path().join(".config").to_string_lossy(),
                    ),
                ],
                &format!("{report}\nclaude selected\n{own}\nclaude own"),
            );
            assert!(
                out.contains("store=[/slots/work] args=[selected]"),
                "{shell}: {out}"
            );
            assert!(out.contains("store=[/mine] args=[own]"), "{shell}: {out}");
            if shell.ends_with("zsh") {
                assert!(
                    out.contains(&format!(
                        "rc=zshrc zdotdir={} xdg=unset.",
                        zdotdir.display()
                    )),
                    "{shell}: {out}"
                );
            } else {
                assert!(out.contains("rc=fish xdg=unset."), "{shell}: {out}");
            }
        }
    }

    #[test]
    fn other_shells_and_a_missing_install_are_left_alone() {
        let temp = tempfile::tempdir().unwrap();
        let accounts = ShellAccounts::new(temp.path().join("s"));
        let mut env = vec![("ZDOTDIR".to_owned(), "/z".to_owned())];
        accounts.apply(&["/bin/zsh".into(), "-l".into()], &mut env);
        assert_eq!(env, [("ZDOTDIR".to_owned(), "/z".to_owned())]);
        accounts.install(&catalog(Vec::new())).unwrap();
        accounts.apply(&["/bin/bash".into(), "-l".into()], &mut env);
        assert_eq!(env, [("ZDOTDIR".to_owned(), "/z".to_owned())]);
        accounts.apply(&["/bin/zsh".into(), "-l".into()], &mut env);
        let value = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(value("DIRI_ZDOTDIR"), Some("/z"));
        assert_eq!(
            value("ZDOTDIR").map(PathBuf::from),
            Some(temp.path().join("s").join("zsh"))
        );
    }
}
