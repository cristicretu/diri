//! Explicit same-host Claude account handoff. Credentials never travel with history.
use super::*;
use diri_proto::{AgentKind, ContinueAccountParams, SessionRecord};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

const MAX_TRANSCRIPT: usize = 64 * 1024 * 1024;
const MARKER: &[u8] = b"\x1eDIRI-ACCOUNT-TRANSCRIPT\n";

/// Lifecycle operations reserve one session without holding the Registry during SSH.
pub(super) struct SessionOperation<'a> {
    server: &'a ControlServer,
    id: Option<String>,
}

impl<'a> SessionOperation<'a> {
    pub(super) fn acquire(
        server: &'a ControlServer,
        method: &str,
        params: Option<&Value>,
    ) -> Result<Self, ControlError> {
        let guarded = matches!(
            method,
            Method::SESSION_CONTINUE_ACCOUNT
                | Method::SESSION_RESUME
                | Method::SESSION_FORK
                | Method::SESSION_KILL
                | Method::SESSION_REMOVE
                | Method::SESSION_MIGRATE
                | Method::SESSION_ARCHIVE
                | Method::SESSION_UNARCHIVE
                | Method::SESSION_REPARENT_WORKTREE
        );
        let id = guarded
            .then(|| params?.get("sessionID")?.as_str().map(str::to_owned))
            .flatten();
        if let Some(id) = &id
            && !server
                .session_operations
                .lock()
                .map_err(poisoned)?
                .insert(id.clone())
        {
            return Err(ControlError::bad_request(
                "Another operation is already changing this session. Wait for it to finish.",
            ));
        }
        Ok(Self { server, id })
    }
}

impl Drop for SessionOperation<'_> {
    fn drop(&mut self) {
        if let Some(id) = &self.id
            && let Ok(mut operations) = self.server.session_operations.lock()
        {
            operations.remove(id);
        }
    }
}

impl ControlServer {
    pub(super) fn session_continue_account(
        &self,
        params: Option<Value>,
    ) -> Result<Value, ControlError> {
        let params: ContinueAccountParams = decode(params)?;
        let source = self
            .registry
            .lock()
            .map_err(poisoned)?
            .record(&params.session_id.0)
            .ok_or_else(|| ControlError::not_found("Session no longer exists"))?;
        if source.kind != AgentKind::CLAUDE_CODE
            || source.effective_kind() != &AgentKind::CLAUDE_CODE
        {
            return Err(ControlError::bad_request(
                "Continue with another account currently supports Claude Code conversations",
            ));
        }
        let conversation = source
            .agent_session_id
            .as_deref()
            .filter(|id| safe_component(id))
            .ok_or_else(|| {
                ControlError::bad_request("Claude has not saved a resumable conversation yet")
            })?;
        if params.account_profile_id.is_empty() {
            return Err(ControlError::bad_request(
                "Choose a saved Claude account profile",
            ));
        }
        let mut profile = self
            .accounts
            .lock()
            .map_err(poisoned)?
            .resolve(
                Some(&params.account_profile_id),
                source.kind.id(),
                source.host.as_deref(),
            )?
            .ok_or_else(|| ControlError::bad_request("Choose a saved Claude account profile"))?;
        let host = source
            .host
            .as_deref()
            .map(|id| self.resolve_host(id))
            .transpose()?;
        let mut target = source.clone();
        target.account_profile = Some(profile.clone());
        // Discover the executable, bind the account, and prepare its directory before stopping.
        let mut spec = if source.host.is_some() {
            self.remote_resume_spec(&target)?
        } else {
            let registry = self.registry.lock().map_err(poisoned)?;
            self.resume_spec(
                &registry,
                &source.id.0,
                source.kind.id(),
                &source.cwd,
                Some(conversation),
            )?
        };
        if source.host.is_none() {
            crate::accounts::bind_pty(&mut profile, &mut spec.pty)?;
        } else {
            // The remote spec already bound argv/env; retain its expanded directory in the record.
            crate::accounts::bind(&mut profile, &mut spec.pty.env)?;
        }
        target.account_profile = Some(profile.clone());
        let home = spec
            .pty
            .env
            .iter()
            .rev()
            .find(|(key, _)| key == "HOME")
            .map(|(_, value)| value)
            .ok_or_else(|| {
                ControlError::bad_request("Execution host did not report its home directory")
            })?;
        let source_root = source.account_profile.as_ref().map_or_else(
            || Path::new(home).join(".claude"),
            |p| PathBuf::from(&p.config_home),
        );
        if source_root == Path::new(&profile.config_home) {
            return Err(ControlError::bad_request(
                "This profile uses the same account directory. Choose a different account.",
            ));
        }
        let source_location = Location::source(&source_root, &source, conversation)?;
        let target_location = Location::new(
            PathBuf::from(&profile.config_home),
            crate::inject::claude_project_slug(&source.cwd),
            conversation.to_owned(),
        )?;
        let storage = Storage {
            remote: host.zip(self.remote.clone()),
        };
        let before = storage
            .read(&source_location)?
            .ok_or_else(missing_transcript)?;
        validate_transcript(&before, conversation)?;
        let destination = storage.read(&target_location)?;
        compatible_destination(destination.as_deref(), &before)?;
        {
            let mut registry = self.registry.lock().map_err(poisoned)?;
            let current = registry
                .record(&source.id.0)
                .ok_or_else(|| ControlError::not_found("Session no longer exists"))?;
            ensure_same_source(&source, &current)?;
            if registry.records().iter().any(|record| {
                record.id != source.id
                    && record.host == source.host
                    && record.agent_session_id.as_deref() == Some(conversation)
                    && record
                        .account_profile
                        .as_ref()
                        .is_some_and(|p| p.config_home == profile.config_home)
                    && registry.get(&record.id.0).is_some()
            }) {
                return Err(ControlError::bad_request(
                    "Another session is using this conversation in the destination account. Stop it first.",
                ));
            }
            registry.persist_now().map_err(io_control_error)?;
            // Termination waits for the old process tree; no two Claude writers share this session.
            registry
                .terminate(&source.id.0, Duration::from_secs(3))
                .map_err(io_control_error)?;
            registry.persist_now().map_err(io_control_error)?;
            self.publish_updated(&registry, &source.id.0);
        }
        let final_result = (|| {
            // Capture the final flushed transcript, including output written during shutdown.
            let final_bytes = storage
                .read(&source_location)?
                .ok_or_else(missing_transcript)?;
            validate_transcript(&final_bytes, conversation)?;
            storage.install(&target_location, &final_bytes)?;
            let mut registry = self.registry.lock().map_err(poisoned)?;
            let stopped = registry
                .record(&source.id.0)
                .ok_or_else(|| ControlError::not_found("Session no longer exists"))?;
            ensure_same_source(&source, &stopped)?;
            let mut target = stopped.clone();
            target.account_profile = Some(profile);
            target.transcript_path = Some(target_location.path().to_string_lossy().into_owned());
            target.needs_input = None;
            target.hibernation = None;
            if let Some(remote) = &spec.remote {
                target.remote_persistence = Some(remote.launch.persistence);
            }
            registry.insert_record(target);
            if let Err(error) = registry.persist_now() {
                registry.insert_record(stopped);
                return Err(io_control_error(error));
            }
            // The new binding is durable before launch. A launch failure retries this account,
            // never the limited account, and recovery capsules use the same binding.
            let result = registry.respawn(spec).map_err(io_control_error);
            self.publish_updated(&registry, &source.id.0);
            result?;
            registry.persist_now().map_err(io_control_error)?;
            encode(
                &registry
                    .record(&source.id.0)
                    .ok_or_else(|| ControlError::internal("Continued session vanished"))?,
            )
        })();
        final_result.map_err(|error: ControlError| ControlError { code: error.code, message: format!("Claude was stopped, but the account handoff could not finish: {}. Your saved conversation is intact; check the session's account and resume when ready.", error.message) })
    }
}

fn ensure_same_source(
    expected: &SessionRecord,
    actual: &SessionRecord,
) -> Result<(), ControlError> {
    if expected.kind != actual.kind
        || expected.cwd != actual.cwd
        || expected.host != actual.host
        || expected.agent_session_id != actual.agent_session_id
        || expected.account_profile != actual.account_profile
    {
        return Err(ControlError::bad_request(
            "Session changed while preparing the account handoff. Try again.",
        ));
    }
    Ok(())
}

fn missing_transcript() -> ControlError {
    ControlError::bad_request(
        "The saved Claude conversation could not be found. Wait for Claude to save it, then try again.",
    )
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|c| !c.is_control() && c != '/' && c != '\\')
}

struct Location {
    root: PathBuf,
    project: String,
    conversation: String,
}
impl Location {
    fn new(root: PathBuf, project: String, conversation: String) -> Result<Self, ControlError> {
        if !root.is_absolute()
            || root.to_string_lossy().chars().any(char::is_control)
            || root
                .components()
                .any(|c| c == std::path::Component::ParentDir)
            || !safe_component(&project)
            || !safe_component(&conversation)
        {
            return Err(ControlError::bad_request(
                "Invalid Claude conversation location",
            ));
        }
        Ok(Self {
            root,
            project,
            conversation,
        })
    }
    fn source(
        root: &Path,
        record: &SessionRecord,
        conversation: &str,
    ) -> Result<Self, ControlError> {
        let project = if let Some(path) = record.transcript_path.as_deref() {
            let relative = Path::new(path)
                .strip_prefix(root.join("projects"))
                .map_err(|_| {
                    ControlError::bad_request("Saved transcript is outside the source account")
                })?;
            let components = relative.components().collect::<Vec<_>>();
            if components.len() != 2
                || components[1].as_os_str()
                    != std::ffi::OsStr::new(&format!("{conversation}.jsonl"))
            {
                return Err(ControlError::bad_request(
                    "Saved transcript does not match this conversation",
                ));
            }
            components[0].as_os_str().to_string_lossy().into_owned()
        } else {
            crate::inject::claude_project_slug(&record.cwd)
        };
        Self::new(root.to_owned(), project, conversation.to_owned())
    }
    fn directory(&self) -> PathBuf {
        self.root.join("projects").join(&self.project)
    }
    fn path(&self) -> PathBuf {
        self.directory()
            .join(format!("{}.jsonl", self.conversation))
    }
    fn input(&self) -> Vec<u8> {
        format!(
            "{}\n{}\n{}\n",
            self.root.display(),
            self.project,
            self.conversation
        )
        .into_bytes()
    }
}

fn validate_transcript(bytes: &[u8], conversation: &str) -> Result<(), ControlError> {
    if bytes.is_empty() || bytes.len() > MAX_TRANSCRIPT || bytes.last() != Some(&b'\n') {
        return Err(ControlError::bad_request(
            "Claude transcript is empty, incomplete, or larger than 64 MiB",
        ));
    }
    let mut messages = 0;
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let value: Value = serde_json::from_slice(line)
            .map_err(|_| ControlError::bad_request("Claude transcript is incomplete or invalid"))?;
        if matches!(
            value.get("type").and_then(Value::as_str),
            Some("user" | "assistant")
        ) {
            if value.get("sessionId").and_then(Value::as_str) != Some(conversation) {
                return Err(ControlError::bad_request(
                    "Claude transcript belongs to another conversation",
                ));
            }
            messages += 1;
        }
    }
    if messages == 0 {
        return Err(missing_transcript());
    }
    Ok(())
}

fn compatible_destination(existing: Option<&[u8]>, source: &[u8]) -> Result<(), ControlError> {
    if existing.is_some_and(|bytes| !source.starts_with(bytes)) {
        return Err(ControlError::bad_request(
            "The destination account has a different version of this conversation. Choose another profile; its history will not be overwritten.",
        ));
    }
    Ok(())
}

struct Storage {
    remote: Option<(
        diri_proto::HostEntry,
        Arc<crate::remote::manager::RemoteManager>,
    )>,
}
impl Storage {
    fn read(&self, location: &Location) -> Result<Option<Vec<u8>>, ControlError> {
        if let Some((host, manager)) = &self.remote {
            let output = manager
                .run_fixed_script(
                    host,
                    READ_TRANSCRIPT,
                    location.input(),
                    Duration::from_secs(20),
                    MAX_TRANSCRIPT + 16384,
                )
                .map_err(|_| {
                    ControlError::internal("Could not read the remote Claude transcript")
                })?;
            if output.status.code() == Some(44) {
                return Ok(None);
            }
            if !output.status.success() || output.stdout_truncated {
                return Err(ControlError::bad_request(
                    "Remote Claude transcript is unavailable, unsafe, or too large",
                ));
            }
            let start = output
                .stdout
                .windows(MARKER.len())
                .position(|part| part == MARKER)
                .ok_or_else(|| ControlError::internal("Missing remote transcript envelope"))?
                + MARKER.len();
            let bytes = output.stdout[start..].to_vec();
            if bytes.len() > MAX_TRANSCRIPT {
                return Err(ControlError::bad_request(
                    "Claude transcript is larger than 64 MiB",
                ));
            }
            return Ok(Some(bytes));
        }
        read_local(location)
    }
    fn install(&self, location: &Location, bytes: &[u8]) -> Result<(), ControlError> {
        if let Some((host, manager)) = &self.remote {
            let mut input = location.input();
            input.extend_from_slice(format!("{}\n", crate::inject::uuid_v4()).as_bytes());
            input.extend_from_slice(bytes);
            let output = manager
                .run_fixed_script(
                    host,
                    INSTALL_TRANSCRIPT,
                    input,
                    Duration::from_secs(30),
                    16384,
                )
                .map_err(|_| {
                    ControlError::internal("Could not install the remote Claude transcript")
                })?;
            if !output.status.success() {
                return Err(ControlError::bad_request(
                    "Remote destination changed, has conflicting history, or cannot store the conversation",
                ));
            }
            return Ok(());
        }
        install_local(location, bytes)
    }
}

fn private_directory(path: &Path, create: bool) -> Result<bool, ControlError> {
    if create {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(e)
                }
            })
            .map_err(|_| ControlError::bad_request("Cannot create the conversation directory"))?;
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => {
            return Err(ControlError::bad_request(
                "Cannot inspect the conversation directory",
            ));
        }
    };
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(ControlError::bad_request(
            "Conversation directories must belong to you and must not be symlinks",
        ));
    }
    Ok(true)
}

fn check_directories(location: &Location, create: bool) -> Result<bool, ControlError> {
    for path in [
        &location.root,
        &location.root.join("projects"),
        &location.directory(),
    ] {
        if !private_directory(path, create)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn read_local(location: &Location) -> Result<Option<Vec<u8>>, ControlError> {
    if !check_directories(location, false)? {
        return Ok(None);
    }
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(location.path())
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(ControlError::bad_request(
                "Cannot read the Claude transcript safely",
            ));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|_| ControlError::bad_request("Cannot inspect the Claude transcript"))?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.len() > MAX_TRANSCRIPT as u64
    {
        return Err(ControlError::bad_request(
            "Claude transcript is unsafe or too large",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_TRANSCRIPT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ControlError::internal("Cannot read the Claude transcript"))?;
    if bytes.len() > MAX_TRANSCRIPT {
        return Err(ControlError::bad_request("Claude transcript is too large"));
    }
    Ok(Some(bytes))
}

fn install_local(location: &Location, bytes: &[u8]) -> Result<(), ControlError> {
    check_directories(location, true)?;
    compatible_destination(read_local(location)?.as_deref(), bytes)?;
    let temporary = location
        .directory()
        .join(format!(".diri-account-{}.tmp", crate::inject::uuid_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|_| ControlError::internal("Cannot stage the Claude conversation"))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| ControlError::internal("Cannot save the Claude conversation"))?;
        compatible_destination(read_local(location)?.as_deref(), bytes)?;
        fs::rename(&temporary, location.path())
            .map_err(|_| ControlError::internal("Cannot activate the Claude conversation"))?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}

// Paths are data on stdin, never shell source. Only one bounded conversation is
// read/written. Startup noise is outside the read envelope; stderr is never surfaced.
const READ_TRANSCRIPT: &str = r#"sh -c 'IFS= read -r root && IFS= read -r project && IFS= read -r conversation || exit 73; for dir in "$root" "$root/projects" "$root/projects/$project"; do [ ! -L "$dir" ] || exit 73; [ -e "$dir" ] || exit 44; [ -d "$dir" ] && [ -O "$dir" ] || exit 73; done; file="$root/projects/$project/$conversation.jsonl"; [ ! -L "$file" ] || exit 73; [ -e "$file" ] || exit 44; [ -f "$file" ] && [ -O "$file" ] || exit 73; [ "$(wc -c < "$file")" -le 67108864 ] || exit 74; printf "\036DIRI-ACCOUNT-TRANSCRIPT\n"; head -c 67108865 "$file"'"#;
const INSTALL_TRANSCRIPT: &str = r#"sh -c 'IFS= read -r root && IFS= read -r project && IFS= read -r conversation && IFS= read -r nonce || exit 73; umask 077; for dir in "$root" "$root/projects" "$root/projects/$project"; do [ ! -L "$dir" ] || exit 73; mkdir -p "$dir" || exit 73; [ -d "$dir" ] && [ -O "$dir" ] || exit 73; done; target="$root/projects/$project/$conversation.jsonl"; tmp="$root/projects/$project/.diri-account-$nonce.tmp"; set -C; : > "$tmp" || exit 75; cleanup() { rm -f "$tmp"; }; trap cleanup 0; cat >> "$tmp" || exit 75; [ "$(wc -c < "$tmp")" -le 67108864 ] || exit 74; [ ! -L "$target" ] || exit 73; if [ -e "$target" ]; then [ -f "$target" ] && [ -O "$target" ] || exit 73; count=$(wc -c < "$target"); [ "$count" -le 67108864 ] || exit 74; head -c "$count" "$tmp" | cmp -s - "$target" || exit 76; fi; mv -f "$tmp" "$target"'"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::{Command, Stdio};

    fn transcript(text: &str) -> Vec<u8> {
        format!("{}\n", json!({"type":"user", "sessionId":"conversation-1", "message":{"role":"user","content":text}})).into_bytes()
    }

    fn location(root: PathBuf) -> Location {
        Location::new(root, "-project".into(), "conversation-1".into()).unwrap()
    }

    fn script(script: &str, location: &Location, bytes: Option<&[u8]>) -> std::process::Output {
        let mut input = location.input();
        if let Some(bytes) = bytes {
            input.extend_from_slice(b"fixture-nonce\n");
            input.extend_from_slice(bytes);
        }
        let mut child = Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Fixtures are small enough to fit in a pipe; no concurrent reader is necessary.
        child.stdin.take().unwrap().write_all(&input).unwrap();
        child.wait_with_output().unwrap()
    }

    #[test]
    fn transcript_requires_complete_matching_conversation_without_exposing_content() {
        let bytes = transcript("private prompt");
        validate_transcript(&bytes, "conversation-1").unwrap();
        for invalid in [
            b"".as_slice(),
            b"{}\n",
            b"private prompt\n",
            &bytes[..bytes.len() - 1],
        ] {
            let error = validate_transcript(invalid, "conversation-1").unwrap_err();
            assert!(!error.message.contains("private prompt"));
        }
        assert!(validate_transcript(&bytes, "other-id").is_err());
        assert!(Location::new("/tmp/../elsewhere".into(), "p".into(), "id".into()).is_err());
        assert!(Location::new("/tmp".into(), "../p".into(), "id".into()).is_err());
    }

    #[test]
    fn local_round_trip_preserves_credentials_permissions_and_conflicting_history() {
        let temp = tempfile::tempdir().unwrap();
        let first = location(temp.path().join("first"));
        let second = location(temp.path().join("second"));
        let initial = transcript("first turn");
        install_local(&first, &initial).unwrap();
        fs::write(first.root.join(".credentials.json"), "source credential").unwrap();
        install_local(&second, &read_local(&first).unwrap().unwrap()).unwrap();
        assert!(!second.root.join(".credentials.json").exists());
        assert_eq!(fs::metadata(second.path()).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(second.directory()).unwrap().mode() & 0o777,
            0o700
        );
        let mut continued = initial.clone();
        continued.extend(transcript("second turn"));
        fs::write(second.path(), &continued).unwrap();
        install_local(&first, &continued).unwrap();
        assert_eq!(read_local(&first).unwrap().unwrap(), continued);
        assert_eq!(
            fs::read_to_string(first.root.join(".credentials.json")).unwrap(),
            "source credential"
        );
        assert!(install_local(&first, &initial).is_err());
        assert_eq!(read_local(&first).unwrap().unwrap(), continued);
        assert!(fs::read_dir(first.directory()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[test]
    fn local_and_remote_scripts_reject_symlinks_and_oversized_files() {
        let temp = tempfile::tempdir().unwrap();
        let destination = location(temp.path().join("destination"));
        install_local(&destination, &transcript("hello")).unwrap();
        let external = temp.path().join("external");
        fs::write(&external, b"untouched").unwrap();
        fs::remove_file(destination.path()).unwrap();
        symlink(&external, destination.path()).unwrap();
        assert!(read_local(&destination).is_err());
        assert!(install_local(&destination, &transcript("hello")).is_err());
        assert!(!script(READ_TRANSCRIPT, &destination, None).status.success());
        assert!(
            !script(INSTALL_TRANSCRIPT, &destination, Some(&transcript("hello")))
                .status
                .success()
        );
        assert_eq!(fs::read(&external).unwrap(), b"untouched");
        fs::remove_file(destination.path()).unwrap();
        fs::File::create(destination.path())
            .unwrap()
            .set_len(MAX_TRANSCRIPT as u64 + 1)
            .unwrap();
        assert!(read_local(&destination).is_err());
        assert!(!script(READ_TRANSCRIPT, &destination, None).status.success());
        fs::remove_dir_all(destination.directory()).unwrap();
        symlink(temp.path(), destination.directory()).unwrap();
        assert!(read_local(&destination).is_err());
        assert!(
            !script(INSTALL_TRANSCRIPT, &destination, Some(&transcript("hello")))
                .status
                .success()
        );
    }

    #[test]
    fn remote_scripts_round_trip_literal_paths_and_refuse_divergence() {
        let temp = tempfile::tempdir().unwrap();
        let destination = location(temp.path().join("account ' $(touch injected)"));
        assert_eq!(
            script(READ_TRANSCRIPT, &destination, None).status.code(),
            Some(44)
        );
        let initial = transcript("first");
        assert!(
            script(INSTALL_TRANSCRIPT, &destination, Some(&initial))
                .status
                .success()
        );
        let read = script(READ_TRANSCRIPT, &destination, None);
        assert!(read.status.success());
        assert_eq!(read.stdout, [MARKER, &initial].concat());
        let continued = [initial.as_slice(), &transcript("second")].concat();
        assert!(
            script(INSTALL_TRANSCRIPT, &destination, Some(&continued))
                .status
                .success()
        );
        assert!(
            !script(
                INSTALL_TRANSCRIPT,
                &destination,
                Some(&transcript("diverged"))
            )
            .status
            .success()
        );
        assert_eq!(fs::read(destination.path()).unwrap(), continued);
        assert_eq!(
            fs::metadata(destination.path()).unwrap().mode() & 0o777,
            0o600
        );
        assert!(!temp.path().join("injected").exists());
    }

    #[test]
    fn lifecycle_reservation_is_exclusive_and_released_on_drop() {
        let temp = tempfile::tempdir().unwrap();
        let server = super::super::tests::server(temp.path());
        let params = json!({"sessionID":"session"});
        let guard =
            SessionOperation::acquire(&server, Method::SESSION_CONTINUE_ACCOUNT, Some(&params))
                .unwrap();
        assert!(
            server
                .dispatch(Method::SESSION_KILL, Some(params.clone()))
                .unwrap_err()
                .message
                .contains("already changing")
        );
        drop(guard);
        assert!(SessionOperation::acquire(&server, Method::SESSION_RESUME, Some(&params)).is_ok());
    }

    #[test]
    fn claude_switches_live_accounts_and_back_with_same_conversation() {
        let temp = tempfile::tempdir().unwrap();
        let server = super::super::tests::server(temp.path());
        let executable = temp.path().join("claude");
        fs::write(&executable, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CLAUDE_CONFIG_DIR/args\"\nprintf '%s\\n' launched >> \"$CLAUDE_CONFIG_DIR/launches\"\nexec /bin/sleep 30\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        server
            .agent_catalog
            .lock()
            .unwrap()
            .configure(
                None,
                "claude-code",
                crate::agent_catalog::AgentPreference {
                    executable_path: Some(executable.to_string_lossy().into_owned()),
                    show_in_quick_create: Some(true),
                },
            )
            .unwrap();
        let profiles = ["work", "personal"].map(|id| diri_proto::AgentAccountProfile {
            id: id.into(),
            label: id.into(),
            agent: "claude-code".into(),
            host: None,
            config_home: temp.path().join(id).to_string_lossy().into_owned(),
            is_default: false,
        });
        for profile in &profiles {
            server
                .accounts
                .lock()
                .unwrap()
                .upsert(profile.clone())
                .unwrap();
        }
        let source: SessionRecord = serde_json::from_value(
            server
                .session_spawn(Some(json!({
                    "kind": AgentKind::CLAUDE_CODE, "cwd":temp.path(), "accountProfileId":"work"
                })))
                .unwrap(),
        )
        .unwrap();
        let wait_launch = |index: usize, count| {
            let path = Path::new(&profiles[index].config_home).join("launches");
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while fs::read_to_string(&path)
                .unwrap_or_default()
                .lines()
                .count()
                < count
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "fake Claude did not launch"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait_launch(0, 1);
        server
            .registry
            .lock()
            .unwrap()
            .update_record(&source.id.0, |r| {
                r.agent_session_id = Some("conversation-1".into());
                r.transcript_path = None;
                r.title = "Preserve this title".into();
            });
        let params = json!({"sessionID":source.id, "accountProfileId":"personal"});
        // Missing history fails before stopping the original live process.
        assert!(
            server
                .dispatch(Method::SESSION_CONTINUE_ACCOUNT, Some(params.clone()))
                .is_err()
        );
        assert!(server.registry.lock().unwrap().get(&source.id.0).is_some());
        let locations = profiles.each_ref().map(|p| {
            Location::new(
                PathBuf::from(&p.config_home),
                crate::inject::claude_project_slug(&source.cwd),
                "conversation-1".into(),
            )
            .unwrap()
        });
        let initial = transcript("first turn");
        install_local(&locations[0], &initial).unwrap();
        install_local(&locations[1], &transcript("different branch")).unwrap();
        assert!(
            server
                .dispatch(Method::SESSION_CONTINUE_ACCOUNT, Some(params.clone()))
                .is_err()
        );
        assert!(server.registry.lock().unwrap().get(&source.id.0).is_some());
        fs::remove_file(locations[1].path()).unwrap();
        let continued: SessionRecord = serde_json::from_value(
            server
                .dispatch(Method::SESSION_CONTINUE_ACCOUNT, Some(params))
                .unwrap(),
        )
        .unwrap();
        wait_launch(1, 1);
        assert_eq!(continued.id, source.id);
        assert_eq!(continued.cwd, source.cwd);
        assert_eq!(continued.title, "Preserve this title");
        assert_eq!(
            continued.agent_session_id.as_deref(),
            Some("conversation-1")
        );
        assert_eq!(continued.account_profile.as_ref(), Some(&profiles[1]));
        assert_eq!(fs::read(locations[1].path()).unwrap(), initial);
        let args = fs::read_to_string(Path::new(&profiles[1].config_home).join("args")).unwrap();
        assert!(args.contains("--resume\nconversation-1\n"));
        let newer = [initial.as_slice(), &transcript("second turn")].concat();
        fs::write(locations[1].path(), &newer).unwrap();
        let back: SessionRecord = serde_json::from_value(
            server
                .dispatch(
                    Method::SESSION_CONTINUE_ACCOUNT,
                    Some(json!({"sessionID":source.id,"accountProfileId":"work"})),
                )
                .unwrap(),
        )
        .unwrap();
        wait_launch(0, 2);
        assert_eq!(back.id, source.id);
        assert_eq!(back.account_profile.as_ref(), Some(&profiles[0]));
        assert_eq!(fs::read(locations[0].path()).unwrap(), newer);
        let durable = fs::read_to_string(temp.path().join("state.json")).unwrap();
        assert!(durable.contains(&profiles[0].config_home));
        server
            .session_kill(Some(json!({"sessionID":source.id})))
            .unwrap();
    }
}
