//! OpenSSH command construction for remote Helper bootstrap and channels.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use diri_proto::HostEntry;

use super::bootstrap::{
    BootstrapError, PLATFORM_PROBE_COMMAND, RemoteInstallLayout, validate_component,
};

const CONTROL_PERSIST_SECONDS: u16 = 60;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub program: OsString,
    pub arguments: Vec<OsString>,
}

#[derive(Clone, Debug)]
pub struct SshTransport {
    executable: OsString,
    destination: String,
    control_path: PathBuf,
}

impl SshTransport {
    #[must_use]
    pub fn new(host: &HostEntry, control_path: impl Into<PathBuf>) -> Self {
        Self {
            executable: OsString::from("ssh"),
            destination: host.ssh.clone(),
            control_path: control_path.into(),
        }
    }

    #[must_use]
    pub fn with_executable(mut self, executable: impl Into<OsString>) -> Self {
        self.executable = executable.into();
        self
    }

    /// Long-lived connection used only to amortize authentication and SSH
    /// handshake cost. It is never a Session persistence mechanism.
    #[must_use]
    pub fn master(&self) -> CommandSpec {
        let mut arguments = vec![
            OsString::from("-T"),
            OsString::from("-M"),
            OsString::from("-N"),
            OsString::from("-o"),
            OsString::from("ControlMaster=yes"),
            OsString::from("-o"),
            OsString::from(format!("ControlPersist={CONTROL_PERSIST_SECONDS}")),
        ];
        push_control_path(&mut arguments, &self.control_path);
        push_keepalives(&mut arguments);
        arguments.push(OsString::from("--"));
        arguments.push(OsString::from(&self.destination));
        CommandSpec {
            program: self.executable.clone(),
            arguments,
        }
    }

    /// Requests shutdown of Diri's private multiplexing master. This affects
    /// only `control_path`; it cannot target the user's unrelated SSH sessions.
    #[must_use]
    pub fn control_exit(&self) -> CommandSpec {
        let mut arguments = vec![
            OsString::from("-T"),
            OsString::from("-O"),
            OsString::from("exit"),
        ];
        push_control_path(&mut arguments, &self.control_path);
        arguments.push(OsString::from("--"));
        arguments.push(OsString::from(&self.destination));
        CommandSpec {
            program: self.executable.clone(),
            arguments,
        }
    }

    #[must_use]
    pub fn platform_probe(&self) -> CommandSpec {
        self.channel(PLATFORM_PROBE_COMMAND)
    }

    pub fn helper_probe(&self, build_id: &str) -> Result<CommandSpec, BootstrapError> {
        validate_component("build id", build_id)?;
        Ok(self.channel(&format!(
            "set -eu; [ -d \"$HOME/.cache\" ] && [ ! -L \"$HOME/.cache\" ] || exit 73; exec \"$HOME/.cache/diri/bin/protocol-{}/{}/diri-remote\" probe --format=json",
            diri_proto::remote_pty::PROTOCOL_MAJOR,
            build_id
        )))
    }

    pub fn helper_command(
        &self,
        build_id: &str,
        command: HelperCommand,
    ) -> Result<CommandSpec, BootstrapError> {
        validate_component("build id", build_id)?;
        Ok(self.channel(&format!(
            "set -eu; [ -d \"$HOME/.cache\" ] && [ ! -L \"$HOME/.cache\" ] || exit 73; exec \"$HOME/.cache/diri/bin/protocol-{}/{}/diri-remote\" {}",
            diri_proto::remote_pty::PROTOCOL_MAJOR,
            build_id,
            command.as_str()
        )))
    }

    /// Receives one artifact on stdin into a nonce-scoped owner-only path.
    #[must_use]
    pub fn upload(&self, layout: &RemoteInstallLayout) -> CommandSpec {
        let root = layout.root();
        let bin = layout.bin_dir();
        let protocol = layout.protocol_dir();
        let version = layout.version_dir();
        let temporary = layout.temporary();
        self.channel(&format!(
            "set -euC; umask 077; \
             [ -d \"$HOME\" ] || exit 73; [ ! -L \"$HOME/.cache\" ] || exit 73; \
             mkdir -p \"$HOME/.cache\"; [ -d \"$HOME/.cache\" ] && [ ! -L \"$HOME/.cache\" ] || exit 73; \
             for p in \"{root}\" \"{bin}\" \"{protocol}\" \"{version}\"; do [ ! -L \"$p\" ] || exit 73; done; \
             mkdir -p \"{version}\"; \
             for p in \"{root}\" \"{bin}\" \"{protocol}\" \"{version}\"; do [ -d \"$p\" ] && [ ! -L \"$p\" ] || exit 73; chmod 700 \"$p\"; done; \
             [ ! -e \"{temporary}\" ] && [ ! -L \"{temporary}\" ] || exit 74; \
             cat > \"{temporary}\"; chmod 700 \"{temporary}\""
        ))
    }

    #[must_use]
    pub fn temporary_probe(&self, layout: &RemoteInstallLayout) -> CommandSpec {
        self.channel(&format!(
            "set -eu; [ -d \"$HOME/.cache\" ] && [ ! -L \"$HOME/.cache\" ] || exit 73; exec \"{}\" probe --format=json",
            layout.temporary()
        ))
    }

    /// Lets the verified temporary Helper atomically hard-link itself into the
    /// versioned final path. This provides no-replace semantics without a
    /// shell-level check/rename race; a subsequent probe verifies the winner.
    #[must_use]
    pub fn commit_upload(&self, layout: &RemoteInstallLayout) -> CommandSpec {
        self.channel(&format!(
            "set -eu; [ -d \"$HOME/.cache\" ] && [ ! -L \"$HOME/.cache\" ] || exit 73; exec \"{}\" activate",
            layout.temporary()
        ))
    }

    /// Cleanup is deliberately limited to this bootstrap attempt's nonce
    /// path.
    #[must_use]
    pub fn cleanup_upload(&self, layout: &RemoteInstallLayout) -> CommandSpec {
        self.channel(&format!(
            "set -eu; [ -d \"$HOME/.cache\" ] && [ ! -L \"$HOME/.cache\" ] || exit 73; rm -f \"{}\"",
            layout.temporary()
        ))
    }

    /// Opens a no-PTY binary channel. `remote_command` must be fixed internal
    /// text; user argv, cwd, prompts, and environment travel over stdin later.
    #[must_use]
    pub fn channel(&self, remote_command: &str) -> CommandSpec {
        let mut arguments = vec![
            OsString::from("-T"),
            OsString::from("-o"),
            OsString::from("ControlMaster=auto"),
            OsString::from("-o"),
            OsString::from(format!("ControlPersist={CONTROL_PERSIST_SECONDS}")),
        ];
        push_control_path(&mut arguments, &self.control_path);
        push_keepalives(&mut arguments);
        // End local option parsing before the user-configured destination.
        // A separator after the destination would instead become part of the
        // remote command sent to the login shell.
        arguments.push(OsString::from("--"));
        arguments.push(OsString::from(&self.destination));
        arguments.push(OsString::from(posix_shell_command(remote_command)));
        CommandSpec {
            program: self.executable.clone(),
            arguments,
        }
    }
}

/// The SSH server always invokes the account's login shell with `-c`. That
/// shell may be fish, csh, or another non-POSIX shell, while Diri's fixed
/// bootstrap scripts intentionally use POSIX syntax. Keep the outer command
/// to the universally supported `exec /bin/sh -c <one quoted argument>`.
fn posix_shell_command(script: &str) -> String {
    format!("exec /bin/sh -c {}", shell_quote(script))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'"'"'"#))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelperCommand {
    Launch,
    Attach,
    Inspect,
    List,
    Kill,
    Gc,
    Environment,
    Directories,
    Persistence,
}

impl HelperCommand {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Launch => "launch",
            Self::Attach => "attach",
            Self::Inspect => "inspect",
            Self::List => "list",
            Self::Kill => "kill",
            Self::Gc => "gc",
            Self::Environment => "environment",
            Self::Directories => "directories",
            Self::Persistence => "persistence",
        }
    }
}

fn push_control_path(arguments: &mut Vec<OsString>, path: &Path) {
    let mut option = OsString::from("ControlPath=");
    option.push(path.as_os_str());
    arguments.push(OsString::from("-o"));
    arguments.push(option);
}

fn push_keepalives(arguments: &mut Vec<OsString>) {
    arguments.extend([
        OsString::from("-o"),
        OsString::from("ConnectTimeout=10"),
        OsString::from("-o"),
        OsString::from("ConnectionAttempts=1"),
        OsString::from("-o"),
        OsString::from("ServerAliveInterval=20"),
        OsString::from("-o"),
        OsString::from("ServerAliveCountMax=3"),
        OsString::from("-o"),
        OsString::from("TCPKeepAlive=yes"),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> HostEntry {
        HostEntry {
            id: "forge".into(),
            name: None,
            ssh: "developer@forge".into(),
            default_cwd: None,
            node: None,
        }
    }

    fn words(spec: &CommandSpec) -> Vec<String> {
        std::iter::once(&spec.program)
            .chain(&spec.arguments)
            .map(|word| word.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn every_protocol_channel_disables_ssh_pty_allocation() {
        let transport = SshTransport::new(&host(), "/tmp/diri master/socket");
        let words = words(&transport.platform_probe());
        assert_eq!(words[0], "ssh");
        assert!(words.contains(&"-T".to_string()));
        assert!(!words.contains(&"-t".to_string()));
        assert!(words.contains(&"ControlMaster=auto".to_string()));
        assert!(words.contains(&"ConnectTimeout=10".to_string()));
        assert!(words.contains(&"ConnectionAttempts=1".to_string()));
        assert!(words.contains(&"ControlPath=/tmp/diri master/socket".to_string()));
        assert_eq!(words[words.len() - 3], "--");
        assert_eq!(words[words.len() - 2], "developer@forge");
        assert_eq!(
            words.last().expect("command"),
            &posix_shell_command(PLATFORM_PROBE_COMMAND)
        );
    }

    #[test]
    fn master_is_an_optimization_without_background_forking() {
        let transport = SshTransport::new(&host(), "/tmp/control");
        let words = words(&transport.master());
        assert!(words.contains(&"-M".to_string()));
        assert!(words.contains(&"-N".to_string()));
        assert!(!words.contains(&"-f".to_string()));
        assert_eq!(words[words.len() - 2], "--");
        assert_eq!(words.last().expect("destination"), "developer@forge");
    }

    #[test]
    fn control_exit_targets_only_the_diri_mux_path() {
        let transport = SshTransport::new(&host(), "/tmp/diri master/socket");
        let words = words(&transport.control_exit());
        assert!(words.windows(2).any(|pair| pair == ["-O", "exit"]));
        assert!(words.contains(&"ControlPath=/tmp/diri master/socket".to_string()));
        assert_eq!(words.last().map(String::as_str), Some("developer@forge"));
        assert!(!words.iter().any(|word| word.contains("pkill")));
    }

    #[test]
    fn helper_path_accepts_only_validated_build_ids() {
        let transport = SshTransport::new(&host(), "/tmp/control");
        let command = transport.helper_probe("build-123").expect("valid");
        let words = words(&command);
        assert!(
            words
                .last()
                .expect("command")
                .contains("protocol-1/build-123/diri-remote")
        );
        assert!(transport.helper_probe("../escape").is_err());
    }

    #[test]
    fn ssh_configuration_and_host_key_policy_are_not_overridden() {
        let transport = SshTransport::new(&host(), "/tmp/control");
        let words = words(&transport.platform_probe());
        assert!(!words.iter().any(|word| word.contains("BatchMode")));
        assert!(
            !words
                .iter()
                .any(|word| word.contains("StrictHostKeyChecking"))
        );
        assert!(!words.iter().any(|word| word.contains("IdentityFile")));
    }

    #[test]
    fn upload_is_owner_only_symlink_checked_and_nonce_scoped() {
        let transport = SshTransport::new(&host(), "/tmp/control");
        let layout = RemoteInstallLayout::new("build-1", "nonce-2").expect("layout");
        let upload = words(&transport.upload(&layout));
        let command = upload.last().expect("command");
        assert!(command.contains("set -euC"));
        assert!(command.contains("umask 077"));
        assert!(command.contains("[ ! -L"));
        assert!(command.contains("chmod 700"));
        assert!(command.contains(".tmp-nonce-2"));

        let cleanup = words(&transport.cleanup_upload(&layout));
        let cleanup = cleanup.last().expect("cleanup command");
        assert!(cleanup.contains("rm -f"));
        assert!(cleanup.contains("[ ! -L \"$HOME/.cache\" ]"));
        assert!(cleanup.contains(".tmp-nonce-2"));
        assert!(!cleanup.contains("diri-remote"));
    }

    #[test]
    fn login_shell_wrapper_preserves_quotes_inside_the_fixed_script() {
        let script = "printf '%s' \"hello world\"";
        let wrapped = posix_shell_command(script);
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &wrapped])
            .output()
            .expect("execute wrapper");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"hello world");
    }
}
