//! Launch-profile persistence and environment isolation, independent of optional nodes.
use diri_proto::{AgentAccountCatalog, AgentAccountProfile, ControlError};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Default, Deserialize, Serialize)]
struct File {
    version: u32,
    profiles: Vec<AgentAccountProfile>,
}

pub struct AccountStore {
    path: PathBuf,
}

impl AccountStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn catalog(&self) -> Result<AgentAccountCatalog, ControlError> {
        let mut input = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&self.path)
        {
            Ok(input) => input,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(AgentAccountCatalog::default());
            }
            Err(_) => return Err(ControlError::internal("Cannot read account profiles")),
        };
        let metadata = input
            .metadata()
            .map_err(|_| ControlError::internal("Cannot inspect account profiles"))?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(ControlError::bad_request(
                "Account profiles must be an owner-only regular file",
            ));
        }
        let mut bytes = Vec::new();
        (&mut input)
            .take(512 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ControlError::internal("Cannot read account profiles"))?;
        if bytes.len() > 512 * 1024 {
            return Err(ControlError::bad_request(
                "Account profile file is too large",
            ));
        }
        let file: File = serde_json::from_slice(&bytes).map_err(|_| {
            ControlError::bad_request("Account profiles are invalid; the file has been preserved")
        })?;
        if file.version != 1 {
            return Err(ControlError::bad_request(
                "Unsupported account profile version",
            ));
        }
        if file.profiles.len() > 64 {
            return Err(ControlError::bad_request("Too many account profiles"));
        }
        let mut ids = std::collections::HashSet::new();
        let mut defaults = std::collections::HashSet::new();
        for profile in &file.profiles {
            validate(profile)?;
            if !ids.insert(&profile.id)
                || (profile.is_default && !defaults.insert((&profile.agent, &profile.host)))
            {
                return Err(ControlError::bad_request(
                    "Account profiles contain duplicate identities or defaults",
                ));
            }
        }
        Ok(AgentAccountCatalog {
            profiles: file.profiles,
        })
    }

    pub fn upsert(
        &self,
        profile: AgentAccountProfile,
    ) -> Result<AgentAccountCatalog, ControlError> {
        validate(&profile)?;
        let mut catalog = self.catalog()?;
        if catalog.profiles.len() >= 64 && !catalog.profiles.iter().any(|p| p.id == profile.id) {
            return Err(ControlError::bad_request(
                "At most 64 account profiles are supported",
            ));
        }
        for item in &mut catalog.profiles {
            if profile.is_default && item.agent == profile.agent && item.host == profile.host {
                item.is_default = false;
            }
        }
        catalog.profiles.retain(|item| item.id != profile.id);
        catalog.profiles.push(profile);
        self.save(&catalog)?;
        Ok(catalog)
    }

    pub fn remove(&self, id: &str) -> Result<AgentAccountCatalog, ControlError> {
        let mut catalog = self.catalog()?;
        catalog.profiles.retain(|profile| profile.id != id);
        self.save(&catalog)?;
        Ok(catalog)
    }

    pub fn resolve(
        &self,
        id: Option<&str>,
        agent: &str,
        host: Option<&str>,
    ) -> Result<Option<AgentAccountProfile>, ControlError> {
        if id == Some("") {
            return Ok(None);
        }
        if id.is_none() && !matches!(agent, "codex" | "claude-code") {
            return Ok(None);
        }
        let catalog = self.catalog()?;
        let profile = match id {
            Some(id) => Some(
                catalog
                    .profiles
                    .into_iter()
                    .find(|p| p.id == id)
                    .ok_or_else(|| {
                        ControlError::not_found("The selected account profile no longer exists")
                    })?,
            ),
            None => catalog
                .profiles
                .into_iter()
                .find(|p| p.is_default && p.agent == agent && p.host.as_deref() == host),
        };
        if let Some(profile) = &profile
            && (profile.agent != agent || profile.host.as_deref() != host)
        {
            return Err(ControlError::bad_request(
                "The selected account belongs to a different Agent or host",
            ));
        }
        Ok(profile)
    }

    fn save(&self, catalog: &AgentAccountCatalog) -> Result<(), ControlError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| ControlError::internal("Missing account directory"))?;
        let temporary = parent.join(format!(".accounts-{}.tmp", crate::inject::uuid_v4()));
        let result = (|| -> io::Result<()> {
            fs::create_dir_all(parent)?;
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)?;
            serde_json::to_writer_pretty(
                &mut file,
                &File {
                    version: 1,
                    profiles: catalog.profiles.clone(),
                },
            )?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(|_| ControlError::internal("Account profiles could not be saved"))
    }
}

fn validate(profile: &AgentAccountProfile) -> Result<(), ControlError> {
    if profile.id.is_empty()
        || profile.id.len() > 80
        || !profile
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ControlError::bad_request("Invalid account profile id"));
    }
    if profile.label.trim().is_empty()
        || profile.label.len() > 120
        || profile.label.chars().any(char::is_control)
    {
        return Err(ControlError::bad_request(
            "Enter an account name of at most 120 bytes",
        ));
    }
    if profile.environment_key().is_none() {
        return Err(ControlError::bad_request(
            "Account profiles support Claude Code and Codex",
        ));
    }
    if profile.host.as_ref().is_some_and(|host| {
        host.is_empty() || host.len() > 128 || host.chars().any(char::is_control)
    }) {
        return Err(ControlError::bad_request("Invalid execution host"));
    }
    let path = &profile.config_home;
    if path.len() > 4096
        || path.chars().any(char::is_control)
        || !(Path::new(path).is_absolute() || path.starts_with("~/"))
        || Path::new(path)
            .components()
            .any(|p| p == std::path::Component::ParentDir)
        || path == "/"
        || path == "~/"
    {
        return Err(ControlError::bad_request(
            "Choose an absolute account directory or a path starting with ~/",
        ));
    }
    Ok(())
}

/// Resolve ~ using the execution host's HOME. Apply after manifest scrubbing.
pub fn bind(
    profile: &mut AgentAccountProfile,
    env: &mut Vec<(String, String)>,
) -> Result<(), ControlError> {
    validate(profile)?;
    if let Some(relative) = profile.config_home.strip_prefix("~/") {
        let home = env
            .iter()
            .rev()
            .find(|(key, _)| key == "HOME")
            .map(|(_, value)| value)
            .filter(|home| Path::new(home).is_absolute())
            .ok_or_else(|| {
                ControlError::bad_request("Execution host did not report a home directory")
            })?;
        profile.config_home = Path::new(home)
            .join(relative)
            .to_string_lossy()
            .into_owned();
    }
    // Ambient provider credentials would silently override the chosen account.
    env.retain(|(key, _)| !ACCOUNT_ENVIRONMENT.contains(&key.as_str()));
    env.push((
        profile.environment_key().expect("validated").into(),
        profile.config_home.clone(),
    ));
    Ok(())
}

pub fn bind_pty(
    profile: &mut AgentAccountProfile,
    pty: &mut crate::pty::PtySpec,
) -> Result<(), ControlError> {
    bind(profile, &mut pty.env)?;
    if profile.host.is_none() {
        prepare_local_directory(&profile.config_home)?;
    }
    // Local Agents run through a login shell. Reassert the account after shell
    // startup files so they cannot redirect this launch to another account.
    if profile.host.is_none() && pty.argv.len() == 5 && pty.argv[3] == "-c" {
        let assignment = format!(
            "{}={}",
            profile.environment_key().expect("validated"),
            profile.config_home
        );
        let quoted = format!("'{}'", assignment.replace('\'', "'\\''"));
        let scrub = ACCOUNT_ENVIRONMENT
            .iter()
            .map(|key| format!("-u {key}"))
            .collect::<Vec<_>>()
            .join(" ");
        pty.argv[4] = format!("/usr/bin/env {scrub} {quoted} {}", pty.argv[4]);
    }
    Ok(())
}

fn prepare_local_directory(path: &str) -> Result<(), ControlError> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|_| ControlError::bad_request("Cannot create the account directory"))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| ControlError::bad_request("Cannot inspect the account directory"))?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(ControlError::bad_request(
            "Account directory must be a directory owned by you, not a symlink",
        ));
    }
    Ok(())
}

/// Stateless Engine-owned setup; the Helper still receives direct argv/env.
pub fn prepare_remote_directory(
    profile: &AgentAccountProfile,
    host: &diri_proto::HostEntry,
    manager: &crate::remote::manager::RemoteManager,
) -> Result<(), ControlError> {
    validate(profile)?;
    if profile.host.as_deref() != Some(host.id.as_str())
        || !Path::new(&profile.config_home).is_absolute()
    {
        return Err(ControlError::bad_request(
            "Account directory is not bound to this host",
        ));
    }
    let input = format!("{}\n", profile.config_home).into_bytes();
    let output = manager.run_fixed_script(
        host,
        DIRECTORY_SCRIPT,
        input,
        std::time::Duration::from_secs(15),
        4096,
    );
    if !output.is_ok_and(|output| output.status.success()) {
        return Err(ControlError::bad_request(
            "Cannot prepare the account directory on this host",
        ));
    }
    Ok(())
}

const DIRECTORY_SCRIPT: &str = r#"sh -c 'IFS= read -r diri_account_dir || exit 73; case "$diri_account_dir" in /*) ;; *) exit 73;; esac; umask 077; [ ! -L "$diri_account_dir" ] && mkdir -p "$diri_account_dir" && [ -d "$diri_account_dir" ] && [ -O "$diri_account_dir" ]'"#;

const ACCOUNT_ENVIRONMENT: &[&str] = &[
    "CODEX_HOME",
    "CODEX_SQLITE_HOME",
    "CODEX_API_KEY",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_SECURESTORAGE_CONFIG_DIR",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

#[cfg(test)]
mod tests {
    use super::*;
    fn profile(id: &str) -> AgentAccountProfile {
        AgentAccountProfile {
            id: id.into(),
            label: id.into(),
            agent: "codex".into(),
            host: None,
            config_home: "~/codex-work".into(),
            is_default: true,
        }
    }
    #[test]
    fn persistence_defaults_targets_and_explicit_cli() {
        let root = tempfile::tempdir().unwrap();
        let store = AccountStore::new(root.path().join("accounts.json"));
        store.upsert(profile("work")).unwrap();
        store.upsert(profile("personal")).unwrap();
        assert_eq!(
            store.resolve(None, "codex", None).unwrap().unwrap().id,
            "personal"
        );
        assert!(
            store
                .resolve(Some("work"), "codex", Some("server"))
                .is_err()
        );
        assert!(store.resolve(Some("work"), "claude-code", None).is_err());
        assert!(store.resolve(Some(""), "codex", None).unwrap().is_none());
        store.remove("personal").unwrap();
        assert!(store.resolve(Some("personal"), "codex", None).is_err());
        assert!(store.resolve(None, "codex", None).unwrap().is_none());
        assert_eq!(
            fs::metadata(&store.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    #[test]
    fn binding_uses_remote_home_and_removes_ambient_credentials() {
        let mut p = profile("work");
        let mut env = vec![
            ("HOME".into(), "/home/remote".into()),
            ("OPENAI_API_KEY".into(), "secret".into()),
            ("PATH".into(), "/bin".into()),
        ];
        bind(&mut p, &mut env).unwrap();
        assert_eq!(p.config_home, "/home/remote/codex-work");
        assert!(!env.iter().any(|(key, _)| key == "OPENAI_API_KEY"));
        assert!(env.contains(&("CODEX_HOME".into(), p.config_home)));
    }

    #[test]
    fn remote_configuration_arguments_are_never_rewritten_as_shell_commands() {
        let mut profile = profile("work");
        profile.host = Some("server".into());
        let argv = vec![
            "codex".into(),
            "-c".into(),
            "model=\"test\"".into(),
            "-c".into(),
            "sandbox_mode=\"read-only\"".into(),
        ];
        let mut pty =
            crate::pty::PtySpec::new(argv.clone(), "/home/remote").env("HOME", "/home/remote");
        bind_pty(&mut profile, &mut pty).unwrap();
        assert_eq!(pty.argv, argv);
        assert_eq!(profile.config_home, "/home/remote/codex-work");
        assert!(
            pty.env
                .contains(&("CODEX_HOME".into(), profile.config_home))
        );
    }
    #[test]
    fn corrupt_or_future_catalog_is_not_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let store = AccountStore::new(root.path().join("accounts.json"));
        store.upsert(profile("work")).unwrap();
        for bytes in [b"broken".as_slice(), br#"{"version":2,"profiles":[]}"#] {
            fs::write(&store.path, bytes).unwrap();
            assert!(store.upsert(profile("personal")).is_err());
            assert_eq!(fs::read(&store.path).unwrap(), bytes);
        }
    }

    #[test]
    fn shell_startup_overrides_cannot_replace_the_selected_account() {
        let root = tempfile::tempdir().unwrap();
        let mut p = profile("work");
        p.config_home = root
            .path()
            .join("Work's $(false) account")
            .to_string_lossy()
            .into_owned();
        let mut pty = crate::pty::PtySpec::new(
            vec![
                "/bin/sh".into(),
                "-i".into(),
                "-l".into(),
                "-c".into(),
                "/usr/bin/env".into(),
            ],
            root.path(),
        );
        bind_pty(&mut p, &mut pty).unwrap();
        let command = format!(
            "export CODEX_HOME=/wrong OPENAI_API_KEY=ambient CLAUDE_SECURESTORAGE_CONFIG_DIR=/wrong; {}",
            pty.argv[4]
        );
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &command])
            .env_clear()
            .output()
            .unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(
            output
                .lines()
                .any(|line| line == format!("CODEX_HOME={}", p.config_home))
        );
        assert!(!output.contains("OPENAI_API_KEY="));
        assert!(!output.contains("CLAUDE_SECURESTORAGE_CONFIG_DIR="));
        assert_eq!(
            fs::metadata(&p.config_home).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn remote_directory_setup_treats_paths_as_data_and_is_idempotent() {
        use std::process::{Command, Stdio};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("work's $(false) account");
        for _ in 0..2 {
            let mut child = Command::new("/bin/sh")
                .args(["-c", DIRECTORY_SCRIPT])
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            writeln!(child.stdin.take().unwrap(), "{}", path.display()).unwrap();
            assert!(child.wait().unwrap().success());
        }
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let link = root.path().join("linked");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(prepare_local_directory(link.to_str().unwrap()).is_err());
    }

    #[test]
    fn unsafe_or_ambiguous_catalogs_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let store = AccountStore::new(root.path().join("accounts.json"));
        for path in ["relative", "~/../escape", "/", "~/", "/tmp/line\nnext"] {
            let mut p = profile("work");
            p.config_home = path.into();
            assert!(store.upsert(p).is_err(), "{path}");
        }
        store.upsert(profile("work")).unwrap();
        let duplicate = serde_json::to_vec(&File {
            version: 1,
            profiles: vec![profile("work"), profile("work")],
        })
        .unwrap();
        fs::write(&store.path, duplicate).unwrap();
        assert!(store.catalog().is_err());
        let target = root.path().join("target");
        fs::rename(&store.path, &target).unwrap();
        std::os::unix::fs::symlink(&target, &store.path).unwrap();
        assert!(store.catalog().is_err());
    }
}
