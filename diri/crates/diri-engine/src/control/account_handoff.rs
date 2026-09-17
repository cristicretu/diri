//! Same-host account handoff. Provider logins never travel with conversation history.
use super::*;
use diri_proto::{AgentKind, ContinueAccountParams, SessionRecord};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

mod tools;

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
                | Method::SESSION_HIBERNATE
                | Method::SESSION_WAKE
                | Method::SESSION_RESUME
                | Method::SESSION_RECONNECT
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
        Self::reserve(server, id)
    }

    pub(super) fn for_session(server: &'a ControlServer, id: &str) -> Result<Self, ControlError> {
        Self::reserve(server, Some(id.to_owned()))
    }

    fn reserve(server: &'a ControlServer, id: Option<String>) -> Result<Self, ControlError> {
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
    pub(super) fn account_switch_all(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let params: diri_proto::SwitchAccountParams = decode(params)?;
        let mut profile = self
            .accounts
            .lock()
            .map_err(poisoned)?
            .catalog()?
            .profiles
            .into_iter()
            .find(|p| p.id == params.account_profile_id)
            .ok_or_else(|| ControlError::not_found("Choose a saved account profile"))?;
        let mut records = self.registry.lock().map_err(poisoned)?.records();
        records.retain(|r| r.kind.id() == profile.agent && r.host == profile.host);
        records.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        let mut result = diri_proto::SwitchAccountResult::default();
        let mut guards = Vec::new();
        let mut prepared = Vec::new();
        for record in records {
            let params = ContinueAccountParams {
                session_id: record.id.clone(),
                account_profile_id: profile.id.clone(),
            };
            let value = serde_json::to_value(&params)
                .map_err(|_| ControlError::internal("Cannot prepare account switch"))?;
            let preparation =
                SessionOperation::acquire(self, Method::SESSION_CONTINUE_ACCOUNT, Some(&value))
                    .and_then(|guard| {
                        let handoff = self.prepare_account_handoff(&params)?;
                        guards.push(guard);
                        Ok(handoff)
                    });
            match preparation {
                Ok(handoff) => prepared.push(handoff),
                Err(error) => result.failures.push(diri_proto::AccountSwitchFailure {
                    session_id: record.id,
                    message: error.message,
                }),
            }
        }
        // Nothing is stopped if any conversation cannot be safely prepared.
        if !result.failures.is_empty() {
            return encode(&result);
        }
        let mut conversations = std::collections::HashSet::new();
        for handoff in &prepared {
            if !conversations.insert(handoff.source.agent_session_id.clone()) {
                return Err(ControlError::bad_request(
                    "Multiple tracked sessions use the same conversation. Close duplicate sessions before switching accounts.",
                ));
            }
        }
        let tools = tools::ToolTransfer::prepare(&prepared)?;
        let mut stopped = Vec::new();
        for handoff in prepared {
            match self.stop_account_handoff(&handoff) {
                Ok(()) => stopped.push(handoff),
                Err(error) => result.failures.push(diri_proto::AccountSwitchFailure {
                    session_id: handoff.source.id,
                    message: error.message,
                }),
            }
        }
        // Do not move refreshable tool credentials while any selected Agent is still running.
        if !result.failures.is_empty() {
            for handoff in stopped {
                result.failures.push(diri_proto::AccountSwitchFailure { session_id: handoff.source.id, message: "Stopped on the original account because another conversation could not stop. Resume it or retry the switch.".into() });
            }
            return encode(&result);
        }
        if let Err(error) = tools.install(&stopped) {
            for handoff in stopped {
                result.failures.push(diri_proto::AccountSwitchFailure {
                    session_id: handoff.source.id,
                    message: format!(
                        "Stopped on the original account: {}. Resume it or retry the switch.",
                        error.message
                    ),
                });
            }
            return encode(&result);
        }
        for handoff in stopped {
            let id = handoff.source.id.clone();
            match self.finish_account_handoff(handoff) {
                Ok(value) => result.switched.push(
                    serde_json::from_value(value)
                        .map_err(|_| ControlError::internal("Invalid switched session"))?,
                ),
                Err(error) => result.failures.push(diri_proto::AccountSwitchFailure {
                    session_id: id,
                    message: error.message,
                }),
            }
        }
        if result.failures.is_empty() {
            profile.is_default = true;
            match self.accounts.lock().map_err(poisoned)?.upsert(profile) {
                Ok(_) => result.default_changed = true,
                Err(error) => result.default_error = Some(error.message),
            }
        }
        encode(&result)
    }

    pub(super) fn session_continue_account(
        &self,
        params: Option<Value>,
    ) -> Result<Value, ControlError> {
        let params: ContinueAccountParams = decode(params)?;
        let mut prepared = self.prepare_account_handoff(&params)?;
        // The single-conversation API is an explicit resume, including stopped sessions.
        prepared.was_running = true;
        self.stop_account_handoff(&prepared)?;
        self.finish_account_handoff(prepared)
    }

    fn prepare_account_handoff(
        &self,
        params: &ContinueAccountParams,
    ) -> Result<PreparedHandoff, ControlError> {
        let source = self
            .registry
            .lock()
            .map_err(poisoned)?
            .record(&params.session_id.0)
            .ok_or_else(|| ControlError::not_found("Session no longer exists"))?;
        if !matches!(
            source.kind.id(),
            AgentKind::CLAUDE_CODE_ID | AgentKind::CODEX_ID
        ) || source.effective_kind() != &source.kind
        {
            return Err(ControlError::bad_request(
                "Continue with another account supports Claude Code and Codex conversations",
            ));
        }
        let conversation = source
            .agent_session_id
            .as_deref()
            .filter(|id| safe_component(id))
            .ok_or_else(|| {
                ControlError::bad_request("This Agent has not saved a resumable conversation yet. Finish or close this setup session before switching all conversations.")
            })?;
        if params.account_profile_id.is_empty() {
            return Err(ControlError::bad_request("Choose a saved account profile"));
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
            .ok_or_else(|| ControlError::bad_request("Choose a saved account profile"))?;
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
            || {
                Path::new(home).join(if source.kind == AgentKind::CODEX {
                    ".codex"
                } else {
                    ".claude"
                })
            },
            |p| PathBuf::from(&p.config_home),
        );
        let source_location = if source.kind == AgentKind::CODEX {
            Location::codex_source(&source_root, &source, conversation, Path::new(home))?
        } else {
            Location::source(&source_root, &source, conversation)?
        };
        let target_location = Location {
            root: PathBuf::from(&profile.config_home),
            relative: source_location.relative.clone(),
        };
        let storage = Storage {
            remote: host.zip(self.remote.clone()),
        };
        let before = storage
            .read(&source_location)?
            .ok_or_else(missing_transcript)?;
        validate_conversation(&before, &source)?;
        if source.kind == AgentKind::CODEX {
            storage.check_codex_destination(&target_location, conversation)?;
        }
        let destination = storage.read(&target_location)?;
        compatible_destination(destination.as_deref(), &before)?;
        let was_running = self
            .registry
            .lock()
            .map_err(poisoned)?
            .get(&source.id.0)
            .is_some();
        Ok(PreparedHandoff {
            source,
            profile,
            spec,
            source_location,
            target_location,
            storage,
            was_running,
        })
    }

    fn stop_account_handoff(&self, prepared: &PreparedHandoff) -> Result<(), ControlError> {
        let source = &prepared.source;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        let current = registry
            .record(&source.id.0)
            .ok_or_else(|| ControlError::not_found("Session no longer exists"))?;
        ensure_same_source(source, &current)?;
        if registry.records().iter().any(|record| {
            record.id != source.id
                && record.host == source.host
                && record.agent_session_id.as_deref() == source.agent_session_id.as_deref()
                && record
                    .account_profile
                    .as_ref()
                    .is_some_and(|p| p.config_home == prepared.profile.config_home)
                && registry.get(&record.id.0).is_some()
        }) {
            return Err(ControlError::bad_request(
                "Another session is using this conversation in the destination account. Stop it first.",
            ));
        }
        registry.persist_now().map_err(io_control_error)?;
        // Termination waits for the old process tree; no two Claude writers share this session.
        drop(registry);
        self.terminate_session_unlocked(&source.id.0, Duration::from_secs(3))?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry.persist_now().map_err(io_control_error)?;
        self.publish_updated(&registry, &source.id.0);
        Ok(())
    }

    fn finish_account_handoff(&self, prepared: PreparedHandoff) -> Result<Value, ControlError> {
        let PreparedHandoff {
            source,
            profile,
            spec,
            source_location,
            target_location,
            storage,
            was_running,
        } = prepared;
        let final_result = (|| {
            // Capture the final flushed transcript, including output written during shutdown.
            let final_bytes = storage
                .read(&source_location)?
                .ok_or_else(missing_transcript)?;
            validate_conversation(&final_bytes, &source)?;
            if source.kind == AgentKind::CODEX {
                storage.check_codex_destination(
                    &target_location,
                    source.agent_session_id.as_deref().expect("validated"),
                )?;
            }
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
            let result = if !was_running {
                Ok(())
            } else if spec.remote.is_some() {
                drop(registry);
                let result = self.spawn_session_unlocked(spec, None);
                registry = self.registry.lock().map_err(poisoned)?;
                result
            } else {
                registry.respawn(spec).map_err(io_control_error)
            };
            self.publish_updated(&registry, &source.id.0);
            result?;
            registry.persist_now().map_err(io_control_error)?;
            encode(
                &registry
                    .record(&source.id.0)
                    .ok_or_else(|| ControlError::internal("Continued session vanished"))?,
            )
        })();
        final_result.map_err(|error: ControlError| ControlError { code: error.code, message: format!("The Agent was stopped, but the account handoff could not finish: {}. Your saved conversation is intact; check the session's account and resume when ready.", error.message) })
    }
}

struct PreparedHandoff {
    source: SessionRecord,
    profile: diri_proto::AgentAccountProfile,
    spec: crate::session::SessionSpec,
    source_location: Location,
    target_location: Location,
    storage: Storage,
    was_running: bool,
}

fn validate_conversation(bytes: &[u8], source: &SessionRecord) -> Result<(), ControlError> {
    let conversation = source
        .agent_session_id
        .as_deref()
        .ok_or_else(missing_transcript)?;
    if source.kind != AgentKind::CODEX {
        return validate_transcript(bytes, conversation);
    }
    if bytes.is_empty() || bytes.len() > MAX_TRANSCRIPT || bytes.last() != Some(&b'\n') {
        return Err(ControlError::bad_request(
            "Codex transcript is empty, incomplete, or larger than 64 MiB",
        ));
    }
    let mut identity = false;
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let value: Value = serde_json::from_slice(line)
            .map_err(|_| ControlError::bad_request("Codex transcript is incomplete or invalid"))?;
        if value["type"] == "session_meta" {
            if identity
                || value["payload"]["id"].as_str() != Some(conversation)
                || value["payload"]["cwd"].as_str() != Some(source.cwd.as_str())
            {
                return Err(ControlError::bad_request(
                    "Codex transcript belongs to another conversation or folder",
                ));
            }
            identity = true;
        }
    }
    if !identity {
        return Err(missing_transcript());
    }
    Ok(())
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
        "The saved conversation could not be found. Finish or close this setup session, then retry the account switch.",
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
    relative: PathBuf,
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
            relative: PathBuf::from("projects")
                .join(project)
                .join(format!("{conversation}.jsonl")),
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
    fn codex_source(
        root: &Path,
        record: &SessionRecord,
        conversation: &str,
        home: &Path,
    ) -> Result<Self, ControlError> {
        let path = record
            .transcript_path
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| {
                if record.host.is_some() {
                    return None;
                }
                crate::history::find_profile_codex_transcript(
                    record.account_profile.as_ref(),
                    home,
                    conversation,
                    &record.cwd,
                )
                .map(|t| t.path().to_owned())
            })
            .ok_or_else(|| {
                ControlError::bad_request("Codex has not saved a discoverable conversation yet")
            })?;
        let relative = path
            .strip_prefix(root)
            .map_err(|_| {
                ControlError::bad_request("Saved transcript is outside the source account")
            })?
            .to_path_buf();
        let components: Vec<_> = relative.components().collect();
        if components.len() != 5
            || components[0].as_os_str() != "sessions"
            || components.iter().any(|c| {
                !matches!(c, std::path::Component::Normal(_))
                    || !safe_component(&c.as_os_str().to_string_lossy())
            })
            || !path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with("rollout-") && n.ends_with(&format!("-{conversation}.jsonl"))
            })
        {
            return Err(ControlError::bad_request(
                "Invalid Codex conversation location",
            ));
        }
        Ok(Self {
            root: root.to_owned(),
            relative,
        })
    }
    fn directory(&self) -> PathBuf {
        self.path().parent().expect("transcript parent").to_owned()
    }
    fn path(&self) -> PathBuf {
        self.root.join(&self.relative)
    }
    fn input(&self) -> Vec<u8> {
        format!("{}\n{}\n", self.root.display(), self.relative.display()).into_bytes()
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
    fn check_codex_destination(
        &self,
        location: &Location,
        conversation: &str,
    ) -> Result<(), ControlError> {
        let conflict = || {
            ControlError::bad_request(
                "The destination contains another rollout for this Codex conversation. Resolve its history before switching accounts.",
            )
        };
        if let Some((host, manager)) = &self.remote {
            let mut input = location.input();
            input.extend_from_slice(format!("{conversation}\n").as_bytes());
            let output = manager
                .run_fixed_script(
                    host,
                    CHECK_CODEX_DESTINATION,
                    input,
                    Duration::from_secs(20),
                    4096,
                )
                .map_err(|_| conflict())?;
            return if output.status.success() {
                Ok(())
            } else {
                Err(conflict())
            };
        }
        let suffix = format!("-{conversation}.jsonl");
        let expected = location.path();
        let mut pending = vec![
            (location.root.join("sessions"), 0),
            (location.root.join("archived_sessions"), 3),
        ];
        let mut count = 0;
        while let Some((directory, depth)) = pending.pop() {
            if !private_directory(&directory, false)? {
                continue;
            }
            for entry in fs::read_dir(directory).map_err(|_| conflict())? {
                count += 1;
                if count > 100_000 {
                    return Err(conflict());
                }
                let entry = entry.map_err(|_| conflict())?;
                let kind = entry.file_type().map_err(|_| conflict())?;
                if kind.is_dir() && depth < 3 {
                    pending.push((entry.path(), depth + 1));
                }
                if depth == 3
                    && entry.file_name().to_string_lossy().ends_with(&suffix)
                    && entry.path() != expected
                {
                    return Err(conflict());
                }
            }
        }
        Ok(())
    }

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
    let mut path = location.root.clone();
    if !private_directory(&path, create)? {
        return Ok(false);
    }
    for part in location
        .relative
        .parent()
        .expect("transcript parent")
        .components()
    {
        path.push(part);
        if !private_directory(&path, create)? {
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
const CHECK_CODEX_DESTINATION: &str = r#"sh -c 'IFS= read -r root && IFS= read -r relative && IFS= read -r conversation || exit 73; expected="$root/$relative"; for file in "$root"/sessions/*/*/*/*-"$conversation".jsonl "$root"/archived_sessions/*-"$conversation".jsonl; do [ -e "$file" ] || continue; [ "$file" = "$expected" ] || exit 76; done'"#;
const READ_TRANSCRIPT: &str = r#"sh -c 'IFS= read -r root && IFS= read -r relative || exit 73; set -f; dir="$root"; [ ! -L "$dir" ] || exit 73; [ -e "$dir" ] || exit 44; [ -d "$dir" ] && [ -O "$dir" ] || exit 73; oldifs="$IFS"; IFS=/; set -- $relative; IFS="$oldifs"; while [ "$#" -gt 1 ]; do dir="$dir/$1"; shift; [ ! -L "$dir" ] || exit 73; [ -e "$dir" ] || exit 44; [ -d "$dir" ] && [ -O "$dir" ] || exit 73; done; file="$root/$relative"; [ ! -L "$file" ] || exit 73; [ -e "$file" ] || exit 44; [ -f "$file" ] && [ -O "$file" ] || exit 73; [ "$(wc -c < "$file")" -le 67108864 ] || exit 74; printf "\036DIRI-ACCOUNT-TRANSCRIPT\n"; head -c 67108865 "$file"'"#;
const INSTALL_TRANSCRIPT: &str = r#"sh -c 'IFS= read -r root && IFS= read -r relative && IFS= read -r nonce || exit 73; set -f; umask 077; dir="$root"; [ ! -L "$dir" ] || exit 73; mkdir -p "$dir" || exit 73; [ -d "$dir" ] && [ -O "$dir" ] || exit 73; oldifs="$IFS"; IFS=/; set -- $relative; IFS="$oldifs"; while [ "$#" -gt 1 ]; do dir="$dir/$1"; shift; [ ! -L "$dir" ] || exit 73; mkdir -p "$dir" || exit 73; [ -d "$dir" ] && [ -O "$dir" ] || exit 73; done; target="$root/$relative"; tmp="$dir/.diri-account-$nonce.tmp"; set -C; : > "$tmp" || exit 75; cleanup() { rm -f "$tmp"; }; trap cleanup 0; cat >> "$tmp" || exit 75; [ "$(wc -c < "$tmp")" -le 67108864 ] || exit 74; [ ! -L "$target" ] || exit 73; if [ -e "$target" ]; then [ -f "$target" ] && [ -O "$target" ] || exit 73; count=$(wc -c < "$target"); [ "$count" -le 67108864 ] || exit 74; head -c "$count" "$tmp" | cmp -s - "$target" || exit 76; fi; mv -f "$tmp" "$target"'"#;

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
    fn bulk_codex_switch_preserves_all_conversations_tools_and_login_isolation() {
        let temp = tempfile::tempdir().unwrap();
        let server = super::super::tests::server(temp.path());
        let executable = temp.path().join("codex");
        fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CODEX_HOME/args\"\nexec /bin/sleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        server
            .agent_catalog
            .lock()
            .unwrap()
            .configure(
                None,
                "codex",
                crate::agent_catalog::AgentPreference {
                    executable_path: Some(executable.to_string_lossy().into_owned()),
                    show_in_quick_create: Some(true),
                },
            )
            .unwrap();
        let profiles = ["one", "two"].map(|id| diri_proto::AgentAccountProfile {
            id: id.into(),
            label: id.into(),
            agent: "codex".into(),
            host: None,
            config_home: temp.path().join(id).to_string_lossy().into_owned(),
            is_default: false,
        });
        for profile in &profiles {
            fs::create_dir(&profile.config_home).unwrap();
            fs::write(
                Path::new(&profile.config_home).join("auth.json"),
                profile.id.as_bytes(),
            )
            .unwrap();
            server
                .accounts
                .lock()
                .unwrap()
                .upsert(profile.clone())
                .unwrap();
        }
        fs::write(
            Path::new(&profiles[0].config_home).join("config.toml"),
            "[mcp_servers.docs]\nurl='https://example.test/mcp'\n",
        )
        .unwrap();
        fs::write(
            Path::new(&profiles[1].config_home).join("config.toml"),
            "model='target-model'\n",
        )
        .unwrap();
        let cache = Path::new(&profiles[0].config_home).join("plugins/cache/fixture/1");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("plugin.json"), b"{}").unwrap();
        let mut sources = Vec::new();
        for index in 0..3 {
            let source: SessionRecord = serde_json::from_value(
                server
                    .session_spawn(Some(
                        json!({"kind":AgentKind::CODEX,"cwd":temp.path(),"accountProfileId":"one"}),
                    ))
                    .unwrap(),
            )
            .unwrap();
            let conversation = format!("11111111-1111-4111-8111-{index:012}");
            let relative = PathBuf::from(format!(
                "sessions/2026/09/17/rollout-2026-09-17T12-00-00-{conversation}.jsonl"
            ));
            let location = Location {
                root: profiles[0].config_home.clone().into(),
                relative: relative.clone(),
            };
            let bytes = format!(
                "{}\n",
                json!({"type":"session_meta","payload":{"id":conversation,"cwd":temp.path()}})
            )
            .into_bytes();
            install_local(&location, &bytes).unwrap();
            server
                .registry
                .lock()
                .unwrap()
                .update_record(&source.id.0, |r| {
                    r.agent_session_id = Some(conversation);
                    r.transcript_path = Some(location.path().to_string_lossy().into_owned());
                    r.title = format!("conversation {index}");
                });
            if index == 2 {
                server
                    .session_kill(Some(json!({"sessionID":source.id})))
                    .unwrap();
            }
            sources.push((source, relative, bytes));
        }
        // A conflict blocks the entire batch before any original process is stopped.
        let collision = Location {
            root: profiles[1].config_home.clone().into(),
            relative: sources[1].1.clone(),
        };
        install_local(&collision, b"conflict\n").unwrap();
        let params = json!({"accountProfileId":"two"});
        let rejected: diri_proto::SwitchAccountResult = serde_json::from_value(
            server
                .dispatch(Method::ACCOUNT_SWITCH_ALL, Some(params.clone()))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(rejected.failures.len(), 1);
        assert!(!rejected.default_changed);
        for (source, _, _) in &sources[..2] {
            assert!(server.registry.lock().unwrap().get(&source.id.0).is_some());
        }
        fs::remove_file(collision.path()).unwrap();
        let result: diri_proto::SwitchAccountResult = serde_json::from_value(
            server
                .dispatch(Method::ACCOUNT_SWITCH_ALL, Some(params))
                .unwrap(),
        )
        .unwrap();
        assert!(result.failures.is_empty(), "{:?}", result.failures);
        assert_eq!(result.switched.len(), 3);
        assert!(result.default_changed);
        for (index, (source, relative, bytes)) in sources.iter().enumerate() {
            let registry = server.registry.lock().unwrap();
            let target = registry.record(&source.id.0).unwrap();
            assert_eq!(target.account_profile.as_ref().unwrap().id, "two");
            assert_eq!(target.title, format!("conversation {index}"));
            assert_eq!(target.cwd, source.cwd);
            assert_eq!(registry.get(&source.id.0).is_some(), index != 2);
            assert_eq!(
                fs::read(Path::new(&profiles[1].config_home).join(relative)).unwrap(),
                *bytes
            );
        }
        assert!(
            fs::read_to_string(Path::new(&profiles[1].config_home).join("config.toml"))
                .unwrap()
                .contains("mcp_servers.docs")
        );
        for profile in &profiles {
            assert_eq!(
                fs::read(Path::new(&profile.config_home).join("auth.json")).unwrap(),
                profile.id.as_bytes()
            );
        }
        assert_eq!(
            fs::read(
                Path::new(&profiles[1].config_home).join("plugins/cache/fixture/1/plugin.json")
            )
            .unwrap(),
            b"{}"
        );
        let back: diri_proto::SwitchAccountResult = serde_json::from_value(
            server
                .dispatch(
                    Method::ACCOUNT_SWITCH_ALL,
                    Some(json!({"accountProfileId":"one"})),
                )
                .unwrap(),
        )
        .unwrap();
        assert!(back.failures.is_empty());
        assert_eq!(back.switched.len(), 3);
        for (source, _, _) in sources {
            server
                .registry
                .lock()
                .unwrap()
                .terminate(&source.id.0, Duration::from_secs(1))
                .unwrap();
        }
    }

    #[test]
    fn bulk_reservation_excludes_launches_and_profile_edits_but_allows_reads() {
        let temp = tempfile::tempdir().unwrap();
        let server = super::super::tests::server(temp.path());
        let guard = server.account_operations.write().unwrap();
        for method in [
            Method::SESSION_SPAWN,
            Method::SESSION_RESUME,
            Method::ACCOUNT_PROFILES_SAVE,
            Method::ACCOUNT_SWITCH_ALL,
        ] {
            assert!(
                server
                    .dispatch(method, Some(json!({})))
                    .unwrap_err()
                    .message
                    .contains("progress")
            );
        }
        assert!(server.dispatch(Method::ACCOUNT_PROFILES_LIST, None).is_ok());
        drop(guard);
        assert!(
            server
                .dispatch(
                    Method::ACCOUNT_SWITCH_ALL,
                    Some(json!({"accountProfileId":"missing"}))
                )
                .unwrap_err()
                .message
                .contains("saved account")
        );
    }

    #[test]
    fn codex_destination_rejects_duplicate_rollouts_even_in_other_dates() {
        let temp = tempfile::tempdir().unwrap();
        let expected = Location {
            root: temp.path().to_owned(),
            relative: "sessions/2026/09/17/rollout-time-conversation.jsonl".into(),
        };
        let duplicate = Location {
            root: temp.path().to_owned(),
            relative: "sessions/2026/09/16/rollout-other-conversation.jsonl".into(),
        };
        let storage = Storage { remote: None };
        storage
            .check_codex_destination(&expected, "conversation")
            .unwrap();
        install_local(&duplicate, b"{}\n").unwrap();
        assert!(
            storage
                .check_codex_destination(&expected, "conversation")
                .is_err()
        );
        fs::remove_file(duplicate.path()).unwrap();
        install_local(&expected, b"{}\n").unwrap();
        storage
            .check_codex_destination(&expected, "conversation")
            .unwrap();
    }

    #[test]
    fn codex_transcript_rejects_wrong_identity_folder_and_partial_records() {
        // Reuse a plain record; validation depends solely on its explicit identity.
        let mut source = super::super::tests::test_record("codex-test");
        source.kind = AgentKind::CODEX;
        source.agent_session_id = Some("conversation".into());
        source.cwd = "/repo".into();
        let good = format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":"conversation","cwd":"/repo"}})
        )
        .into_bytes();
        validate_conversation(&good, &source).unwrap();
        for bytes in [
            b"{}\n".as_slice(),
            &good[..good.len() - 1],
            b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"other\",\"cwd\":\"/repo\"}}\n",
        ] {
            assert!(validate_conversation(bytes, &source).is_err());
        }
        source.cwd = "/other".into();
        assert!(validate_conversation(&good, &source).is_err());
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
