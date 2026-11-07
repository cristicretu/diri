use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use diri_proto::{
    BlobChunk, BlobHasParams, BlobHasResult, BlobPutParams, BlobReadParams, CheckpointFile,
    CheckpointManifest, CheckpointPrepareParams, CheckpointStageResult, MoveAbortParams,
    MoveCommitParams, MovePhase, MoveRecord, ProviderKind,
};
use sha2::{Digest, Sha256};

use crate::accounts::now_seconds;
use crate::config::{
    NodePaths, atomic_json, hex_decode, hex_encode, random_hex, set_owner_directory,
};
use crate::error::{NodeError, NodeResult};

const CHECKPOINT_VERSION: u32 = 1;
const MAX_FILES: usize = 100_000;
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const DEFAULT_CHUNK_BYTES: u64 = 512 * 1024;
const MAX_CHUNK_BYTES: u64 = 1024 * 1024;

pub struct CheckpointStore {
    node_id: String,
    paths: NodePaths,
    upload_lock: Mutex<()>,
    /// Serialises every load-transition-save of a move record so two
    /// concurrent requests can never both pass the same guard.
    move_lock: Mutex<()>,
}

/// An event that may move a checkpoint's [`MoveRecord`] to a new phase.
#[derive(Clone, Copy, Debug)]
enum MoveEvent<'a> {
    /// The target restores the checkpoint into its quarantine.
    Staged,
    /// The target activated the provider state; it can resume the session.
    TargetReady,
    /// Either side records the move under `lease_id`.
    Commit { lease_id: &'a str },
    /// Either side gives the move up.
    Abort,
}

/// The single guarded transition table for checkpoint moves.
///
/// Source records are `Prepared` until they commit or abort. Target records
/// walk `Transferring -> Staged -> TargetReady -> Committed`; a target may
/// only commit once the restore is staged *and* its provider state is
/// active. `Committed` and `Aborted` are terminal. `Ok(None)` means the event
/// is an idempotent retry that leaves the record untouched.
fn next_move_phase(record: &MoveRecord, event: MoveEvent<'_>) -> NodeResult<Option<MovePhase>> {
    use MovePhase::{Aborted, Committed, Prepared, Staged, TargetReady, Transferring};
    let phase = record.phase;
    let refused = |what: &str| {
        Err(NodeError::Conflict(format!(
            "{what} is not allowed while the move is {phase:?}"
        )))
    };
    match event {
        MoveEvent::Staged => match phase {
            Transferring => Ok(Some(Staged)),
            Staged | TargetReady => Ok(None),
            Prepared | Committed | Aborted => refused("staging"),
        },
        MoveEvent::TargetReady => match phase {
            Staged => Ok(Some(TargetReady)),
            TargetReady => Ok(None),
            Prepared | Transferring | Committed | Aborted => refused("activating"),
        },
        MoveEvent::Commit { lease_id } => match phase {
            Prepared | TargetReady => Ok(Some(Committed)),
            Committed if record.lease_id.as_deref() == Some(lease_id) => Ok(None),
            Committed => Err(NodeError::Conflict(
                "checkpoint already committed under another lease".into(),
            )),
            Aborted => Err(NodeError::Conflict("an aborted move cannot commit".into())),
            Transferring | Staged => refused("committing"),
        },
        MoveEvent::Abort => match phase {
            Committed => Err(NodeError::Conflict(
                "a committed move needs a new reverse move, not abort".into(),
            )),
            Aborted => Ok(None),
            Prepared | Transferring | Staged | TargetReady => Ok(Some(Aborted)),
        },
    }
}

impl CheckpointStore {
    pub fn new(paths: NodePaths, node_id: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            paths,
            upload_lock: Mutex::new(()),
            move_lock: Mutex::new(()),
        }
    }

    pub fn prepare(
        &self,
        params: CheckpointPrepareParams,
        provider_config_home: &Path,
    ) -> NodeResult<CheckpointManifest> {
        let workspace = fs::canonicalize(&params.workspace_root).map_err(|error| {
            NodeError::BadRequest(format!("workspace is not accessible: {error}"))
        })?;
        if !workspace.is_dir() {
            return Err(NodeError::BadRequest("workspace is not a directory".into()));
        }
        let checkpoint_id = format!("cp-{}", random_hex(16)?);
        let mut files = Vec::new();
        let mut excluded = Vec::new();
        let mut total = 0_u64;
        self.walk_workspace(
            &workspace,
            &workspace,
            &mut files,
            &mut excluded,
            &mut total,
        )?;
        let provider_state = params.provider_session_id.as_deref().and_then(|id| {
            find_provider_state(provider_config_home, params.provider, id).and_then(|path| {
                self.store_file(&path, Path::new("provider/session.jsonl"))
                    .ok()
            })
        });
        let manifest = CheckpointManifest {
            version: CHECKPOINT_VERSION,
            checkpoint_id: checkpoint_id.clone(),
            source_node_id: self.node_id.clone(),
            session_id: params.session_id.clone(),
            provider: params.provider,
            profile_id: params.profile_id,
            workspace_root: workspace.to_string_lossy().into_owned(),
            provider_session_id: params.provider_session_id,
            mode: params.mode,
            created_at: now_seconds(),
            files,
            provider_state,
            excluded,
        };
        self.save_manifest(&manifest)?;
        self.save_move(&MoveRecord {
            checkpoint_id,
            session_id: params.session_id,
            source_node_id: self.node_id.clone(),
            target_node_id: None,
            phase: MovePhase::Prepared,
            lease_id: None,
            reason: None,
            updated_at: now_seconds(),
        })?;
        Ok(manifest)
    }

    pub fn put_manifest(&self, manifest: &CheckpointManifest) -> NodeResult<()> {
        validate_manifest(manifest)?;
        if manifest.source_node_id == self.node_id {
            return Err(NodeError::Conflict(
                "source manifest already belongs to this node".into(),
            ));
        }
        let _moves = self.lock_moves()?;
        // A repeated put may only refresh a transfer that has not progressed;
        // it must never resurrect a staged, committed or aborted move.
        match self.load_move(&manifest.checkpoint_id) {
            Ok(existing) if existing.phase != MovePhase::Transferring => {
                return Err(NodeError::Conflict(format!(
                    "checkpoint manifest cannot be replaced while the move is {:?}",
                    existing.phase
                )));
            }
            Ok(_) | Err(NodeError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        self.save_manifest(manifest)?;
        self.save_move(&MoveRecord {
            checkpoint_id: manifest.checkpoint_id.clone(),
            session_id: manifest.session_id.clone(),
            source_node_id: manifest.source_node_id.clone(),
            target_node_id: Some(self.node_id.clone()),
            phase: MovePhase::Transferring,
            lease_id: None,
            reason: None,
            updated_at: now_seconds(),
        })
    }

    pub fn missing_blobs(&self, params: BlobHasParams) -> NodeResult<BlobHasResult> {
        let mut missing = Vec::new();
        for digest in params.digests {
            validate_digest(&digest)?;
            if !self.blob_path(&digest).is_file() {
                missing.push(digest);
            }
        }
        Ok(BlobHasResult { missing })
    }

    pub fn read_blob(&self, params: BlobReadParams) -> NodeResult<BlobChunk> {
        validate_digest(&params.digest)?;
        let path = self.blob_path(&params.digest);
        let mut file = File::open(&path)
            .map_err(|_| NodeError::NotFound(format!("blob `{}`", params.digest)))?;
        let size = file.metadata()?.len();
        if params.offset > size {
            return Err(NodeError::BadRequest("blob offset exceeds its size".into()));
        }
        let requested = if params.max_bytes == 0 {
            DEFAULT_CHUNK_BYTES
        } else {
            params.max_bytes.min(MAX_CHUNK_BYTES)
        };
        let count = usize::try_from(requested.min(size - params.offset)).unwrap_or(0);
        let mut bytes = vec![0_u8; count];
        file.seek(SeekFrom::Start(params.offset))?;
        file.read_exact(&mut bytes)?;
        Ok(BlobChunk {
            digest: params.digest,
            offset: params.offset,
            hex: hex_encode(&bytes),
            eof: params.offset + u64::try_from(count).unwrap_or(u64::MAX) == size,
        })
    }

    pub fn put_blob(&self, params: BlobPutParams) -> NodeResult<bool> {
        let _upload = self
            .upload_lock
            .lock()
            .map_err(|_| NodeError::Protocol("checkpoint upload lock is poisoned".into()))?;
        validate_digest(&params.digest)?;
        let bytes = hex_decode(&params.hex)?;
        if bytes.len() > usize::try_from(MAX_CHUNK_BYTES).unwrap_or(usize::MAX) {
            return Err(NodeError::BadRequest("blob chunk is too large".into()));
        }
        let expected_size = self.expected_blob_size(&params.digest)?;
        let final_path = self.blob_path(&params.digest);
        if final_path.is_file() {
            if fs::metadata(&final_path)?.len() != expected_size
                || sha256_file(&final_path)? != params.digest
            {
                return Err(NodeError::Conflict(format!(
                    "blob `{}` has unexpected contents",
                    params.digest
                )));
            }
            return Ok(true);
        }
        let partial = self.paths.blobs.join(format!(".{}.partial", params.digest));
        let current = fs::metadata(&partial).map_or(0, |metadata| metadata.len());
        if current != params.offset {
            return Err(NodeError::Conflict(format!(
                "blob offset mismatch: expected {current}, got {}",
                params.offset
            )));
        }
        let received = current.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        if received > expected_size || received > MAX_FILE_BYTES {
            return Err(NodeError::BadRequest(format!(
                "blob exceeds its declared size of {expected_size} bytes"
            )));
        }
        if params.eof && received != expected_size {
            return Err(NodeError::BadRequest(format!(
                "blob ended at {received} bytes; expected {expected_size}"
            )));
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&partial)?;
        file.write_all(&bytes)?;
        file.sync_data()?;
        if !params.eof {
            return Ok(false);
        }
        let actual = sha256_file(&partial)?;
        if actual != params.digest {
            let _ = fs::remove_file(&partial);
            return Err(NodeError::BadRequest(format!(
                "blob digest mismatch: expected {}, received {actual}",
                params.digest
            )));
        }
        fs::rename(partial, final_path)?;
        Ok(true)
    }

    pub fn manifest(&self, checkpoint_id: &str) -> NodeResult<CheckpointManifest> {
        self.load_manifest(checkpoint_id)
    }

    pub fn stage(
        &self,
        checkpoint_id: &str,
        provider_config_home: &Path,
    ) -> NodeResult<CheckpointStageResult> {
        let manifest = self.load_manifest(checkpoint_id)?;
        let _moves = self.lock_moves()?;
        let movement = self.load_move(checkpoint_id)?;
        // Guard before touching the disk: an aborted or committed move must
        // never be restored (or re-committed under a new lease) again.
        next_move_phase(&movement, MoveEvent::Staged)?;
        let destination = self.paths.restores.join(checkpoint_id);
        if !destination.exists() {
            let temporary = self.staging_path(checkpoint_id);
            if temporary.exists() {
                return Err(NodeError::Conflict(
                    "checkpoint already has a staging operation".into(),
                ));
            }
            fs::create_dir_all(&temporary)?;
            set_owner_directory(&temporary)?;
            let result = (|| {
                for entry in &manifest.files {
                    self.restore_file(&temporary, entry)?;
                }
                if let Some(provider_state) = &manifest.provider_state {
                    self.restore_file(&temporary.join(".dirijor"), provider_state)?;
                }
                fs::rename(&temporary, &destination)?;
                Ok::<_, NodeError>(())
            })();
            if let Err(error) = result {
                // The partial restore is ours alone and never touched the live
                // workspace; drop it so a retry is not refused as a
                // concurrent staging operation.
                let _ = fs::remove_dir_all(&temporary);
                return Err(error);
            }
        }
        let movement = self.advance_move(movement, MoveEvent::Staged)?;
        self.activate_provider_state(&manifest, provider_config_home)?;
        self.advance_move(movement, MoveEvent::TargetReady)?;
        Ok(CheckpointStageResult {
            checkpoint_id: checkpoint_id.to_owned(),
            quarantine_path: destination.to_string_lossy().into_owned(),
            ready: true,
        })
    }

    pub fn commit(&self, params: MoveCommitParams) -> NodeResult<MoveRecord> {
        if params.lease_id.trim().is_empty() {
            return Err(NodeError::BadRequest("move lease cannot be empty".into()));
        }
        let _moves = self.lock_moves()?;
        let mut movement = self.load_move(&params.checkpoint_id)?;
        if next_move_phase(
            &movement,
            MoveEvent::Commit {
                lease_id: &params.lease_id,
            },
        )?
        .is_none()
        {
            return Ok(movement);
        }
        movement.target_node_id = Some(params.target_node_id);
        movement.lease_id = Some(params.lease_id.clone());
        self.advance_move(
            movement,
            MoveEvent::Commit {
                lease_id: &params.lease_id,
            },
        )
    }

    pub fn abort(&self, params: MoveAbortParams) -> NodeResult<MoveRecord> {
        let _moves = self.lock_moves()?;
        let mut movement = self.load_move(&params.checkpoint_id)?;
        let first_abort = next_move_phase(&movement, MoveEvent::Abort)?.is_some();
        // A failed or interrupted restore may have left its private staging
        // directory behind; it never reached the quarantine, so drop it.
        let staging = self.staging_path(&params.checkpoint_id);
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        if !first_abort {
            return Ok(movement);
        }
        movement.reason = Some(params.reason);
        self.advance_move(movement, MoveEvent::Abort)
    }

    pub fn rollback_provider_state(
        &self,
        manifest: &CheckpointManifest,
        provider_config_home: &Path,
    ) -> NodeResult<()> {
        let Some(state) = &manifest.provider_state else {
            return Ok(());
        };
        let Some(destination) = provider_state_destination(manifest, provider_config_home)? else {
            return Ok(());
        };
        if destination.is_file() && sha256_file(&destination)? == state.digest {
            fs::remove_file(destination)?;
        }
        Ok(())
    }

    pub fn pending_moves(&self) -> usize {
        fs::read_dir(&self.paths.moves)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| fs::read(entry.path()).ok())
            .filter_map(|bytes| serde_json::from_slice::<MoveRecord>(&bytes).ok())
            .filter(|record| !matches!(record.phase, MovePhase::Committed | MovePhase::Aborted))
            .count()
    }

    fn walk_workspace(
        &self,
        root: &Path,
        directory: &Path,
        files: &mut Vec<CheckpointFile>,
        excluded: &mut Vec<String>,
        total: &mut u64,
    ) -> NodeResult<()> {
        let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let relative = path.strip_prefix(root).expect("walk stays below root");
            let file_type = entry.file_type()?;
            if should_exclude(relative, file_type.is_dir()) || file_type.is_symlink() {
                excluded.push(relative.to_string_lossy().into_owned());
                continue;
            }
            if file_type.is_dir() {
                self.walk_workspace(root, &path, files, excluded, total)?;
                continue;
            }
            if !file_type.is_file() {
                excluded.push(relative.to_string_lossy().into_owned());
                continue;
            }
            if files.len() >= MAX_FILES {
                return Err(NodeError::BadRequest(format!(
                    "workspace exceeds {MAX_FILES} files"
                )));
            }
            let size = entry.metadata()?.len();
            if size > MAX_FILE_BYTES {
                excluded.push(relative.to_string_lossy().into_owned());
                continue;
            }
            *total = total.saturating_add(size);
            if *total > MAX_TOTAL_BYTES {
                return Err(NodeError::BadRequest(
                    "workspace checkpoint exceeds 8 GiB".into(),
                ));
            }
            files.push(self.store_file(&path, relative)?);
        }
        Ok(())
    }

    fn store_file(&self, source: &Path, relative: &Path) -> NodeResult<CheckpointFile> {
        validate_relative_path(relative)?;
        let digest = sha256_file(source)?;
        let destination = self.blob_path(&digest);
        if !destination.exists() {
            let temporary = self
                .paths
                .blobs
                .join(format!(".{digest}.tmp-{}", std::process::id()));
            fs::copy(source, &temporary)?;
            let copied_digest = sha256_file(&temporary)?;
            if copied_digest != digest {
                return Err(NodeError::Conflict(
                    "file changed while checkpointing; retry at a turn boundary".into(),
                ));
            }
            fs::rename(temporary, &destination)?;
        }
        #[cfg(unix)]
        let unix_mode = Some(fs::metadata(source)?.permissions().mode() & 0o777);
        #[cfg(not(unix))]
        let unix_mode = None;
        Ok(CheckpointFile {
            path: relative.to_string_lossy().into_owned(),
            digest,
            size: fs::metadata(source)?.len(),
            unix_mode,
        })
    }

    fn restore_file(&self, root: &Path, entry: &CheckpointFile) -> NodeResult<()> {
        let relative = Path::new(&entry.path);
        validate_relative_path(relative)?;
        validate_digest(&entry.digest)?;
        let source = self.blob_path(&entry.digest);
        if !source.is_file()
            || fs::metadata(&source)?.len() != entry.size
            || sha256_file(&source)? != entry.digest
        {
            return Err(NodeError::Conflict(format!(
                "checkpoint blob `{}` is missing or corrupt",
                entry.digest
            )));
        }
        let destination = root.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, &destination)?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode {
            fs::set_permissions(destination, fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    }

    fn activate_provider_state(
        &self,
        manifest: &CheckpointManifest,
        config_home: &Path,
    ) -> NodeResult<()> {
        let Some(state) = &manifest.provider_state else {
            return Ok(());
        };
        let destination = provider_state_destination(manifest, config_home)?.ok_or_else(|| {
            NodeError::Protocol("provider state has no provider session id".into())
        })?;
        let session_id = manifest
            .provider_session_id
            .as_deref()
            .expect("destination required it");
        if destination.exists() {
            if sha256_file(&destination)? == state.digest {
                return Ok(());
            }
            return Err(NodeError::Conflict(format!(
                "provider session `{session_id}` already exists with different contents"
            )));
        }
        let parent = destination
            .parent()
            .expect("provider state destination has a parent");
        fs::create_dir_all(parent)?;
        set_owner_directory(parent)?;
        let temporary = parent.join(format!(".{session_id}.staging-{}", std::process::id()));
        fs::copy(self.blob_path(&state.digest), &temporary)?;
        if sha256_file(&temporary)? != state.digest {
            return Err(NodeError::Conflict(
                "provider state blob is missing or corrupt".into(),
            ));
        }
        fs::rename(temporary, destination)?;
        Ok(())
    }

    fn save_manifest(&self, manifest: &CheckpointManifest) -> NodeResult<()> {
        validate_manifest(manifest)?;
        atomic_json(&self.manifest_path(&manifest.checkpoint_id), manifest)?;
        Ok(())
    }

    fn expected_blob_size(&self, digest: &str) -> NodeResult<u64> {
        let mut expected = None;
        for entry in fs::read_dir(&self.paths.checkpoints)? {
            let entry = entry?;
            if !entry.file_type()?.is_file()
                || entry.path().extension().and_then(|value| value.to_str()) != Some("json")
            {
                continue;
            }
            let manifest: CheckpointManifest = serde_json::from_slice(&fs::read(entry.path())?)?;
            validate_manifest(&manifest)?;
            for file in manifest.files.iter().chain(&manifest.provider_state) {
                if file.digest != digest {
                    continue;
                }
                if expected.is_some_and(|size| size != file.size) {
                    return Err(NodeError::Conflict(format!(
                        "blob `{digest}` has conflicting declared sizes"
                    )));
                }
                expected = Some(file.size);
            }
        }
        expected.ok_or_else(|| {
            NodeError::BadRequest(format!(
                "blob `{digest}` is not referenced by an accepted checkpoint manifest"
            ))
        })
    }

    fn load_manifest(&self, checkpoint_id: &str) -> NodeResult<CheckpointManifest> {
        validate_checkpoint_id(checkpoint_id)?;
        let path = self.manifest_path(checkpoint_id);
        let bytes = fs::read(path)
            .map_err(|_| NodeError::NotFound(format!("checkpoint `{checkpoint_id}`")))?;
        let manifest: CheckpointManifest = serde_json::from_slice(&bytes)?;
        validate_manifest(&manifest)?;
        Ok(manifest)
    }

    fn lock_moves(&self) -> NodeResult<std::sync::MutexGuard<'_, ()>> {
        self.move_lock
            .lock()
            .map_err(|_| NodeError::Protocol("checkpoint move lock is poisoned".into()))
    }

    /// Applies `event` through [`next_move_phase`] and persists the result.
    fn advance_move(
        &self,
        mut movement: MoveRecord,
        event: MoveEvent<'_>,
    ) -> NodeResult<MoveRecord> {
        if let Some(phase) = next_move_phase(&movement, event)? {
            movement.phase = phase;
            movement.updated_at = now_seconds();
            self.save_move(&movement)?;
        }
        Ok(movement)
    }

    fn staging_path(&self, checkpoint_id: &str) -> PathBuf {
        self.paths
            .restores
            .join(format!(".{checkpoint_id}.staging"))
    }

    fn save_move(&self, movement: &MoveRecord) -> NodeResult<()> {
        atomic_json(&self.move_path(&movement.checkpoint_id), movement)?;
        Ok(())
    }

    fn load_move(&self, checkpoint_id: &str) -> NodeResult<MoveRecord> {
        validate_checkpoint_id(checkpoint_id)?;
        let bytes = fs::read(self.move_path(checkpoint_id))
            .map_err(|_| NodeError::NotFound(format!("move `{checkpoint_id}`")))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn blob_path(&self, digest: &str) -> PathBuf {
        self.paths.blobs.join(digest)
    }

    fn manifest_path(&self, checkpoint_id: &str) -> PathBuf {
        self.paths.checkpoints.join(format!("{checkpoint_id}.json"))
    }

    fn move_path(&self, checkpoint_id: &str) -> PathBuf {
        self.paths.moves.join(format!("{checkpoint_id}.json"))
    }
}

fn provider_state_destination(
    manifest: &CheckpointManifest,
    config_home: &Path,
) -> NodeResult<Option<PathBuf>> {
    let Some(session_id) = manifest.provider_session_id.as_deref() else {
        return Ok(None);
    };
    if session_id.is_empty() || session_id.contains(['/', '\\']) {
        return Err(NodeError::BadRequest("invalid provider session id".into()));
    }
    Ok(Some(match manifest.provider {
        ProviderKind::Claude => config_home
            .join("projects/-dirijor-imports")
            .join(format!("{session_id}.jsonl")),
        ProviderKind::Codex => config_home
            .join("sessions/dirijor-imports")
            .join(format!("rollout-dirijor-{session_id}.jsonl")),
    }))
}

fn validate_manifest(manifest: &CheckpointManifest) -> NodeResult<()> {
    if manifest.version != CHECKPOINT_VERSION {
        return Err(NodeError::Protocol(format!(
            "unsupported checkpoint version {}",
            manifest.version
        )));
    }
    validate_checkpoint_id(&manifest.checkpoint_id)?;
    if manifest.files.len() > MAX_FILES {
        return Err(NodeError::BadRequest(
            "checkpoint has too many files".into(),
        ));
    }
    let mut total = 0_u64;
    for entry in manifest.files.iter().chain(&manifest.provider_state) {
        validate_relative_path(Path::new(&entry.path))?;
        validate_digest(&entry.digest)?;
        if entry.size > MAX_FILE_BYTES {
            return Err(NodeError::BadRequest("checkpoint file is too large".into()));
        }
        total = total.saturating_add(entry.size);
    }
    if total > MAX_TOTAL_BYTES {
        return Err(NodeError::BadRequest("checkpoint is too large".into()));
    }
    Ok(())
}

fn validate_checkpoint_id(id: &str) -> NodeResult<()> {
    if !id.starts_with("cp-")
        || id.len() != 35
        || !id[3..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(NodeError::BadRequest("invalid checkpoint id".into()));
    }
    Ok(())
}

fn validate_digest(digest: &str) -> NodeResult<()> {
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(NodeError::BadRequest("invalid SHA-256 digest".into()));
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> NodeResult<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(NodeError::BadRequest(format!(
            "unsafe checkpoint path `{}`",
            path.display()
        )));
    }
    Ok(())
}

fn should_exclude(path: &Path, directory: bool) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if directory
        && matches!(
            name,
            ".git" | ".ssh" | ".codex" | ".claude" | "node_modules" | "target" | ".build"
        )
    {
        return true;
    }
    name == ".env"
        || name.starts_with(".env.")
        || matches!(
            name,
            "auth.json" | ".credentials.json" | "credentials.json" | "id_rsa" | "id_ed25519"
        )
}

fn sha256_file(path: &Path) -> NodeResult<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex_encode(&digest.finalize()))
}

fn find_provider_state(
    config_home: &Path,
    provider: ProviderKind,
    session_id: &str,
) -> Option<PathBuf> {
    if session_id.is_empty() || session_id.contains(['/', '\\']) {
        return None;
    }
    let root = match provider {
        ProviderKind::Claude => config_home.join("projects"),
        ProviderKind::Codex => config_home.join("sessions"),
    };
    let mut candidates = Vec::new();
    find_matching_jsonl(&root, session_id, &mut candidates);
    candidates.into_iter().max_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    })
}

fn find_matching_jsonl(root: &Path, session_id: &str, result: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            find_matching_jsonl(&path, session_id, result);
        } else if file_type.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            && path
                .file_stem()
                .is_some_and(|stem| stem.to_string_lossy().contains(session_id))
        {
            result.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::TransferMode;

    /// A second node that accepted `manifest` and holds every blob the
    /// source stored, as after a completed blob transfer.
    fn receive(
        root: &Path,
        source: &NodePaths,
        manifest: &CheckpointManifest,
    ) -> (NodePaths, CheckpointStore) {
        let paths = NodePaths::for_root(root.join("target-node"));
        paths.create_layout().expect("target layout");
        let target = CheckpointStore::new(paths.clone(), "target");
        target
            .put_manifest(manifest)
            .expect("target accepts manifest");
        for entry in fs::read_dir(&source.blobs).expect("source blobs").flatten() {
            fs::copy(entry.path(), paths.blobs.join(entry.file_name())).expect("copy blob");
        }
        (paths, target)
    }

    /// A source checkpoint of a one-file workspace and its receiving target.
    fn moved_checkpoint(
        root: &Path,
    ) -> (
        NodePaths,
        CheckpointStore,
        NodePaths,
        CheckpointStore,
        CheckpointManifest,
    ) {
        let paths = NodePaths::for_root(root.join("source-node"));
        paths.create_layout().expect("layout");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::write(workspace.join("README.md"), "hello\n").expect("readme");
        let source = CheckpointStore::new(paths.clone(), "source");
        let manifest = source
            .prepare(
                CheckpointPrepareParams {
                    session_id: "session-1".into(),
                    provider: ProviderKind::Claude,
                    profile_id: "work".into(),
                    workspace_root: workspace.to_string_lossy().into_owned(),
                    provider_session_id: None,
                    mode: TransferMode::Move,
                },
                root,
            )
            .expect("checkpoint");
        let (target_paths, target) = receive(root, &paths, &manifest);
        (paths, source, target_paths, target, manifest)
    }

    fn commit_params(manifest: &CheckpointManifest, lease: &str) -> MoveCommitParams {
        MoveCommitParams {
            checkpoint_id: manifest.checkpoint_id.clone(),
            target_node_id: "target".into(),
            lease_id: lease.into(),
        }
    }

    fn abort_params(manifest: &CheckpointManifest) -> MoveAbortParams {
        MoveAbortParams {
            checkpoint_id: manifest.checkpoint_id.clone(),
            reason: "test".into(),
        }
    }

    fn phase(store: &CheckpointStore, manifest: &CheckpointManifest) -> MovePhase {
        store
            .load_move(&manifest.checkpoint_id)
            .expect("move")
            .phase
    }

    #[test]
    fn target_walks_staged_then_ready_before_it_may_commit() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let home = directory.path().join("home");
        let (_, source, _, target, manifest) = moved_checkpoint(directory.path());
        assert_eq!(phase(&target, &manifest), MovePhase::Transferring);
        assert!(matches!(
            target.commit(commit_params(&manifest, "lease-1")),
            Err(NodeError::Conflict(_))
        ));
        target.stage(&manifest.checkpoint_id, &home).expect("stage");
        assert_eq!(phase(&target, &manifest), MovePhase::TargetReady);
        // A retried stage is idempotent and keeps the phase.
        target
            .stage(&manifest.checkpoint_id, &home)
            .expect("restage");
        assert_eq!(phase(&target, &manifest), MovePhase::TargetReady);
        target
            .commit(commit_params(&manifest, "lease-1"))
            .expect("target commit");
        source
            .commit(commit_params(&manifest, "lease-1"))
            .expect("source commit");
        assert_eq!(phase(&source, &manifest), MovePhase::Committed);
        // Same lease is idempotent; a new lease is a conflict.
        target
            .commit(commit_params(&manifest, "lease-1"))
            .expect("idempotent commit");
        assert!(matches!(
            target.commit(commit_params(&manifest, "lease-2")),
            Err(NodeError::Conflict(_))
        ));
    }

    #[test]
    fn terminal_moves_can_never_be_staged_or_recommitted() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let home = directory.path().join("home");
        let (_, _, _, target, manifest) = moved_checkpoint(directory.path());
        target.abort(abort_params(&manifest)).expect("abort");
        assert!(matches!(
            target.stage(&manifest.checkpoint_id, &home),
            Err(NodeError::Conflict(_))
        ));
        assert!(matches!(
            target.put_manifest(&manifest),
            Err(NodeError::Conflict(_))
        ));
        assert!(matches!(
            target.commit(commit_params(&manifest, "lease-1")),
            Err(NodeError::Conflict(_))
        ));
        assert_eq!(phase(&target, &manifest), MovePhase::Aborted);

        let directory = tempfile::tempdir().expect("temporary directory");
        let (_, _, _, target, manifest) = moved_checkpoint(directory.path());
        target.stage(&manifest.checkpoint_id, &home).expect("stage");
        target
            .commit(commit_params(&manifest, "lease-1"))
            .expect("commit");
        assert!(matches!(
            target.stage(&manifest.checkpoint_id, &home),
            Err(NodeError::Conflict(_))
        ));
        assert!(matches!(
            target.abort(abort_params(&manifest)),
            Err(NodeError::Conflict(_))
        ));
        let record = target.load_move(&manifest.checkpoint_id).expect("move");
        assert_eq!(record.phase, MovePhase::Committed);
        assert_eq!(record.lease_id.as_deref(), Some("lease-1"));
    }

    #[test]
    fn source_cannot_stage_its_own_prepared_checkpoint() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let (_, source, _, _, manifest) = moved_checkpoint(directory.path());
        assert!(matches!(
            source.stage(&manifest.checkpoint_id, directory.path()),
            Err(NodeError::Conflict(_))
        ));
        assert_eq!(phase(&source, &manifest), MovePhase::Prepared);
    }

    #[test]
    fn a_failed_restore_leaves_no_staging_and_can_be_retried() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let home = directory.path().join("home");
        let (source_paths, _, target_paths, target, manifest) = moved_checkpoint(directory.path());
        let blob = target_paths.blobs.join(&manifest.files[0].digest);
        fs::write(&blob, "corrupt").expect("corrupt blob");
        assert!(matches!(
            target.stage(&manifest.checkpoint_id, &home),
            Err(NodeError::Conflict(_))
        ));
        assert!(!target.staging_path(&manifest.checkpoint_id).exists());
        assert_eq!(phase(&target, &manifest), MovePhase::Transferring);
        fs::copy(source_paths.blobs.join(&manifest.files[0].digest), &blob).expect("repair");
        target
            .stage(&manifest.checkpoint_id, &home)
            .expect("retry after repair");
        assert_eq!(phase(&target, &manifest), MovePhase::TargetReady);
    }

    #[test]
    fn abort_removes_a_leftover_staging_directory() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let (_, _, _, target, manifest) = moved_checkpoint(directory.path());
        let staging = target.staging_path(&manifest.checkpoint_id);
        fs::create_dir_all(staging.join("partial")).expect("interrupted staging");
        target.abort(abort_params(&manifest)).expect("abort");
        assert!(!staging.exists());
    }

    #[test]
    fn a_crash_after_restore_records_the_staged_phase_on_retry() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let (_, _, target_paths, target, manifest) = moved_checkpoint(directory.path());
        fs::create_dir_all(target_paths.restores.join(&manifest.checkpoint_id))
            .expect("restored before crash");
        target
            .stage(&manifest.checkpoint_id, directory.path())
            .expect("stage");
        assert_eq!(phase(&target, &manifest), MovePhase::TargetReady);
    }

    #[test]
    fn checkpoint_is_content_addressed_and_excludes_secrets() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = NodePaths::for_root(directory.path().join("node"));
        paths.create_layout().expect("layout");
        let workspace = directory.path().join("workspace");
        fs::create_dir_all(workspace.join("src")).expect("workspace");
        fs::write(workspace.join("src/main.rs"), "fn main() {}\n").expect("source");
        fs::write(workspace.join(".env"), "SECRET=never-copy\n").expect("secret");
        let config_home = directory.path().join("codex-home");
        fs::create_dir_all(&config_home).expect("config home");
        let store = CheckpointStore::new(paths.clone(), "local");
        let manifest = store
            .prepare(
                CheckpointPrepareParams {
                    session_id: "session-1".into(),
                    provider: ProviderKind::Codex,
                    profile_id: "personal".into(),
                    workspace_root: workspace.to_string_lossy().into_owned(),
                    provider_session_id: None,
                    mode: TransferMode::Move,
                },
                &config_home,
            )
            .expect("checkpoint");
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].path, "src/main.rs");
        assert_eq!(manifest.excluded, vec![".env"]);
        assert!(paths.blobs.join(&manifest.files[0].digest).is_file());
    }

    #[test]
    fn restore_stays_quarantined_until_a_separate_commit() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = NodePaths::for_root(directory.path().join("node"));
        paths.create_layout().expect("layout");
        let workspace = directory.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::write(workspace.join("README.md"), "hello\n").expect("readme");
        let store = CheckpointStore::new(paths.clone(), "forge");
        let manifest = store
            .prepare(
                CheckpointPrepareParams {
                    session_id: "session-1".into(),
                    provider: ProviderKind::Claude,
                    profile_id: "work".into(),
                    workspace_root: workspace.to_string_lossy().into_owned(),
                    provider_session_id: None,
                    mode: TransferMode::Fork,
                },
                directory.path(),
            )
            .expect("checkpoint");
        let provider_home = directory.path().join("provider-home");
        fs::create_dir_all(&provider_home).expect("provider home");
        let (_target_paths, target) = receive(directory.path(), &paths, &manifest);
        let staged = target
            .stage(&manifest.checkpoint_id, &provider_home)
            .expect("stage");
        assert!(
            Path::new(&staged.quarantine_path)
                .join("README.md")
                .is_file()
        );
        assert_eq!(
            fs::read_to_string(workspace.join("README.md")).expect("live file"),
            "hello\n"
        );
    }

    #[test]
    fn relative_paths_reject_traversal() {
        assert!(validate_relative_path(Path::new("src/main.rs")).is_ok());
        assert!(validate_relative_path(Path::new("../auth.json")).is_err());
        assert!(validate_relative_path(Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn blob_upload_must_match_an_accepted_manifest_size() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = NodePaths::for_root(directory.path().join("node"));
        paths.create_layout().expect("layout");
        let store = CheckpointStore::new(paths.clone(), "target");
        let digest = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        store
            .put_manifest(&CheckpointManifest {
                version: CHECKPOINT_VERSION,
                checkpoint_id: "cp-0123456789abcdef0123456789abcdef".into(),
                source_node_id: "source".into(),
                session_id: "session-1".into(),
                provider: ProviderKind::Codex,
                profile_id: "personal".into(),
                workspace_root: "/workspace".into(),
                provider_session_id: None,
                mode: TransferMode::Move,
                created_at: 0,
                files: vec![CheckpointFile {
                    path: "src/main.rs".into(),
                    digest: digest.into(),
                    size: 3,
                    unix_mode: None,
                }],
                provider_state: None,
                excluded: Vec::new(),
            })
            .expect("manifest");

        let oversized = store.put_blob(BlobPutParams {
            digest: digest.into(),
            offset: 0,
            hex: "61626364".into(),
            eof: true,
        });
        assert!(matches!(oversized, Err(NodeError::BadRequest(_))));
        assert!(!paths.blobs.join(format!(".{digest}.partial")).exists());

        assert!(
            store
                .put_blob(BlobPutParams {
                    digest: digest.into(),
                    offset: 0,
                    hex: "616263".into(),
                    eof: true,
                })
                .expect("bounded blob completes")
        );
    }

    #[test]
    fn staging_installs_provider_state_inside_the_target_profile_home() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = NodePaths::for_root(directory.path().join("node"));
        paths.create_layout().expect("layout");
        let workspace = directory.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        let source_home = directory.path().join("source-codex");
        let rollout = source_home.join("sessions/2026/08/02/rollout-test-thread-1.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).expect("sessions");
        fs::write(&rollout, "{\"type\":\"session_meta\"}\n").expect("rollout");
        let store = CheckpointStore::new(paths.clone(), "local");
        let manifest = store
            .prepare(
                CheckpointPrepareParams {
                    session_id: "session-1".into(),
                    provider: ProviderKind::Codex,
                    profile_id: "work".into(),
                    workspace_root: workspace.to_string_lossy().into_owned(),
                    provider_session_id: Some("thread-1".into()),
                    mode: TransferMode::Move,
                },
                &source_home,
            )
            .expect("checkpoint");
        assert!(manifest.provider_state.is_some());
        let target_home = directory.path().join("target-codex");
        let (_target_paths, target) = receive(directory.path(), &paths, &manifest);
        target
            .stage(&manifest.checkpoint_id, &target_home)
            .expect("stage");
        assert!(
            target_home
                .join("sessions/dirijor-imports/rollout-dirijor-thread-1.jsonl")
                .is_file()
        );
    }
}
