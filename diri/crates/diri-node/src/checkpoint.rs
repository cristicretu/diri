use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

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
}

impl CheckpointStore {
    pub fn new(paths: NodePaths, node_id: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            paths,
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
        validate_digest(&params.digest)?;
        let bytes = hex_decode(&params.hex)?;
        if bytes.len() > usize::try_from(MAX_CHUNK_BYTES).unwrap_or(usize::MAX) {
            return Err(NodeError::BadRequest("blob chunk is too large".into()));
        }
        let final_path = self.blob_path(&params.digest);
        if final_path.is_file() {
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
        let destination = self.paths.restores.join(checkpoint_id);
        if destination.exists() {
            self.activate_provider_state(&manifest, provider_config_home)?;
            return Ok(CheckpointStageResult {
                checkpoint_id: checkpoint_id.to_owned(),
                quarantine_path: destination.to_string_lossy().into_owned(),
                ready: true,
            });
        }
        let temporary = self
            .paths
            .restores
            .join(format!(".{checkpoint_id}.staging"));
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
        if result.is_err() {
            // Leave the quarantine for inspection; never mutate the live
            // workspace when a restore is incomplete.
            return result.map(|_| unreachable!());
        }
        self.activate_provider_state(&manifest, provider_config_home)?;
        let mut movement = self.load_move(checkpoint_id)?;
        movement.phase = MovePhase::Staged;
        movement.updated_at = now_seconds();
        self.save_move(&movement)?;
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
        let mut movement = self.load_move(&params.checkpoint_id)?;
        if movement.phase == MovePhase::Aborted {
            return Err(NodeError::Conflict("an aborted move cannot commit".into()));
        }
        if movement.phase == MovePhase::Committed {
            if movement.lease_id.as_deref() == Some(&params.lease_id) {
                return Ok(movement);
            }
            return Err(NodeError::Conflict(
                "checkpoint already committed under another lease".into(),
            ));
        }
        movement.phase = MovePhase::Committed;
        movement.target_node_id = Some(params.target_node_id);
        movement.lease_id = Some(params.lease_id);
        movement.updated_at = now_seconds();
        self.save_move(&movement)?;
        Ok(movement)
    }

    pub fn abort(&self, params: MoveAbortParams) -> NodeResult<MoveRecord> {
        let mut movement = self.load_move(&params.checkpoint_id)?;
        if movement.phase == MovePhase::Committed {
            return Err(NodeError::Conflict(
                "a committed move needs a new reverse move, not abort".into(),
            ));
        }
        movement.phase = MovePhase::Aborted;
        movement.reason = Some(params.reason);
        movement.updated_at = now_seconds();
        self.save_move(&movement)?;
        Ok(movement)
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
        if !source.is_file() || sha256_file(&source)? != entry.digest {
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

    fn load_manifest(&self, checkpoint_id: &str) -> NodeResult<CheckpointManifest> {
        validate_checkpoint_id(checkpoint_id)?;
        let path = self.manifest_path(checkpoint_id);
        let bytes = fs::read(path)
            .map_err(|_| NodeError::NotFound(format!("checkpoint `{checkpoint_id}`")))?;
        let manifest: CheckpointManifest = serde_json::from_slice(&bytes)?;
        validate_manifest(&manifest)?;
        Ok(manifest)
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
        let staged = store
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
        let store = CheckpointStore::new(paths, "local");
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
        store
            .stage(&manifest.checkpoint_id, &target_home)
            .expect("stage");
        assert!(
            target_home
                .join("sessions/dirijor-imports/rollout-dirijor-thread-1.jsonl")
                .is_file()
        );
    }
}
